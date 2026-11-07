//! Guest-agent side: serve channels opened by the host.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::messages::{hex, stream, Control, OpenRequest};
use super::mux::{Channel, Event, Session};
use super::pty;

/// How long to keep draining output after the main process exits. Output
/// from background processes that still hold the pipe is cut off after this.
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);

const READ_CHUNK: usize = 32 * 1024;

const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Largest file accepted by an upload or served by a download.
    pub max_transfer_bytes: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig { max_transfer_bytes: 2 * 1024 * 1024 * 1024 }
    }
}

/// Handle channels until the session closes. Each channel gets a thread.
pub fn serve(
    session: Session,
    incoming: mpsc::Receiver<(Channel, OpenRequest)>,
    config: ServerConfig,
) {
    for (ch, req) in incoming {
        let config = config.clone();
        std::thread::spawn(move || handle(ch, req, &config));
    }
    drop(session);
}

pub fn handle(ch: Channel, req: OpenRequest, config: &ServerConfig) {
    match req {
        OpenRequest::Health => {
            let _ = ch.send_control(&Control::Health {
                status: "ok".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            });
            ch.close();
        }
        OpenRequest::Exec { command, workdir, env, user, timeout_secs } => {
            let cmd = build_command(&command, &user, workdir.as_deref(), &env, None);
            exec(ch, cmd, timeout_secs);
        }
        OpenRequest::Shell { user, command, cols, rows, term, workdir, env } => {
            let term = term.unwrap_or_else(|| "xterm-256color".into());
            shell(ch, &user, command.as_deref(), cols, rows, &term, workdir.as_deref(), &env);
        }
        OpenRequest::Upload { path, size, mode } => {
            match upload(&ch, Path::new(&path), size, mode, config) {
                Ok((size, sha256)) => {
                    let _ = ch.send_control(&Control::UploadDone { size, sha256 });
                    ch.close();
                }
                Err(e) => ch.close_with_error(e),
            }
        }
        OpenRequest::Download { path } => {
            if let Err(e) = download(&ch, Path::new(&path), config) {
                ch.close_with_error(e);
            } else {
                ch.close();
            }
        }
        OpenRequest::PortForward { port } => port_forward(ch, port),
    }
}

fn home_for(user: &str) -> String {
    if user == "root" {
        "/root".into()
    } else {
        format!("/home/{user}")
    }
}

/// Build the command for `command` as `user`. With `command == None` this is
/// an interactive login shell (`shell` mode).
fn build_command(
    command: &str,
    user: &str,
    workdir: Option<&str>,
    env: &HashMap<String, String>,
    term: Option<&str>,
) -> Command {
    let mut cmd = if user == "root" {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    } else {
        let mut c = Command::new("su");
        c.args(["-", user, "-c", command]);
        c
    };
    apply_env(&mut cmd, user, workdir, env, term);
    cmd
}

fn login_shell(user: &str, workdir: Option<&str>, env: &HashMap<String, String>, term: &str) -> Command {
    let mut cmd = if user == "root" {
        let shell = if Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        let mut c = Command::new(shell);
        c.arg("-l");
        c
    } else {
        let mut c = Command::new("su");
        c.args(["-", user]);
        c
    };
    apply_env(&mut cmd, user, workdir, env, Some(term));
    cmd
}

fn apply_env(
    cmd: &mut Command,
    user: &str,
    workdir: Option<&str>,
    env: &HashMap<String, String>,
    term: Option<&str>,
) {
    let home = home_for(user);
    match workdir {
        Some(dir) => {
            cmd.current_dir(dir);
        }
        None if Path::new(&home).is_dir() => {
            cmd.current_dir(&home);
        }
        None => {}
    }
    cmd.env("HOME", &home)
        .env("USER", user)
        .env("PATH", SYSTEM_PATH)
        .env("LANG", "C.UTF-8")
        .env("TERM", term.unwrap_or("xterm-256color"));
    for (k, v) in env {
        cmd.env(k, v);
    }
}

fn kill_group(pgid: i32, signal: i32) {
    if pgid > 0 {
        // SAFETY: plain syscall; a stale pgid at worst returns ESRCH.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

/// Copy a reader onto a channel stream, then send EOF on it.
fn pump<R: Read>(mut r: R, ch: Channel, tag: u8, done: mpsc::Sender<()>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if ch.send(tag, &buf[..n]).is_err() {
                    let _ = done.send(());
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // EIO is how a PTY master reports that the slave side is gone.
            Err(_) => break,
        }
    }
    let _ = ch.send_eof(tag);
    let _ = done.send(());
}

/// Wait for `n` pumps to finish, but no longer than `OUTPUT_DRAIN`.
fn wait_pumps(done: &mpsc::Receiver<()>, n: usize) {
    let deadline = Instant::now() + OUTPUT_DRAIN;
    for _ in 0..n {
        let left = deadline.saturating_duration_since(Instant::now());
        if done.recv_timeout(left).is_err() {
            return;
        }
    }
}

fn exit_control(status: io::Result<ExitStatus>, timed_out: bool, started: Instant) -> Control {
    let duration_ms = started.elapsed().as_millis() as u64;
    match status {
        Ok(s) => Control::Exit { code: s.code(), signal: s.signal(), timed_out, duration_ms },
        Err(e) => Control::Error { message: format!("failed to wait for process: {e}") },
    }
}

/// Arm a timer that kills the process group after `secs` (0 = never).
/// Drop the returned sender to disarm it.
fn arm_timeout(pgid: i32, secs: u64, fired: Arc<AtomicBool>) -> Option<mpsc::Sender<()>> {
    if secs == 0 {
        return None;
    }
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(Duration::from_secs(secs)) {
            fired.store(true, Ordering::SeqCst);
            kill_group(pgid, libc::SIGKILL);
        }
    });
    Some(tx)
}

fn exec(ch: Channel, mut cmd: Command, timeout_secs: u64) {
    let started = Instant::now();
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ch.close_with_error(format!("failed to start command: {e}")),
    };
    let pgid = child.id() as i32;
    let finished = Arc::new(AtomicBool::new(false));

    let (done_tx, done_rx) = mpsc::channel();
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    {
        let (ch, tx) = (ch.clone(), done_tx.clone());
        std::thread::spawn(move || pump(out, ch, stream::STDOUT, tx));
    }
    {
        let (ch, tx) = (ch.clone(), done_tx);
        std::thread::spawn(move || pump(err, ch, stream::STDERR, tx));
    }

    // Input: stdin bytes, signals, and channel close.
    {
        let ch = ch.clone();
        let finished = finished.clone();
        let mut stdin = child.stdin.take();
        std::thread::spawn(move || {
            while let Some(ev) = ch.recv() {
                match ev {
                    Event::Data { stream: stream::STDIN, bytes } => {
                        if let Some(s) = stdin.as_mut() {
                            if s.write_all(&bytes).is_err() {
                                stdin = None;
                            }
                        }
                    }
                    Event::Eof { stream: stream::STDIN } => stdin = None,
                    Event::Control(Control::Signal { signal }) => kill_group(pgid, signal),
                    _ => {}
                }
            }
            // The client went away before the process finished.
            if !finished.load(Ordering::SeqCst) {
                kill_group(pgid, libc::SIGKILL);
            }
        });
    }

    let timed_out = Arc::new(AtomicBool::new(false));
    let timer = arm_timeout(pgid, timeout_secs, timed_out.clone());
    let status = child.wait();
    drop(timer);
    wait_pumps(&done_rx, 2);

    finished.store(true, Ordering::SeqCst);
    let _ = ch.send_control(&exit_control(status, timed_out.load(Ordering::SeqCst), started));
    ch.close();
}

#[allow(clippy::too_many_arguments)]
fn shell(
    ch: Channel,
    user: &str,
    command: Option<&str>,
    cols: u16,
    rows: u16,
    term: &str,
    workdir: Option<&str>,
    env: &HashMap<String, String>,
) {
    let started = Instant::now();
    let pty = match pty::open(cols, rows) {
        Ok(p) => p,
        Err(e) => return ch.close_with_error(format!("failed to allocate a pty: {e}")),
    };
    let cmd = match command {
        Some(c) => build_command(c, user, workdir, env, Some(term)),
        None => login_shell(user, workdir, env, term),
    };
    let mut child = match pty::spawn(cmd, pty.slave) {
        Ok(c) => c,
        Err(e) => return ch.close_with_error(format!("failed to start shell: {e}")),
    };
    let pgid = child.id() as i32;
    let master_fd = pty.master.as_raw_fd();
    let master = File::from(pty.master);
    let finished = Arc::new(AtomicBool::new(false));

    let (done_tx, done_rx) = mpsc::channel();
    let reader = match master.try_clone() {
        Ok(r) => r,
        Err(e) => {
            kill_group(pgid, libc::SIGKILL);
            let _ = child.wait();
            return ch.close_with_error(format!("failed to clone pty: {e}"));
        }
    };
    {
        let ch = ch.clone();
        std::thread::spawn(move || pump(reader, ch, stream::STDOUT, done_tx));
    }

    {
        let ch = ch.clone();
        let finished = finished.clone();
        let mut writer = master;
        std::thread::spawn(move || {
            while let Some(ev) = ch.recv() {
                match ev {
                    Event::Data { stream: stream::STDIN, bytes } => {
                        let _ = writer.write_all(&bytes);
                    }
                    Event::Control(Control::Resize { cols, rows }) => {
                        let _ = pty::resize(master_fd, cols, rows);
                    }
                    Event::Control(Control::Signal { signal }) => kill_group(pgid, signal),
                    _ => {}
                }
            }
            if !finished.load(Ordering::SeqCst) {
                kill_group(pgid, libc::SIGHUP);
            }
            // `writer` (the last master handle) is dropped here.
        });
    }

    let status = child.wait();
    wait_pumps(&done_rx, 1);
    finished.store(true, Ordering::SeqCst);
    let _ = ch.send_control(&exit_control(status, false, started));
    ch.close();
}

/// Removes a temp file unless disarmed.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

/// Temp path in the same directory as `dest`, so the final rename is atomic.
fn temp_path_for(dest: &Path) -> Result<PathBuf, String> {
    let name = dest
        .file_name()
        .ok_or_else(|| format!("{} has no file name", dest.display()))?;
    let dir = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Ok(dir.join(format!(
        ".{}.claw-{:08x}.part",
        name.to_string_lossy(),
        rand::random::<u32>()
    )))
}

fn upload(
    ch: &Channel,
    dest: &Path,
    size: u64,
    mode: Option<u32>,
    config: &ServerConfig,
) -> Result<(u64, String), String> {
    if size > config.max_transfer_bytes {
        return Err(format!(
            "upload of {size} bytes exceeds the {} byte limit",
            config.max_transfer_bytes
        ));
    }
    let tmp_path = temp_path_for(dest)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| format!("cannot create {}: {e}", tmp_path.display()))?;
    let mut tmp = TempFile(Some(tmp_path.clone()));

    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut got_eof = false;
    loop {
        match ch.recv() {
            Some(Event::Data { stream: stream::STDIN, bytes }) => {
                if got_eof {
                    return Err("data after end of upload".into());
                }
                written += bytes.len() as u64;
                if written > size {
                    return Err(format!("received more than the declared {size} bytes"));
                }
                hasher.update(&bytes);
                file.write_all(&bytes).map_err(|e| format!("write failed: {e}"))?;
            }
            Some(Event::Eof { stream: stream::STDIN }) => got_eof = true,
            Some(Event::Control(Control::Checksum { sha256 })) => {
                if !got_eof {
                    return Err("checksum arrived before the end of the data".into());
                }
                if written != size {
                    return Err(format!("expected {size} bytes, received {written}"));
                }
                let actual = hex(&hasher.finalize());
                if actual != sha256.to_ascii_lowercase() {
                    return Err(format!("checksum mismatch: expected {sha256}, got {actual}"));
                }
                file.sync_all().map_err(|e| format!("fsync failed: {e}"))?;
                if let Some(m) = mode {
                    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(m))
                        .map_err(|e| format!("chmod failed: {e}"))?;
                }
                fs::rename(&tmp_path, dest)
                    .map_err(|e| format!("cannot move into {}: {e}", dest.display()))?;
                tmp.0 = None;
                return Ok((written, actual));
            }
            Some(_) => {}
            None => return Err("connection closed before the upload finished".into()),
        }
    }
}

fn download(ch: &Channel, path: &Path, config: &ServerConfig) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > config.max_transfer_bytes {
        return Err(format!(
            "{} is {} bytes, over the {} byte limit",
            path.display(),
            meta.len(),
            config.max_transfer_bytes
        ));
    }
    let send_err = |e: io::Error| format!("send failed: {e}");
    ch.send_control(&Control::FileMeta { size: meta.len(), mode: meta.permissions().mode() & 0o7777 })
        .map_err(send_err)?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read failed: {e}")),
        };
        hasher.update(&buf[..n]);
        ch.send(stream::STDOUT, &buf[..n]).map_err(send_err)?;
    }
    ch.send_eof(stream::STDOUT).map_err(send_err)?;
    ch.send_control(&Control::Checksum { sha256: hex(&hasher.finalize()) })
        .map_err(send_err)?;
    Ok(())
}

fn port_forward(ch: Channel, port: u16) {
    let tcp = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => return ch.close_with_error(format!("cannot connect to 127.0.0.1:{port}: {e}")),
    };
    let reader = match tcp.try_clone() {
        Ok(r) => r,
        Err(e) => return ch.close_with_error(format!("socket error: {e}")),
    };
    let (done_tx, done_rx) = mpsc::channel();
    let out = {
        let ch = ch.clone();
        std::thread::spawn(move || pump(reader, ch, stream::STDOUT, done_tx))
    };

    let mut writer = &tcp;
    let mut clean_eof = false;
    while let Some(ev) = ch.recv() {
        match ev {
            Event::Data { stream: stream::STDIN, bytes } => {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
            Event::Eof { stream: stream::STDIN } => {
                let _ = tcp.shutdown(Shutdown::Write);
                clean_eof = true;
                break;
            }
            _ => {}
        }
    }
    if !clean_eof {
        let _ = tcp.shutdown(Shutdown::Both);
    }
    // Let the guest -> client direction finish, then close.
    let _ = done_rx.recv();
    let _ = out.join();
    ch.close();
}
