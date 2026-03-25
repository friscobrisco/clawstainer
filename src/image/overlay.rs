use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const MACHINES_DIR: &str = "/var/lib/clawstainer/machines";

/// Set up an overlay filesystem for a machine.
/// Returns the path to the merged rootfs.
pub fn setup(machine_id: &str, base_path: &PathBuf) -> Result<PathBuf> {
    let machine_dir = PathBuf::from(MACHINES_DIR).join(machine_id);
    let upper = machine_dir.join("upper");
    let work = machine_dir.join("work");
    let merged = machine_dir.join("rootfs");

    std::fs::create_dir_all(&upper).context("Failed to create overlay upper dir")?;
    std::fs::create_dir_all(&work).context("Failed to create overlay work dir")?;
    std::fs::create_dir_all(&merged).context("Failed to create overlay merged dir")?;

    let mount_opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        base_path.display(),
        upper.display(),
        work.display()
    );

    let status = Command::new("mount")
        .args([
            "-t",
            "overlay",
            "overlay",
            "-o",
            &mount_opts,
            merged.to_str().unwrap(),
        ])
        .status()
        .context("Failed to mount overlay filesystem")?;

    if !status.success() {
        anyhow::bail!("Failed to mount overlay for machine {machine_id}");
    }

    Ok(merged)
}

/// Tear down the overlay filesystem and remove machine directory
pub fn teardown(machine_id: &str) -> Result<()> {
    let machine_dir = PathBuf::from(MACHINES_DIR).join(machine_id);
    let merged = machine_dir.join("rootfs");

    // Unmount overlay
    if merged.exists() {
        let _ = Command::new("umount").arg(merged.to_str().unwrap()).output();
    }

    // Remove machine directory
    if machine_dir.exists() {
        std::fs::remove_dir_all(&machine_dir)
            .context("Failed to remove machine directory")?;
    }

    Ok(())
}
