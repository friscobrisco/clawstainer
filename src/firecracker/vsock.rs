//! Host-side vsock client.
//!
//! Firecracker exposes the guest's vsock device as a Unix socket on the host
//! (`uds_path` in the `/vsock` config). For host-initiated connections the
//! host connects to that socket and sends `CONNECT <port>\n`; Firecracker
//! answers `OK <host_port>\n` and from then on the socket is a byte stream to
//! the guest port. (`{uds_path}_{port}` is only used for connections the
//! *guest* initiates.)

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use clawstainer::wire;

const AGENT_PORT: u32 = 52;

/// Connect to the guest agent's port through Firecracker's vsock proxy.
pub fn connect(vsock_uds_path: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(vsock_uds_path)
        .with_context(|| format!("Failed to connect to vsock socket at {vsock_uds_path}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "CONNECT {AGENT_PORT}").context("Failed to send vsock CONNECT")?;
    stream.flush()?;

    let reply = wire::frame::read_line_unbuffered(&mut stream, 64)
        .context("Failed to read vsock CONNECT reply")?;
    if !reply.starts_with(b"OK ") {
        bail!(
            "Firecracker refused the vsock connection to port {AGENT_PORT}: {:?}",
            String::from_utf8_lossy(&reply).trim()
        );
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// Open a streaming (CLAW/2) session with the guest agent.
pub fn connect_wire(vsock_uds_path: &str) -> Result<wire::Session> {
    let stream = connect(vsock_uds_path)?;
    wire::Session::client(stream, wire::Config::default())
        .context("Guest agent did not accept the streaming protocol")
}

/// Wait for the guest agent to become reachable
pub fn wait_for_agent(vsock_uds_path: &str, timeout_ms: u64) -> Result<()> {
    let start = std::time::Instant::now();

    loop {
        if let Ok(session) = connect_wire(vsock_uds_path) {
            if wire::client::health(&session).is_ok() {
                return Ok(());
            }
        }

        if start.elapsed().as_millis() as u64 > timeout_ms {
            bail!("Timed out waiting for guest agent");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
