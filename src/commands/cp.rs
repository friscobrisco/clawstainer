use anyhow::Result;
use serde::Serialize;
use std::process::Command;

use crate::cli::CpArgs;
use crate::error::ClawError;
use crate::output;
use crate::state::StateStore;

#[derive(Debug, Serialize)]
struct CpResult {
    machine_id: String,
    direction: String,
    src: String,
    dst: String,
}

fn parse_path(s: &str) -> (Option<&str>, &str) {
    if let Some((left, right)) = s.split_once(':') {
        if left.starts_with("sb-") {
            return (Some(left), right);
        }
    }
    (None, s)
}

pub fn run(args: CpArgs, state: &StateStore) -> Result<()> {
    let (src_machine, src_path) = parse_path(&args.src);
    let (dst_machine, dst_path) = parse_path(&args.dst);

    let (machine_id, direction) = match (src_machine, dst_machine) {
        (Some(id), None) => (id, "pull"),
        (None, Some(id)) => (id, "push"),
        (Some(_), Some(_)) => {
            return Err(ClawError::CopyFailed(
                "Cannot copy directly between two sandboxes".to_string(),
            )
            .into());
        }
        (None, None) => {
            return Err(ClawError::CopyFailed(
                "One of src or dst must be a sandbox path (MACHINE_ID:/path)".to_string(),
            )
            .into());
        }
    };

    // Validate machine exists and is running
    state.get_running_machine(machine_id)?;

    let status = match direction {
        "pull" => Command::new("machinectl")
            .args(["copy-from", machine_id, src_path, dst_path])
            .status()
            .map_err(|e| ClawError::CopyFailed(format!("Failed to run machinectl: {e}")))?,
        "push" => Command::new("machinectl")
            .args(["copy-to", machine_id, src_path, dst_path])
            .status()
            .map_err(|e| ClawError::CopyFailed(format!("Failed to run machinectl: {e}")))?,
        _ => unreachable!(),
    };

    if !status.success() {
        return Err(ClawError::CopyFailed(format!(
            "machinectl copy-{} exited with status: {status}",
            if direction == "pull" { "from" } else { "to" }
        ))
        .into());
    }

    let result = CpResult {
        machine_id: machine_id.to_string(),
        direction: direction.to_string(),
        src: src_path.to_string(),
        dst: dst_path.to_string(),
    };

    if output::resolve_format(&args.format) == "json" {
        output::print_json(&result);
    } else {
        println!("Copied {} {} -> {}", direction, result.src, result.dst);
    }

    Ok(())
}
