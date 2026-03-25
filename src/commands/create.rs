use anyhow::Result;

use crate::cli::CreateArgs;
use crate::output;
use crate::runtime::{CreateOpts, Runtime};
use crate::state::StateStore;

pub fn run(args: CreateArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    let opts = CreateOpts {
        name: args.name,
        memory_mb: args.memory,
        cpus: args.cpus,
        network: args.network,
        timeout: args.timeout,
    };

    let machine = runtime.create(opts, state)?;
    output::print_json(&machine);
    Ok(())
}
