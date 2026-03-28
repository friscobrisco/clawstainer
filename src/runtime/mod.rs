pub mod firecracker;
pub mod nspawn;

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;

use crate::state::StateStore;

pub trait Runtime {
    fn create(&self, opts: CreateOpts, state: &StateStore) -> Result<MachineInfo>;
    fn exec(&self, machine_id: &str, opts: ExecOpts) -> Result<ExecResult>;
    fn shell(&self, machine_id: &str, user: &str) -> Result<()>;
    fn destroy(&self, machine_id: &str, state: &StateStore) -> Result<DestroyResult>;
    fn status(&self, machine_id: &str) -> Result<MachineStatus>;
}

pub struct CreateOpts {
    pub name: Option<String>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub network: String,
    pub timeout: u64,
    pub runtime: String,
    pub security: String,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub env_file: Option<String>,
    pub from_snapshot: Option<String>,
}

pub struct ExecOpts {
    pub command: String,
    pub timeout: u64,
    pub workdir: String,
    pub env: HashMap<String, String>,
    pub user: String,
}

#[derive(Debug, Serialize)]
pub struct MachineInfo {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ExecResult {
    pub machine_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_time_us: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct DestroyResult {
    pub machine_id: String,
    pub status: String,
    pub uptime_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct MachineStatus {
    pub id: String,
    pub status: String,
    pub pid: Option<u32>,
}

/// Check that we're running on Linux
pub fn require_linux() -> Result<()> {
    if cfg!(not(target_os = "linux")) {
        return Err(crate::error::ClawError::RuntimeUnavailable(
            std::env::consts::OS.to_string(),
        ).into());
    }
    Ok(())
}
