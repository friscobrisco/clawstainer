//! Binary framing.
//!
//! ```text
//! +-----------+--------+-----------+-----------------+
//! | channel   | kind   | length    | payload         |
//! | u32 BE    | u8     | u32 BE    | `length` bytes  |
//! +-----------+--------+-----------+-----------------+
//! ```
//!
//! A connection starts with a one-line handshake: the client sends
//! [`PREAMBLE`], the server answers with the same line, then both sides speak
//! frames. The line lets the agent tell this protocol apart from the legacy
//! newline-delimited JSON protocol on the same port.

use std::io::{self, Read, Write};

/// Handshake line sent by both sides.
pub const PREAMBLE: &[u8] = b"CLAW/2\n";

/// Largest payload a single frame may carry.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// Per-channel receive window in bytes of `Data` payload (excluding the
/// stream tag). Both sides use the same constant, so it is part of the
/// protocol version.
pub const WINDOW: u32 = 256 * 1024;

const HEADER_LEN: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Client opens a channel. Payload: JSON [`super::OpenRequest`].
    Open = 1,
    /// Payload: one stream-tag byte followed by data.
    Data = 2,
    /// Payload: one stream-tag byte. No more data on that stream.
    Eof = 3,
    /// Payload: JSON [`super::Control`].
    Control = 4,
    /// Payload: u32 BE credit increment, in bytes.
    Window = 5,
    /// The sender is done with the channel. Empty payload.
    Close = 6,
    /// Channel 0. Payload echoed back in the matching `Pong`.
    Ping = 7,
    Pong = 8,
}

impl Kind {
    fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Open,
            2 => Kind::Data,
            3 => Kind::Eof,
            4 => Kind::Control,
            5 => Kind::Window,
            6 => Kind::Close,
            7 => Kind::Ping,
            8 => Kind::Pong,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub kind: Kind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(channel: u32, kind: Kind, payload: Vec<u8>) -> Frame {
        Frame { channel, kind, payload }
    }
}

/// Write one frame as a single buffer, then flush.
pub fn write_frame<W: Write + ?Sized>(w: &mut W, f: &Frame) -> io::Result<()> {
    if f.payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame payload too large",
        ));
    }
    let mut buf = Vec::with_capacity(HEADER_LEN + f.payload.len());
    buf.extend_from_slice(&f.channel.to_be_bytes());
    buf.push(f.kind as u8);
    buf.extend_from_slice(&(f.payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&f.payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Read one frame. Returns `Ok(None)` on a clean EOF at a frame boundary.
pub fn read_frame<R: Read + ?Sized>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut hdr = [0u8; HEADER_LEN];
    // Distinguish a clean close (no bytes) from a truncated header.
    loop {
        match r.read(&mut hdr[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    r.read_exact(&mut hdr[1..])?;

    let channel = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let kind = Kind::from_u8(hdr[4]).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("unknown frame kind {}", hdr[4]))
    })?;
    let len = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload of {len} bytes exceeds {MAX_PAYLOAD}"),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame { channel, kind, payload }))
}

/// Read one `\n`-terminated line byte by byte, so nothing past the newline is
/// consumed. Used for the handshake, where the remaining bytes belong to the
/// next protocol layer. Returns the line including the newline.
pub fn read_line_unbuffered<R: Read + ?Sized>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < max {
        match r.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(line)
}

/// Client side of the handshake.
pub fn client_handshake<S: Read + Write + ?Sized>(s: &mut S) -> io::Result<()> {
    s.write_all(PREAMBLE)?;
    s.flush()?;
    let reply = read_line_unbuffered(s, 64)?;
    if reply != PREAMBLE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "peer does not speak CLAW/2 (got {:?})",
                String::from_utf8_lossy(&reply)
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(7, Kind::Data, vec![1, 2, 3, 4]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).unwrap();
        assert_eq!(buf.len(), HEADER_LEN + 4);
        let mut cur = io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap(), Some(f));
        assert_eq!(read_frame(&mut cur).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_payload() {
        let f = Frame::new(1, Kind::Data, vec![0; MAX_PAYLOAD + 1]);
        assert!(write_frame(&mut Vec::new(), &f).is_err());

        let mut hdr = vec![0, 0, 0, 1, Kind::Data as u8];
        hdr.extend_from_slice(&((MAX_PAYLOAD as u32) + 1).to_be_bytes());
        assert!(read_frame(&mut io::Cursor::new(hdr)).is_err());
    }

    #[test]
    fn rejects_unknown_kind_and_truncation() {
        let bad = vec![0, 0, 0, 1, 99, 0, 0, 0, 0];
        assert!(read_frame(&mut io::Cursor::new(bad)).is_err());
        let truncated = vec![0, 0, 0, 1, Kind::Data as u8, 0, 0, 0, 5, 1, 2];
        assert!(read_frame(&mut io::Cursor::new(truncated)).is_err());
    }

    #[test]
    fn unbuffered_line_stops_at_newline() {
        let mut cur = io::Cursor::new(b"CLAW/2\nrest".to_vec());
        assert_eq!(read_line_unbuffered(&mut cur, 64).unwrap(), PREAMBLE);
        let mut rest = String::new();
        cur.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest");
    }
}
