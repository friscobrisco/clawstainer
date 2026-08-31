//! Lima VM proxy layer for macOS.
//!
//! When running on macOS, clawstainer transparently proxies all commands
//! into a Lima Linux VM. The user just types `clawstainer create ...`
//! and this module handles ensuring the VM exists, is running, and
//! re-executes the clawstainer binary inside it.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

const VM_NAME: &str = "clawstainer";
const LIMA_CONFIG: &str = include_str!("../lima-clawstainer.yaml");
const PROJECT_DIR_PLACEHOLDER: &str = "__PROJECT_DIR__";
const LINUX_BINARY: &str = "/tmp/clawstainer-target/release/clawstainer";

#[derive(Clone, Debug, Serialize)]
pub struct VmStatus {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
}

impl VmStatus {
    fn not_created() -> Self {
        Self {
            name: VM_NAME.to_string(),
            status: "NotCreated".to_string(),
            vm_type: None,
            arch: None,
            cpus: None,
            memory_bytes: None,
            disk_bytes: None,
            directory: None,
        }
    }

    pub fn exists(&self) -> bool {
        self.status != "NotCreated"
    }

    pub fn is_running(&self) -> bool {
        self.status.eq_ignore_ascii_case("running")
    }
}

#[derive(Debug, Serialize)]
pub struct RepairResult {
    pub action: &'static str,
    pub path: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SandboxSummary {
    pub id: String,
    pub name: String,
    pub status: String,
}

/// Check if we're on macOS and need to proxy through Lima
pub fn needs_proxy() -> bool {
    cfg!(target_os = "macos")
}

/// Proxy the current CLI invocation into the Lima VM.
/// This re-executes the same command inside Linux and exits with its exit code.
pub fn proxy_to_vm() -> Result<()> {
    start_vm()?;

    // The project dir is mounted in the VM. The Linux binary is built under
    // /tmp so Linux artifacts never conflict with the macOS target directory.
    let project = project_dir()?;
    // Build the Linux binary if it doesn't exist or is older than src/
    ensure_linux_binary(&project, LINUX_BINARY)?;

    // Reconstruct the full argument list (skip argv[0])
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Build the command to run inside the VM
    let mut inner_cmd = format!("sudo {}", shell_escape(LINUX_BINARY));
    for arg in &args {
        inner_cmd.push(' ');
        inner_cmd.push_str(&shell_escape(arg));
    }

    let status = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "bash", "-c", &inner_cmd])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("Failed to execute command in Lima VM. Is Lima installed? (brew install lima)")?;

    std::process::exit(status.code().unwrap_or(1));
}

fn ensure_limactl() -> Result<()> {
    // Check if limactl is available
    let which = Command::new("which").arg("limactl").output();

    if which.is_err() || !which.unwrap().status.success() {
        anyhow::bail!(
            "Lima is not installed. Install it with: brew install lima\n\
             clawstainer uses a lightweight Linux VM to run sandboxes on macOS."
        );
    }

    Ok(())
}

/// Return the current VM status without starting or creating it.
pub fn vm_status() -> Result<VmStatus> {
    ensure_limactl()?;

    let output = Command::new("limactl")
        .args(["list", "--json"])
        .output()
        .context("Failed to list Lima VMs")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to list Lima VMs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(parse_vm_status(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_vm_status(stdout: &str) -> VmStatus {
    for line in stdout.lines() {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("name").and_then(|v| v.as_str()) != Some(VM_NAME) {
            continue;
        }
        return VmStatus {
            name: VM_NAME.to_string(),
            status: value
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string(),
            vm_type: value
                .get("vmType")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            arch: value
                .get("arch")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            cpus: value.get("cpus").and_then(|v| v.as_u64()),
            memory_bytes: value.get("memory").and_then(|v| v.as_u64()),
            disk_bytes: value.get("disk").and_then(|v| v.as_u64()),
            directory: value
                .get("dir")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        };
    }

    VmStatus::not_created()
}

/// Start the existing VM or create it on first use.
pub fn start_vm() -> Result<VmStatus> {
    ensure_limactl()?;
    let _ = repair_stale_pid()?;
    let status = vm_status()?;

    if !status.exists() {
        eprintln!("Setting up clawstainer VM (first-time setup, this takes ~2 minutes)...");
        create_vm()?;
        install_binary_in_vm()?;
        return vm_status();
    }

    if !status.is_running() {
        eprintln!("Starting clawstainer VM...");
        let status = Command::new("limactl")
            .args(["start", VM_NAME])
            .status()
            .context("Failed to start Lima VM")?;
        if !status.success() {
            anyhow::bail!("Failed to start Lima VM");
        }
    }

    vm_status()
}

/// Stop the VM without deleting its disk or sandbox state.
pub fn stop_vm(force: bool) -> Result<VmStatus> {
    let status = vm_status()?;
    if !status.exists() || !status.is_running() {
        return Ok(status);
    }

    let mut cmd = Command::new("limactl");
    cmd.args(["stop", VM_NAME]);
    if force {
        cmd.arg("--force");
    }
    let stopped = cmd.status().context("Failed to stop Lima VM")?;
    if !stopped.success() {
        anyhow::bail!("Failed to stop Lima VM");
    }
    vm_status()
}

/// Remove a stale host-agent PID file, but never touch one owned by a live process.
pub fn repair_stale_pid() -> Result<RepairResult> {
    let path = lima_instance_dir().join("ha.pid");
    if !path.exists() {
        return Ok(RepairResult {
            action: "repair",
            path: path.to_string_lossy().to_string(),
            status: "not_needed",
            pid: None,
        });
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let pid = raw.trim().parse::<i32>().ok();
    if vm_status()?.is_running() {
        return Ok(RepairResult {
            action: "repair",
            path: path.to_string_lossy().to_string(),
            status: "vm_running",
            pid,
        });
    }
    if let Some(pid) = pid {
        if process_is_alive(pid)? {
            return Ok(RepairResult {
                action: "repair",
                path: path.to_string_lossy().to_string(),
                status: "live_process",
                pid: Some(pid),
            });
        }
    }

    std::fs::remove_file(&path)
        .with_context(|| format!("Failed to remove stale {}", path.display()))?;
    Ok(RepairResult {
        action: "repair",
        path: path.to_string_lossy().to_string(),
        status: "removed_stale_pid",
        pid,
    })
}

/// Rebuild the Linux CLI inside the existing VM without deleting VM data.
pub fn rebuild_vm() -> Result<VmStatus> {
    start_vm()?;
    let removed = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "rm", "-f", LINUX_BINARY])
        .status()
        .context("Failed to remove the cached Linux clawstainer binary")?;
    if !removed.success() {
        anyhow::bail!("Failed to remove the cached Linux clawstainer binary");
    }

    let project = project_dir()?;
    ensure_linux_binary(&project, LINUX_BINARY)?;
    vm_status()
}

/// Inspect sandboxes before a destructive VM recreation.
pub fn list_vm_sandboxes() -> Result<Vec<SandboxSummary>> {
    let status = vm_status()?;
    if !status.exists() {
        return Ok(Vec::new());
    }
    start_vm()?;
    let project = project_dir()?;
    ensure_linux_binary(&project, LINUX_BINARY)?;

    let output = Command::new("limactl")
        .args([
            "shell",
            VM_NAME,
            "--",
            "sudo",
            LINUX_BINARY,
            "list",
            "--status",
            "all",
            "--format",
            "json",
        ])
        .output()
        .context("Failed to inspect sandboxes inside the Lima VM")?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to inspect sandboxes inside the Lima VM: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let values: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("Lima returned invalid sandbox state")?;
    Ok(values
        .into_iter()
        .map(|value| SandboxSummary {
            id: value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            status: value
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        })
        .collect())
}

/// Delete and recreate the Lima VM. Callers must perform confirmation first.
pub fn recreate_vm() -> Result<VmStatus> {
    let status = vm_status()?;
    if status.exists() {
        let _ = repair_stale_pid()?;
        let deleted = Command::new("limactl")
            .args(["delete", "--force", "--yes", VM_NAME])
            .status()
            .context("Failed to delete Lima VM")?;
        if !deleted.success() {
            anyhow::bail!("Failed to delete Lima VM");
        }
    }

    create_vm()?;
    install_binary_in_vm()?;
    vm_status()
}

/// Create the Lima VM from the embedded config
fn create_vm() -> Result<()> {
    // Write config to temp file
    let config_path = std::env::temp_dir().join("clawstainer-lima.yaml");
    let project_dir = project_dir()?;
    let config = LIMA_CONFIG.replace(PROJECT_DIR_PLACEHOLDER, &project_dir);
    std::fs::write(&config_path, config).context("Failed to write Lima config")?;

    let status = Command::new("limactl")
        .args(["create", "--name", VM_NAME, config_path.to_str().unwrap()])
        .status()
        .context("Failed to create Lima VM")?;

    if !status.success() {
        anyhow::bail!("Failed to create Lima VM");
    }

    let status = Command::new("limactl")
        .args(["start", VM_NAME])
        .status()
        .context("Failed to start Lima VM")?;

    if !status.success() {
        anyhow::bail!("Failed to start Lima VM");
    }

    // Clean up temp file
    let _ = std::fs::remove_file(config_path);

    Ok(())
}

fn lima_instance_dir() -> PathBuf {
    let lima_home = std::env::var_os("LIMA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".lima")))
        .unwrap_or_else(|| PathBuf::from(".lima"));
    lima_home.join(VM_NAME)
}

fn process_is_alive(pid: i32) -> Result<bool> {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return Ok(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(std::io::Error::last_os_error()).context("Failed to inspect Lima host-agent PID"),
    }
}

/// Build and install the Linux binary inside the VM
fn install_binary_in_vm() -> Result<()> {
    eprintln!("Building clawstainer for Linux...");
    let project_dir = project_dir()?;
    ensure_linux_binary(&project_dir, LINUX_BINARY)?;

    // Symlink the binary into /usr/local/bin inside the VM
    let link_cmd = format!("sudo ln -sf '{}' /usr/local/bin/clawstainer", LINUX_BINARY);

    let _ = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "bash", "-c", &link_cmd])
        .status();

    eprintln!("Setup complete.");
    Ok(())
}

/// Build the Linux binary inside the VM if needed
fn ensure_linux_binary(project: &str, linux_binary: &str) -> Result<()> {
    // Rebuild when any source, manifest, embedded config, or component recipe
    // changed. Checking only src/main.rs misses most real code changes.
    let ts_file = format!("{}/target/.clawstainer-build-ts", project);
    let check_cmd = format!(
        "test -f '{linux_binary}' && test -f '{ts_file}' && \
         ! find '{project}/src' '{project}/Cargo.toml' '{project}/Cargo.lock' \
           '{project}/components.yaml' '{project}/lima-clawstainer.yaml' \
           -newer '{ts_file}' -print -quit 2>/dev/null | grep -q ."
    );
    let check = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "bash", "-c", &check_cmd])
        .status();

    if let Ok(s) = check {
        if s.success() {
            return Ok(());
        }
    }

    eprintln!("Building clawstainer for Linux (first run after changes)...");

    // Ensure Rust is available
    let _ = Command::new("limactl")
        .args([
            "shell", VM_NAME, "--",
            "bash", "-c",
            "which rustc || (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y) 1>&2",
        ])
        .status();

    let build_cmd = format!(
        "source \"$HOME/.cargo/env\" && cd '{}' && CARGO_TARGET_DIR=/tmp/clawstainer-target cargo build --release 1>&2",
        project
    );

    let status = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "bash", "-c", &build_cmd])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("Failed to build Linux binary in VM")?;

    if !status.success() {
        anyhow::bail!("Failed to build clawstainer for Linux");
    }

    // Write build timestamp so subsequent checks are fast
    let ts_file = format!("{}/target/.clawstainer-build-ts", project);
    let _ = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "touch", &ts_file])
        .status();

    Ok(())
}

fn project_dir() -> Result<String> {
    // CARGO_MANIFEST_DIR is set at compile time to the directory containing Cargo.toml
    const PROJECT_DIR: &str = env!("CARGO_MANIFEST_DIR");
    Ok(PROJECT_DIR.to_string())
}

fn shell_escape(s: &str) -> String {
    if s.contains(' ')
        || s.contains('\'')
        || s.contains('"')
        || s.contains('$')
        || s.contains('!')
        || s.contains('(')
        || s.contains(')')
        || s.contains('&')
        || s.contains(';')
        || s.contains('|')
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clawstainer_vm_from_json_lines() {
        let stdout = concat!(
            r#"{"name":"other","status":"Running"}"#,
            "\n",
            r#"{"name":"clawstainer","status":"Stopped","vmType":"vz","arch":"aarch64","cpus":4,"memory":17179869184,"disk":32212254720,"dir":"/tmp/clawstainer"}"#,
            "\n"
        );

        let status = parse_vm_status(stdout);
        assert!(status.exists());
        assert!(!status.is_running());
        assert_eq!(status.vm_type.as_deref(), Some("vz"));
        assert_eq!(status.cpus, Some(4));
        assert_eq!(status.memory_bytes, Some(17_179_869_184));
    }

    #[test]
    fn missing_vm_reports_not_created() {
        let status = parse_vm_status(r#"{"name":"other","status":"Running"}"#);
        assert!(!status.exists());
        assert_eq!(status.status, "NotCreated");
    }

    #[test]
    fn shell_escape_quotes_metacharacters() {
        assert_eq!(shell_escape("plain"), "plain");
        assert_eq!(shell_escape("two words"), "'two words'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }
}
