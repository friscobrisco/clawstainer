use anyhow::Result;

use crate::cli::CreateArgs;
use crate::output;
use crate::runtime::{CreateOpts, Runtime};
use crate::state::StateStore;

pub fn run(args: CreateArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    let cap_add = args.cap_add
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default();
    let cap_drop = args.cap_drop
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default();

    let format = output::resolve_format(&args.format).to_string();

    let opts = CreateOpts {
        name: args.name,
        memory_mb: args.memory,
        cpus: args.cpus,
        network: args.network,
        timeout: args.timeout,
        runtime: args.runtime,
        security: args.security,
        cap_add,
        cap_drop,
        env_file: args.env_file,
        from_snapshot: args.from,
        linger: args.linger,
    };

    let machine = runtime.create(opts, state)?;

    if format == "json" {
        output::print_json(&machine);
    } else {
        println!(
            "{:<14} {:<16} {:<10} {}",
            machine.id,
            machine.name,
            machine.status,
            machine.ip.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}
