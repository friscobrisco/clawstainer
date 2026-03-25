use anyhow::{Context, Result};
use std::process::Command;

const BRIDGE_NAME: &str = "claw-br0";
const BRIDGE_IP: &str = "10.0.0.1/24";

/// Idempotently ensure the bridge exists and is up
pub fn ensure_bridge() -> Result<()> {
    // Check if bridge already exists
    let output = Command::new("ip")
        .args(["link", "show", BRIDGE_NAME])
        .output();

    if let Ok(o) = output {
        if o.status.success() {
            return Ok(()); // Bridge already exists
        }
    }

    // Create bridge
    Command::new("ip")
        .args(["link", "add", BRIDGE_NAME, "type", "bridge"])
        .output()
        .context("Failed to create bridge")?;

    // Assign IP
    Command::new("ip")
        .args(["addr", "add", BRIDGE_IP, "dev", BRIDGE_NAME])
        .output()
        .context("Failed to assign IP to bridge")?;

    // Bring up
    Command::new("ip")
        .args(["link", "set", BRIDGE_NAME, "up"])
        .output()
        .context("Failed to bring up bridge")?;

    Ok(())
}

/// Remove the bridge (call when last machine is destroyed)
pub fn remove_bridge() -> Result<()> {
    Command::new("ip")
        .args(["link", "set", BRIDGE_NAME, "down"])
        .output()
        .context("Failed to bring down bridge")?;

    Command::new("ip")
        .args(["link", "delete", BRIDGE_NAME])
        .output()
        .context("Failed to delete bridge")?;

    Ok(())
}
