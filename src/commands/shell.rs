use anyhow::Result;

use crate::cli::ShellArgs;
use crate::error::ClawError;
use crate::runtime::Runtime;
use crate::state::StateStore;

pub fn run(args: ShellArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    // Verify machine exists and is running
    state.with_read_lock(|s| {
        let machine = s.machines.get(&args.machine_id)
            .ok_or_else(|| ClawError::MachineNotFound(args.machine_id.clone()))?;
        if machine.status != "running" {
            return Err(ClawError::MachineNotRunning(
                args.machine_id.clone(),
                machine.status.clone(),
            ).into());
        }
        Ok(())
    })?;

    runtime.shell(&args.machine_id, &args.user)?;
    Ok(())
}
