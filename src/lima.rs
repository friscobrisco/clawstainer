//! Lima VM proxy layer for macOS.
//!
//! When running on macOS, clawstainer transparently proxies all commands
//! into a Lima Linux VM. The user just types `clawstainer create ...`
//! and this module handles ensuring the VM exists, is running, and
//! re-executes the clawstainer binary inside it.

use anyhow::{Context, Result};
use std::process::Command;

const VM_NAME: &str = "clawstainer";
const LIMA_CONFIG: &str = include_str!("../lima-clawstainer.yaml");
const PROJECT_DIR_PLACEHOLDER: &str = "__PROJECT_DIR__";

/// Check if we're on macOS and need to proxy through Lima
pub fn needs_proxy() -> bool {
    cfg!(target_os = "macos")
}

/// Proxy the current CLI invocation into the Lima VM.
/// This re-executes the same command inside Linux and exits with its exit code.
pub fn proxy_to_vm() -> Result<()> {
    ensure_vm_running()?;

    // The project dir is mounted in the VM. The Linux binary is built into
    // target-linux/ to avoid conflicting with the macOS build in target/.
    let project = project_dir()?;
    let linux_binary = "/tmp/clawstainer-target/release/clawstainer".to_string();

    // Build the Linux binary if it doesn't exist or is older than src/
    ensure_linux_binary(&project, &linux_binary)?;

    // Reconstruct the full argument list (skip argv[0])
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Build the command to run inside the VM
    let mut inner_cmd = format!("sudo {}", shell_escape(&linux_binary));
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

/// Ensure the Lima VM exists and is running
fn ensure_vm_running() -> Result<()> {
    // Check if limactl is available
    let which = Command::new("which")
        .arg("limactl")
        .output();

    if which.is_err() || !which.unwrap().status.success() {
        anyhow::bail!(
            "Lima is not installed. Install it with: brew install lima\n\
             clawstainer uses a lightweight Linux VM to run sandboxes on macOS."
        );
    }

    // Check if VM exists and its status
    let output = Command::new("limactl")
        .args(["list", "--json"])
        .output()
        .context("Failed to list Lima VMs")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    let vm_exists = stdout.contains(&format!("\"name\":\"{VM_NAME}\""));
    let vm_running = stdout.contains("\"status\":\"Running\"");

    if !vm_exists {
        eprintln!("Setting up clawstainer VM (first-time setup, this takes ~2 minutes)...");
        create_vm()?;
        install_binary_in_vm()?;
        return Ok(());
    }

    if !vm_running {
        eprintln!("Starting clawstainer VM...");
        let status = Command::new("limactl")
            .args(["start", VM_NAME])
            .status()
            .context("Failed to start Lima VM")?;
        if !status.success() {
            anyhow::bail!("Failed to start Lima VM");
        }
    }

    Ok(())
}

/// Create the Lima VM from the embedded config
fn create_vm() -> Result<()> {
    // Write config to temp file
    let config_path = std::env::temp_dir().join("clawstainer-lima.yaml");
    let project_dir = project_dir()?;
    let config = LIMA_CONFIG.replace(PROJECT_DIR_PLACEHOLDER, &project_dir);
    std::fs::write(&config_path, config)
        .context("Failed to write Lima config")?;

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

/// Build and install the Linux binary inside the VM
fn install_binary_in_vm() -> Result<()> {
    eprintln!("Building clawstainer for Linux...");

    // Install Rust in VM if needed
    let _ = Command::new("limactl")
        .args([
            "shell", VM_NAME, "--",
            "bash", "-c",
            "which rustc || (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y) 1>&2",
        ])
        .status();

    // Build inside the VM (project dir is mounted)
    let project_dir = project_dir()?;
    let build_cmd = format!(
        "source \"$HOME/.cargo/env\" && cd '{}' && cargo build --release 1>&2",
        project_dir
    );

    let status = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "bash", "-c", &build_cmd])
        .status()
        .context("Failed to build clawstainer in VM")?;

    if !status.success() {
        anyhow::bail!("Failed to build clawstainer in VM");
    }

    // Symlink the binary into /usr/local/bin inside the VM
    let binary_path = format!("{}/target/release/clawstainer", project_dir);
    let link_cmd = format!("sudo ln -sf '{}' /usr/local/bin/clawstainer", binary_path);

    let _ = Command::new("limactl")
        .args(["shell", VM_NAME, "--", "bash", "-c", &link_cmd])
        .status();

    eprintln!("Setup complete.");
    Ok(())
}

/// Build the Linux binary inside the VM if needed
fn ensure_linux_binary(project: &str, linux_binary: &str) -> Result<()> {
    // Quick check: compare a build timestamp file against source modification times.
    // This avoids expensive `find -newer` traversals on every invocation.
    let ts_file = format!("{}/target/.clawstainer-build-ts", project);
    let check_cmd = format!(
        "test -f '{linux_binary}' && test -f '{ts_file}' && \
         [ \"$(stat -c %Y '{ts_file}' 2>/dev/null || stat -f %m '{ts_file}')\" -ge \
           \"$(stat -c %Y '{project}/src/main.rs' 2>/dev/null || stat -f %m '{project}/src/main.rs')\" ]"
    );
    let check = Command::new("limactl")
        .args([
            "shell", VM_NAME, "--",
            "bash", "-c", &check_cmd,
        ])
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
    if s.contains(' ') || s.contains('\'') || s.contains('"') || s.contains('$')
        || s.contains('!') || s.contains('(') || s.contains(')')
        || s.contains('&') || s.contains(';') || s.contains('|')
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}
