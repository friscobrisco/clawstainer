//! JSON payloads carried in `Open` and `Control` frames.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Stream tags used in `Data` and `Eof` frames.
pub mod stream {
    /// Client -> server: process stdin, upload bytes, forwarded TCP bytes.
    pub const STDIN: u8 = 0;
    /// Server -> client: process stdout / PTY output, download bytes,
    /// forwarded TCP bytes.
    pub const STDOUT: u8 = 1;
    /// Server -> client: process stderr.
    pub const STDERR: u8 = 2;
}

fn default_user() -> String {
    "root".to_string()
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// What a channel is for. Sent once, in the `Open` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenRequest {
    /// Replies with [`Control::Health`] and closes.
    Health,

    /// Run a command with piped stdin/stdout/stderr. Output streams back as
    /// it is produced; the channel ends with [`Control::Exit`].
    Exec {
        command: String,
        #[serde(default)]
        workdir: Option<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default = "default_user")]
        user: String,
        /// 0 = no timeout.
        #[serde(default)]
        timeout_secs: u64,
    },

    /// Interactive login shell (or `command`) on a PTY. Output arrives on
    /// `STDOUT`; the client sends keystrokes on `STDIN` and
    /// [`Control::Resize`] when its terminal changes size.
    Shell {
        #[serde(default = "default_user")]
        user: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default = "default_rows")]
        rows: u16,
        #[serde(default)]
        term: Option<String>,
        #[serde(default)]
        workdir: Option<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },

    /// Client streams exactly `size` bytes on `STDIN`, then `Eof`, then
    /// [`Control::Checksum`]. The server writes to a temp file, verifies
    /// size and SHA-256, renames into place, and replies
    /// [`Control::UploadDone`].
    Upload {
        path: String,
        size: u64,
        #[serde(default)]
        mode: Option<u32>,
    },

    /// Server replies [`Control::FileMeta`], streams the file on `STDOUT`,
    /// then `Eof`, then [`Control::Checksum`].
    Download { path: String },

    /// Connect to `127.0.0.1:port` inside the guest and relay bytes:
    /// client -> guest on `STDIN`, guest -> client on `STDOUT`.
    PortForward { port: u16 },
}

/// In-channel control messages, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Health { status: String, version: String },
    Resize { cols: u16, rows: u16 },
    /// Deliver a signal to the process group of an exec or shell.
    Signal { signal: i32 },
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
    },
    FileMeta { size: u64, mode: u32 },
    Checksum { sha256: String },
    UploadDone { size: u64, sha256: String },
    Error { message: String },
}

/// Lowercase hex encoding for checksums.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_defaults() {
        let req: OpenRequest =
            serde_json::from_str(r#"{"type":"exec","command":"true"}"#).unwrap();
        assert_eq!(
            req,
            OpenRequest::Exec {
                command: "true".into(),
                workdir: None,
                env: HashMap::new(),
                user: "root".into(),
                timeout_secs: 0,
            }
        );
        let shell: OpenRequest = serde_json::from_str(r#"{"type":"shell"}"#).unwrap();
        assert!(matches!(shell, OpenRequest::Shell { cols: 80, rows: 24, .. }));
    }

    #[test]
    fn control_roundtrip() {
        let c = Control::Exit { code: Some(3), signal: None, timed_out: false, duration_ms: 12 };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""type":"exit""#));
        assert_eq!(serde_json::from_str::<Control>(&json).unwrap(), c);
    }

    #[test]
    fn hex_encodes() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }
}
