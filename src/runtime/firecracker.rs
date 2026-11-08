use anyhow::Result;
use std::process::Command;

use crate::error::ClawError;
use crate::firecracker::{api::FirecrackerApi, assets, rootfs, vsock as fc_vsock};
use clawstainer::vmnet::{self, VmNet};
use clawstainer::wire;

/// Kills the VM process and removes its directory on drop unless disarmed,
/// so a failed create doesn't leave a running Firecracker behind.
struct VmGuard {
    id: String,
    pid: Option<u32>,
}

impl VmGuard {
    fn disarm(&mut self) {
        self.id.clear();
    }
}

impl Drop for VmGuard {
    fn drop(&mut self) {
        if self.id.is_empty() {
            return;
        }
        if let Some(pid) = self.pid {
            // SAFETY: plain kill(2); the pid is our own child.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
                libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
            }
        }
        // Keep the log of a failed create for debugging.
        let failed_dir = "/var/lib/clawstainer/failed";
        let _ = std::fs::create_dir_all(failed_dir);
        let _ = std::fs::copy(
            format!("{MACHINES_DIR}/{}/firecracker.log", self.id),
            format!("{failed_dir}/{}.log", self.id),
        );
        let _ = rootfs::cleanup_vm_rootfs(&self.id);
    }
}

/// Tears down a VM's network on drop unless disarmed, so a failed create
/// doesn't leak a TAP, a firewall chain, or an address slot.
struct NetGuard<'a> {
    state: &'a StateStore,
    vm: Option<VmNet>,
}

impl NetGuard<'_> {
    fn disarm(&mut self) {
        self.vm = None;
    }
}

impl Drop for NetGuard<'_> {
    fn drop(&mut self) {
        if let Some(vm) = self.vm.take() {
            vmnet::teardown(&vm);
            let _ = self.state.with_lock(|s| {
                s.network.vm_slots.remove(&vm.slot);
                Ok(())
            });
        }
    }
}

/// How long a cold boot may take before the guest agent must answer. A full
/// Ubuntu userspace takes several seconds, longer under nested virtualization.
const AGENT_BOOT_TIMEOUT_MS: u64 = 60_000;

/// Output kept per stream for a buffered exec; the rest is counted, not stored.
const MAX_EXEC_OUTPUT_BYTES: usize = 1_048_576;

/// A writer that keeps the first `cap` bytes and counts everything.
struct CappedBuffer {
    buf: Vec<u8>,
    cap: usize,
    total: u64,
}

impl CappedBuffer {
    fn new(cap: usize) -> Self {
        CappedBuffer { buf: Vec::new(), cap, total: 0 }
    }
    fn bytes(&self) -> &[u8] {
        &self.buf
    }
    fn total(&self) -> u64 {
        self.total
    }
    fn truncated(&self) -> bool {
        self.total > self.buf.len() as u64
    }
}

impl std::io::Write for CappedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.total += data.len() as u64;
        let room = self.cap.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&data[..room.min(data.len())]);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
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

    fn pid_path(machine_id: &str) -> String {
        format!("{MACHINES_DIR}/{machine_id}/firecracker.pid")
    }

    /// The live Firecracker pid for this machine, if it is running. Checks
    /// the process's command line so a recycled pid is never mistaken for
    /// the VM.
    fn live_pid(machine_id: &str, hint: Option<u32>) -> Option<u32> {
        let from_file = std::fs::read_to_string(Self::pid_path(machine_id))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        [hint, from_file].into_iter().flatten().find(|&pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|cmd| {
                    let cmd = String::from_utf8_lossy(&cmd);
                    cmd.contains("firecracker") && cmd.contains(machine_id)
                })
                .unwrap_or(false)
        })
    }

    fn socket_path(machine_id: &str) -> String {
        format!("{MACHINES_DIR}/{machine_id}/firecracker.sock")
    }

    fn vsock_path(machine_id: &str) -> String {
        format!("{MACHINES_DIR}/{machine_id}/vsock.sock")
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

        // Everything created from here on is removed again if create fails.
        let mut vm_guard = VmGuard { id: id.clone(), pid: None };

        // Create per-VM rootfs
        let vm_rootfs = rootfs::create_vm_rootfs(&id)
            .map_err(|e| ClawError::CreateFailed(format!("Rootfs creation failed: {e}")))?;

        // Network setup: a routed TAP with its own /30 and firewall chain.
        // The guard tears it down again if any later step fails.
        let mut net_guard = NetGuard { state, vm: None };
        if opts.network == "nat" {
            let slot = state.with_lock(|s| {
                let slot = vmnet::allocate_slot(&s.network.vm_slots)?;
                s.network.vm_slots.insert(slot, id.clone());
                Ok(slot)
            })?;
            let vm = VmNet::new(slot, &id)?;
            net_guard.vm = Some(vm.clone());
            vmnet::setup(&vm)
                .map_err(|e| ClawError::CreateFailed(format!("Network setup failed: {e:#}")))?;
        }
        let ip = net_guard.vm.as_ref().map(|vm| vm.guest_ip.to_string());

        // Prepare socket path
        let sock_path = Self::socket_path(&id);

        // Remove stale socket if exists
        let _ = std::fs::remove_file(&sock_path);

        // Start Firecracker process
        // Firecracker's own log (and the guest serial console) go here.
        let log_path = format!("{MACHINES_DIR}/{id}/firecracker.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|e| ClawError::CreateFailed(format!("Cannot create {log_path}: {e}")))?;
        let child = Command::new(fc_bin.to_str().unwrap())
            .args(["--api-sock", &sock_path])
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .map_err(|e| ClawError::CreateFailed(format!("Failed to start Firecracker: {e}")))?;

        let pid = child.id();
        vm_guard.pid = Some(pid);
        std::fs::write(Self::pid_path(&id), pid.to_string())
            .map_err(|e| ClawError::CreateFailed(format!("Cannot write pid file: {e}")))?;
        let now = chrono::Utc::now();

        // Wait for API to be ready
        let api = FirecrackerApi::new(&sock_path);
        api.wait_for_ready(5000)
            .map_err(|e| ClawError::CreateFailed(format!("Firecracker API not ready: {e}")))?;

        // Configure the VM via API
        let base_args = "console=ttyS0 reboot=k panic=1 pci=off";
        let boot_args = match &net_guard.vm {
            Some(vm) => format!("{base_args} {}", vm.kernel_ip_arg()),
            None => base_args.to_string(),
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
        if let Some(vm) = &net_guard.vm {
            let resp = api.put("/network-interfaces/eth0", &serde_json::json!({
                "iface_id": "eth0",
                "guest_mac": vm.mac,
                "host_dev_name": vm.tap,
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

        // PUT /vsock — enables host<->guest communication
        let vsock_path = Self::vsock_path(&id);
        let resp = api.put("/vsock", &serde_json::json!({
            "vsock_id": "1",
            "guest_cid": 3,
            "uds_path": vsock_path,
        }))?;
        if !resp.is_success() {
            return Err(ClawError::CreateFailed(format!("Failed to set vsock: {}", resp.body)).into());
        }

        // Start the VM
        let resp = api.put("/actions", &serde_json::json!({
            "action_type": "InstanceStart",
        }))?;
        if !resp.is_success() {
            return Err(ClawError::CreateFailed(format!("Failed to start VM: {}", resp.body)).into());
        }

        // Wait for guest agent to become reachable
        eprintln!("Waiting for guest agent...");
        fc_vsock::wait_for_agent(&vsock_path, AGENT_BOOT_TIMEOUT_MS)
            .map_err(|e| ClawError::CreateFailed(format!("Guest agent not reachable: {e}")))?;

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
            security: opts.security.clone(),
            has_env_file: opts.env_file.is_some(),
            linger: opts.linger,
            fleet_name: None,
        };

        state.with_lock(|s| {
            s.machines.insert(id.clone(), machine);
            Ok(())
        })?;
        net_guard.disarm();
        vm_guard.disarm();

        // Auto-destroy timeout.
        // We call the full destroy() path (not just `pkill`) so that state.json,
        // the IP allocation, the TAP device, and the rootfs are all cleaned up.
        // Otherwise `claw list` would keep showing the VM as running and its
        // resources would leak until the user ran destroy manually.
        if opts.timeout > 0 {
            let timeout = opts.timeout;
            let destroy_id = id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(timeout));
                if let Ok(state) = StateStore::new() {
                    let _ = FirecrackerRuntime.destroy(&destroy_id, &state);
                }
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

        let vsock_path = Self::vsock_path(machine_id);
        let session = fc_vsock::connect_wire(&vsock_path)
            .map_err(|e| ClawError::ExecFailed(format!("Cannot reach guest agent: {e:#}")))?;

        let req = wire::OpenRequest::Exec {
            command: opts.command,
            workdir: Some(opts.workdir),
            env: opts.env,
            user: opts.user,
            timeout_secs: opts.timeout,
        };
        let mut stdout = CappedBuffer::new(MAX_EXEC_OUTPUT_BYTES);
        let mut stderr = CappedBuffer::new(MAX_EXEC_OUTPUT_BYTES);
        let exit = wire::client::exec(&session, &req, None::<std::io::Empty>, &mut stdout, &mut stderr)
            .map_err(|e| ClawError::ExecFailed(format!("{e:#}")))?;

        let truncated = stdout.truncated() || stderr.truncated();
        Ok(ExecResult {
            machine_id: machine_id.to_string(),
            exit_code: exit.exit_code(),
            stdout: String::from_utf8_lossy(stdout.bytes()).into_owned(),
            stderr: String::from_utf8_lossy(stderr.bytes()).into_owned(),
            duration_ms: exit.duration_ms,
            timed_out: exit.timed_out,
            truncated,
            total_bytes: truncated.then(|| stdout.total() + stderr.total()),
            peak_memory_bytes: None,
            cpu_time_us: None,
        })
    }

    fn shell(&self, machine_id: &str, user: &str) -> Result<()> {
        require_linux()?;

        let vsock_path = Self::vsock_path(machine_id);
        let session = fc_vsock::connect_wire(&vsock_path)
            .map_err(|e| ClawError::ExecFailed(format!("Cannot reach guest agent: {e:#}")))?;
        let exit = wire::client::interactive_shell(&session, user, None)
            .map_err(|e| ClawError::ExecFailed(format!("{e:#}")))?;
        if exit.exit_code() != 0 {
            return Err(ClawError::ExecFailed(format!(
                "Shell exited with status {}",
                exit.exit_code()
            ))
            .into());
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

        // Ask the guest to shut down (x86 only; aarch64 has no Ctrl-Alt-Del),
        // give it a moment, then make sure the VMM is gone.
        if let Some(pid) = Self::live_pid(machine_id, machine.pid) {
            let sock_path = Self::socket_path(machine_id);
            if std::path::Path::new(&sock_path).exists() {
                let api = FirecrackerApi::new(&sock_path);
                let _ = api.put("/actions", &serde_json::json!({
                    "action_type": "SendCtrlAltDel",
                }));
            }
            let alive = || unsafe { libc::kill(pid as i32, 0) } == 0;
            for _ in 0..30 {
                if !alive() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if alive() {
                // SAFETY: plain kill(2) on the VM's own pid (verified above).
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                for _ in 0..50 {
                    if !alive() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            if alive() {
                return Err(ClawError::ExecFailed(format!(
                    "Firecracker process {pid} for {machine_id} did not exit"
                ))
                .into());
            }
        }

        // Clean up the VM's network: its TAP, firewall chain, and slot.
        let slot = state.with_read_lock(|s| {
            Ok(s.network.vm_slots.iter().find(|(_, m)| m.as_str() == machine_id).map(|(k, _)| *k))
        })?;
        match slot.map(|slot| VmNet::new(slot, machine_id)) {
            Some(Ok(vm)) => vmnet::teardown(&vm),
            // Machines created before per-VM networking used a bridged TAP.
            _ => {
                let _ = network::tap::delete_tap(&network::tap::tap_name(machine_id));
            }
        }

        // Clean up rootfs and socket
        let _ = rootfs::cleanup_vm_rootfs(machine_id);

        // Release IP and remove from state
        state.with_lock(|s| {
            if let Some(m) = s.machines.remove(machine_id) {
                if let Some(ref ip) = m.ip {
                    network::ipam::release(&mut s.network, ip);
                }
            }
            s.network.vm_slots.retain(|_, m| m.as_str() != machine_id);
            if s.network.vm_slots.is_empty() {
                let _ = vmnet::remove_table();
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

        let pid = Self::live_pid(machine_id, None);
        let status = if pid.is_some() { "running" } else { "stopped" }.to_string();

        Ok(MachineStatus {
            id: machine_id.to_string(),
            status,
            pid,
        })
    }
}
