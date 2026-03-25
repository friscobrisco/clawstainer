//! Host-side vsock client.
//!
//! Firecracker exposes guest vsock as a Unix domain socket on the host.
//! The UDS path is: {vsock_uds_path}_{cid}_{port}
//! We connect to this to talk to the claw-agent inside the VM.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const GUEST_CID: u32 = 3;
const AGENT_PORT: u32 = 52;

/// Connect to the guest agent via the vsock UDS
pub fn connect(vsock_uds_path: &str) -> Result<UnixStream> {
    // Firecracker creates UDS at: {path}_{cid}_{port}
    let uds_path = format!("{vsock_uds_path}_{GUEST_CID}_{AGENT_PORT}");

    UnixStream::connect(&uds_path)
        .with_context(|| format!("Failed to connect to guest agent at {uds_path}"))
}

/// Send a JSON request and read a JSON response
pub fn request(stream: &UnixStream, req: &serde_json::Value) -> Result<serde_json::Value> {
    let mut writer = stream;
    let reader = BufReader::new(stream);

    let req_json = serde_json::to_string(req)?;
    writeln!(writer, "{req_json}")
        .context("Failed to send request to guest agent")?;
    writer.flush()?;

    let mut line = String::new();
    let mut reader = reader;
    reader.read_line(&mut line)
        .context("Failed to read response from guest agent")?;

    serde_json::from_str(&line)
        .context("Failed to parse guest agent response")
}

/// Wait for the guest agent to become reachable
pub fn wait_for_agent(vsock_uds_path: &str, timeout_ms: u64) -> Result<()> {
    let start = std::time::Instant::now();

    loop {
        if let Ok(stream) = connect(vsock_uds_path) {
            // Try a health check
            let health_req = serde_json::json!({"type": "health"});
            if let Ok(resp) = request(&stream, &health_req) {
                if resp.get("type").and_then(|t| t.as_str()) == Some("health_result") {
                    return Ok(());
                }
            }
        }

        if start.elapsed().as_millis() as u64 > timeout_ms {
            anyhow::bail!("Timed out waiting for guest agent");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
