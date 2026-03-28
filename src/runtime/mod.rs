pub mod firecracker;
pub mod nspawn;

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::state::StateStore;

/// Generate a random machine ID in the format `sb-{8 hex chars}`
pub fn generate_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let hex: u32 = rng.gen();
    format!("sb-{:08x}", hex)
}

/// Generate a random human-readable machine name in the format `{adjective}-{animal}`
pub fn generate_name() -> String {
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

/// Poll a condition with exponential backoff.
///
/// Starts at `initial_delay`, doubles each iteration up to `max_delay`,
/// and gives up after `timeout` total elapsed time.
/// Returns Ok(()) when `check` returns true, or an error on timeout.
pub fn poll_with_backoff<F>(
    mut check: F,
    initial_delay: Duration,
    max_delay: Duration,
    timeout: Duration,
    timeout_msg: &str,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    let mut delay = initial_delay;

    loop {
        if check() {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            anyhow::bail!("{}", timeout_msg);
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        std::thread::sleep(delay.min(remaining));
        delay = (delay * 2).min(max_delay);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_generate_id_format() {
        let id = generate_id();
        assert!(id.starts_with("sb-"), "ID should start with 'sb-': {id}");
        assert_eq!(id.len(), 11, "ID should be 11 chars (sb- + 8 hex): {id}");
        // Verify the hex portion is valid
        let hex_part = &id[3..];
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "ID hex portion should be valid hex: {hex_part}"
        );
    }

    #[test]
    fn test_generate_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| generate_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        // With 32-bit random hex, 100 IDs should all be unique
        assert_eq!(ids.len(), unique.len(), "Generated IDs should be unique");
    }

    #[test]
    fn test_generate_name_format() {
        let name = generate_name();
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 2, "Name should be adjective-animal: {name}");
        assert!(!parts[0].is_empty(), "Adjective should not be empty");
        assert!(!parts[1].is_empty(), "Animal should not be empty");
    }

    #[test]
    fn test_poll_with_backoff_immediate_success() {
        let result = poll_with_backoff(
            || true,
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_secs(1),
            "should not timeout",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_poll_with_backoff_eventual_success() {
        let counter = AtomicU32::new(0);
        let result = poll_with_backoff(
            || {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                count >= 3 // Succeed on 4th call
            },
            Duration::from_millis(10),
            Duration::from_millis(50),
            Duration::from_secs(5),
            "should not timeout",
        );
        assert!(result.is_ok());
        assert!(counter.load(Ordering::SeqCst) >= 4);
    }

    #[test]
    fn test_poll_with_backoff_timeout() {
        let start = Instant::now();
        let result = poll_with_backoff(
            || false,
            Duration::from_millis(10),
            Duration::from_millis(50),
            Duration::from_millis(200),
            "timed out as expected",
        );
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("timed out as expected"));
        // Should have taken roughly the timeout duration, not much more
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_poll_with_backoff_respects_max_delay() {
        let call_times: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let times = call_times.clone();
        let counter = AtomicU32::new(0);

        let _ = poll_with_backoff(
            || {
                times.lock().unwrap().push(Instant::now());
                let count = counter.fetch_add(1, Ordering::SeqCst);
                count >= 5
            },
            Duration::from_millis(10),
            Duration::from_millis(30), // Max delay cap
            Duration::from_secs(5),
            "should not timeout",
        );

        let times = call_times.lock().unwrap();
        // With max_delay=30ms, gaps between calls should never exceed ~50ms
        // (accounting for scheduling jitter)
        for i in 1..times.len() {
            let gap = times[i].duration_since(times[i - 1]);
            assert!(
                gap < Duration::from_millis(100),
                "Gap between polls should be bounded by max_delay: {:?}",
                gap
            );
        }
    }

    use std::sync::{Arc, Mutex};
}
