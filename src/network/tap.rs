use anyhow::Result;
use std::process::Command;

/// Delete a TAP device. Used to clean up Firecracker machines created before
/// per-VM networking (`clawstainer::vmnet`), which used bridged TAPs.
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
