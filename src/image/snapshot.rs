use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

use crate::error::ClawError;

const SNAPSHOTS_DIR: &str = "/var/lib/clawstainer/snapshots";
const MACHINES_DIR: &str = "/var/lib/clawstainer/machines";

#[derive(Debug, Serialize)]
pub struct SnapshotInfo {
    pub name: String,
    pub size_bytes: u64,
    pub created_at: String,
}

pub fn create(machine_id: &str, name: &str) -> Result<SnapshotInfo> {
    let upper_dir = PathBuf::from(MACHINES_DIR).join(machine_id).join("upper");
    if !upper_dir.exists() {
        return Err(ClawError::SnapshotFailed(format!(
            "No overlay upper layer found for machine {machine_id}"
        ))
        .into());
    }

    let snapshots_dir = PathBuf::from(SNAPSHOTS_DIR);
    std::fs::create_dir_all(&snapshots_dir)
        .context("Failed to create snapshots directory")?;

    let tarball = snapshots_dir.join(format!("{name}.tar.gz"));
    if tarball.exists() {
        return Err(ClawError::SnapshotFailed(format!(
            "Snapshot '{name}' already exists"
        ))
        .into());
    }

    let tarball_str = tarball
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Snapshot path contains invalid UTF-8"))?;
    let upper_str = upper_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Upper dir path contains invalid UTF-8"))?;

    let status = Command::new("tar")
        .args(["-czf", tarball_str, "-C", upper_str, "."])
        .status()
        .context("Failed to run tar")?;

    if !status.success() {
        return Err(ClawError::SnapshotFailed("tar failed to create snapshot".to_string()).into());
    }

    let meta = std::fs::metadata(&tarball)?;
    let created_at = meta
        .modified()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    Ok(SnapshotInfo {
        name: name.to_string(),
        size_bytes: meta.len(),
        created_at,
    })
}

pub fn list() -> Result<Vec<SnapshotInfo>> {
    let snapshots_dir = PathBuf::from(SNAPSHOTS_DIR);
    if !snapshots_dir.exists() {
        return Ok(Vec::new());
    }

    let mut snapshots = Vec::new();
    for entry in std::fs::read_dir(&snapshots_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("gz") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .strip_suffix(".tar")
                .unwrap_or(
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(""),
                )
                .to_string();

            let meta = std::fs::metadata(&path)?;
            let created_at = meta
                .modified()
                .ok()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_default();

            snapshots.push(SnapshotInfo {
                name,
                size_bytes: meta.len(),
                created_at,
            });
        }
    }

    snapshots.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(snapshots)
}

pub fn delete(name: &str) -> Result<()> {
    let tarball = PathBuf::from(SNAPSHOTS_DIR).join(format!("{name}.tar.gz"));
    if !tarball.exists() {
        return Err(ClawError::SnapshotFailed(format!("Snapshot '{name}' not found")).into());
    }
    std::fs::remove_file(&tarball).context("Failed to delete snapshot")?;

    // Also clean up extracted dir if it exists
    let extracted = PathBuf::from(SNAPSHOTS_DIR).join(name);
    if extracted.exists() {
        let _ = std::fs::remove_dir_all(&extracted);
    }

    Ok(())
}

pub fn extract(name: &str) -> Result<PathBuf> {
    let tarball = PathBuf::from(SNAPSHOTS_DIR).join(format!("{name}.tar.gz"));
    if !tarball.exists() {
        return Err(ClawError::SnapshotFailed(format!("Snapshot '{name}' not found")).into());
    }

    let extracted = PathBuf::from(SNAPSHOTS_DIR).join(name);
    if extracted.exists() {
        // Already extracted
        return Ok(extracted);
    }

    std::fs::create_dir_all(&extracted).context("Failed to create snapshot extract dir")?;

    let tarball_str = tarball
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Snapshot tarball path contains invalid UTF-8"))?;
    let extracted_str = extracted
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Snapshot extract path contains invalid UTF-8"))?;

    let status = Command::new("tar")
        .args(["-xzf", tarball_str, "-C", extracted_str])
        .status()
        .context("Failed to extract snapshot")?;

    if !status.success() {
        let _ = std::fs::remove_dir_all(&extracted);
        return Err(ClawError::SnapshotFailed("Failed to extract snapshot".to_string()).into());
    }

    Ok(extracted)
}
