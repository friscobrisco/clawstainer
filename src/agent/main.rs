//! claw-agent: lightweight guest agent for Firecracker VMs.
//!
//! Runs inside each VM and listens on vsock port 52. Each connection speaks
//! one of two protocols, chosen by its first line:
//!
//! - `CLAW/2`: the streaming, multiplexed protocol in `clawstainer::wire`
//!   (exec with live output, PTY shell, file transfer, port forwarding).
//! - anything else: the legacy newline-delimited JSON protocol
//!   (buffered exec, health, shutdown), kept for older hosts.

mod executor;
mod protocol;
mod vsock;

use std::io::{BufRead, BufReader, Write};

use clawstainer::wire::{self, frame, mux::Parts, server::ServerConfig};

/// Longest first line accepted while deciding which protocol a connection
/// speaks. Legacy requests carry the whole command on one line.
const MAX_FIRST_LINE: usize = 1024 * 1024;

fn main() {
    eprintln!("claw-agent: starting on vsock port 52...");

    let listener = match vsock::VsockListener::bind(52) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("claw-agent: failed to bind vsock: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("claw-agent: listening for connections");

    // One config for the whole agent: freeze state must be shared, because
    // the thaw after a resume arrives on a new connection.
    let config = ServerConfig::default();

    loop {
        match listener.accept() {
            Ok(stream) => {
                let config = config.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, config) {
                        eprintln!("claw-agent: connection error: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("claw-agent: accept error: {e}");
            }
        }
    }
}

fn handle_connection(
    stream: vsock::VsockStream,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let first = frame::read_line_unbuffered(&mut &stream, MAX_FIRST_LINE)?;
    if first.is_empty() {
        return Ok(());
    }
    if first == frame::PREAMBLE {
        return serve_streaming(stream, config);
    }

    // Legacy protocol: the first line is already a request.
    let mut writer = &stream;
    let first = String::from_utf8_lossy(&first).into_owned();
    if !handle_legacy_line(&first, &mut writer)? {
        return Ok(());
    }
    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        if !handle_legacy_line(&line?, &mut writer)? {
            break;
        }
    }
    Ok(())
}

fn serve_streaming(
    stream: vsock::VsockStream,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let reader = stream.try_clone()?;
    let killer = stream.try_clone()?;
    let parts = Parts {
        reader: Box::new(reader),
        writer: Box::new(stream),
        shutdown: Box::new(move || killer.shutdown()),
    };
    let (session, incoming) = wire::Session::server_after_preamble(parts, wire::Config::default())?;
    wire::server::serve(session, incoming, config);
    Ok(())
}

/// Handle one legacy JSON request. Returns `false` when the connection
/// should end (shutdown requested).
fn handle_legacy_line(
    line: &str,
    writer: &mut &vsock::VsockStream,
) -> Result<bool, Box<dyn std::error::Error>> {
    if line.trim().is_empty() {
        return Ok(true);
    }

    let request: protocol::Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            let err_resp = protocol::Response::Error {
                message: format!("Invalid request: {e}"),
            };
            writeln!(writer, "{}", serde_json::to_string(&err_resp)?)?;
            return Ok(true);
        }
    };

    let response = match request {
        protocol::Request::Exec { command, timeout, workdir, env, user } => {
            executor::exec(command, timeout, workdir, env, user)
        }
        protocol::Request::Health => protocol::Response::HealthResult {
            status: "ok".to_string(),
        },
        protocol::Request::Shutdown => {
            writeln!(writer, "{}", serde_json::to_string(&protocol::Response::Ok)?)?;
            writer.flush()?;
            eprintln!("claw-agent: shutdown requested");
            std::process::Command::new("poweroff").spawn().ok();
            return Ok(false);
        }
    };

    writeln!(writer, "{}", serde_json::to_string(&response)?)?;
    writer.flush()?;
    Ok(true)
}
