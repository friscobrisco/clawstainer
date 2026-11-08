//! Running Firecracker under its jailer.
//!
//! Each VM's VMM runs in its own chroot as its own unprivileged uid/gid with
//! no capabilities, inside a cgroup v2 group with memory, CPU and process
//! limits. The jail root is also the machine directory: the VM's disk,
//! kernel link, log, API socket and vsock socket all live in it, and every
//! path handed to the Firecracker API is relative to it.
//!
//! ```text
//! /var/lib/clawstainer/jail/firecracker/<id>/root/     (owned by the VM uid)
//!   firecracker       copied in by the jailer
//!   vmlinux           hard link to the shared kernel (root-owned, 0644)
//!   rootfs.ext4       the VM's disk (VM uid, 0600)
//!   run/firecracker.socket, v.sock, firecracker.pid, firecracker.log
//! ```
//!
//! Encrypted snapshots are kept outside the jail (see `SNAPSHOT_DIR`) so a
//! compromised VMM can neither read nor delete them.

use std::os::unix::fs::{chown, DirBuilderExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const JAIL_BASE: &str = "/var/lib/clawstainer/jail";
/// Root-only directory for encrypted snapshots, one subdirectory per VM.
pub const SNAPSHOT_DIR: &str = "/var/lib/clawstainer/snapshots";
/// cgroup v2 parent for every VM's group.
pub const PARENT_CGROUP: &str = "clawstainer";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// VM uids/gids are `UID_BASE + slot`.
pub const UID_BASE: u32 = 400_000;
/// Memory the VMM may use beyond the guest's RAM (device emulation,
/// page tables, snapshot I/O).
const VMM_OVERHEAD_MIB: u64 = 256;
/// Threads the VMM may run (API, VMM, one per vCPU, plus headroom).
const PIDS_MAX: u32 = 64;
/// Inside-the-jail paths, as Firecracker sees them.
pub const API_SOCK: &str = "/run/firecracker.socket";
pub const VSOCK: &str = "/v.sock";
pub const KERNEL: &str = "/vmlinux";
pub const ROOTFS: &str = "/rootfs.ext4";
pub const LOG: &str = "/firecracker.log";
pub const PID_FILE: &str = "/firecracker.pid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jail {
    pub id: String,
    pub uid: u32,
    pub gid: u32,
}

impl Jail {
    pub fn new(id: &str, slot: u32) -> Jail {
        Jail { id: id.to_string(), uid: UID_BASE + slot, gid: UID_BASE + slot }
    }

    /// The jail root on the host. Depends only on the id, so paths can be
    /// found without knowing the slot.
    pub fn root_for(id: &str) -> PathBuf {
        PathBuf::from(JAIL_BASE).join("firecracker").join(id).join("root")
    }

    pub fn root(&self) -> PathBuf {
        Self::root_for(&self.id)
    }

    /// Host path of a path inside the jail.
    pub fn host_path_for(id: &str, inner: &str) -> PathBuf {
        Self::root_for(id).join(inner.trim_start_matches('/'))
    }

    pub fn host_path(&self, inner: &str) -> PathBuf {
        Self::host_path_for(&self.id, inner)
    }

    pub fn snapshot_dir_for(id: &str) -> PathBuf {
        PathBuf::from(SNAPSHOT_DIR).join(id)
    }

    fn cgroup_dir(&self) -> PathBuf {
        PathBuf::from(CGROUP_ROOT).join(PARENT_CGROUP).join(&self.id)
    }

    /// The jailer command line (pure, so it can be unit-tested).
    pub fn jailer_args(&self, firecracker: &Path, memory_mib: u32, vcpus: u32) -> Vec<String> {
        let mem_bytes = (memory_mib as u64 + VMM_OVERHEAD_MIB) * 1024 * 1024;
        let cpu_quota = vcpus.max(1) as u64 * 100_000;
        vec![
            "--id".into(),
            self.id.clone(),
            "--exec-file".into(),
            firecracker.display().to_string(),
            "--uid".into(),
            self.uid.to_string(),
            "--gid".into(),
            self.gid.to_string(),
            "--chroot-base-dir".into(),
            JAIL_BASE.into(),
            "--cgroup-version".into(),
            "2".into(),
            "--parent-cgroup".into(),
            PARENT_CGROUP.into(),
            "--cgroup".into(),
            format!("memory.max={mem_bytes}"),
            "--cgroup".into(),
            "memory.swap.max=0".into(),
            "--cgroup".into(),
            format!("cpu.max={cpu_quota} 100000"),
            "--cgroup".into(),
            format!("pids.max={PIDS_MAX}"),
            "--resource-limit".into(),
            "no-file=4096".into(),
            "--".into(),
            "--api-sock".into(),
            API_SOCK.into(),
        ]
    }

    /// Create the jail root and link the kernel into it. The jailer adds
    /// its own files (binary, device nodes, run/) when it starts.
    pub fn prepare(&self, kernel: &Path) -> Result<()> {
        fs_mkdir(&PathBuf::from(JAIL_BASE), 0o711)?;
        let root = self.root();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("cannot create jail {}", root.display()))?;
        let link = self.host_path(KERNEL);
        let _ = std::fs::remove_file(&link);
        // A hard link shares the kernel without letting the VM modify it
        // (it stays root-owned and read-only to the VM uid). Fall back to a
        // copy if the kernel lives on another filesystem.
        if std::fs::hard_link(kernel, &link).is_err() {
            std::fs::copy(kernel, &link).context("cannot place the kernel in the jail")?;
        }
        ensure_parent_cgroup()?;
        Ok(())
    }

    /// Give the VM uid ownership of a file or directory in the jail.
    pub fn give(&self, path: &Path) -> Result<()> {
        chown(path, Some(self.uid), Some(self.gid))
            .with_context(|| format!("cannot chown {}", path.display()))
    }

    /// Before starting the jailer again on an existing jail (resume):
    /// remove what the previous run created, keep the VM's disk and log.
    pub fn reset_for_restart(&self) -> Result<()> {
        let root = self.root();
        for dir in ["dev", "run", "sys"] {
            let p = root.join(dir);
            if p.exists() {
                std::fs::remove_dir_all(&p).with_context(|| format!("cannot reset {}", p.display()))?;
            }
        }
        for file in ["firecracker", PID_FILE.trim_start_matches('/'), VSOCK.trim_start_matches('/')] {
            let _ = std::fs::remove_file(root.join(file));
        }
        let _ = std::fs::remove_dir(self.cgroup_dir());
        Ok(())
    }

    /// Start the jailer; Firecracker's output goes to the jail log.
    pub fn spawn(&self, jailer: &Path, firecracker: &Path, memory_mib: u32, vcpus: u32) -> Result<u32> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.host_path(LOG))?;
        let child = Command::new(jailer)
            .args(self.jailer_args(firecracker, memory_mib, vcpus))
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("Failed to start the Firecracker jailer")?;
        // The jailer execs Firecracker in place, so this pid is the VMM's.
        Ok(child.id())
    }

    /// Remove the jail directory and the VM's cgroup. Call after the VMM is
    /// gone.
    pub fn remove(id: &str) {
        let jail_dir = PathBuf::from(JAIL_BASE).join("firecracker").join(id);
        let _ = std::fs::remove_dir_all(jail_dir);
        let _ = std::fs::remove_dir(PathBuf::from(CGROUP_ROOT).join(PARENT_CGROUP).join(id));
    }
}

fn fs_mkdir(path: &Path, mode: u32) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .with_context(|| format!("cannot create {}", path.display()))
}

/// Create `/sys/fs/cgroup/clawstainer` with the cpu, memory and pids
/// controllers enabled for its children.
fn ensure_parent_cgroup() -> Result<()> {
    let root = PathBuf::from(CGROUP_ROOT);
    if !root.join("cgroup.controllers").exists() {
        bail!("cgroup v2 is not mounted at {CGROUP_ROOT}");
    }
    let parent = root.join(PARENT_CGROUP);
    if !parent.exists() {
        std::fs::create_dir(&parent)
            .with_context(|| format!("cannot create cgroup {}", parent.display()))?;
    }
    let enabled = std::fs::read_to_string(parent.join("cgroup.subtree_control")).unwrap_or_default();
    for ctrl in ["cpu", "memory", "pids"] {
        if !enabled.split_whitespace().any(|c| c == ctrl) {
            std::fs::write(parent.join("cgroup.subtree_control"), format!("+{ctrl}"))
                .with_context(|| format!("cannot enable the {ctrl} controller for {PARENT_CGROUP}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_and_paths_derive_from_slot_and_id() {
        let j = Jail::new("sb-12345678", 7);
        assert_eq!((j.uid, j.gid), (400_007, 400_007));
        assert_eq!(j.root(), PathBuf::from("/var/lib/clawstainer/jail/firecracker/sb-12345678/root"));
        assert_eq!(
            j.host_path(API_SOCK),
            PathBuf::from("/var/lib/clawstainer/jail/firecracker/sb-12345678/root/run/firecracker.socket")
        );
        assert_eq!(Jail::snapshot_dir_for("sb-1"), PathBuf::from("/var/lib/clawstainer/snapshots/sb-1"));
    }

    #[test]
    fn jailer_args_set_identity_limits_and_api_socket() {
        let j = Jail::new("sb-1", 3);
        let args = j.jailer_args(Path::new("/opt/fc/firecracker"), 512, 2);
        let joined = args.join(" ");
        assert!(joined.contains("--id sb-1 --exec-file /opt/fc/firecracker --uid 400003 --gid 400003"));
        assert!(joined.contains("--chroot-base-dir /var/lib/clawstainer/jail"));
        assert!(joined.contains("--cgroup-version 2 --parent-cgroup clawstainer"));
        assert!(args.contains(&format!("memory.max={}", (512u64 + 256) * 1024 * 1024)));
        assert!(args.contains(&"memory.swap.max=0".to_string()));
        assert!(args.contains(&"cpu.max=200000 100000".to_string()));
        assert!(args.contains(&"pids.max=64".to_string()));
        assert!(joined.ends_with("-- --api-sock /run/firecracker.socket"));
    }
}
