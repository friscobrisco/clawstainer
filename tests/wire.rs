//! End-to-end tests for the CLAW/2 streaming protocol: a real client
//! session and a real agent server talking over a Unix socket pair.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use clawstainer::wire::client;
use clawstainer::wire::frame::{self, Frame, Kind, WINDOW};
use clawstainer::wire::server::{self, GuestOps, ServerConfig};
use std::sync::{Arc, Mutex};
use clawstainer::wire::{stream, Config, Control, Event, OpenRequest, Session};
use sha2::{Digest, Sha256};

fn pair_with(server_cfg: ServerConfig, cfg: Config) -> Session {
    let (a, b) = UnixStream::pair().unwrap();
    let scfg = cfg.clone();
    std::thread::spawn(move || {
        let (session, incoming) = Session::server(b, scfg).unwrap();
        server::serve(session, incoming, server_cfg);
    });
    Session::client(a, cfg).unwrap()
}

fn pair() -> Session {
    pair_with(ServerConfig::default(), Config::default())
}

fn exec_req(command: &str, workdir: &str) -> OpenRequest {
    OpenRequest::Exec {
        command: command.into(),
        workdir: Some(workdir.into()),
        env: HashMap::new(),
        user: "root".into(),
        timeout_secs: 0,
        session_id: None,
        keep_secs: 0,
    }
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

#[test]
fn health_reports_version() {
    let s = pair();
    assert_eq!(client::health(&s).unwrap(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn exec_streams_stdout_stderr_and_exit_code() {
    let s = pair();
    let dir = tempfile::tempdir().unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = client::exec(
        &s,
        &exec_req("echo out; echo err >&2; pwd; exit 7", dir.path().to_str().unwrap()),
        None::<io::Empty>,
        &mut out,
        &mut err,
    )
    .unwrap();
    assert_eq!(exit.code, Some(7));
    assert!(!exit.timed_out);
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("out\n"), "{out}");
    // macOS tempdirs resolve through /private.
    assert!(out.trim_end().ends_with(dir.path().file_name().unwrap().to_str().unwrap()));
    assert_eq!(String::from_utf8(err).unwrap(), "err\n");
}

#[test]
fn exec_passes_env() {
    let s = pair();
    let mut env = HashMap::new();
    env.insert("CLAW_TEST".to_string(), "hello".to_string());
    let req = OpenRequest::Exec {
        command: "printf %s \"$CLAW_TEST\"".into(),
        workdir: Some("/".into()),
        env,
        user: "root".into(),
        timeout_secs: 0,
        session_id: None,
        keep_secs: 0,
    };
    let mut out = Vec::new();
    client::exec(&s, &req, None::<io::Empty>, &mut out, &mut io::sink()).unwrap();
    assert_eq!(out, b"hello");
}

/// 8 MiB through `cat` is 32x the window, so this only passes if credit is
/// granted back correctly in both directions, and bytes arrive intact.
#[test]
fn exec_stdin_roundtrip_is_flow_controlled_and_intact() {
    let s = pair();
    let data: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let mut out = Vec::new();
    let exit = client::exec(&s, &exec_req("cat", "/"), Some(io::Cursor::new(data.clone())), &mut out, &mut io::sink())
        .unwrap();
    assert_eq!(exit.code, Some(0));
    assert_eq!(out.len(), data.len());
    assert_eq!(Sha256::digest(&out), Sha256::digest(&data));
}

#[test]
fn exec_timeout_kills_process_group() {
    let s = pair();
    let req = OpenRequest::Exec {
        command: "sleep 30 & sleep 30".into(),
        workdir: Some("/".into()),
        env: HashMap::new(),
        user: "root".into(),
        timeout_secs: 1,
        session_id: None,
        keep_secs: 0,
    };
    let start = Instant::now();
    let exit = client::exec(&s, &req, None::<io::Empty>, &mut io::sink(), &mut io::sink()).unwrap();
    assert!(exit.timed_out);
    assert_eq!(exit.signal, Some(libc::SIGKILL));
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn exec_signal_is_delivered() {
    let s = pair();
    let ch = s.open(&exec_req("trap 'exit 42' TERM; echo ready; while :; do sleep 0.05; done", "/")).unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    // Wait for the trap to be installed.
    loop {
        match ch.recv().expect("channel closed early") {
            Event::Data { stream: stream::STDOUT, bytes } if bytes.starts_with(b"ready") => break,
            _ => {}
        }
    }
    ch.send_control(&Control::Signal { signal: libc::SIGTERM }).unwrap();
    let exit = loop {
        if let Event::Control(Control::Exit { code, .. }) = ch.recv().expect("channel closed before exit") {
            break code;
        }
    };
    assert_eq!(exit, Some(42));
}

#[test]
fn closing_an_exec_channel_kills_the_process() {
    let s = pair();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pid");
    let cmd = format!("echo $$ > {}; exec sleep 30", marker.display());
    let ch = s.open(&exec_req(&cmd, "/")).unwrap();
    assert!(wait_until(Duration::from_secs(5), || fs::read_to_string(&marker).map(|p| !p.trim().is_empty()).unwrap_or(false)));
    let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    ch.close();
    let gone = wait_until(Duration::from_secs(5), || unsafe { libc::kill(pid, 0) } != 0);
    assert!(gone, "process {pid} survived its channel closing");
}

#[test]
fn concurrent_channels_are_independent() {
    let s = pair();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = &s;
                scope.spawn(move || {
                    let mut out = Vec::new();
                    let exit = client::exec(s, &exec_req(&format!("echo {i}"), "/"), None::<io::Empty>, &mut out, &mut io::sink())
                        .unwrap();
                    (i, exit.code, String::from_utf8(out).unwrap())
                })
            })
            .collect();
        for h in handles {
            let (i, code, out) = h.join().unwrap();
            assert_eq!(code, Some(0));
            assert_eq!(out, format!("{i}\n"));
        }
    });
}

#[test]
fn pty_shell_has_a_terminal_and_resizes() {
    let s = pair();
    let req = OpenRequest::Shell {
        user: "root".into(),
        command: Some("test -t 0 && echo is-tty; stty size; read line; echo got:$line; stty size; exit 3".into()),
        cols: 100,
        rows: 40,
        term: None,
        workdir: Some("/".into()),
        env: HashMap::new(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // Send the resize first, then the line the shell waits for.
    let (in_w, in_r) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        tx.send((120, 50)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let mut w = in_w;
        w.write_all(b"hello\n").unwrap();
        std::thread::sleep(Duration::from_secs(2));
    });
    let mut out = Vec::new();
    let exit = client::pty_session(&s, &req, in_r, &mut out, Some(rx)).unwrap();
    let out = String::from_utf8_lossy(&out);
    assert_eq!(exit.code, Some(3), "{out}");
    assert!(out.contains("is-tty"), "{out}");
    assert!(out.contains("40 100"), "{out}");
    assert!(out.contains("got:hello"), "{out}");
    assert!(out.contains("50 120"), "{out}");
}

#[test]
fn upload_and_download_roundtrip() {
    let s = pair();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    let data: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(&src, &data).unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();

    let remote = dir.path().join("remote.bin");
    let (size, sha) = client::upload(&s, &src, remote.to_str().unwrap(), None).unwrap();
    assert_eq!(size, data.len() as u64);
    assert_eq!(sha, clawstainer::wire::messages::hex(&Sha256::digest(&data)));
    assert_eq!(fs::read(&remote).unwrap(), data);
    assert_eq!(fs::metadata(&remote).unwrap().permissions().mode() & 0o777, 0o750);

    let back = dir.path().join("back.bin");
    let (size2, sha2) = client::download(&s, remote.to_str().unwrap(), &back).unwrap();
    assert_eq!((size2, &sha2), (size, &sha));
    assert_eq!(fs::read(&back).unwrap(), data);
    assert_eq!(fs::metadata(&back).unwrap().permissions().mode() & 0o777, 0o750);

    // No temp files left behind.
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn upload_over_the_limit_is_rejected_without_writing() {
    let s = pair_with(ServerConfig { max_transfer_bytes: 1024, ..ServerConfig::default() }, Config::default());
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big");
    fs::write(&src, vec![7u8; 4096]).unwrap();
    let dest = dir.path().join("dest");
    let err = client::upload(&s, &src, dest.to_str().unwrap(), None).unwrap_err();
    assert!(err.to_string().contains("limit"), "{err}");
    assert!(!dest.exists());
}

#[test]
fn upload_with_wrong_checksum_is_rejected_and_cleaned_up() {
    let s = pair();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dest");
    let ch = s
        .open(&OpenRequest::Upload { path: dest.to_str().unwrap().into(), size: 5, mode: None })
        .unwrap();
    ch.send(stream::STDIN, b"hello").unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    ch.send_control(&Control::Checksum { sha256: "00".repeat(32) }).unwrap();
    let msg = loop {
        match ch.recv().expect("closed without an answer") {
            Event::Control(Control::Error { message }) => break message,
            Event::Control(Control::UploadDone { .. }) => panic!("bad checksum accepted"),
            _ => {}
        }
    };
    assert!(msg.contains("checksum mismatch"), "{msg}");
    assert!(!dest.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "temp file left behind");
}

#[test]
fn download_errors_are_reported() {
    let s = pair();
    let dir = tempfile::tempdir().unwrap();
    let err = client::download(&s, "/definitely/not/here", &dir.path().join("x")).unwrap_err();
    assert!(err.to_string().contains("cannot open"), "{err}");
    let err = client::download(&s, dir.path().to_str().unwrap(), &dir.path().join("y")).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
}

#[test]
fn port_forward_relays_both_directions() {
    let s = pair();
    // Echo server standing in for a service inside the guest.
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = upstream.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut c, _) = upstream.accept().unwrap();
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).unwrap();
        c.write_all(b"echo:").unwrap();
        c.write_all(&buf).unwrap();
    });

    // A local listener the "user" connects to.
    let local = TcpListener::bind("127.0.0.1:0").unwrap();
    let local_addr = local.local_addr().unwrap();
    let user = std::thread::spawn(move || {
        let mut c = TcpStream::connect(local_addr).unwrap();
        c.write_all(&vec![b'x'; 300_000]).unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        c.read_to_end(&mut reply).unwrap();
        reply
    });
    let (conn, _) = local.accept().unwrap();
    client::forward(&s, port, conn).unwrap();
    let reply = user.join().unwrap();
    assert!(reply.starts_with(b"echo:"));
    assert_eq!(reply.len(), 5 + 300_000);
}

#[test]
fn port_forward_to_closed_port_reports_error() {
    let s = pair();
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let local = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = local.local_addr().unwrap();
    let _c = TcpStream::connect(addr).unwrap();
    let (conn, _) = local.accept().unwrap();
    let err = client::forward(&s, port, conn).unwrap_err();
    assert!(err.to_string().contains("cannot connect"), "{err}");
}

/// Handshake by hand so the test controls exactly what the peer sends.
fn raw_server_peer() -> (Session, UnixStream) {
    let (a, mut b) = UnixStream::pair().unwrap();
    let peer = std::thread::spawn(move || {
        let line = frame::read_line_unbuffered(&mut b, 64).unwrap();
        assert_eq!(line, frame::PREAMBLE);
        b.write_all(frame::PREAMBLE).unwrap();
        b
    });
    let cfg = Config { heartbeat: Duration::from_millis(50), ..Config::default() };
    let s = Session::client(a, cfg).unwrap();
    (s, peer.join().unwrap())
}

#[test]
fn silent_peer_is_dropped_after_missed_heartbeats() {
    let (s, _peer) = raw_server_peer();
    // The peer never reads or answers pings.
    assert!(wait_until(Duration::from_secs(2), || s.is_closed()), "session stayed open");
}

#[test]
fn window_overrun_closes_the_session() {
    let (s, mut peer) = raw_server_peer();
    let _ch = s.open(&OpenRequest::Health).unwrap();
    // On macOS, shutting down one end of a socketpair does not unblock a
    // pending write on the other end, so bound the raw writes.
    peer.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    // Never consumed, so anything beyond one window is a violation.
    let chunk = vec![0u8; 60_000];
    let mut sent = 0usize;
    while sent <= WINDOW as usize + 60_000 {
        let mut payload = vec![stream::STDOUT];
        payload.extend_from_slice(&chunk);
        if frame::write_frame(&mut peer, &Frame::new(1, Kind::Data, payload)).is_err() {
            break;
        }
        sent += chunk.len();
    }
    assert!(wait_until(Duration::from_secs(2), || s.is_closed()), "overrun was tolerated");
}

#[test]
fn server_refuses_garbage_open_but_keeps_session() {
    let (a, b) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let (session, incoming) = Session::server(b, Config::default()).unwrap();
        server::serve(session, incoming, ServerConfig::default());
    });
    let mut a2 = a.try_clone().unwrap();
    let mut a = a;
    frame::client_handshake(&mut a).unwrap();
    frame::write_frame(&mut a2, &Frame::new(5, Kind::Open, b"{not json".to_vec())).unwrap();
    // Expect an error control and a close on channel 5.
    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        let f = frame::read_frame(&mut a).unwrap().unwrap();
        if f.channel == 5 {
            kinds.push(f.kind);
        }
    }
    assert_eq!(kinds, vec![Kind::Control, Kind::Close]);
    // Session still works: a valid health open on the same transport.
    let payload = serde_json::to_vec(&OpenRequest::Health).unwrap();
    frame::write_frame(&mut a2, &Frame::new(6, Kind::Open, payload)).unwrap();
    loop {
        let f = frame::read_frame(&mut a).unwrap().unwrap();
        if f.channel == 6 && f.kind == Kind::Control {
            let c: Control = serde_json::from_slice(&f.payload).unwrap();
            assert!(matches!(c, Control::Health { .. }));
            break;
        }
    }
}

/// Records guest operations instead of performing them.
#[derive(Default)]
struct RecordingOps {
    log: Mutex<Vec<String>>,
    fail_freeze: bool,
}

impl RecordingOps {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl GuestOps for RecordingOps {
    fn freeze(&self) -> io::Result<()> {
        if self.fail_freeze {
            return Err(io::Error::other("device busy"));
        }
        self.log.lock().unwrap().push("freeze".into());
        Ok(())
    }
    fn thaw(&self) -> io::Result<()> {
        self.log.lock().unwrap().push("thaw".into());
        Ok(())
    }
    fn set_clock(&self, unix_nanos: u64) -> io::Result<()> {
        self.log.lock().unwrap().push(format!("clock {unix_nanos}"));
        Ok(())
    }
}

fn config_with(ops: Arc<RecordingOps>) -> ServerConfig {
    ServerConfig { ops, ..ServerConfig::default() }
}

#[test]
fn freeze_survives_reconnect_and_thaws_once() {
    let ops = Arc::new(RecordingOps::default());
    let cfg = config_with(ops.clone());

    // Freeze on one connection, which then goes away (the VM is paused).
    let s1 = pair_with(cfg.clone(), Config::default());
    client::guest_control(&s1, &OpenRequest::Freeze { auto_thaw_secs: 30 }).unwrap();
    // A second freeze while frozen doesn't freeze twice.
    client::guest_control(&s1, &OpenRequest::Freeze { auto_thaw_secs: 30 }).unwrap();
    drop(s1);

    // After resume: a new connection sets the clock and thaws.
    let s2 = pair_with(cfg, Config::default());
    client::guest_control(&s2, &OpenRequest::SetClock { unix_nanos: 1_700_000_000_123_456_789 }).unwrap();
    client::guest_control(&s2, &OpenRequest::Thaw).unwrap();
    assert_eq!(ops.log(), vec!["freeze", "clock 1700000000123456789", "thaw"]);
}

#[test]
fn freeze_auto_thaws_when_the_host_never_comes_back() {
    let ops = Arc::new(RecordingOps::default());
    let s = pair_with(config_with(ops.clone()), Config::default());
    client::guest_control(&s, &OpenRequest::Freeze { auto_thaw_secs: 1 }).unwrap();
    assert!(wait_until(Duration::from_secs(3), || ops.log() == vec!["freeze", "thaw"]), "{:?}", ops.log());
}

#[test]
fn stale_auto_thaw_does_not_undo_a_newer_freeze() {
    let ops = Arc::new(RecordingOps::default());
    let s = pair_with(config_with(ops.clone()), Config::default());
    client::guest_control(&s, &OpenRequest::Freeze { auto_thaw_secs: 1 }).unwrap();
    client::guest_control(&s, &OpenRequest::Thaw).unwrap();
    client::guest_control(&s, &OpenRequest::Freeze { auto_thaw_secs: 30 }).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    // The first freeze's 1 s timer fired but must not thaw the second.
    assert_eq!(ops.log(), vec!["freeze", "thaw", "freeze"]);
}

#[test]
fn freeze_failure_is_reported() {
    let ops = Arc::new(RecordingOps { fail_freeze: true, ..RecordingOps::default() });
    let s = pair_with(config_with(ops), Config::default());
    let err = client::guest_control(&s, &OpenRequest::Freeze { auto_thaw_secs: 30 }).unwrap_err();
    assert!(err.to_string().contains("device busy"), "{err}");
}

// ---- detachable execs ----------------------------------------------------

fn detachable(command: &str, id: &str, keep_secs: u64) -> OpenRequest {
    OpenRequest::Exec {
        command: command.into(),
        workdir: Some("/".into()),
        env: HashMap::new(),
        user: "root".into(),
        timeout_secs: 0,
        session_id: Some(id.into()),
        keep_secs,
    }
}

/// Read stdout until it contains `marker`, tracking progress like the
/// client does. Returns what was read.
fn read_until(ch: &clawstainer::wire::Channel, marker: &str, progress: &mut client::Progress) -> String {
    let mut out = String::new();
    while !out.contains(marker) {
        match ch.recv().expect("channel closed early") {
            Event::Data { stream: stream::STDOUT, bytes } => {
                progress.stdout += bytes.len() as u64;
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            Event::Data { stream: stream::STDERR, bytes } => progress.stderr += bytes.len() as u64,
            _ => {}
        }
    }
    out
}

#[test]
fn reattach_resumes_output_exactly_after_a_dropped_connection() {
    let cfg = ServerConfig::default();
    let s1 = pair_with(cfg.clone(), Config::default());
    let ch = s1
        .open(&detachable("for i in $(seq 1 40); do echo line$i; sleep 0.02; done; exit 4", "job-1", 30))
        .unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    let mut progress = client::Progress::default();
    let first = read_until(&ch, "line5\n", &mut progress);
    drop(s1); // the connection dies mid-command

    std::thread::sleep(Duration::from_millis(200));
    let s2 = pair_with(cfg, Config::default());
    let mut rest = Vec::new();
    let exit = client::attach(&s2, "job-1", &mut rest, &mut io::sink(), &mut progress).unwrap();
    assert_eq!(exit.code, Some(4));
    let all = first + &String::from_utf8(rest).unwrap();
    let expected: String = (1..=40).map(|i| format!("line{i}\n")).collect();
    assert_eq!(all, expected, "output was duplicated or lost across the reattach");
    assert_eq!(progress.lost, 0);
}

#[test]
fn exit_while_detached_is_kept_for_reattach() {
    let cfg = ServerConfig::default();
    let s1 = pair_with(cfg.clone(), Config::default());
    let ch = s1.open(&detachable("echo a; sleep 0.3; echo b; echo e >&2; exit 9", "job-2", 30)).unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    let mut progress = client::Progress::default();
    read_until(&ch, "a\n", &mut progress);
    drop(s1);
    std::thread::sleep(Duration::from_millis(800)); // finishes while detached

    let s2 = pair_with(cfg, Config::default());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = client::attach(&s2, "job-2", &mut out, &mut err, &mut progress).unwrap();
    assert_eq!(exit.code, Some(9));
    assert_eq!(out, b"b\n");
    assert_eq!(err, b"e\n");
}

#[test]
fn abandoned_session_is_killed_after_its_grace_period() {
    let cfg = ServerConfig::default();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pid");
    let s1 = pair_with(cfg.clone(), Config::default());
    let cmd = format!("echo $$ > {}; echo started; exec sleep 30", marker.display());
    let ch = s1.open(&detachable(&cmd, "job-3", 1)).unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    read_until(&ch, "started", &mut client::Progress::default());
    let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    drop(s1);

    assert!(unsafe { libc::kill(pid, 0) } == 0, "killed too early");
    let gone = wait_until(Duration::from_secs(5), || unsafe { libc::kill(pid, 0) } != 0);
    assert!(gone, "abandoned process {pid} was not killed");
    // Once its status has also expired, the session is forgotten.
    let s2 = pair_with(cfg, Config::default());
    let forgotten = wait_until(Duration::from_secs(5), || {
        client::attach(&s2, "job-3", &mut io::sink(), &mut io::sink(), &mut client::Progress::default())
            .map_err(|e| e.to_string().contains("no session"))
            .err()
            .unwrap_or(false)
    });
    assert!(forgotten);
}

#[test]
fn overflow_while_detached_is_reported_not_hidden() {
    let cfg = ServerConfig { detach_buffer_bytes: 1000, ..ServerConfig::default() };
    let s1 = pair_with(cfg.clone(), Config::default());
    let ch = s1
        .open(&detachable("echo start; sleep 0.3; head -c 50000 /dev/zero | tr '\\0' x; echo; echo tail", "job-4", 30))
        .unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    let mut progress = client::Progress::default();
    read_until(&ch, "start\n", &mut progress);
    drop(s1);
    std::thread::sleep(Duration::from_millis(1000));

    let s2 = pair_with(cfg, Config::default());
    let mut out = Vec::new();
    let exit = client::attach(&s2, "job-4", &mut out, &mut io::sink(), &mut progress).unwrap();
    assert_eq!(exit.code, Some(0));
    assert!(progress.lost > 40_000, "lost = {}", progress.lost);
    assert!(out.len() <= 1000);
    assert!(String::from_utf8_lossy(&out).ends_with("\ntail\n"));
    // Offsets still account for every byte the process wrote.
    assert_eq!(progress.stdout, 6 + 50_000 + 1 + 5);
}

#[test]
fn newer_attach_supersedes_older_and_unknown_sessions_are_refused() {
    let cfg = ServerConfig::default();
    let s = pair_with(cfg.clone(), Config::default());
    let ch = s.open(&detachable("echo go; sleep 1; echo done", "job-5", 30)).unwrap();
    ch.send_eof(stream::STDIN).unwrap();
    read_until(&ch, "go", &mut client::Progress::default());

    let s2 = pair_with(cfg.clone(), Config::default());
    let attached = std::thread::spawn(move || {
        let mut out = Vec::new();
        let p = &mut client::Progress { stdout: 3, ..Default::default() };
        let exit = client::attach(&s2, "job-5", &mut out, &mut io::sink(), p).unwrap();
        (exit.code, out)
    });
    // The original channel is told it lost the session.
    let reason = loop {
        match ch.recv().expect("closed without a reason") {
            Event::Control(Control::Error { message }) => break message,
            _ => {}
        }
    };
    assert!(reason.contains("another connection"), "{reason}");
    assert_eq!(attached.join().unwrap(), (Some(0), b"done\n".to_vec()));

    let err = client::attach(&s, "nope", &mut io::sink(), &mut io::sink(), &mut client::Progress::default())
        .unwrap_err();
    assert!(err.to_string().contains("no session"), "{err}");
}

#[test]
fn starting_a_duplicate_session_id_is_refused() {
    let cfg = ServerConfig::default();
    let s = pair_with(cfg, Config::default());
    let a = s.open(&detachable("sleep 1", "dup", 30)).unwrap();
    a.send_eof(stream::STDIN).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut p = client::Progress::default();
    let err = client::exec_detachable(&s, &detachable("true", "dup", 30), &mut io::sink(), &mut io::sink(), &mut p)
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}
