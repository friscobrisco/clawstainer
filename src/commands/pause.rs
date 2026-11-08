use anyhow::Result;

use crate::cli::MachineArgs;
use crate::output;
use crate::runtime::Runtime;
use crate::state::StateStore;

pub fn pause(args: MachineArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    let result = runtime.pause(&args.machine_id, state)?;
    output::print_json(&result);
    Ok(())
}

pub fn resume(args: MachineArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    let result = runtime.resume(&args.machine_id, state)?;
    output::print_json(&result);
    Ok(())
}
