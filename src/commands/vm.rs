use anyhow::Result;
use serde::Serialize;
use std::io::{self, IsTerminal, Write};

use crate::cli::{VmArgs, VmCommands};
use crate::error::ClawError;
use crate::{lima, output};

#[derive(Serialize)]
struct VmActionResult<T: Serialize> {
    action: &'static str,
    result: T,
}

pub fn run(args: &VmArgs) -> Result<()> {
    if !lima::needs_proxy() {
        return Err(ClawError::VmError(
            "VM commands are only available on macOS; Linux runs clawstainer directly".to_string(),
        )
        .into());
    }

    match &args.command {
        VmCommands::Status => {
            let status = lima::vm_status().map_err(vm_error)?;
            print_vm_result("status", &status, &args.format);
        }
        VmCommands::Start => {
            let status = lima::start_vm().map_err(vm_error)?;
            print_vm_result("start", &status, &args.format);
        }
        VmCommands::Stop { force } => {
            let status = lima::stop_vm(*force).map_err(vm_error)?;
            print_vm_result("stop", &status, &args.format);
        }
        VmCommands::Rebuild => {
            let status = lima::rebuild_vm().map_err(vm_error)?;
            print_vm_result("rebuild", &status, &args.format);
        }
        VmCommands::Repair => {
            let result = lima::repair_stale_pid().map_err(vm_error)?;
            if output::resolve_format(&args.format) == "json" {
                output::print_json(&result);
            } else {
                println!("VM repair: {} ({})", result.status, result.path);
            }
        }
        VmCommands::Recreate { yes, force } => {
            recreate(*yes, *force, &args.format)?;
        }
    }

    Ok(())
}

fn recreate(yes: bool, force: bool, format: &str) -> Result<()> {
    let current = lima::vm_status().map_err(vm_error)?;
    if current.exists() {
        match lima::list_vm_sandboxes() {
            Ok(sandboxes) if !sandboxes.is_empty() => {
                eprintln!("The Lima VM contains {} sandbox(es):", sandboxes.len());
                for sandbox in &sandboxes {
                    eprintln!("  {}  {}  {}", sandbox.id, sandbox.name, sandbox.status);
                }
                if !force {
                    return Err(ClawError::VmError(
                        "Refusing to recreate a VM that contains sandboxes; destroy them first or pass --force"
                            .to_string(),
                    )
                    .into());
                }
            }
            Ok(_) => {}
            Err(error) if !force => {
                return Err(ClawError::VmError(format!(
                    "Could not inspect sandbox state ({error:#}); pass --force only if losing all VM data is acceptable"
                ))
                .into());
            }
            Err(error) => {
                eprintln!("Warning: sandbox state could not be inspected: {error:#}");
            }
        }

        if !yes {
            confirm_recreate()?;
        }
    }

    let status = lima::recreate_vm().map_err(vm_error)?;
    print_vm_result("recreate", &status, format);
    Ok(())
}

fn confirm_recreate() -> Result<()> {
    if !io::stdin().is_terminal() {
        return Err(ClawError::VmError(
            "Recreation requires confirmation; rerun with --yes for non-interactive use"
                .to_string(),
        )
        .into());
    }

    eprintln!(
        "WARNING: recreating the Lima VM permanently deletes every sandbox, snapshot, log, and state record inside it."
    );
    eprint!("Type 'recreate' to continue: ");
    io::stderr().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    if response.trim() != "recreate" {
        return Err(ClawError::VmError("Recreation cancelled".to_string()).into());
    }
    Ok(())
}

fn print_vm_result(action: &'static str, status: &lima::VmStatus, format: &str) {
    if output::resolve_format(format) == "json" {
        output::print_json(&VmActionResult {
            action,
            result: status,
        });
        return;
    }

    let resources = match (status.cpus, status.memory_bytes, status.disk_bytes) {
        (Some(cpus), Some(memory), Some(disk)) => format!(
            ", {cpus} CPU, {} memory, {} disk",
            format_bytes(memory),
            format_bytes(disk)
        ),
        _ => String::new(),
    };
    println!(
        "VM {}: {}{}",
        status.name,
        status.status.to_lowercase(),
        resources
    );
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    format!("{:.0} GiB", bytes as f64 / GIB)
}

fn vm_error(error: anyhow::Error) -> anyhow::Error {
    ClawError::VmError(format!("{error:#}")).into()
}
