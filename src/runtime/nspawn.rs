use anyhow::{Context, Result};
use std::process::Command;
use std::time::Instant;

use crate::error::ClawError;
use crate::image;
use crate::network;
use crate::state::{Machine, StateStore};

use super::{
    require_linux, CreateOpts, DestroyResult, ExecOpts, ExecResult, MachineInfo, MachineStatus,
    Runtime,
};

const MAX_OUTPUT_BYTES: usize = 1_048_576; // 1MB

pub struct NspawnRuntime;

impl NspawnRuntime {
    pub fn new() -> Self {
        Self
    }

    fn generate_id() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let hex: u32 = rng.gen();
        format!("sb-{:08x}", hex)
    }

    fn generate_name() -> String {
        use rand::seq::SliceRandom;
        let adjectives = [
            "bold", "calm", "dark", "fast", "keen", "loud", "neat", "pale", "quick", "shy",
            "slim", "warm", "wise", "cool", "bright",
        ];
        let animals = [
            "parrot", "falcon", "otter", "panda", "raven", "tiger", "whale", "zebra", "eagle",
            "koala", "lynx", "moose", "newt", "owl", "fox",
        ];
        let mut rng = rand::thread_rng();
        let adj = adjectives.choose(&mut rng).unwrap();
        let animal = animals.choose(&mut rng).unwrap();
        format!("{adj}-{animal}")
    }
}

impl Runtime for NspawnRuntime {
    fn create(&self, opts: CreateOpts, state: &StateStore) -> Result<MachineInfo> {
        require_linux()?;

        let id = Self::generate_id();
        let name = opts.name.unwrap_or_else(Self::generate_name);

        // Ensure base image exists
        let base_path = image::bootstrap::ensure_base_image()?;

        // Setup overlay filesystem
        let root_path = image::overlay::setup(&id, &base_path)?;

        // Allocate IP if networking is enabled
        let ip = if opts.network == "nat" {
            // Ensure bridge and NAT are up
            network::bridge::ensure_bridge()?;
            network::nat::ensure_nat()?;
            let allocated_ip = state.with_lock(|s| {
                let ip = network::ipam::allocate(&mut s.network)?;
                Ok(ip)
            })?;
            Some(allocated_ip)
        } else {
            None
        };

        // Build nspawn command
        let mut cmd = Command::new("systemd-nspawn");
        cmd.args([
            "--boot",
            &format!("--machine={id}"),
            "-D",
            root_path.to_str().unwrap(),
            "--register=yes",
            &format!("--property=MemoryMax={}M", opts.memory_mb),
            &format!("--property=CPUQuota={}%", opts.cpus * 100),
        ]);

        if opts.network == "nat" {
            cmd.args(["--network-veth", "--network-bridge=claw-br0"]);
        } else {
            cmd.arg("--private-network");
        }

        // Launch in background
        let child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| ClawError::CreateFailed(format!("Failed to launch systemd-nspawn: {e}")))?;

        let pid = child.id();
        let now = chrono::Utc::now();

        // Wait for machine to become ready and D-Bus to be available
        wait_for_machine_ready(&id)?;

        // If we have an IP, configure it inside the container
        if let Some(ref ip) = ip {
            // Wait a bit more for systemd inside the container to fully initialize
            wait_for_exec_ready(&id)?;
            configure_container_network(&id, ip)?;
        }

        // Save to state
        let machine = Machine {
            id: id.clone(),
            name: name.clone(),
            status: "running".to_string(),
            pid: Some(pid),
            ip: ip.clone(),
            memory_mb: opts.memory_mb,
            cpus: opts.cpus,
            network: opts.network,
            created_at: now,
            components: Vec::new(),
            timeout: opts.timeout,
            root_path: root_path.to_string_lossy().to_string(),
            runtime: "nspawn".to_string(),
        };

        state.with_lock(|s| {
            s.machines.insert(id.clone(), machine);
            Ok(())
        })?;

        // If timeout > 0, spawn a background thread to auto-destroy
        if opts.timeout > 0 {
            let timeout = opts.timeout;
            let destroy_id = id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(timeout));
                let _ = Command::new("machinectl")
                    .args(["poweroff", &destroy_id])
                    .output();
            });
        }

        Ok(MachineInfo {
            id,
            name,
            status: "running".to_string(),
            ip,
            created_at: now.to_rfc3339(),
        })
    }

    fn exec(&self, machine_id: &str, opts: ExecOpts) -> Result<ExecResult> {
        require_linux()?;

        let start = Instant::now();

        let mut cmd = Command::new("systemd-run");
        cmd.args([
            &format!("--machine={machine_id}"),
            "--wait",
            "--pipe",
            "--collect",
            "--service-type=exec",
            &format!("--property=RuntimeMaxSec={}", opts.timeout),
        ]);

        if opts.user != "root" {
            cmd.arg(format!("--uid={}", opts.user));
        }

        if opts.workdir != "/root" {
            cmd.arg(format!("--working-directory={}", opts.workdir));
        }

        // Set standard environment variables that scripts expect
        let home_buf = format!("/home/{}", opts.user);
        let home = if opts.user == "root" { "/root" } else { &home_buf };
        let default_env = [
            ("HOME", home.to_string()),
            ("USER", opts.user.clone()),
            ("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()),
            ("LANG", "C.UTF-8".to_string()),
            ("TERM", "xterm-256color".to_string()),
        ];

        for (k, v) in &default_env {
            if !opts.env.contains_key(*k) {
                cmd.arg(format!("--setenv={k}={v}"));
            }
        }

        for (k, v) in &opts.env {
            cmd.arg(format!("--setenv={k}={v}"));
        }

        cmd.arg("--");
        cmd.args(["sh", "-c", &opts.command]);

        let output = cmd.output()
            .map_err(|e| ClawError::ExecFailed(format!("Failed to execute systemd-run: {e}")))?;
        let duration_ms = start.elapsed().as_millis() as u64;

        let timed_out = output.status.code() == Some(255);

        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut truncated = false;
        let mut total_bytes = None;

        if stdout.len() > MAX_OUTPUT_BYTES {
            let total = stdout.len() as u64;
            stdout.truncate(MAX_OUTPUT_BYTES);
            truncated = true;
            total_bytes = Some(total);
        }
        if stderr.len() > MAX_OUTPUT_BYTES {
            stderr.truncate(MAX_OUTPUT_BYTES);
        }

        Ok(ExecResult {
            machine_id: machine_id.to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout,
            stderr,
            duration_ms,
            timed_out,
            truncated,
            total_bytes,
        })
    }

    fn shell(&self, machine_id: &str, user: &str) -> Result<()> {
        require_linux()?;

        let status = Command::new("machinectl")
            .args(["shell", &format!("{user}@{machine_id}")])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| ClawError::ExecFailed(format!("Failed to launch machinectl shell: {e}")))?;

        if !status.success() {
            return Err(ClawError::ExecFailed(format!("Shell exited with status: {status}")).into());
        }
        Ok(())
    }

    fn destroy(&self, machine_id: &str, state: &StateStore) -> Result<DestroyResult> {
        require_linux()?;

        let machine = state.with_read_lock(|s| {
            s.machines
                .get(machine_id)
                .cloned()
                .ok_or_else(|| ClawError::MachineNotFound(machine_id.to_string()).into())
        })?;

        let uptime = chrono::Utc::now()
            .signed_duration_since(machine.created_at)
            .num_seconds() as u64;

        // Graceful shutdown
        let _ = Command::new("machinectl")
            .args(["poweroff", machine_id])
            .output();

        // Wait up to 5 seconds
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let output = Command::new("machinectl")
                .args(["show", machine_id, "--property=State"])
                .output();
            match output {
                Ok(o) => {
                    let state_str = String::from_utf8_lossy(&o.stdout);
                    if state_str.contains("State=") && !state_str.contains("running") {
                        break;
                    }
                }
                Err(_) => break, // Machine already gone
            }
        }

        // Force terminate if still running
        let _ = Command::new("machinectl")
            .args(["terminate", machine_id])
            .output();

        // Clean up overlay
        let _ = image::overlay::teardown(machine_id);

        // Release IP and remove from state
        state.with_lock(|s| {
            if let Some(m) = s.machines.remove(machine_id) {
                if let Some(ref ip) = m.ip {
                    network::ipam::release(&mut s.network, ip);
                }
            }
            // If no machines left, tear down bridge
            if s.machines.is_empty() {
                let _ = network::bridge::remove_bridge();
                let _ = network::nat::remove_nat();
            }
            Ok(())
        })?;

        Ok(DestroyResult {
            machine_id: machine_id.to_string(),
            status: "destroyed".to_string(),
            uptime_seconds: uptime,
        })
    }

    fn status(&self, machine_id: &str) -> Result<MachineStatus> {
        require_linux()?;

        let output = Command::new("machinectl")
            .args(["show", machine_id, "--property=State", "--property=Leader"])
            .output()
            .context("Failed to query machine status")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut status = "unknown".to_string();
        let mut pid = None;

        for line in stdout.lines() {
            if let Some(val) = line.strip_prefix("State=") {
                status = val.to_string();
            }
            if let Some(val) = line.strip_prefix("Leader=") {
                pid = val.parse().ok();
            }
        }

        Ok(MachineStatus {
            id: machine_id.to_string(),
            status,
            pid,
        })
    }
}

fn wait_for_machine_ready(machine_id: &str) -> Result<()> {
    for i in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let output = Command::new("machinectl")
            .args(["show", machine_id, "--property=State"])
            .output();
        if let Ok(o) = output {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.contains("State=running") {
                return Ok(());
            }
        }
        if i == 29 {
            return Err(ClawError::CreateFailed(
                format!("Timed out waiting for machine {machine_id} to become ready")
            ).into());
        }
    }
    Ok(())
}

fn wait_for_exec_ready(machine_id: &str) -> Result<()> {
    // Wait until we can successfully execute a command inside the container.
    // This ensures D-Bus and systemd are fully initialized.
    for i in 0..50 {
        let output = Command::new("systemd-run")
            .args([
                &format!("--machine={machine_id}"),
                "--wait",
                "--pipe",
                "--collect",
                "--",
                "true",
            ])
            .output();

        if let Ok(o) = output {
            if o.status.success() {
                return Ok(());
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
        if i == 49 {
            return Err(ClawError::CreateFailed(
                format!("Timed out waiting for exec readiness in machine {machine_id}")
            ).into());
        }
    }
    Ok(())
}

fn configure_container_network(machine_id: &str, ip: &str) -> Result<()> {
    // Configure networking inside the container using ip commands directly.
    // This is more reliable than depending on systemd-networkd being active.
    let cmd_str = format!(
        "ip link set host0 up && \
         ip addr add {ip}/24 dev host0 && \
         ip route add default via 10.0.0.1 && \
         echo 'nameserver 8.8.8.8\nnameserver 8.8.4.4' > /etc/resolv.conf"
    );

    let output = Command::new("systemd-run")
        .args([
            &format!("--machine={machine_id}"),
            "--wait",
            "--pipe",
            "--collect",
            "--",
            "sh",
            "-c",
            &cmd_str,
        ])
        .output()
        .context("Failed to configure container networking")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ClawError::CreateFailed(
            format!("Failed to configure container networking: {stderr}")
        ).into());
    }

    Ok(())
}
