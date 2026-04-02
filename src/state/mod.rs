pub mod lock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::ClawError;
use crate::network::NetworkState;
use crate::runtime::Runtime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub status: String,
    pub pid: Option<u32>,
    pub ip: Option<String>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub network: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub components: Vec<String>,
    pub timeout: u64,
    pub root_path: String,
    #[serde(default = "default_runtime")]
    pub runtime: String,
    #[serde(default = "default_security")]
    pub security: String,
    #[serde(default)]
    pub has_env_file: bool,
    #[serde(default)]
    pub linger: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_name: Option<String>,
}

fn default_runtime() -> String {
    "nspawn".to_string()
}

fn default_security() -> String {
    "strict".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub machines: HashMap<String, Machine>,
    pub network: NetworkState,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            machines: HashMap::new(),
            network: NetworkState::default(),
        }
    }
}

pub struct StateStore {
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl StateStore {
    pub fn new() -> Result<Self> {
        let base = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
            .join(".clawstainer");
        Self::with_base_dir(base)
    }

    /// Create a StateStore with a custom base directory (useful for testing)
    pub fn with_base_dir(base: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Failed to create directory: {}", base.display()))?;

        Ok(Self {
            state_path: base.join("state.json"),
            lock_path: base.join("state.lock"),
        })
    }

    fn read_state(&self) -> Result<State> {
        if !self.state_path.exists() {
            return Ok(State::default());
        }
        let data =
            std::fs::read_to_string(&self.state_path).context("Failed to read state file")?;
        if data.trim().is_empty() {
            return Ok(State::default());
        }
        serde_json::from_str(&data).context("Failed to parse state file")
    }

    fn write_state(&self, state: &State) -> Result<()> {
        let data = serde_json::to_string_pretty(state).context("Failed to serialize state")?;

        // Atomic write: write to temp file, then rename
        let tmp_path = self.state_path.with_extension("tmp");
        std::fs::write(&tmp_path, &data).context("Failed to write temp state file")?;
        std::fs::rename(&tmp_path, &self.state_path).context("Failed to rename temp state file")?;

        Ok(())
    }

    /// Execute a function with an exclusive lock on the state file.
    /// The function receives a mutable reference to the state, and
    /// any changes are written back atomically after the function returns.
    pub fn with_lock<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut State) -> Result<T>,
    {
        let lock = lock::FileLock::new(&self.lock_path)?;
        lock.lock_exclusive()?;

        let mut state = self.read_state()?;
        let result = f(&mut state)?;
        self.write_state(&state)?;

        lock.unlock()?;
        Ok(result)
    }

    /// Execute a function with a shared (read) lock on the state file.
    pub fn with_read_lock<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&State) -> Result<T>,
    {
        let lock = lock::FileLock::new(&self.lock_path)?;
        lock.lock_shared()?;

        let state = self.read_state()?;
        let result = f(&state)?;

        lock.unlock()?;
        Ok(result)
    }

    pub fn get_machine(&self, machine_id: &str) -> Result<Machine> {
        self.with_read_lock(|s| {
            s.machines
                .get(machine_id)
                .cloned()
                .ok_or_else(|| ClawError::MachineNotFound(machine_id.to_string()).into())
        })
    }

    pub fn reconcile_machine(&self, machine_id: &str, runtime: &dyn Runtime) -> Result<Machine> {
        let machine = self.get_machine(machine_id)?;
        if machine.status != "running" {
            return Ok(machine);
        }

        let live = runtime.status(machine_id)?;
        if live.status == machine.status && live.pid == machine.pid {
            return Ok(machine);
        }

        self.update_machine_status(machine_id, &live.status, live.pid)?;

        let mut machine = machine;
        machine.status = live.status;
        machine.pid = live.pid;
        Ok(machine)
    }

    pub fn get_running_machine_live(
        &self,
        machine_id: &str,
        runtime: &dyn Runtime,
    ) -> Result<Machine> {
        let machine = self.reconcile_machine(machine_id, runtime)?;
        if machine.status != "running" {
            return Err(ClawError::MachineNotRunning(
                machine_id.to_string(),
                machine.status.clone(),
            )
            .into());
        }
        Ok(machine)
    }

    pub fn get_machine_ip_live(&self, machine_id: &str, runtime: &dyn Runtime) -> Result<String> {
        let machine = self.get_running_machine_live(machine_id, runtime)?;
        machine.ip.ok_or_else(|| {
            ClawError::ExecFailed("Machine has no IP (network=none)".to_string()).into()
        })
    }

    pub fn update_machine_status(
        &self,
        machine_id: &str,
        status: &str,
        pid: Option<u32>,
    ) -> Result<()> {
        self.with_lock(|s| {
            let machine = s
                .machines
                .get_mut(machine_id)
                .ok_or_else(|| ClawError::MachineNotFound(machine_id.to_string()))?;
            machine.status = status.to_string();
            machine.pid = pid;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        CreateOpts, DestroyResult, ExecOpts, ExecResult, MachineInfo, MachineStatus, Runtime,
    };

    struct MockRuntime {
        status: String,
        pid: Option<u32>,
    }

    impl Runtime for MockRuntime {
        fn create(&self, _opts: CreateOpts, _state: &StateStore) -> Result<MachineInfo> {
            unreachable!()
        }

        fn exec(&self, _machine_id: &str, _opts: ExecOpts) -> Result<ExecResult> {
            unreachable!()
        }

        fn shell(&self, _machine_id: &str, _user: &str) -> Result<()> {
            unreachable!()
        }

        fn destroy(&self, _machine_id: &str, _state: &StateStore) -> Result<DestroyResult> {
            unreachable!()
        }

        fn status(&self, machine_id: &str) -> Result<MachineStatus> {
            Ok(MachineStatus {
                id: machine_id.to_string(),
                status: self.status.clone(),
                pid: self.pid,
            })
        }
    }

    fn test_machine(id: &str) -> Machine {
        Machine {
            id: id.to_string(),
            name: "test-machine".to_string(),
            status: "running".to_string(),
            pid: Some(12345),
            ip: Some("10.0.0.2".to_string()),
            memory_mb: 512,
            cpus: 1,
            network: "nat".to_string(),
            created_at: chrono::Utc::now(),
            components: vec!["python3".to_string()],
            timeout: 0,
            root_path: "/tmp/test".to_string(),
            runtime: "nspawn".to_string(),
            security: "strict".to_string(),
            has_env_file: false,
            linger: false,
            fleet_name: None,
        }
    }

    #[test]
    fn test_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        let machines = store.with_read_lock(|s| Ok(s.machines.len())).unwrap();

        assert_eq!(machines, 0);
    }

    #[test]
    fn test_add_and_read_machine() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        // Add a machine
        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-00000001".to_string(), test_machine("sb-00000001"));
                Ok(())
            })
            .unwrap();

        // Read it back
        let machine = store
            .with_read_lock(|s| Ok(s.machines.get("sb-00000001").cloned()))
            .unwrap();

        assert!(machine.is_some());
        let m = machine.unwrap();
        assert_eq!(m.id, "sb-00000001");
        assert_eq!(m.name, "test-machine");
        assert_eq!(m.status, "running");
        assert_eq!(m.memory_mb, 512);
    }

    #[test]
    fn test_remove_machine() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-00000001".to_string(), test_machine("sb-00000001"));
                Ok(())
            })
            .unwrap();

        store
            .with_lock(|s| {
                s.machines.remove("sb-00000001");
                Ok(())
            })
            .unwrap();

        let count = store.with_read_lock(|s| Ok(s.machines.len())).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_multiple_machines() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-00000001".to_string(), test_machine("sb-00000001"));
                s.machines
                    .insert("sb-00000002".to_string(), test_machine("sb-00000002"));
                s.machines
                    .insert("sb-00000003".to_string(), test_machine("sb-00000003"));
                Ok(())
            })
            .unwrap();

        let count = store.with_read_lock(|s| Ok(s.machines.len())).unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_state_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();

        // Write with one instance
        {
            let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();
            store
                .with_lock(|s| {
                    s.machines
                        .insert("sb-00000001".to_string(), test_machine("sb-00000001"));
                    Ok(())
                })
                .unwrap();
        }

        // Read with a new instance
        {
            let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();
            let count = store.with_read_lock(|s| Ok(s.machines.len())).unwrap();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn test_state_json_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-test".to_string(), test_machine("sb-test"));
                s.network
                    .allocated_ips
                    .insert("10.0.0.2".to_string(), "sb-test".to_string());
                s.network.next_octet = 3;
                Ok(())
            })
            .unwrap();

        // Verify the JSON file is valid
        let data = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
        let state: State = serde_json::from_str(&data).unwrap();
        assert_eq!(state.version, 1);
        assert_eq!(state.machines.len(), 1);
        assert_eq!(state.network.next_octet, 3);
    }

    #[test]
    fn test_get_machine_returns_machine() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-test".to_string(), test_machine("sb-test"));
                Ok(())
            })
            .unwrap();

        let machine = store.get_machine("sb-test").unwrap();
        assert_eq!(machine.id, "sb-test");
    }

    #[test]
    fn test_get_machine_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        let err = store.get_machine("sb-missing").unwrap_err();
        assert!(err.to_string().contains("No machine with ID 'sb-missing'"));
    }

    #[test]
    fn test_reconcile_machine_updates_stale_running_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-test".to_string(), test_machine("sb-test"));
                Ok(())
            })
            .unwrap();

        let runtime = MockRuntime {
            status: "stopped".to_string(),
            pid: None,
        };

        let machine = store.reconcile_machine("sb-test", &runtime).unwrap();
        assert_eq!(machine.status, "stopped");
        assert_eq!(machine.pid, None);

        let persisted = store.get_machine("sb-test").unwrap();
        assert_eq!(persisted.status, "stopped");
        assert_eq!(persisted.pid, None);
    }

    #[test]
    fn test_get_running_machine_live_rejects_stale_running_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-test".to_string(), test_machine("sb-test"));
                Ok(())
            })
            .unwrap();

        let runtime = MockRuntime {
            status: "stopped".to_string(),
            pid: None,
        };

        let err = store
            .get_running_machine_live("sb-test", &runtime)
            .unwrap_err();
        assert!(err.to_string().contains("status: stopped"));
    }

    #[test]
    fn test_get_running_machine_live_returns_machine() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-test".to_string(), test_machine("sb-test"));
                Ok(())
            })
            .unwrap();

        let runtime = MockRuntime {
            status: "running".to_string(),
            pid: Some(12345),
        };

        let machine = store.get_running_machine_live("sb-test", &runtime).unwrap();
        assert_eq!(machine.status, "running");
    }

    #[test]
    fn test_get_machine_ip_live_returns_ip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                s.machines
                    .insert("sb-test".to_string(), test_machine("sb-test"));
                Ok(())
            })
            .unwrap();

        let runtime = MockRuntime {
            status: "running".to_string(),
            pid: Some(12345),
        };

        let ip = store.get_machine_ip_live("sb-test", &runtime).unwrap();
        assert_eq!(ip, "10.0.0.2");
    }

    #[test]
    fn test_get_machine_ip_live_rejects_machine_without_ip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_base_dir(dir.path().to_path_buf()).unwrap();

        store
            .with_lock(|s| {
                let mut machine = test_machine("sb-test");
                machine.ip = None;
                s.machines.insert("sb-test".to_string(), machine);
                Ok(())
            })
            .unwrap();

        let runtime = MockRuntime {
            status: "running".to_string(),
            pid: Some(12345),
        };

        let err = store.get_machine_ip_live("sb-test", &runtime).unwrap_err();
        assert!(err.to_string().contains("Machine has no IP"));
    }
}
