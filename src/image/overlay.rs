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

    // Unmount overlay — try normal unmount first, then lazy unmount as fallback
    if merged.exists() {
        let result = Command::new("umount").arg(merged.to_str().unwrap()).output();
        if result.is_err() || !result.unwrap().status.success() {
            // Lazy unmount as fallback (detaches immediately, cleans up when no longer in use)
            let _ = Command::new("umount")
                .args(["-l", merged.to_str().unwrap()])
                .output();
            // Brief wait for lazy unmount to release
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // Remove machine directory
    if machine_dir.exists() {
        // Best-effort: clear heavy cache dirs first to free space even if full removal fails
        let cache_dirs = ["upper/var/cache/apt", "upper/tmp", "upper/root/.cache"];
        for dir in &cache_dirs {
            let path = machine_dir.join(dir);
            if path.exists() {
                let _ = std::fs::remove_dir_all(&path);
            }
        }

        std::fs::remove_dir_all(&machine_dir)
            .context("Failed to remove machine directory")?;
    }

    Ok(())
}
