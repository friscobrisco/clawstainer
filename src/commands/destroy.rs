use anyhow::Result;

use crate::cli::DestroyArgs;
use crate::error::ClawError;
use crate::output;
use crate::runtime::Runtime;
use crate::state::StateStore;

pub fn run(args: DestroyArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    if args.all {
        let machine_ids: Vec<String> = state.with_read_lock(|s| {
            Ok(s.machines.keys().cloned().collect())
        })?;

        for id in machine_ids {
            let result = runtime.destroy(&id, state)?;
            output::print_json(&result);
        }
        return Ok(());
    }

    let machine_id = args.machine_id
        .ok_or(ClawError::ExecFailed("Either a machine ID or --all must be specified".to_string()))?;

    let result = runtime.destroy(&machine_id, state)?;
    output::print_json(&result);
    Ok(())
}
