use anyhow::Result;
use std::process::Command;
use std::time::Instant;

use crate::error::ClawError;
use crate::firecracker::{api::FirecrackerApi, assets, rootfs};
use crate::network;
use crate::state::{Machine, StateStore};

use super::{
    require_linux, CreateOpts, DestroyResult, ExecOpts, ExecResult, MachineInfo, MachineStatus,
    Runtime,
};

const MACHINES_DIR: &str = "/var/lib/clawstainer/machines";

pub struct FirecrackerRuntime;

impl FirecrackerRuntime {
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

    fn socket_path(machine_id: &str) -> String {
        format!("{MACHINES_DIR}/{machine_id}/firecracker.sock")
    }
}

impl Runtime for FirecrackerRuntime {
    fn create(&self, opts: CreateOpts, state: &StateStore) -> Result<MachineInfo> {
        require_linux()?;

        let id = Self::generate_id();
        let name = opts.name.unwrap_or_else(Self::generate_name);

        // Ensure Firecracker binary and kernel are available
        let (fc_bin, kernel_path) = assets::ensure_assets()
            .map_err(|e| ClawError::CreateFailed(format!("Asset setup failed: {e}")))?;

        // Create per-VM rootfs
        let vm_rootfs = rootfs::create_vm_rootfs(&id)
            .map_err(|e| ClawError::CreateFailed(format!("Rootfs creation failed: {e}")))?;

        // Network setup
        let ip = if opts.network == "nat" {
            network::bridge::ensure_bridge()?;
            network::nat::ensure_nat()?;
            let allocated_ip = state.with_lock(|s| {
                let ip = network::ipam::allocate(&mut s.network)?;
                Ok(ip)
            })?;

            // Create TAP device and attach to bridge
            let tap = network::tap::tap_name(&id);
            network::tap::create_tap(&tap)
                .map_err(|e| ClawError::CreateFailed(format!("TAP setup failed: {e}")))?;

            Some(allocated_ip)
        } else {
            None
        };

        // Prepare socket path
        let sock_path = Self::socket_path(&id);

        // Remove stale socket if exists
        let _ = std::fs::remove_file(&sock_path);

        // Start Firecracker process
        let child = Command::new(fc_bin.to_str().unwrap())
            .args(["--api-sock", &sock_path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| ClawError::CreateFailed(format!("Failed to start Firecracker: {e}")))?;

        let pid = child.id();
        let now = chrono::Utc::now();

        // Wait for API to be ready
        let api = FirecrackerApi::new(&sock_path);
        api.wait_for_ready(5000)
            .map_err(|e| ClawError::CreateFailed(format!("Firecracker API not ready: {e}")))?;

        // Configure the VM via API
        let tap_name = network::tap::tap_name(&id);
        let guest_mac = format!("AA:FC:00:00:00:{:02x}", ip.as_ref().map(|i| {
            i.split('.').last().unwrap_or("2").parse::<u8>().unwrap_or(2)
        }).unwrap_or(2));

        // Boot args with static IP configuration
        let boot_args = if let Some(ref ip) = ip {
            format!(
                "console=ttyS0 reboot=k panic=1 pci=off ip={ip}::10.0.0.1:255.255.255.0::eth0:off"
            )
        } else {
            "console=ttyS0 reboot=k panic=1 pci=off".to_string()
        };

        // PUT /boot-source
        let resp = api.put("/boot-source", &serde_json::json!({
            "kernel_image_path": kernel_path.to_str().unwrap(),
            "boot_args": boot_args,
        }))?;
        if !resp.is_success() {
            return Err(ClawError::CreateFailed(format!("Failed to set boot source: {}", resp.body)).into());
        }

        // PUT /drives/rootfs
        let resp = api.put("/drives/rootfs", &serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": vm_rootfs.to_str().unwrap(),
            "is_root_device": true,
            "is_read_only": false,
        }))?;
        if !resp.is_success() {
            return Err(ClawError::CreateFailed(format!("Failed to set drive: {}", resp.body)).into());
        }

        // PUT /network-interfaces/eth0 (only if NAT networking)
        if opts.network == "nat" {
            let resp = api.put("/network-interfaces/eth0", &serde_json::json!({
                "iface_id": "eth0",
                "guest_mac": guest_mac,
                "host_dev_name": tap_name,
            }))?;
            if !resp.is_success() {
                return Err(ClawError::CreateFailed(format!("Failed to set network: {}", resp.body)).into());
            }
        }

        // PUT /machine-config
        let resp = api.put("/machine-config", &serde_json::json!({
            "vcpu_count": opts.cpus,
            "mem_size_mib": opts.memory_mb,
        }))?;
        if !resp.is_success() {
            return Err(ClawError::CreateFailed(format!("Failed to set machine config: {}", resp.body)).into());
        }

        // Start the VM
        let resp = api.put("/actions", &serde_json::json!({
            "action_type": "InstanceStart",
        }))?;
        if !resp.is_success() {
            return Err(ClawError::CreateFailed(format!("Failed to start VM: {}", resp.body)).into());
        }

        // Wait for VM to boot (give it a moment for the kernel to initialize)
        std::thread::sleep(std::time::Duration::from_millis(500));

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
            root_path: vm_rootfs.to_string_lossy().to_string(),
            runtime: "firecracker".to_string(),
        };

        state.with_lock(|s| {
            s.machines.insert(id.clone(), machine);
            Ok(())
        })?;

        // Auto-destroy timeout
        if opts.timeout > 0 {
            let timeout = opts.timeout;
            let destroy_id = id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(timeout));
                // Kill the firecracker process
                let _ = Command::new("pkill")
                    .args(["-f", &format!("firecracker.*{destroy_id}")])
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

        // For now, use SSH via the VM's IP address.
        // TODO: Replace with vsock guest agent for better performance.
        let start = Instant::now();

        // Get the machine's IP
        let sock_path = Self::socket_path(machine_id);
        if !std::path::Path::new(&sock_path).exists() {
            return Err(ClawError::MachineNotFound(machine_id.to_string()).into());
        }

        // Execute via SSH (requires SSH to be set up in the rootfs)
        // Fallback: use Firecracker serial console
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", &format!("ConnectTimeout={}", opts.timeout),
            "-o", "LogLevel=ERROR",
        ]);

        if opts.user != "root" {
            cmd.arg(format!("{}@{}", opts.user, "10.0.0.2")); // TODO: get IP from state
        } else {
            cmd.arg(format!("root@10.0.0.2")); // TODO: get IP from state
        }

        cmd.arg(&opts.command);

        let output = cmd.output()
            .map_err(|e| ClawError::ExecFailed(format!("SSH exec failed: {e}")))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(ExecResult {
            machine_id: machine_id.to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            duration_ms,
            timed_out: false,
            truncated: false,
            total_bytes: None,
        })
    }

    fn shell(&self, machine_id: &str, user: &str) -> Result<()> {
        require_linux()?;

        // TODO: Replace with vsock-based shell
        let status = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR",
                &format!("{user}@10.0.0.2"), // TODO: get IP from state
            ])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| ClawError::ExecFailed(format!("SSH shell failed: {e}")))?;

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

        // Send shutdown via API
        let sock_path = Self::socket_path(machine_id);
        if std::path::Path::new(&sock_path).exists() {
            let api = FirecrackerApi::new(&sock_path);
            let _ = api.put("/actions", &serde_json::json!({
                "action_type": "SendCtrlAltDel",
            }));

            // Wait up to 3 seconds for graceful shutdown
            if let Some(pid) = machine.pid {
                for _ in 0..30 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    // Check if process still exists
                    let alive = Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    if !alive {
                        break;
                    }
                }

                // Force kill if still running
                let _ = Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
            }
        }

        // Clean up TAP device
        let tap = network::tap::tap_name(machine_id);
        let _ = network::tap::delete_tap(&tap);

        // Clean up rootfs and socket
        let _ = rootfs::cleanup_vm_rootfs(machine_id);

        // Release IP and remove from state
        state.with_lock(|s| {
            if let Some(m) = s.machines.remove(machine_id) {
                if let Some(ref ip) = m.ip {
                    network::ipam::release(&mut s.network, ip);
                }
            }
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

        let sock_path = Self::socket_path(machine_id);
        let status = if std::path::Path::new(&sock_path).exists() {
            "running".to_string()
        } else {
            "stopped".to_string()
        };

        Ok(MachineStatus {
            id: machine_id.to_string(),
            status,
            pid: None,
        })
    }
}
