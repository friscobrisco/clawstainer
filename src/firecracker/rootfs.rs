use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const FC_DIR: &str = "/var/lib/clawstainer/firecracker";
const MACHINES_DIR: &str = "/var/lib/clawstainer/machines";
const ROOTFS_SIZE_MB: u64 = 2048;

/// Ensure the base ext4 rootfs image exists (converted from directory-based base image)
pub fn ensure_base_ext4() -> Result<PathBuf> {
    let base_ext4 = PathBuf::from(FC_DIR).join("base-rootfs.ext4");

    if base_ext4.exists() {
        return Ok(base_ext4);
    }

    // Ensure the directory-based base image exists first
    let base_dir = crate::image::bootstrap::ensure_base_image()?;

    eprintln!("Converting base image to ext4 (one-time operation)...");

    // Create sparse ext4 image
    let status = Command::new("dd")
        .args([
            "if=/dev/zero",
            &format!("of={}", base_ext4.display()),
            "bs=1M",
            &format!("count={ROOTFS_SIZE_MB}"),
        ])
        .status()
        .context("Failed to create rootfs image")?;

    if !status.success() {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to create rootfs image");
    }

    // Format as ext4
    let status = Command::new("mkfs.ext4")
        .args(["-F", base_ext4.to_str().unwrap()])
        .status()
        .context("Failed to format rootfs as ext4")?;

    if !status.success() {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to format rootfs");
    }

    // Mount and copy base image contents
    let mnt = PathBuf::from(FC_DIR).join("mnt");
    std::fs::create_dir_all(&mnt)?;

    let status = Command::new("mount")
        .args([base_ext4.to_str().unwrap(), mnt.to_str().unwrap()])
        .status()
        .context("Failed to mount rootfs image")?;

    if !status.success() {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to mount rootfs image");
    }

    // Copy base image contents
    let status = Command::new("cp")
        .args(["-a", &format!("{}/.", base_dir.display()), mnt.to_str().unwrap()])
        .status();

    // Unmount regardless of copy result
    let _ = Command::new("umount").arg(mnt.to_str().unwrap()).status();
    let _ = std::fs::remove_dir(&mnt);

    if !status.map(|s| s.success()).unwrap_or(false) {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to copy base image to ext4");
    }

    eprintln!("Base ext4 image ready.");
    Ok(base_ext4)
}

/// Create a per-VM rootfs by sparse-copying the base ext4 image
pub fn create_vm_rootfs(machine_id: &str) -> Result<PathBuf> {
    let base_ext4 = ensure_base_ext4()?;
    let machine_dir = PathBuf::from(MACHINES_DIR).join(machine_id);
    std::fs::create_dir_all(&machine_dir)?;

    let vm_rootfs = machine_dir.join("rootfs.ext4");

    let status = Command::new("cp")
        .args(["--sparse=always", base_ext4.to_str().unwrap(), vm_rootfs.to_str().unwrap()])
        .status()
        .context("Failed to copy rootfs for VM")?;

    if !status.success() {
        anyhow::bail!("Failed to create VM rootfs");
    }

    Ok(vm_rootfs)
}

/// Clean up a VM's rootfs
pub fn cleanup_vm_rootfs(machine_id: &str) -> Result<()> {
    let machine_dir = PathBuf::from(MACHINES_DIR).join(machine_id);
    if machine_dir.exists() {
        std::fs::remove_dir_all(&machine_dir)
            .context("Failed to remove VM directory")?;
    }
    Ok(())
}
