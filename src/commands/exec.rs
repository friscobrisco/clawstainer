use anyhow::Result;
use std::collections::HashMap;

use crate::cli::ExecArgs;
use crate::execlog;
use crate::output;
use crate::runtime::{ExecOpts, Runtime};
use crate::state::StateStore;

pub fn run(args: ExecArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    state.get_running_machine(&args.machine_id)?;

    let mut env = HashMap::new();
    for e in &args.envs {
        if let Some((k, v)) = e.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }

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

    output::print_json(&result);
    Ok(())
}
