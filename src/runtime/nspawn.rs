use anyhow::{Context, Result};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::ClawError;
use crate::image;
use crate::network;
use crate::state::{Machine, StateStore};

use std::collections::HashSet;

use super::{
    generate_id, generate_name, poll_with_backoff, require_linux, CreateOpts, DestroyResult,
    ExecOpts, ExecResult, MachineInfo, MachineStatus, Runtime,
};

const MAX_OUTPUT_BYTES: usize = 1_048_576; // 1MB

/// Capabilities dropped in "strict" security profile.
const STRICT_CAP_DROP: &[&str] = &["CAP_NET_RAW", "CAP_SYS_PTRACE", "CAP_MKNOD"];

pub struct NspawnRuntime;

impl NspawnRuntime {
    pub fn new() -> Self {
        Self
    }

    /// Resolve the final set of capabilities to drop based on security profile + overrides.
    fn resolve_caps_to_drop(opts: &CreateOpts) -> Vec<String> {
        let mut drop_set: HashSet<String> = HashSet::new();

        // Start from the profile baseline
        if opts.security == "strict" {
            for cap in STRICT_CAP_DROP {
                drop_set.insert(cap.to_string());
            }
        }

        // Apply explicit --cap-drop additions
        for cap in &opts.cap_drop {
            drop_set.insert(cap.clone());
        }

        // Apply explicit --cap-add removals
        for cap in &opts.cap_add {
            drop_set.remove(cap);
        }

        let mut caps: Vec<String> = drop_set.into_iter().collect();
        caps.sort();
        caps
    }
}

impl Runtime for NspawnRuntime {
    fn create(&self, opts: CreateOpts, state: &StateStore) -> Result<MachineInfo> {
        require_linux()?;

        let id = generate_id();
        let caps_to_drop = Self::resolve_caps_to_drop(&opts);
        let name = opts.name.unwrap_or_else(generate_name);
        let create_start = Instant::now();

        eprint!("Creating {name}...");

        // Ensure base image exists
        let base_path = image::bootstrap::ensure_base_image()?;
        eprint!(" base image ok,");

        // Extract snapshot if creating from one
        let snapshot_dir = if let Some(ref snap_name) = opts.from_snapshot {
            Some(image::snapshot::extract(snap_name)?)
        } else {
            None
        };

        // Setup overlay filesystem
        let root_path = image::overlay::setup(&id, &base_path, snapshot_dir.as_ref())?;
        eprint!(" overlay ok,");

        // Enable systemd lingering if requested
        if opts.linger {
            let linger_dir = root_path.join("var/lib/systemd/linger");
            std::fs::create_dir_all(&linger_dir).map_err(|e| {
                ClawError::CreateFailed(format!("Failed to create linger dir: {e}"))
            })?;
            std::fs::File::create(linger_dir.join("root"))
                .map_err(|e| ClawError::CreateFailed(format!("Failed to enable linger: {e}")))?;
            eprint!(" linger ok,");
        }

        // Allocate IP if networking is enabled
        let ip = if opts.network == "nat" {
            // Ensure bridge and NAT are up
            network::bridge::ensure_bridge()?;
            network::nat::ensure_nat()?;
            let allocated_ip = state.with_lock(|s| {
                let ip = network::ipam::allocate(&mut s.network, &id)?;
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
            // Use overlay-backed /tmp instead of tiny tmpfs default
            &format!("--tmpfs=/tmp:mode=1777,size={}M", opts.memory_mb),
        ]);

        // Apply security profile
        if !caps_to_drop.is_empty() {
            cmd.arg(format!("--drop-capability={}", caps_to_drop.join(",")));
        }
        if opts.security == "strict" {
            cmd.arg("--no-new-privileges=true");
        }

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
            .map_err(|e| {
                ClawError::CreateFailed(format!("Failed to launch systemd-nspawn: {e}"))
            })?;

        let pid = child.id();
        let now = chrono::Utc::now();

        // Wait for machine to be fully ready
        eprint!(" booting...");
        let needs_exec = ip.is_some() || opts.env_file.is_some();
        wait_for_machine_fully_ready(&id, needs_exec)?;

        // If we have an IP, configure it inside the container
        if let Some(ref ip) = ip {
            configure_container_network(&id, ip)?;
            eprint!(" network ok,");
        }

        // Inject env file if provided
        let has_env_file = if let Some(ref env_file_path) = opts.env_file {
            inject_env_file(&id, env_file_path)?;
            eprint!(" env ok,");
            true
        } else {
            false
        };

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
            security: opts.security.clone(),
            has_env_file,
            linger: opts.linger,
            fleet_name: None,
        };

        state.with_lock(|s| {
            s.machines.insert(id.clone(), machine);
            Ok(())
        })?;

        eprintln!(" ready ({:.1}s)", create_start.elapsed().as_secs_f64());

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
        let home = if opts.user == "root" {
            "/root"
        } else {
            &home_buf
        };
        let default_env = [
            ("HOME", home.to_string()),
            ("USER", opts.user.clone()),
            (
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            ),
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

        let output = cmd
            .output()
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

        let (peak_memory_bytes, cpu_time_us) = read_cgroup_stats(machine_id);

        Ok(ExecResult {
            machine_id: machine_id.to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout,
            stderr,
            duration_ms,
            timed_out,
            truncated,
            total_bytes,
            peak_memory_bytes,
            cpu_time_us,
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
            .map_err(|e| {
                ClawError::ExecFailed(format!("Failed to launch machinectl shell: {e}"))
            })?;

        if !status.success() {
            return Err(
                ClawError::ExecFailed(format!("Shell exited with status: {status}")).into(),
            );
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

        // Wait for shutdown with exponential backoff
        let destroy_id = machine_id.to_string();
        let _ = poll_with_backoff(
            || {
                Command::new("machinectl")
                    .args(["show", &destroy_id, "--property=State"])
                    .output()
                    .map(|o| {
                        let s = String::from_utf8_lossy(&o.stdout);
                        s.contains("State=") && !s.contains("running")
                    })
                    .unwrap_or(true) // Machine already gone
            },
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(5),
            "Timed out waiting for machine shutdown",
        );

        // Force terminate if still running
        let _ = Command::new("machinectl")
            .args(["terminate", machine_id])
            .output();

        // Clean up overlay — if this fails, mark as failed instead of removing
        let cleanup_failed = image::overlay::teardown(machine_id).is_err();

        state.with_lock(|s| {
            if cleanup_failed {
                // Mark as failed so the user can investigate
                if let Some(m) = s.machines.get_mut(machine_id) {
                    m.status = "failed".to_string();
                }
            } else {
                // Cleanup succeeded — remove from state and release IP
                if let Some(m) = s.machines.remove(machine_id) {
                    if let Some(ref ip) = m.ip {
                        network::ipam::release(&mut s.network, ip);
                    }
                }
            }
            // If no running machines left, tear down bridge
            let has_running = s.machines.values().any(|m| m.status == "running");
            if !has_running {
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

        if !output.status.success() {
            return Ok(MachineStatus {
                id: machine_id.to_string(),
                status: "stopped".to_string(),
                pid: None,
            });
        }

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
    let id = machine_id.to_string();
    poll_with_backoff(
        || {
            Command::new("machinectl")
                .args(["show", &id, "--property=State"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("State=running"))
                .unwrap_or(false)
        },
        Duration::from_millis(50),
        Duration::from_secs(1),
        Duration::from_secs(10),
        &format!("Timed out waiting for machine {machine_id} to become ready"),
    )
    .map_err(|e| ClawError::CreateFailed(e.to_string()))?;
    Ok(())
}

fn wait_for_exec_ready(machine_id: &str) -> Result<()> {
    let id = machine_id.to_string();
    poll_with_backoff(
        || {
            Command::new("systemd-run")
                .args([
                    &format!("--machine={id}"),
                    "--wait",
                    "--pipe",
                    "--collect",
                    "--",
                    "true",
                ])
                .output()
                .ok()
                .map(|o| o.status.success())
                .unwrap_or(false)
        },
        Duration::from_millis(50),
        Duration::from_secs(1),
        Duration::from_secs(10),
        &format!("Timed out waiting for exec readiness in machine {machine_id}"),
    )
    .map_err(|e| ClawError::CreateFailed(e.to_string()))?;
    Ok(())
}

/// Wait for machine to be fully ready for use (running + exec ready if needed).
fn wait_for_machine_fully_ready(machine_id: &str, needs_exec: bool) -> Result<()> {
    wait_for_machine_ready(machine_id)?;
    if needs_exec {
        wait_for_exec_ready(machine_id)?;
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
        return Err(ClawError::CreateFailed(format!(
            "Failed to configure container networking: {stderr}"
        ))
        .into());
    }

    Ok(())
}

/// Read a .env file from the host and write its contents into /etc/environment
/// inside the container. Parses KEY=VAL lines, skips comments and blank lines.
fn inject_env_file(machine_id: &str, env_file_path: &str) -> Result<()> {
    let contents = std::fs::read_to_string(env_file_path)
        .with_context(|| format!("Failed to read env file: {env_file_path}"))?;

    // Parse and filter: keep only KEY=VAL lines, skip comments and blanks
    let mut env_lines = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.contains('=') {
            env_lines.push(trimmed.to_string());
        }
    }

    if env_lines.is_empty() {
        return Ok(());
    }

    // Use printf to avoid shell interpretation of the values
    let cmd_str = format!(
        "printf '%s\\n' {} >> /etc/environment",
        env_lines
            .iter()
            .map(|l| format!("'{}'", l.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
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
        .context("Failed to inject env file into container")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ClawError::CreateFailed(format!("Failed to inject env file: {stderr}")).into());
    }

    Ok(())
}

fn read_cgroup_stats(machine_id: &str) -> (Option<u64>, Option<u64>) {
    let escaped_id = machine_id.replace('-', "\\x2d");
    let cgroup_base = format!("/sys/fs/cgroup/machine.slice/machine-{}.scope", escaped_id);
    let peak_memory = std::fs::read_to_string(format!("{}/memory.peak", cgroup_base))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let cpu_time = std::fs::read_to_string(format!("{}/cpu.stat", cgroup_base))
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("usage_usec"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        });
    (peak_memory, cpu_time)
}
