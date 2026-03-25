use anyhow::{Context, Result};
use std::path::PathBuf;

use super::ExecLogEntry;

fn default_log_dir() -> Result<PathBuf> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".clawstainer")
        .join("logs");
    Ok(path)
}

pub fn read_last(machine_id: &str, n: usize) -> Result<Vec<ExecLogEntry>> {
    let dir = default_log_dir()?;
    read_last_from_dir(&dir, machine_id, n)
}

pub fn read_last_from_dir(dir: &PathBuf, machine_id: &str, n: usize) -> Result<Vec<ExecLogEntry>> {
    let path = dir.join(format!("{machine_id}.jsonl"));

    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&path)
        .context("Failed to read exec log file")?;

    let entries: Vec<ExecLogEntry> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Return last N entries
    let start = entries.len().saturating_sub(n);
    Ok(entries[start..].to_vec())
}
