use anyhow::Result;
use std::process::Command;

use crate::cli::PortForwardArgs;
use crate::error::ClawError;
use crate::state::StateStore;

pub fn run(args: PortForwardArgs, state: &StateStore) -> Result<()> {
    // Parse port mapping
    let (host_port, sandbox_port) = parse_port(&args.port)?;

    let ip = state.get_machine_ip(&args.machine_id)?;

    // Add DNAT rule: forward host_port -> sandbox_ip:sandbox_port
    let status = Command::new("iptables")
        .args([
            "-t", "nat",
            "-A", "PREROUTING",
            "-p", "tcp",
            "--dport", &host_port.to_string(),
            "-j", "DNAT",
            "--to-destination", &format!("{ip}:{sandbox_port}"),
        ])
        .status();

    if status.map(|s| s.success()).unwrap_or(false) {
        // Also add for locally-originated traffic
        let _ = Command::new("iptables")
            .args([
                "-t", "nat",
                "-A", "OUTPUT",
                "-p", "tcp",
                "--dport", &host_port.to_string(),
                "-j", "DNAT",
                "--to-destination", &format!("{ip}:{sandbox_port}"),
            ])
            .status();
    }

    // Allow forwarding to this destination
    let _ = Command::new("iptables")
        .args([
            "-A", "FORWARD",
            "-p", "tcp",
            "-d", &ip,
            "--dport", &sandbox_port.to_string(),
            "-j", "ACCEPT",
        ])
        .status();

    eprintln!(
        "Forwarding port {host_port} -> {ip}:{sandbox_port} (sandbox {})",
        args.machine_id
    );
    eprintln!("Access at: localhost:{host_port}");

    Ok(())
}

fn parse_port(port_str: &str) -> Result<(u16, u16)> {
    if let Some((host, sandbox)) = port_str.split_once(':') {
        let h: u16 = host.parse()
            .map_err(|_| ClawError::ExecFailed(format!("Invalid host port: {host}")))?;
        let s: u16 = sandbox.parse()
            .map_err(|_| ClawError::ExecFailed(format!("Invalid sandbox port: {sandbox}")))?;
        Ok((h, s))
    } else {
        let p: u16 = port_str.parse()
            .map_err(|_| ClawError::ExecFailed(format!("Invalid port: {port_str}")))?;
        Ok((p, p))
    }
}
