use anyhow::Result;

use crate::cli::ShellArgs;
use crate::runtime::Runtime;
use crate::state::StateStore;

pub fn run(args: ShellArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    state.get_running_machine_live(&args.machine_id, runtime)?;

    runtime.shell(&args.machine_id, &args.user)?;
    Ok(())
}
