//! Channel multiplexer over a single byte stream.
//!
//! A [`Session`] owns one transport. A reader thread demultiplexes incoming
//! frames into per-channel inboxes; writers serialize whole frames through a
//! shared lock. A heartbeat thread pings the peer and closes the session if
//! the peer goes quiet.
//!
//! Only the client opens channels. The server receives `(Channel,
//! OpenRequest)` pairs from the receiver returned by [`Session::server`].

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use super::frame::{self, Frame, Kind, MAX_PAYLOAD, WINDOW};
use super::messages::{Control, OpenRequest};

/// Missed heartbeat intervals before a session is declared dead.
const MISSED_HEARTBEATS: u32 = 3;

/// Pending opens the server will queue before refusing new channels.
const ACCEPT_BACKLOG: usize = 64;

#[derive(Debug, Clone)]
pub struct Config {
    /// How often to ping. The session closes after three silent intervals.
    pub heartbeat: Duration,
    /// Maximum concurrently open channels (server side).
    pub max_channels: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            heartbeat: Duration::from_secs(15),
            max_channels: 64,
        }
    }
}

/// Something delivered to a channel.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Data { stream: u8, bytes: Vec<u8> },
    Eof { stream: u8 },
    Control(Control),
}

/// A transport split into its read half, write half, and a way to tear it
/// down from another thread (which unblocks the reader).
pub struct Parts {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub shutdown: Box<dyn Fn() + Send + Sync>,
}

pub trait Transport: Read + Write + Sized {
    fn into_parts(self) -> io::Result<Parts>;
}

impl Transport for UnixStream {
    fn into_parts(self) -> io::Result<Parts> {
        let reader = self.try_clone()?;
        let killer = self.try_clone()?;
        Ok(Parts {
            reader: Box::new(reader),
            writer: Box::new(self),
            shutdown: Box::new(move || {
                let _ = killer.shutdown(Shutdown::Both);
            }),
        })
    }
}

impl Transport for TcpStream {
    fn into_parts(self) -> io::Result<Parts> {
        let reader = self.try_clone()?;
        let killer = self.try_clone()?;
        Ok(Parts {
            reader: Box::new(reader),
            writer: Box::new(self),
            shutdown: Box::new(move || {
                let _ = killer.shutdown(Shutdown::Both);
            }),
        })
    }
}

struct Inbox {
    events: VecDeque<Event>,
    /// Data bytes queued but not yet taken by the application.
    buffered: usize,
    closed: bool,
}

struct Credit {
    avail: u64,
    closed: bool,
}

struct ChanState {
    inbox: Mutex<Inbox>,
    inbox_cv: Condvar,
    credit: Mutex<Credit>,
    credit_cv: Condvar,
    /// Bytes the application consumed that have not been granted back yet.
    unacked: Mutex<u32>,
}

impl ChanState {
    fn new() -> Arc<ChanState> {
        Arc::new(ChanState {
            inbox: Mutex::new(Inbox { events: VecDeque::new(), buffered: 0, closed: false }),
            inbox_cv: Condvar::new(),
            credit: Mutex::new(Credit { avail: WINDOW as u64, closed: false }),
            credit_cv: Condvar::new(),
            unacked: Mutex::new(0),
        })
    }

    fn push(&self, ev: Event) {
        let mut inbox = self.inbox.lock().unwrap();
        if !inbox.closed {
            inbox.events.push_back(ev);
            self.inbox_cv.notify_all();
        }
    }

    fn mark_closed(&self) {
        {
            let mut inbox = self.inbox.lock().unwrap();
            inbox.closed = true;
            self.inbox_cv.notify_all();
        }
        let mut credit = self.credit.lock().unwrap();
        credit.closed = true;
        self.credit_cv.notify_all();
    }
}

struct Inner {
    writer: Mutex<Box<dyn Write + Send>>,
    channels: Mutex<HashMap<u32, Arc<ChanState>>>,
    accept_tx: Mutex<Option<mpsc::SyncSender<(Channel, OpenRequest)>>>,
    closed: AtomicBool,
    last_rx: Mutex<Instant>,
    next_id: AtomicU32,
    shutdown: Box<dyn Fn() + Send + Sync>,
    config: Config,
}

impl Inner {
    fn send_frame(&self, f: &Frame) -> io::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(broken());
        }
        let res = {
            let mut w = self.writer.lock().unwrap();
            frame::write_frame(&mut **w, f)
        };
        if res.is_err() {
            self.close();
        }
        res
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        (self.shutdown)();
        let chans: Vec<Arc<ChanState>> =
            self.channels.lock().unwrap().drain().map(|(_, c)| c).collect();
        for c in chans {
            c.mark_closed();
        }
        // Dropping the sender ends the server's accept loop.
        self.accept_tx.lock().unwrap().take();
    }

    fn get(&self, id: u32) -> Option<Arc<ChanState>> {
        self.channels.lock().unwrap().get(&id).cloned()
    }

    /// Refuse a channel the peer tried to open.
    fn refuse(&self, id: u32, message: String) {
        if let Ok(payload) = serde_json::to_vec(&Control::Error { message }) {
            let _ = self.send_frame(&Frame::new(id, Kind::Control, payload));
        }
        let _ = self.send_frame(&Frame::new(id, Kind::Close, Vec::new()));
    }

    /// Route one incoming frame. `Err` means the peer broke the protocol and
    /// the session must close.
    fn dispatch(self: &Arc<Self>, f: Frame) -> Result<(), String> {
        match f.kind {
            Kind::Ping => {
                let _ = self.send_frame(&Frame::new(0, Kind::Pong, f.payload));
                Ok(())
            }
            Kind::Pong => Ok(()),
            Kind::Open => self.on_open(f),
            Kind::Data => {
                let (&tag, bytes) = f
                    .payload
                    .split_first()
                    .ok_or("data frame without a stream tag")?;
                let Some(st) = self.get(f.channel) else { return Ok(()) };
                let mut inbox = st.inbox.lock().unwrap();
                if inbox.closed {
                    return Ok(());
                }
                inbox.buffered += bytes.len();
                if inbox.buffered > WINDOW as usize {
                    return Err(format!("peer overran the window on channel {}", f.channel));
                }
                inbox.events.push_back(Event::Data { stream: tag, bytes: bytes.to_vec() });
                st.inbox_cv.notify_all();
                Ok(())
            }
            Kind::Eof => {
                let tag = *f.payload.first().ok_or("eof frame without a stream tag")?;
                if let Some(st) = self.get(f.channel) {
                    st.push(Event::Eof { stream: tag });
                }
                Ok(())
            }
            Kind::Control => {
                let msg: Control = serde_json::from_slice(&f.payload)
                    .map_err(|e| format!("bad control message: {e}"))?;
                if let Some(st) = self.get(f.channel) {
                    st.push(Event::Control(msg));
                }
                Ok(())
            }
            Kind::Window => {
                let bytes: [u8; 4] = f
                    .payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| "window frame must carry 4 bytes")?;
                let inc = u32::from_be_bytes(bytes) as u64;
                if let Some(st) = self.get(f.channel) {
                    let mut credit = st.credit.lock().unwrap();
                    credit.avail += inc;
                    if credit.avail > (WINDOW as u64) * 2 {
                        return Err("peer granted more credit than the window".into());
                    }
                    st.credit_cv.notify_all();
                }
                Ok(())
            }
            Kind::Close => {
                let st = self.channels.lock().unwrap().remove(&f.channel);
                if let Some(st) = st {
                    st.mark_closed();
                }
                Ok(())
            }
        }
    }

    fn on_open(self: &Arc<Self>, f: Frame) -> Result<(), String> {
        let tx = match self.accept_tx.lock().unwrap().as_ref() {
            Some(tx) => tx.clone(),
            None => return Err("peer tried to open a channel on a client session".into()),
        };
        if f.channel == 0 {
            return Err("channel 0 is reserved".into());
        }
        let req: OpenRequest = match serde_json::from_slice(&f.payload) {
            Ok(r) => r,
            Err(e) => {
                self.refuse(f.channel, format!("invalid open request: {e}"));
                return Ok(());
            }
        };
        let st = {
            let mut chans = self.channels.lock().unwrap();
            if chans.contains_key(&f.channel) {
                return Err(format!("channel {} opened twice", f.channel));
            }
            if chans.len() >= self.config.max_channels {
                drop(chans);
                self.refuse(f.channel, "too many open channels".into());
                return Ok(());
            }
            let st = ChanState::new();
            chans.insert(f.channel, st.clone());
            st
        };
        let ch = Channel { id: f.channel, inner: self.clone(), state: st };
        if let Err(mpsc::TrySendError::Full((ch, _))) = tx.try_send((ch, req)) {
            ch.close_with_error("server busy");
        }
        Ok(())
    }
}

fn broken() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "channel closed")
}

/// One end of a multiplexed connection. Dropping it closes the transport.
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Connect as a client: run the handshake, then start the session.
    pub fn client<T: Transport>(mut transport: T, config: Config) -> io::Result<Session> {
        frame::client_handshake(&mut transport)?;
        let (s, _) = Session::start(transport.into_parts()?, config, false);
        Ok(s)
    }

    /// Accept as a server: read the client's preamble, answer it, and start.
    pub fn server<T: Transport>(
        mut transport: T,
        config: Config,
    ) -> io::Result<(Session, mpsc::Receiver<(Channel, OpenRequest)>)> {
        let line = frame::read_line_unbuffered(&mut transport, 64)?;
        if line != frame::PREAMBLE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "missing CLAW/2 preamble"));
        }
        Session::server_after_preamble(transport.into_parts()?, config)
    }

    /// For callers that already consumed the client's preamble (the agent
    /// sniffs it to support the legacy protocol on the same port).
    pub fn server_after_preamble(
        mut parts: Parts,
        config: Config,
    ) -> io::Result<(Session, mpsc::Receiver<(Channel, OpenRequest)>)> {
        parts.writer.write_all(frame::PREAMBLE)?;
        parts.writer.flush()?;
        let (s, rx) = Session::start(parts, config, true);
        Ok((s, rx.expect("server sessions always accept")))
    }

    fn start(
        parts: Parts,
        config: Config,
        accept: bool,
    ) -> (Session, Option<mpsc::Receiver<(Channel, OpenRequest)>>) {
        let (tx, rx) = if accept {
            let (tx, rx) = mpsc::sync_channel(ACCEPT_BACKLOG);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let heartbeat = config.heartbeat;
        let inner = Arc::new(Inner {
            writer: Mutex::new(parts.writer),
            channels: Mutex::new(HashMap::new()),
            accept_tx: Mutex::new(tx),
            closed: AtomicBool::new(false),
            last_rx: Mutex::new(Instant::now()),
            next_id: AtomicU32::new(1),
            shutdown: parts.shutdown,
            config,
        });

        let reader_inner = inner.clone();
        let mut reader = parts.reader;
        std::thread::spawn(move || {
            // Ends on EOF, a read error, or a protocol violation.
            while let Ok(Some(f)) = frame::read_frame(&mut *reader) {
                *reader_inner.last_rx.lock().unwrap() = Instant::now();
                if let Err(why) = reader_inner.dispatch(f) {
                    eprintln!("wire: protocol error, closing session: {why}");
                    break;
                }
            }
            reader_inner.close();
        });

        let weak: Weak<Inner> = Arc::downgrade(&inner);
        std::thread::spawn(move || heartbeat_loop(weak, heartbeat));

        (Session { inner }, rx)
    }

    /// Open a channel (client side).
    pub fn open(&self, req: &OpenRequest) -> io::Result<Channel> {
        if self.is_closed() {
            return Err(broken());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let st = ChanState::new();
        self.inner.channels.lock().unwrap().insert(id, st.clone());
        let payload = serde_json::to_vec(req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if let Err(e) = self.inner.send_frame(&Frame::new(id, Kind::Open, payload)) {
            self.inner.channels.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok(Channel { id, inner: self.inner.clone(), state: st })
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.inner.close();
    }

    /// Number of channels currently open on this session.
    pub fn open_channels(&self) -> usize {
        self.inner.channels.lock().unwrap().len()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.inner.close();
    }
}

fn heartbeat_loop(weak: Weak<Inner>, interval: Duration) {
    let mut nonce: u64 = 0;
    loop {
        std::thread::sleep(interval);
        let Some(inner) = weak.upgrade() else { return };
        if inner.closed.load(Ordering::SeqCst) {
            return;
        }
        let silent = inner.last_rx.lock().unwrap().elapsed();
        if silent > interval * MISSED_HEARTBEATS {
            eprintln!("wire: peer silent for {silent:?}, closing session");
            inner.close();
            return;
        }
        // Never block behind a stalled data write: if the writer is busy,
        // skip this ping. The silence check above still runs next time.
        let write_failed = match inner.writer.try_lock() {
            Ok(mut w) => {
                nonce += 1;
                let ping = Frame::new(0, Kind::Ping, nonce.to_be_bytes().to_vec());
                frame::write_frame(&mut **w, &ping).is_err()
            }
            Err(_) => false,
        };
        if write_failed {
            inner.close();
            return;
        }
    }
}

/// Returned by [`Channel::recv_timeout`] when the deadline passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout;

/// One logical stream inside a session. Cheap to clone; clones share state,
/// so a reader thread and a writer thread can each hold one.
#[derive(Clone)]
pub struct Channel {
    id: u32,
    inner: Arc<Inner>,
    state: Arc<ChanState>,
}

impl Channel {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Send bytes on a stream, blocking while the peer's window is full.
    /// Fails once the channel or session is closed.
    pub fn send(&self, stream: u8, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = {
                let mut credit = self.state.credit.lock().unwrap();
                loop {
                    if credit.closed {
                        return Err(broken());
                    }
                    if credit.avail > 0 {
                        break;
                    }
                    credit = self.state.credit_cv.wait(credit).unwrap();
                }
                let n = (credit.avail as usize).min(MAX_PAYLOAD - 1).min(data.len());
                credit.avail -= n as u64;
                n
            };
            let mut payload = Vec::with_capacity(n + 1);
            payload.push(stream);
            payload.extend_from_slice(&data[..n]);
            self.inner.send_frame(&Frame::new(self.id, Kind::Data, payload))?;
            data = &data[n..];
        }
        Ok(())
    }

    pub fn send_eof(&self, stream: u8) -> io::Result<()> {
        self.check_open()?;
        self.inner.send_frame(&Frame::new(self.id, Kind::Eof, vec![stream]))
    }

    pub fn send_control(&self, msg: &Control) -> io::Result<()> {
        self.check_open()?;
        let payload =
            serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.inner.send_frame(&Frame::new(self.id, Kind::Control, payload))
    }

    fn check_open(&self) -> io::Result<()> {
        if self.state.credit.lock().unwrap().closed {
            Err(broken())
        } else {
            Ok(())
        }
    }

    /// Next event, blocking. `None` once the channel is closed and drained.
    /// Events queued before a close are still delivered.
    pub fn recv(&self) -> Option<Event> {
        self.recv_inner(None).ok().flatten()
    }

    /// Like [`recv`](Self::recv) with a deadline.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Event>, Timeout> {
        self.recv_inner(Some(Instant::now() + timeout))
    }

    fn recv_inner(&self, deadline: Option<Instant>) -> Result<Option<Event>, Timeout> {
        let mut inbox = self.state.inbox.lock().unwrap();
        loop {
            if let Some(ev) = inbox.events.pop_front() {
                if let Event::Data { bytes, .. } = &ev {
                    inbox.buffered -= bytes.len();
                    let n = bytes.len() as u32;
                    drop(inbox);
                    self.consumed(n);
                }
                return Ok(Some(ev));
            }
            if inbox.closed {
                return Ok(None);
            }
            inbox = match deadline {
                None => self.state.inbox_cv.wait(inbox).unwrap(),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(Timeout);
                    }
                    self.state.inbox_cv.wait_timeout(inbox, d - now).unwrap().0
                }
            };
        }
    }

    /// Grant credit back once half the window has been consumed, so the
    /// peer is never stalled for long and grants stay infrequent.
    fn consumed(&self, n: u32) {
        let grant = {
            let mut unacked = self.state.unacked.lock().unwrap();
            *unacked += n;
            if *unacked >= WINDOW / 2 {
                std::mem::take(&mut *unacked)
            } else {
                0
            }
        };
        if grant > 0 && !self.is_closed() {
            let _ = self
                .inner
                .send_frame(&Frame::new(self.id, Kind::Window, grant.to_be_bytes().to_vec()));
        }
    }

    pub fn is_closed(&self) -> bool {
        self.state.inbox.lock().unwrap().closed
    }

    /// Close the channel. Safe to call more than once and from any clone.
    pub fn close(&self) {
        let removed = self.inner.channels.lock().unwrap().remove(&self.id).is_some();
        if removed {
            let _ = self.inner.send_frame(&Frame::new(self.id, Kind::Close, Vec::new()));
        }
        self.state.mark_closed();
    }

    /// Send an error message, then close.
    pub fn close_with_error(&self, message: impl Into<String>) {
        let _ = self.send_control(&Control::Error { message: message.into() });
        self.close();
    }
}
