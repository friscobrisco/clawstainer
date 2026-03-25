use anyhow::{Context, Result};
use std::process::Command;

const SUBNET: &str = "10.0.0.0/24";

/// Ensure NAT masquerade rules and IP forwarding are set up
pub fn ensure_nat() -> Result<()> {
    // Enable IP forwarding
    Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .output()
        .context("Failed to enable IP forwarding")?;

    // Check if masquerade rule already exists
    let check = Command::new("iptables")
        .args(["-t", "nat", "-C", "POSTROUTING", "-s", SUBNET, "-j", "MASQUERADE"])
        .output();

    if let Ok(o) = check {
        if o.status.success() {
            return Ok(()); // Rule already exists
        }
    }

    // Add masquerade rule
    Command::new("iptables")
        .args(["-t", "nat", "-A", "POSTROUTING", "-s", SUBNET, "-j", "MASQUERADE"])
        .output()
        .context("Failed to add masquerade rule")?;

    // Block inter-sandbox traffic
    let _ = Command::new("iptables")
        .args(["-I", "FORWARD", "-s", SUBNET, "-d", SUBNET, "-j", "DROP"])
        .output();

    Ok(())
}

/// Remove NAT rules (call when last machine is destroyed)
pub fn remove_nat() -> Result<()> {
    let _ = Command::new("iptables")
        .args(["-t", "nat", "-D", "POSTROUTING", "-s", SUBNET, "-j", "MASQUERADE"])
        .output();

    let _ = Command::new("iptables")
        .args(["-D", "FORWARD", "-s", SUBNET, "-d", SUBNET, "-j", "DROP"])
        .output();

    Ok(())
}
