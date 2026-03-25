use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const FC_DIR: &str = "/var/lib/clawstainer/firecracker";
const MACHINES_DIR: &str = "/var/lib/clawstainer/machines";
const ROOTFS_SIZE_MB: u64 = 2048;

const AGENT_SERVICE: &str = r#"[Unit]
Description=Clawstainer Guest Agent
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/claw-agent
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
"#;

/// Ensure the claw-agent binary is built for Linux and available
fn ensure_agent_binary() -> Result<PathBuf> {
    let agent_path = PathBuf::from(FC_DIR).join("bin").join("claw-agent");

    if agent_path.exists() {
        return Ok(agent_path);
    }

    std::fs::create_dir_all(agent_path.parent().unwrap())?;

    // The agent needs to be a Linux binary. Check if we have one at
    // target-linux/release/claw-agent (built inside Lima VM)
    let project_dir = env!("CARGO_MANIFEST_DIR");
    let linux_agent = PathBuf::from(project_dir)
        .join("target-linux")
        .join("release")
        .join("claw-agent");

    if linux_agent.exists() {
        std::fs::copy(&linux_agent, &agent_path)
            .context("Failed to copy claw-agent binary")?;
        return Ok(agent_path);
    }

    // Try the regular target dir (if we're already on Linux)
    let local_agent = PathBuf::from(project_dir)
        .join("target")
        .join("release")
        .join("claw-agent");

    if local_agent.exists() {
        std::fs::copy(&local_agent, &agent_path)
            .context("Failed to copy claw-agent binary")?;
        return Ok(agent_path);
    }

    anyhow::bail!(
        "claw-agent binary not found. Build it with:\n  \
         cargo build --release --bin claw-agent\n  \
         (or inside Lima: CARGO_TARGET_DIR=target-linux cargo build --release --bin claw-agent)"
    );
}

/// Ensure the base ext4 rootfs image exists (converted from directory-based base image)
/// with the claw-agent injected and enabled as a systemd service.
pub fn ensure_base_ext4() -> Result<PathBuf> {
    let base_ext4 = PathBuf::from(FC_DIR).join("base-rootfs.ext4");

    if base_ext4.exists() {
        return Ok(base_ext4);
    }

    // Ensure the directory-based base image exists first
    let base_dir = crate::image::bootstrap::ensure_base_image()?;

    // Ensure we have the agent binary
    let agent_bin = ensure_agent_binary()?;

    eprintln!("Converting base image to ext4 (one-time operation)...");

    // Create ext4 image
    let status = Command::new("dd")
        .args([
            "if=/dev/zero",
            &format!("of={}", base_ext4.display()),
            "bs=1M",
            &format!("count={ROOTFS_SIZE_MB}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to create rootfs image")?;

    if !status.success() {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to create rootfs image");
    }

    // Format as ext4
    let status = Command::new("mkfs.ext4")
        .args(["-F", base_ext4.to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to format rootfs as ext4")?;

    if !status.success() {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to format rootfs");
    }

    // Mount the image
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
    let copy_ok = Command::new("cp")
        .args(["-a", &format!("{}/.", base_dir.display()), mnt.to_str().unwrap()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if copy_ok {
        // Inject claw-agent binary
        let agent_dest = mnt.join("usr/local/bin/claw-agent");
        std::fs::create_dir_all(agent_dest.parent().unwrap()).ok();
        let _ = std::fs::copy(&agent_bin, &agent_dest);
        // Make executable
        let _ = Command::new("chmod")
            .args(["+x", agent_dest.to_str().unwrap()])
            .status();

        // Inject systemd service
        let service_dir = mnt.join("etc/systemd/system");
        std::fs::create_dir_all(&service_dir).ok();
        let _ = std::fs::write(
            service_dir.join("claw-agent.service"),
            AGENT_SERVICE,
        );

        // Enable the service (symlink into multi-user.target.wants)
        let wants_dir = mnt.join("etc/systemd/system/multi-user.target.wants");
        std::fs::create_dir_all(&wants_dir).ok();
        let _ = std::os::unix::fs::symlink(
            "/etc/systemd/system/claw-agent.service",
            wants_dir.join("claw-agent.service"),
        );

        // Write resolv.conf
        let resolv = mnt.join("etc/resolv.conf");
        if resolv.is_symlink() {
            let _ = std::fs::remove_file(&resolv);
        }
        let _ = std::fs::write(&resolv, "nameserver 8.8.8.8\nnameserver 8.8.4.4\n");
    }

    // Unmount
    let _ = Command::new("umount").arg(mnt.to_str().unwrap()).status();
    let _ = std::fs::remove_dir(&mnt);

    if !copy_ok {
        let _ = std::fs::remove_file(&base_ext4);
        anyhow::bail!("Failed to copy base image to ext4");
    }

    eprintln!("Base ext4 image ready (with claw-agent injected).");
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
