//! Library surface shared by the `clawstainer` CLI and the `claw-agent`
//! guest binary.
//!
//! Only the pieces that both binaries (and the future cloud host daemon)
//! need live here. Everything else stays private to `src/main.rs`.

pub mod wire;
