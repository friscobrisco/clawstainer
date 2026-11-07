//! Host side: drive the operations a guest agent serves.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use super::messages::{hex, stream, Control, OpenRequest};
use super::mux::{Channel, Event, Session};

const READ_CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
}

impl ExitInfo {
    /// Shell-style exit code: the code, or 128 + signal.
    pub fn exit_code(&self) -> i32 {
        match (self.code, self.signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            (None, None) => -1,
        }
    }
}

/// Health check. Returns the agent's version.
pub fn health(session: &Session) -> Result<String> {
    let ch = session.open(&OpenRequest::Health)?;
    while let Some(ev) = ch.recv() {
        match ev {
            Event::Control(Control::Health { version, .. }) => {
                ch.close();
                return Ok(version);
            }
            Event::Control(Control::Error { message }) => bail!(message),
            _ => {}
        }
    }
    bail!("agent closed the health channel without answering")
}

/// Pump `input` into the channel's STDIN, then send EOF. Runs on its own
/// thread; stops quietly if the channel closes first.
fn spawn_input<R: Read + Send + 'static>(ch: Channel, mut input: R) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if ch.send(stream::STDIN, &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = ch.send_eof(stream::STDIN);
    });
}

/// Read events until `Exit`, routing STDOUT/STDERR to the writers.
fn collect_until_exit(
    ch: &Channel,
    stdout: &mut dyn Write,
    mut stderr: Option<&mut dyn Write>,
) -> Result<ExitInfo> {
    while let Some(ev) = ch.recv() {
        match ev {
            Event::Data { stream: stream::STDOUT, bytes } => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Event::Data { stream: stream::STDERR, bytes } => {
                if let Some(w) = stderr.as_mut() {
                    w.write_all(&bytes)?;
                    w.flush()?;
                }
            }
            Event::Control(Control::Exit { code, signal, timed_out, duration_ms }) => {
                ch.close();
                return Ok(ExitInfo { code, signal, timed_out, duration_ms });
            }
            Event::Control(Control::Error { message }) => {
                ch.close();
                bail!(message);
            }
            _ => {}
        }
    }
    bail!("connection to the guest agent was lost before the command finished")
}

/// Run an `Exec` request, streaming output as it arrives.
/// `stdin = None` closes the process's stdin immediately.
pub fn exec<R: Read + Send + 'static>(
    session: &Session,
    req: &OpenRequest,
    stdin: Option<R>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitInfo> {
    if !matches!(req, OpenRequest::Exec { .. }) {
        bail!("exec needs an Exec request");
    }
    let ch = session.open(req)?;
    match stdin {
        Some(input) => spawn_input(ch.clone(), input),
        None => ch.send_eof(stream::STDIN)?,
    }
    collect_until_exit(&ch, stdout, Some(stderr))
}

/// Run a `Shell` request with the given input and output (no terminal
/// handling). `resize` receives `(cols, rows)` updates to forward.
pub fn pty_session<R: Read + Send + 'static>(
    session: &Session,
    req: &OpenRequest,
    input: R,
    output: &mut dyn Write,
    resize: Option<std::sync::mpsc::Receiver<(u16, u16)>>,
) -> Result<ExitInfo> {
    if !matches!(req, OpenRequest::Shell { .. }) {
        bail!("pty_session needs a Shell request");
    }
    let ch = session.open(req)?;
    spawn_input(ch.clone(), input);
    if let Some(rx) = resize {
        let ch = ch.clone();
        std::thread::spawn(move || {
            for (cols, rows) in rx {
                if ch.send_control(&Control::Resize { cols, rows }).is_err() {
                    return;
                }
            }
        });
    }
    collect_until_exit(&ch, output, None)
}

/// Current size of the terminal on `fd`, if it is one.
pub fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize struct.
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) };
    if r < 0 || ws.ws_col == 0 {
        None
    } else {
        Some((ws.ws_col, ws.ws_row))
    }
}

/// Puts a terminal in raw mode and restores it on drop.
pub struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    pub fn enable(fd: i32) -> io::Result<RawMode> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr/tcsetattr on a caller-provided fd.
        if unsafe { libc::tcgetattr(fd, &mut t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

/// Interactive shell on the caller's terminal: raw mode, live resize, and
/// output to stdout. Returns when the remote shell exits.
pub fn interactive_shell(session: &Session, user: &str, command: Option<String>) -> Result<ExitInfo> {
    let (cols, rows) = terminal_size(libc::STDOUT_FILENO).unwrap_or((80, 24));
    let term = std::env::var("TERM").ok();
    let req = OpenRequest::Shell {
        user: user.to_string(),
        command,
        cols,
        rows,
        term,
        workdir: None,
        env: Default::default(),
    };

    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let _raw = if is_tty { Some(RawMode::enable(libc::STDIN_FILENO)?) } else { None };

    // Poll for size changes instead of installing a SIGWINCH handler.
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut last = (cols, rows);
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                if let Some(now) = terminal_size(libc::STDOUT_FILENO) {
                    if now != last {
                        last = now;
                        if tx.send(now).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }

    let mut out = io::stdout();
    let result = pty_session(session, &req, io::stdin(), &mut out, Some(rx));
    stop.store(true, Ordering::SeqCst);
    result
}

/// Upload a local file to `remote`. Returns `(size, sha256)` as confirmed by
/// the agent.
pub fn upload(session: &Session, local: &Path, remote: &str, mode: Option<u32>) -> Result<(u64, String)> {
    let mut file = File::open(local).with_context(|| format!("cannot open {}", local.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("{} is not a regular file", local.display());
    }
    let mode = mode.or(Some(meta.permissions().mode() & 0o7777));
    let ch = session.open(&OpenRequest::Upload { path: remote.to_string(), size: meta.len(), mode })?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut sent_all = true;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                ch.close();
                return Err(e).context("read failed");
            }
        };
        hasher.update(&buf[..n]);
        if ch.send(stream::STDIN, &buf[..n]).is_err() {
            // The agent rejected the upload; its reason is queued below.
            sent_all = false;
            break;
        }
    }
    if sent_all {
        let _ = ch.send_eof(stream::STDIN);
        let _ = ch.send_control(&Control::Checksum { sha256: hex(&hasher.finalize()) });
    }

    while let Some(ev) = ch.recv() {
        match ev {
            Event::Control(Control::UploadDone { size, sha256 }) => {
                ch.close();
                return Ok((size, sha256));
            }
            Event::Control(Control::Error { message }) => {
                ch.close();
                bail!("upload rejected: {message}");
            }
            _ => {}
        }
    }
    bail!("connection closed before the upload was confirmed")
}

/// Download `remote` to a local path, atomically and checksum-verified.
pub fn download(session: &Session, remote: &str, local: &Path) -> Result<(u64, String)> {
    let ch = session.open(&OpenRequest::Download { path: remote.to_string() })?;

    let (size, mode) = match ch.recv() {
        Some(Event::Control(Control::FileMeta { size, mode })) => (size, mode),
        Some(Event::Control(Control::Error { message })) => {
            ch.close();
            bail!("download failed: {message}");
        }
        Some(other) => {
            ch.close();
            bail!("unexpected first message from agent: {other:?}");
        }
        None => bail!("connection closed before the download started"),
    };

    let name = local
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", local.display()))?;
    let dir = match local.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let tmp_path = dir.join(format!(".{}.claw-{:08x}.part", name.to_string_lossy(), rand::random::<u32>()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .with_context(|| format!("cannot create {}", tmp_path.display()))?;

    let result = (|| -> Result<String> {
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let mut got_eof = false;
        while let Some(ev) = ch.recv() {
            match ev {
                Event::Data { stream: stream::STDOUT, bytes } => {
                    received += bytes.len() as u64;
                    hasher.update(&bytes);
                    file.write_all(&bytes)?;
                }
                Event::Eof { stream: stream::STDOUT } => got_eof = true,
                Event::Control(Control::Checksum { sha256 }) => {
                    if !got_eof {
                        bail!("checksum arrived before the end of the data");
                    }
                    if received != size {
                        bail!("expected {size} bytes, received {received} (file changed during download?)");
                    }
                    let actual = hex(&hasher.finalize());
                    if actual != sha256 {
                        bail!("checksum mismatch: agent sent {sha256}, received data hashes to {actual}");
                    }
                    return Ok(actual);
                }
                Event::Control(Control::Error { message }) => bail!("download failed: {message}"),
                _ => {}
            }
        }
        bail!("connection closed before the download finished")
    })();
    ch.close();

    match result {
        Ok(sha) => {
            file.sync_all()?;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
            fs::rename(&tmp_path, local)
                .with_context(|| format!("cannot move into {}", local.display()))?;
            Ok((size, sha))
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// Relay one local TCP connection to `127.0.0.1:port` in the guest. Returns
/// when both directions are finished.
pub fn forward(session: &Session, port: u16, local: TcpStream) -> Result<()> {
    let ch = session.open(&OpenRequest::PortForward { port })?;
    spawn_input(ch.clone(), local.try_clone()?);

    let mut writer = &local;
    let mut result = Ok(());
    while let Some(ev) = ch.recv() {
        match ev {
            Event::Data { stream: stream::STDOUT, bytes } => {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
            Event::Eof { stream: stream::STDOUT } => {
                let _ = local.shutdown(Shutdown::Write);
            }
            Event::Control(Control::Error { message }) => {
                result = Err(anyhow!(message));
                break;
            }
            _ => {}
        }
    }
    // Unblock the input thread if the guest side ended first.
    let _ = local.shutdown(Shutdown::Both);
    ch.close();
    result
}
