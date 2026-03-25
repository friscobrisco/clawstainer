//! claw-agent: lightweight guest agent for Firecracker VMs.
//!
//! Runs inside each VM, listens on vsock port 52, and handles
//! exec/shell/health/shutdown commands from the host.

mod protocol;
mod vsock;
mod executor;

use std::io::{BufRead, BufReader, Write};

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

    loop {
        match listener.accept() {
            Ok(stream) => {
                // Handle each connection in a new thread
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream) {
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

fn handle_connection(stream: vsock::VsockStream) -> Result<(), Box<dyn std::error::Error>> {
    let reader = BufReader::new(&stream);
    let mut writer = &stream;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request: protocol::Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = protocol::Response::Error {
                    message: format!("Invalid request: {e}"),
                };
                let resp_json = serde_json::to_string(&err_resp)?;
                writeln!(writer, "{resp_json}")?;
                continue;
            }
        };

        let response = match request {
            protocol::Request::Exec {
                command,
                timeout,
                workdir,
                env,
                user,
            } => executor::exec(command, timeout, workdir, env, user),

            protocol::Request::Health => protocol::Response::HealthResult {
                status: "ok".to_string(),
            },

            protocol::Request::Shutdown => {
                let resp = protocol::Response::Ok;
                let resp_json = serde_json::to_string(&resp)?;
                writeln!(writer, "{resp_json}")?;
                writer.flush()?;

                // Graceful shutdown
                eprintln!("claw-agent: shutdown requested");
                std::process::Command::new("poweroff").spawn().ok();
                return Ok(());
            }
        };

        let resp_json = serde_json::to_string(&response)?;
        writeln!(writer, "{resp_json}")?;
        writer.flush()?;
    }

    Ok(())
}
