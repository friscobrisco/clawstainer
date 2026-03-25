use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Minimal HTTP-over-Unix-socket client for the Firecracker API.
/// Firecracker's API is simple: PUT/GET with JSON bodies over a UDS.
pub struct FirecrackerApi {
    socket_path: String,
}

impl FirecrackerApi {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Wait for the socket to become available
    pub fn wait_for_ready(&self, timeout_ms: u64) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            if Path::new(&self.socket_path).exists() {
                // Try connecting
                if UnixStream::connect(&self.socket_path).is_ok() {
                    return Ok(());
                }
            }
            if start.elapsed().as_millis() as u64 > timeout_ms {
                anyhow::bail!("Timed out waiting for Firecracker API socket");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Send a PUT request
    pub fn put(&self, path: &str, body: &serde_json::Value) -> Result<ApiResponse> {
        let body_str = serde_json::to_string(body)?;
        let request = format!(
            "PUT {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            path,
            body_str.len(),
            body_str
        );

        self.send_request(&request)
    }

    /// Send a PATCH request
    pub fn patch(&self, path: &str, body: &serde_json::Value) -> Result<ApiResponse> {
        let body_str = serde_json::to_string(body)?;
        let request = format!(
            "PATCH {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            path,
            body_str.len(),
            body_str
        );

        self.send_request(&request)
    }

    fn send_request(&self, request: &str) -> Result<ApiResponse> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .context("Failed to connect to Firecracker API socket")?;

        stream.write_all(request.as_bytes())
            .context("Failed to send request")?;

        stream.shutdown(std::net::Shutdown::Write)
            .context("Failed to shutdown write")?;

        let mut response = String::new();
        stream.read_to_string(&mut response)
            .context("Failed to read response")?;

        parse_response(&response)
    }
}

#[derive(Debug)]
pub struct ApiResponse {
    pub status: u16,
    pub body: String,
}

impl ApiResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn parse_response(raw: &str) -> Result<ApiResponse> {
    // Parse minimal HTTP response: "HTTP/1.1 204 No Content\r\n..."
    let status_line = raw.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let body = raw
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .to_string();

    Ok(ApiResponse { status, body })
}
