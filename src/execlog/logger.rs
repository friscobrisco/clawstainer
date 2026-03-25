use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;

use crate::runtime::ExecResult;

use super::ExecLogEntry;

fn default_log_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".clawstainer")
        .join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn append(machine_id: &str, command: &str, result: &ExecResult) -> Result<()> {
    let dir = default_log_dir()?;
    append_to_dir(&dir, machine_id, command, result)
}

pub fn append_to_dir(dir: &PathBuf, machine_id: &str, command: &str, result: &ExecResult) -> Result<()> {
    let path = dir.join(format!("{machine_id}.jsonl"));

    let entry = ExecLogEntry {
        timestamp: chrono::Utc::now(),
        command: command.to_string(),
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        timed_out: result.timed_out,
    };

    let line = serde_json::to_string(&entry)
        .context("Failed to serialize exec log entry")?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("Failed to open exec log file")?;

    writeln!(file, "{line}").context("Failed to write exec log entry")?;

    Ok(())
}
