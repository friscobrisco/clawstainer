//! `wire`: the streaming protocol between a host and the guest agent.
//!
//! One transport (a vsock connection, or a Unix/TCP socket in tests) carries
//! many independent **channels**. Each channel is one operation: an exec, a
//! PTY shell, a file upload/download, a forwarded TCP connection, or a health
//! check. The same framing is meant to be relayed unchanged over the cloud
//! WebSocket later.
//!
//! Guarantees:
//! - **Flow control:** every channel has a fixed receive window
//!   ([`frame::WINDOW`]). A sender blocks once it has used up its credit and
//!   resumes when the receiver grants more. A peer that exceeds the window is
//!   a protocol violation and the session is closed, so buffers stay bounded.
//! - **Liveness:** both sides ping every heartbeat interval. A session that
//!   hears nothing for three intervals closes itself.
//! - **Integrity:** uploads and downloads carry a SHA-256 checksum, are size
//!   checked, and land on disk with an atomic rename.
//! - **Reattach:** an exec started with a `session_id` survives a dropped
//!   connection; its output is buffered in the guest and a new connection can
//!   attach from the byte offsets it already has (see [`detach`]). Execs
//!   without one, and shells, are killed when their channel closes.

pub mod client;
pub mod detach;
pub mod frame;
pub mod messages;
pub mod mux;
pub mod pty;
pub mod server;

pub use messages::{stream, Control, OpenRequest};
pub use mux::{Channel, Config, Event, Session, Transport};
