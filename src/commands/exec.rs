use anyhow::Result;
use std::collections::HashMap;

use crate::cli::ExecArgs;
use crate::execlog;
use crate::output;
use crate::runtime::{ExecOpts, Runtime};
use crate::state::StateStore;

pub fn run(args: ExecArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    state.get_running_machine_live(&args.machine_id, runtime)?;

    let mut env = HashMap::new();
    for e in &args.envs {
        if let Some((k, v)) = e.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }

    let format = output::resolve_format(&args.format).to_string();

    let opts = ExecOpts {
        command: args.command.clone(),
        timeout: args.timeout,
        workdir: args.workdir,
        env,
        user: args.user,
    };

    let result = runtime.exec(&args.machine_id, opts)?;

    // Log the exec
    execlog::logger::append(&args.machine_id, &args.command, &result)?;

    if format == "json" {
        output::print_json(&result);
    } else {
        // In table mode, print stdout directly (most common use case)
        if !result.stdout.is_empty() {
            print!("{}", result.stdout);
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
        }
        if result.exit_code != 0 {
            eprintln!(
                "[exit {} in {:.1}s{}]",
                result.exit_code,
                result.duration_ms as f64 / 1000.0,
                if result.timed_out { " TIMED OUT" } else { "" }
            );
        }
    }
    Ok(())
}
