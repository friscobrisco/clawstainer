use anyhow::{Context, Result};
use std::process::Command;

const BRIDGE_NAME: &str = "claw-br0";

/// Create a TAP device and attach it to the bridge
pub fn create_tap(name: &str) -> Result<()> {
    Command::new("ip")
        .args(["tuntap", "add", name, "mode", "tap"])
        .output()
        .context("Failed to create TAP device")?;

    Command::new("ip")
        .args(["link", "set", name, "master", BRIDGE_NAME])
        .output()
        .context("Failed to attach TAP to bridge")?;

    Command::new("ip")
        .args(["link", "set", name, "up"])
        .output()
        .context("Failed to bring up TAP device")?;

    Ok(())
}

/// Delete a TAP device
pub fn delete_tap(name: &str) -> Result<()> {
    let _ = Command::new("ip")
        .args(["link", "delete", name])
        .output();
    Ok(())
}

/// Generate a TAP device name from a machine ID (max 15 chars for interface names)
pub fn tap_name(machine_id: &str) -> String {
    // machine_id is like "sb-a1b2c3d4" — use the hex part
    let short = machine_id.trim_start_matches("sb-");
    format!("tap-{short}")
}
