//! Firecracker microVM runtime.
//!
//! Every VM runs under Firecracker's jailer (see `firecracker::jail`): its
//! own chroot, its own unprivileged uid/gid with no capabilities, and a
//! cgroup with memory/CPU/process limits. Each VM also gets its own routed
//! TAP and firewall chain (`clawstainer::vmnet`), and talks to the host only
//! over vsock using the CLAW/2 protocol (`clawstainer::wire`).
//!
//! A per-VM slot (in `state.network.vm_slots`) determines both the VM's
//! network addresses and its uid, so they never collide.

use anyhow::{anyhow, bail, Context, Result};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clawstainer::snapcrypt::{self, LocalKeyStore, SnapshotMeta};
use clawstainer::vmnet::{self, VmNet};
use clawstainer::wire;

use crate::error::ClawError;
use crate::firecracker::jail::{self, Jail};
use crate::firecracker::{api::FirecrackerApi, assets, rootfs, vsock as fc_vsock};
use crate::network;
use crate::state::{Machine, StateStore};

use super::{
    require_linux, CreateOpts, DestroyResult, ExecOpts, ExecResult, MachineInfo, MachineStatus,
    PauseResult, Runtime,
};

const KEYS_DIR: &str = "/var/lib/clawstainer/keys";
const FAILED_DIR: &str = "/var/lib/clawstainer/failed";

/// How long a cold boot may take before the guest agent must answer. A full
/// Ubuntu userspace takes several seconds, longer under nested virtualization.
const AGENT_BOOT_TIMEOUT_MS: u64 = 60_000;

/// How long an exec keeps running in the guest after its connection drops,
/// and how many times the CLI tries to reattach to it.
const EXEC_REATTACH_GRACE_SECS: u64 = 120;
const EXEC_REATTACH_RETRIES: u32 = 5;

/// Output kept per stream for a buffered exec; the rest is counted, not stored.
const MAX_EXEC_OUTPUT_BYTES: usize = 1_048_576;

/// Snapshot create/load of a large VM can take minutes on slow disks.
const SNAPSHOT_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Turn a non-2xx Firecracker API response into an error.
fn api_ok(resp: Result<crate::firecracker::api::ApiResponse>, what: &str) -> Result<()> {
    let resp = resp.with_context(|| format!("Firecracker API call failed: {what}"))?;
    if !resp.is_success() {
        bail!("Firecracker refused to {what} ({}): {}", resp.status, resp.body.trim());
    }
    Ok(())
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// SIGKILL a VMM and wait (up to 2 s) for it to disappear.
fn kill_and_wait(pid: u32) -> Result<()> {
    // SAFETY: plain kill(2) on a pid we verified belongs to the VM.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    for _ in 0..100 {
        if !alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("Firecracker process {pid} did not exit")
}

const LOCKS_DIR: &str = "/var/lib/clawstainer/locks";

/// Per-machine lock held for the whole of a pause or resume. The kernel
/// drops it if the holder dies, which is how recovery tells an interrupted
/// operation from one still in progress.
fn op_lock(machine_id: &str, wait: bool) -> Result<Option<std::fs::File>> {
    use fs2::FileExt;
    mkdir_private(Path::new(LOCKS_DIR))?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(Path::new(LOCKS_DIR).join(format!("{machine_id}.lock")))?;
    if wait {
        f.lock_exclusive()?;
        Ok(Some(f))
    } else {
        Ok(f.try_lock_exclusive().ok().map(|_| f))
    }
}

fn mkdir_private(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("cannot create {}", path.display()))
}

/// What a snapshot must match to be restorable on this host.
struct SnapshotBinding {
    sandbox_id: String,
    kernel_sha256: String,
    cpu_model: String,
}

impl SnapshotBinding {
    fn current(sandbox_id: &str, kernel: &Path) -> Result<SnapshotBinding> {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut std::fs::File::open(kernel)?, &mut hasher)?;
        Ok(SnapshotBinding {
            sandbox_id: sandbox_id.to_string(),
            kernel_sha256: wire::messages::hex(&hasher.finalize()),
            cpu_model: cpu_model(),
        })
    }

    fn for_kind(&self, kind: &str) -> SnapshotMeta {
        SnapshotMeta {
            sandbox_id: self.sandbox_id.clone(),
            kind: kind.to_string(),
            firecracker_version: assets::FC_VERSION.to_string(),
            kernel_sha256: self.kernel_sha256.clone(),
            cpu_model: self.cpu_model.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    fn check(&self, found: &SnapshotMeta, kind: &str) -> Result<()> {
        let mismatch = |what: &str, want: &str, got: &str| {
            Err(anyhow!("{kind} snapshot was taken with {what} {got:?}, this host has {want:?}"))
        };
        if found.sandbox_id != self.sandbox_id {
            return mismatch("sandbox", &self.sandbox_id, &found.sandbox_id);
        }
        if found.kind != kind {
            return mismatch("kind", kind, &found.kind);
        }
        if found.firecracker_version != assets::FC_VERSION {
            return mismatch("Firecracker", assets::FC_VERSION, &found.firecracker_version);
        }
        if found.kernel_sha256 != self.kernel_sha256 {
            return mismatch("kernel", &self.kernel_sha256, &found.kernel_sha256);
        }
        if found.cpu_model != self.cpu_model {
            return mismatch("CPU", &self.cpu_model, &found.cpu_model);
        }
        Ok(())
    }
}

/// A stable description of the host CPU model from /proc/cpuinfo.
fn cpu_model() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| {
        info.lines()
            .find(|l| l.split(':').next().map(str::trim) == Some(key))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    };
    if let Some(name) = field("model name") {
        return name;
    }
    // aarch64 exposes the implementer and part number instead of a name.
    match (field("CPU implementer"), field("CPU part")) {
        (Some(i), Some(p)) => format!("arm implementer {i} part {p}"),
        _ => "unknown".to_string(),
    }
}

/// Undoes a partial create on drop unless disarmed: kills the VMM, tears
/// down the network, removes the jail and cgroup, and releases the slot.
/// The Firecracker log is kept in `FAILED_DIR` for debugging.
struct CreateGuard<'a> {
    state: &'a StateStore,
    id: String,
    slot: Option<u32>,
    net: Option<VmNet>,
    pid: Option<u32>,
    armed: bool,
}

impl Drop for CreateGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(pid) = self.pid {
            let _ = kill_and_wait(pid);
            // SAFETY: reap our own child so it doesn't linger as a zombie.
            unsafe {
                libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
        if let Some(vm) = &self.net {
            vmnet::teardown(vm);
        }
        let _ = std::fs::create_dir_all(FAILED_DIR);
        let _ = std::fs::copy(
            Jail::host_path_for(&self.id, jail::LOG),
            format!("{FAILED_DIR}/{}.log", self.id),
        );
        Jail::remove(&self.id);
        if let Some(slot) = self.slot {
            let _ = self.state.with_lock(|s| {
                s.network.vm_slots.remove(&slot);
                Ok(())
            });
        }
    }
}

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

    fn socket_path(machine_id: &str) -> PathBuf {
        Jail::host_path_for(machine_id, jail::API_SOCK)
    }

    fn vsock_path(machine_id: &str) -> String {
        Jail::host_path_for(machine_id, jail::VSOCK).display().to_string()
    }

    fn pid_path(machine_id: &str) -> PathBuf {
        Jail::host_path_for(machine_id, jail::PID_FILE)
    }

    fn api(machine_id: &str) -> FirecrackerApi {
        FirecrackerApi::new(&Self::socket_path(machine_id).display().to_string())
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

    /// Repair a VM whose last pause was interrupted (e.g. the CLI was
    /// killed mid-way): the VMM is alive with its vCPUs paused and the guest
    /// frozen, and a plaintext snapshot may be sitting in its jail. Removes
    /// the plaintext and any partial encrypted files, resumes the vCPUs, and
    /// thaws the guest. A no-op for a healthy VM.
    fn recover_interrupted_pause(machine_id: &str) -> Result<()> {
        let work = Jail::host_path_for(machine_id, "/snapshot.work");
        let api = Self::api(machine_id);
        let vcpus_paused = api
            .get("/")
            .ok()
            .and_then(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok())
            .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(|s| s == "Paused"))
            .unwrap_or(false);
        if !work.exists() && !vcpus_paused {
            return Ok(());
        }
        // A pause or resume still in progress holds the lock: leave it be.
        let Some(_lock) = op_lock(machine_id, false)? else {
            return Ok(());
        };
        eprintln!("Recovering {machine_id} from an interrupted pause...");
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(Jail::snapshot_dir_for(machine_id));
        if vcpus_paused {
            api_ok(api.patch("/vm", &serde_json::json!({ "state": "Resumed" })), "resume vCPUs")?;
        }
        // The guest's vsock can refuse the first connection right after its
        // vCPUs resume; wait until the agent answers, then thaw.
        let vsock = Self::vsock_path(machine_id);
        fc_vsock::wait_for_agent(&vsock, 15_000)?;
        let session = fc_vsock::connect_wire(&vsock)?;
        wire::client::guest_control(&session, &wire::OpenRequest::Thaw)
            .context("could not thaw the guest after an interrupted pause")?;
        Ok(())
    }

    /// The jail for an existing machine (its slot gives its uid).
    fn jail_for(machine_id: &str, state: &StateStore) -> Result<Jail> {
        let slot = state.with_read_lock(|s| {
            Ok(s.network.vm_slots.iter().find(|(_, m)| m.as_str() == machine_id).map(|(k, _)| *k))
        })?;
        let slot = slot.ok_or_else(|| anyhow!("{machine_id} has no slot; it predates jailing"))?;
        Ok(Jail::new(machine_id, slot))
    }
}

impl Runtime for FirecrackerRuntime {
    fn create(&self, opts: CreateOpts, state: &StateStore) -> Result<MachineInfo> {
        require_linux()?;

        let id = Self::generate_id();
        let name = opts.name.unwrap_or_else(Self::generate_name);

        let assets = assets::ensure_assets()
            .map_err(|e| ClawError::CreateFailed(format!("Asset setup failed: {e:#}")))?;

        // Everything created from here on is undone if create fails.
        let mut guard = CreateGuard { state, id: id.clone(), slot: None, net: None, pid: None, armed: true };

        // One slot gives the VM both its uid and its network addresses.
        let slot = state.with_lock(|s| {
            let slot = vmnet::allocate_slot(&s.network.vm_slots)?;
            s.network.vm_slots.insert(slot, id.clone());
            Ok(slot)
        })?;
        guard.slot = Some(slot);
        let jail = Jail::new(&id, slot);
        jail.prepare(&assets.kernel)
            .map_err(|e| ClawError::CreateFailed(format!("Jail setup failed: {e:#}")))?;

        let vm_rootfs = jail.host_path(jail::ROOTFS);
        rootfs::create_vm_rootfs(&vm_rootfs)
            .map_err(|e| ClawError::CreateFailed(format!("Rootfs creation failed: {e:#}")))?;
        jail.give(&vm_rootfs)?;

        // A routed TAP owned by the VM's uid (the jailed VMM has no
        // CAP_NET_ADMIN), with its own /30 and firewall chain.
        let net = if opts.network == "nat" {
            let vm = VmNet::new(slot, &id)?;
            guard.net = Some(vm.clone());
            vmnet::setup_owned(&vm, Some((jail.uid, jail.gid)))
                .map_err(|e| ClawError::CreateFailed(format!("Network setup failed: {e:#}")))?;
            Some(vm)
        } else {
            None
        };
        let ip = net.as_ref().map(|vm| vm.guest_ip.to_string());

        let pid = jail
            .spawn(&assets.jailer, &assets.firecracker, opts.memory_mb, opts.cpus)
            .map_err(|e| ClawError::CreateFailed(format!("{e:#}")))?;
        guard.pid = Some(pid);
        let now = chrono::Utc::now();

        let api = Self::api(&id);
        api.wait_for_ready(5000)
            .map_err(|e| ClawError::CreateFailed(format!("Firecracker API not ready: {e}")))?;

        // Every path below is inside the jail.
        let base_args = "console=ttyS0 reboot=k panic=1 pci=off";
        let boot_args = match &net {
            Some(vm) => format!("{base_args} {}", vm.kernel_ip_arg()),
            None => base_args.to_string(),
        };
        let configure = || -> Result<()> {
            api_ok(api.put("/boot-source", &serde_json::json!({
                "kernel_image_path": jail::KERNEL,
                "boot_args": boot_args,
            })), "set the boot source")?;
            api_ok(api.put("/drives/rootfs", &serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": jail::ROOTFS,
                "is_root_device": true,
                "is_read_only": false,
            })), "attach the root disk")?;
            if let Some(vm) = &net {
                api_ok(api.put("/network-interfaces/eth0", &serde_json::json!({
                    "iface_id": "eth0",
                    "guest_mac": vm.mac,
                    "host_dev_name": vm.tap,
                })), "attach the network")?;
            }
            api_ok(api.put("/machine-config", &serde_json::json!({
                "vcpu_count": opts.cpus,
                "mem_size_mib": opts.memory_mb,
            })), "set the machine config")?;
            api_ok(api.put("/vsock", &serde_json::json!({
                "vsock_id": "1",
                "guest_cid": 3,
                "uds_path": jail::VSOCK,
            })), "attach vsock")?;
            api_ok(api.put("/actions", &serde_json::json!({ "action_type": "InstanceStart" })), "start the VM")
        };
        configure().map_err(|e| ClawError::CreateFailed(format!("{e:#}")))?;

        eprintln!("Waiting for guest agent...");
        fc_vsock::wait_for_agent(&Self::vsock_path(&id), AGENT_BOOT_TIMEOUT_MS)
            .map_err(|e| ClawError::CreateFailed(format!("Guest agent not reachable: {e}")))?;

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
        guard.armed = false;

        // NOTE: this thread lives in the CLI process, which exits right after
        // create returns, so the timeout only fires for long-lived callers.
        // Enforcing timeouts belongs in the host daemon / control plane (M2).
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
        Self::recover_interrupted_pause(machine_id)
            .map_err(|e| ClawError::ExecFailed(format!("{e:#}")))?;

        let vsock = Self::vsock_path(machine_id);
        let connect = || {
            fc_vsock::connect_wire(&vsock)
                .map_err(|e| ClawError::ExecFailed(format!("Cannot reach guest agent: {e:#}")))
        };

        // A detachable exec: if the vsock connection drops (VMM hiccup,
        // host load), reconnect and resume the output where it stopped
        // instead of killing the command.
        let session_id = format!("exec-{:016x}", rand::random::<u64>());
        let req = wire::OpenRequest::Exec {
            command: opts.command,
            workdir: Some(opts.workdir),
            env: opts.env,
            user: opts.user,
            timeout_secs: opts.timeout,
            session_id: Some(session_id.clone()),
            keep_secs: EXEC_REATTACH_GRACE_SECS,
        };
        let mut stdout = CappedBuffer::new(MAX_EXEC_OUTPUT_BYTES);
        let mut stderr = CappedBuffer::new(MAX_EXEC_OUTPUT_BYTES);
        let mut progress = wire::client::Progress::default();

        let session = connect()?;
        let mut result = wire::client::exec_detachable(&session, &req, &mut stdout, &mut stderr, &mut progress);
        drop(session);
        // Once the command has started, a dropped connection (or the agent
        // briefly unreachable while it restarts) is retried by reattaching.
        let mut retries = 0;
        let mut reattaching = false;
        while let Err(e) = &result {
            let retryable = e.is::<wire::client::ConnectionLost>() || (reattaching && e.is::<ClawError>());
            if !retryable || retries == EXEC_REATTACH_RETRIES {
                break;
            }
            retries += 1;
            reattaching = true;
            eprintln!("Connection to the guest agent dropped; reattaching ({retries}/{EXEC_REATTACH_RETRIES})...");
            std::thread::sleep(std::time::Duration::from_millis(500 * retries as u64));
            result = connect().map_err(anyhow::Error::from).and_then(|session| {
                wire::client::attach(&session, &session_id, &mut stdout, &mut stderr, &mut progress)
            });
        }
        let exit = result.map_err(|e| ClawError::ExecFailed(format!("{e:#}")))?;

        let truncated = stdout.truncated() || stderr.truncated() || progress.lost > 0;
        Ok(ExecResult {
            machine_id: machine_id.to_string(),
            exit_code: exit.exit_code(),
            stdout: String::from_utf8_lossy(stdout.bytes()).into_owned(),
            stderr: String::from_utf8_lossy(stderr.bytes()).into_owned(),
            duration_ms: exit.duration_ms,
            timed_out: exit.timed_out,
            truncated,
            total_bytes: truncated.then(|| stdout.total() + stderr.total() + progress.lost),
            peak_memory_bytes: None,
            cpu_time_us: None,
        })
    }

    fn shell(&self, machine_id: &str, user: &str) -> Result<()> {
        require_linux()?;
        Self::recover_interrupted_pause(machine_id)
            .map_err(|e| ClawError::ExecFailed(format!("{e:#}")))?;

        let session = fc_vsock::connect_wire(&Self::vsock_path(machine_id))
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
            if Self::socket_path(machine_id).exists() {
                let _ = Self::api(machine_id)
                    .put("/actions", &serde_json::json!({ "action_type": "SendCtrlAltDel" }));
            }
            for _ in 0..30 {
                if !alive(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if alive(pid) {
                kill_and_wait(pid).map_err(|e| ClawError::ExecFailed(format!("{e:#}")))?;
            }
        }

        // Network: TAP, firewall chain.
        let slot = state.with_read_lock(|s| {
            Ok(s.network.vm_slots.iter().find(|(_, m)| m.as_str() == machine_id).map(|(k, _)| *k))
        })?;
        if let Some(Ok(vm)) = slot.map(|slot| VmNet::new(slot, machine_id)) {
            vmnet::teardown(&vm);
        }

        // Jail (disk, sockets, log), cgroup, and encrypted snapshots.
        Jail::remove(machine_id);
        let _ = std::fs::remove_dir_all(Jail::snapshot_dir_for(machine_id));
        let _ = std::fs::remove_file(Path::new(LOCKS_DIR).join(format!("{machine_id}.lock")));

        state.with_lock(|s| {
            s.machines.remove(machine_id);
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

    fn pause(&self, machine_id: &str, state: &StateStore) -> Result<PauseResult> {
        require_linux()?;
        let started = Instant::now();
        let machine = state.get_machine(machine_id)?;
        if machine.status != "running" {
            bail!("{machine_id} is {}, not running", machine.status);
        }
        let pid = Self::live_pid(machine_id, machine.pid)
            .ok_or_else(|| anyhow!("{machine_id} has no running Firecracker process"))?;
        Self::recover_interrupted_pause(machine_id)?;
        let _lock = op_lock(machine_id, true)?;
        let jail = Self::jail_for(machine_id, state)?;
        let assets = assets::ensure_assets()?;
        let meta = SnapshotBinding::current(machine_id, &assets.kernel)?;

        // Firecracker (as the VM uid) writes the plaintext snapshot into a
        // private directory in its jail; only the encrypted result leaves it.
        let work_inner = "/snapshot.work";
        let work = jail.host_path(work_inner);
        let _ = std::fs::remove_dir_all(&work);
        mkdir_private(&work)?;
        jail.give(&work)?;
        let snap_dir = Jail::snapshot_dir_for(machine_id);
        mkdir_private(&snap_dir)?;
        let (mem_enc, state_enc) = (snap_dir.join("memory.snap"), snap_dir.join("vmstate.snap"));
        let api = Self::api(machine_id).with_timeout(SNAPSHOT_API_TIMEOUT);
        let vsock = Self::vsock_path(machine_id);

        // 1. Flush and freeze the guest filesystem so the disk is consistent
        //    with the memory image. The agent thaws by itself if we vanish.
        {
            let session = fc_vsock::connect_wire(&vsock)?;
            wire::client::guest_control(&session, &wire::OpenRequest::Freeze { auto_thaw_secs: 300 })
                .context("Could not freeze the guest filesystem")?;
        }

        let result = (|| -> Result<u64> {
            // 2. Stop the vCPUs, 3. write the snapshot, 4. encrypt it.
            api_ok(api.patch("/vm", &serde_json::json!({ "state": "Paused" })), "pause vCPUs")?;
            api_ok(
                api.put("/snapshot/create", &serde_json::json!({
                    "snapshot_type": "Full",
                    "snapshot_path": format!("{work_inner}/vmstate"),
                    "mem_file_path": format!("{work_inner}/memory"),
                })),
                "create snapshot",
            )?;
            let keys = LocalKeyStore::new(KEYS_DIR);
            let key_id = LocalKeyStore::DEFAULT_KEY_ID;
            snapcrypt::encrypt_file(&work.join("memory"), &mem_enc, &keys, key_id, &meta.for_kind("memory"), snapcrypt::DEFAULT_CHUNK)?;
            snapcrypt::encrypt_file(&work.join("vmstate"), &state_enc, &keys, key_id, &meta.for_kind("vmstate"), snapcrypt::DEFAULT_CHUNK)?;
            Ok(std::fs::metadata(&mem_enc)?.len() + std::fs::metadata(&state_enc)?.len())
        })();
        // Plaintext memory never outlives this call.
        let _ = std::fs::remove_dir_all(&work);

        let snapshot_bytes = match result {
            Ok(bytes) => bytes,
            Err(e) => {
                // Roll back: keep the VM running exactly as before.
                let _ = std::fs::remove_file(&mem_enc);
                let _ = std::fs::remove_file(&state_enc);
                let _ = api.patch("/vm", &serde_json::json!({ "state": "Resumed" }));
                if let Ok(session) = fc_vsock::connect_wire(&vsock) {
                    let _ = wire::client::guest_control(&session, &wire::OpenRequest::Thaw);
                }
                return Err(e.context(format!("Pause of {machine_id} failed; it is still running")));
            }
        };

        // 5. The snapshot is safe on disk: stop the VMM and free its memory.
        kill_and_wait(pid)?;
        state.update_machine_status(machine_id, "paused", None)?;

        Ok(PauseResult {
            machine_id: machine_id.to_string(),
            status: "paused".to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            snapshot_bytes: Some(snapshot_bytes),
        })
    }

    fn resume(&self, machine_id: &str, state: &StateStore) -> Result<PauseResult> {
        require_linux()?;
        let started = Instant::now();
        let machine = state.get_machine(machine_id)?;
        if machine.status != "paused" {
            bail!("{machine_id} is {}, not paused", machine.status);
        }
        let jail = Self::jail_for(machine_id, state)?;
        let snap_dir = Jail::snapshot_dir_for(machine_id);
        let (mem_enc, state_enc) = (snap_dir.join("memory.snap"), snap_dir.join("vmstate.snap"));
        if !mem_enc.exists() || !state_enc.exists() {
            bail!("{machine_id} is marked paused but its snapshot files are missing");
        }

        // A memory snapshot only restores on the same CPU model, kernel,
        // and Firecracker version it was taken with.
        let assets = assets::ensure_assets()?;
        let expected = SnapshotBinding::current(machine_id, &assets.kernel)?;
        for (path, kind) in [(&mem_enc, "memory"), (&state_enc, "vmstate")] {
            let (_, found) = snapcrypt::read_meta(path)?;
            expected.check(&found, kind)?;
        }

        let _lock = op_lock(machine_id, true)?;
        // After a host reboot the VM's TAP and firewall chain are gone (the
        // jail and encrypted snapshot survive): recreate them on the same
        // slot, so the restored guest keeps its address.
        if machine.network == "nat" {
            let vm = VmNet::new(jail.uid - jail::UID_BASE, machine_id)?;
            if !Path::new("/sys/class/net").join(&vm.tap).exists() {
                vmnet::setup_owned(&vm, Some((jail.uid, jail.gid)))
                    .context("could not recreate the VM's network")?;
            }
        }
        // An interrupted earlier resume may have left a VMM (and plaintext)
        // behind; start from a clean jail.
        if let Some(pid) = Self::live_pid(machine_id, None) {
            kill_and_wait(pid)?;
        }
        let work_inner = "/snapshot.work";
        let work = jail.host_path(work_inner);
        let _ = std::fs::remove_dir_all(&work);
        mkdir_private(&work)?;
        let keys = LocalKeyStore::new(KEYS_DIR);

        let result = (|| -> Result<u32> {
            snapcrypt::decrypt_file(&mem_enc, &work.join("memory"), &keys)?;
            snapcrypt::decrypt_file(&state_enc, &work.join("vmstate"), &keys)?;
            for p in [work.clone(), work.join("memory"), work.join("vmstate")] {
                jail.give(&p)?;
            }

            // Start a fresh jailed VMM on the same jail.
            jail.reset_for_restart()?;
            jail.prepare(&assets.kernel)?;
            let pid = jail.spawn(&assets.jailer, &assets.firecracker, machine.memory_mb, machine.cpus)?;
            let restore = (|| -> Result<()> {
                let api = Self::api(machine_id).with_timeout(SNAPSHOT_API_TIMEOUT);
                api.wait_for_ready(5000)?;
                api_ok(
                    api.put("/snapshot/load", &serde_json::json!({
                        "snapshot_path": format!("{work_inner}/vmstate"),
                        "mem_backend": { "backend_type": "File", "backend_path": format!("{work_inner}/memory") },
                        "enable_diff_snapshots": false,
                        "resume_vm": true,
                    })),
                    "load snapshot",
                )?;
                let vsock = Self::vsock_path(machine_id);
                fc_vsock::wait_for_agent(&vsock, 15_000)?;
                let session = fc_vsock::connect_wire(&vsock)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos() as u64;
                wire::client::guest_control(&session, &wire::OpenRequest::SetClock { unix_nanos: now })?;
                wire::client::guest_control(&session, &wire::OpenRequest::Thaw)?;
                Ok(())
            })();
            if let Err(e) = restore {
                let _ = kill_and_wait(pid);
                return Err(e);
            }
            Ok(pid)
        })();
        // The memory file is mapped privately by Firecracker, so the
        // plaintext can be unlinked as soon as the load has happened.
        let _ = std::fs::remove_dir_all(&work);

        let pid = result.map_err(|e| {
            e.context(format!("Resume of {machine_id} failed; it is still paused and its snapshot is kept"))
        })?;
        let _ = std::fs::remove_file(&mem_enc);
        let _ = std::fs::remove_file(&state_enc);
        state.update_machine_status(machine_id, "running", Some(pid))?;

        Ok(PauseResult {
            machine_id: machine_id.to_string(),
            status: "running".to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            snapshot_bytes: None,
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
