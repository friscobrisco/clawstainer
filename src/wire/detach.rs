//! Execs that survive a dropped connection.
//!
//! An `Exec` with a `session_id` is not tied to its channel. Its stdout and
//! stderr go into bounded ring buffers (oldest bytes dropped first), so the
//! process never stalls when nobody is attached. Any channel can `Attach`
//! with the byte offsets it already has and receive exactly what follows,
//! then the exit status. If no channel is attached for `keep_secs`, the
//! process group is killed. Once the exit status has been delivered to a
//! client the session is forgotten; if it exits with nobody attached, its
//! output and status are kept for `keep_secs` and then discarded.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::messages::{stream, Control};
use super::mux::{Channel, Event};

/// Every detachable exec in this agent, by session id.
#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<String, Arc<Detached>>>,
}

impl Registry {
    fn get(&self, id: &str) -> Option<Arc<Detached>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }
}

/// Bounded output buffer that remembers absolute byte offsets.
struct Ring {
    data: VecDeque<u8>,
    /// Absolute offset of `data[0]`.
    start: u64,
    cap: usize,
    eof: bool,
}

impl Ring {
    fn new(cap: usize) -> Ring {
        Ring { data: VecDeque::new(), start: 0, cap, eof: false }
    }

    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    fn push(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
        let excess = self.data.len().saturating_sub(self.cap);
        if excess > 0 {
            self.data.drain(..excess);
            self.start += excess as u64;
        }
    }

    /// Up to `max` bytes from absolute `from`, plus how many bytes before
    /// `from` were already dropped.
    fn read(&self, from: u64, max: usize) -> (Vec<u8>, u64) {
        let lost = self.start.saturating_sub(from);
        let from = from.max(self.start);
        let skip = (from - self.start) as usize;
        let bytes = self.data.iter().skip(skip).take(max).copied().collect();
        (bytes, lost)
    }
}

struct State {
    out: [Ring; 2],
    exit: Option<Control>,
    stdin: Option<ChildStdin>,
    /// Bumped on every attach and detach, so stale timers do nothing.
    generation: u64,
    attached: Option<Channel>,
}

pub struct Detached {
    pgid: i32,
    keep: Duration,
    state: Mutex<State>,
    changed: Condvar,
}

fn kill_group(pgid: i32, signal: i32) {
    if pgid > 0 {
        // SAFETY: plain syscall on the session's own process group.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

impl Detached {
    fn ring_index(tag: u8) -> usize {
        if tag == stream::STDERR {
            1
        } else {
            0
        }
    }

    /// Start the timer that kills (if running) or forgets (if exited) the
    /// session unless someone attaches first.
    fn arm_expiry(self: &Arc<Self>, registry: &Arc<Registry>, id: &str) {
        let generation = self.state.lock().unwrap().generation;
        let (me, registry, id) = (self.clone(), registry.clone(), id.to_string());
        std::thread::spawn(move || {
            std::thread::sleep(me.keep);
            let st = me.state.lock().unwrap();
            if st.generation != generation || st.attached.is_some() {
                return;
            }
            if st.exit.is_none() {
                eprintln!("claw-agent: session {id} not reattached in {:?}, killing it", me.keep);
                kill_group(me.pgid, libc::SIGKILL);
                // Its exit will re-arm expiry to forget it.
            } else {
                drop(st);
                registry.sessions.lock().unwrap().remove(&id);
            }
        });
    }
}

/// Start a detachable exec and attach `ch` to it.
pub fn start(
    registry: &Arc<Registry>,
    ch: Channel,
    session_id: String,
    mut cmd: Command,
    timeout_secs: u64,
    keep: Duration,
    buffer_bytes: usize,
) {
    if registry.get(&session_id).is_some() {
        return ch.close_with_error(format!("session {session_id} already exists"));
    }
    let started = Instant::now();
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ch.close_with_error(format!("failed to start command: {e}")),
    };
    let pgid = child.id() as i32;
    let det = Arc::new(Detached {
        pgid,
        keep,
        state: Mutex::new(State {
            out: [Ring::new(buffer_bytes), Ring::new(buffer_bytes)],
            exit: None,
            stdin: child.stdin.take(),
            generation: 0,
            attached: None,
        }),
        changed: Condvar::new(),
    });
    registry.sessions.lock().unwrap().insert(session_id.clone(), det.clone());

    // Output pumps never block on the network: they only fill the rings.
    let mut pumps = Vec::new();
    for (reader, tag) in [
        (Box::new(child.stdout.take().expect("piped")) as Box<dyn Read + Send>, stream::STDOUT),
        (Box::new(child.stderr.take().expect("piped")) as Box<dyn Read + Send>, stream::STDERR),
    ] {
        let det = det.clone();
        pumps.push(std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut st = det.state.lock().unwrap();
                        st.out[Detached::ring_index(tag)].push(&buf[..n]);
                        det.changed.notify_all();
                    }
                }
            }
            det.state.lock().unwrap().out[Detached::ring_index(tag)].eof = true;
            det.changed.notify_all();
        }));
    }

    // Waiter: records the exit, then lets the session expire.
    {
        let (det, registry, id) = (det.clone(), registry.clone(), session_id.clone());
        std::thread::spawn(move || {
            let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            if timeout_secs > 0 {
                let timed_out = timed_out.clone();
                std::thread::spawn(move || {
                    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        stop_rx.recv_timeout(Duration::from_secs(timeout_secs))
                    {
                        timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                        kill_group(pgid, libc::SIGKILL);
                    }
                });
            }
            let status = child.wait();
            drop(stop_tx);
            // Let the pumps drain (bounded), so output precedes the exit.
            let deadline = Instant::now() + Duration::from_secs(2);
            for p in pumps {
                while !p.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let exit = match status {
                Ok(s) => Control::Exit {
                    code: s.code(),
                    signal: s.signal(),
                    timed_out: timed_out.load(std::sync::atomic::Ordering::SeqCst),
                    duration_ms: started.elapsed().as_millis() as u64,
                },
                Err(e) => Control::Error { message: format!("failed to wait for process: {e}") },
            };
            let detached = {
                let mut st = det.state.lock().unwrap();
                st.exit = Some(exit);
                st.stdin = None;
                st.out[0].eof = true;
                st.out[1].eof = true;
                det.changed.notify_all();
                st.attached.is_none()
            };
            if detached {
                det.arm_expiry(&registry, &id);
            }
        });
    }

    attach(registry, ch, &session_id, 0, 0);
}

/// Attach `ch` to an existing session, replaying from the given offsets.
pub fn attach(registry: &Arc<Registry>, ch: Channel, session_id: &str, out_from: u64, err_from: u64) {
    let Some(det) = registry.get(session_id) else {
        return ch.close_with_error(format!("no session {session_id} (it ended or expired)"));
    };
    // The newest attachment wins; an older one is told why it lost.
    let my_generation = {
        let mut st = det.state.lock().unwrap();
        if let Some(old) = st.attached.take() {
            old.close_with_error("attached from another connection");
        }
        st.generation += 1;
        st.attached = Some(ch.clone());
        det.changed.notify_all();
        st.generation
    };

    // Input from this attachment.
    {
        let (det, ch) = (det.clone(), ch.clone());
        std::thread::spawn(move || {
            while let Some(ev) = ch.recv() {
                match ev {
                    Event::Data { stream: stream::STDIN, bytes } => {
                        // Never hold the lock across a blocking write: the
                        // output pumps need it while the process writes.
                        let taken = {
                            let mut st = det.state.lock().unwrap();
                            if st.generation != my_generation {
                                return;
                            }
                            st.stdin.take()
                        };
                        if let Some(mut s) = taken {
                            if s.write_all(&bytes).is_ok() {
                                let mut st = det.state.lock().unwrap();
                                if st.exit.is_none() {
                                    st.stdin = Some(s);
                                }
                            }
                        }
                    }
                    Event::Eof { stream: stream::STDIN } => {
                        let mut st = det.state.lock().unwrap();
                        if st.generation == my_generation {
                            st.stdin = None;
                        }
                    }
                    Event::Control(Control::Signal { signal }) => kill_group(det.pgid, signal),
                    _ => {}
                }
            }
            // The channel closed: detach and start the grace period.
            det.changed.notify_all();
        });
    }

    // Output to this attachment, replaying then following live.
    let registry = registry.clone();
    let session_id = session_id.to_string();
    std::thread::spawn(move || {
        let mut cursors = [out_from, err_from];
        let tags = [stream::STDOUT, stream::STDERR];
        loop {
            let mut to_send = Vec::new();
            let mut exit = None;
            {
                let mut st = det.state.lock().unwrap();
                loop {
                    if st.generation != my_generation || ch.is_closed() {
                        break;
                    }
                    for i in 0..2 {
                        let (bytes, lost) = st.out[i].read(cursors[i], 60 * 1024);
                        if lost > 0 {
                            to_send.push(Err((tags[i], lost)));
                            cursors[i] += lost;
                        }
                        if !bytes.is_empty() {
                            cursors[i] += bytes.len() as u64;
                            to_send.push(Ok((tags[i], bytes)));
                        }
                    }
                    let drained = (0..2).all(|i| st.out[i].eof && cursors[i] >= st.out[i].end());
                    if drained {
                        exit = st.exit.clone();
                    }
                    if !to_send.is_empty() || exit.is_some() {
                        break;
                    }
                    st = det
                        .changed
                        .wait_timeout(st, Duration::from_millis(500))
                        .unwrap()
                        .0;
                }
            }
            if to_send.is_empty() && exit.is_none() {
                break; // detached or superseded
            }
            for item in to_send {
                let sent = match item {
                    Ok((tag, bytes)) => ch.send(tag, &bytes),
                    Err((tag, lost)) => ch.send_control(&Control::OutputLost { stream: tag, bytes: lost }),
                };
                if sent.is_err() {
                    break;
                }
            }
            if let Some(exit) = exit {
                let _ = ch.send_eof(stream::STDOUT);
                let _ = ch.send_eof(stream::STDERR);
                if ch.send_control(&exit).is_ok() {
                    // The status reached a client: the session is finished.
                    let mut st = det.state.lock().unwrap();
                    st.attached = None;
                    st.generation += 1;
                    drop(st);
                    registry.sessions.lock().unwrap().remove(&session_id);
                    ch.close();
                    return;
                }
                ch.close();
                break;
            }
        }
        // Detach if this attachment is still the current one.
        let mut st = det.state.lock().unwrap();
        if st.generation == my_generation {
            st.attached = None;
            st.generation += 1;
            drop(st);
            det.arm_expiry(&registry, &session_id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn ring_keeps_offsets_and_reports_loss() {
        let mut r = Ring::new(4);
        r.push(b"abc");
        assert_eq!(r.read(0, 10), (b"abc".to_vec(), 0));
        r.push(b"def");
        // "ab" were dropped; data is "cdef" starting at offset 2.
        assert_eq!((r.start, r.end()), (2, 6));
        assert_eq!(r.read(0, 10), (b"cdef".to_vec(), 2));
        assert_eq!(r.read(4, 10), (b"ef".to_vec(), 0));
        assert_eq!(r.read(4, 1), (b"e".to_vec(), 0));
        assert_eq!(r.read(6, 10), (Vec::new(), 0));
    }
}
