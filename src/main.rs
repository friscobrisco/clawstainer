mod cli;
mod commands;
mod component;
mod error;
mod execlog;
mod firecracker;
mod image;
mod lima;
mod network;
mod output;
mod runtime;
mod state;

use clap::Parser;

use cli::{Cli, Commands};
use error::ClawError;
use output::CliError;
use runtime::firecracker::FirecrackerRuntime;
use runtime::nspawn::NspawnRuntime;
use runtime::Runtime;
use state::StateStore;

fn main() {
    // On macOS, transparently proxy all commands into a Lima Linux VM
    if lima::needs_proxy() {
        if let Err(e) = lima::proxy_to_vm() {
            CliError::new("runtime_unavailable", format!("{e:#}")).exit();
        }
        unreachable!();
    }

    // On Linux, run directly
    let cli = Cli::parse();

    let state = match StateStore::new() {
        Ok(s) => s,
        Err(e) => {
            CliError::new("init_failed", format!("Failed to initialize state: {e}")).exit();
        }
    };

    let result = match cli.command {
        Commands::Create(args) => {
            let rt = make_runtime(&args.runtime);
            commands::create::run(args, rt.as_ref(), &state)
        }
        Commands::Provision(args) => {
            let rt = runtime_for_machine(&args.machine_id, &state);
            commands::provision::run(args, rt.as_ref(), &state)
        }
        Commands::Exec(args) => {
            let rt = runtime_for_machine(&args.machine_id, &state);
            commands::exec::run(args, rt.as_ref(), &state)
        }
        Commands::Shell(args) => {
            let rt = runtime_for_machine(&args.machine_id, &state);
            commands::shell::run(args, rt.as_ref(), &state)
        }
        Commands::Destroy(args) => {
            if args.all {
                // For --all, use nspawn as default (destroy reads per-machine runtime)
                let rt = NspawnRuntime::new();
                commands::destroy::run(args, &rt, &state)
            } else if let Some(ref id) = args.machine_id {
                let rt = runtime_for_machine(id, &state);
                commands::destroy::run(args, rt.as_ref(), &state)
            } else {
                let rt = NspawnRuntime::new();
                commands::destroy::run(args, &rt, &state)
            }
        }
        Commands::List(args) => commands::list::run(args, &state),
        Commands::Logs(args) => commands::logs::run(args),
        Commands::PortForward(args) => commands::port_forward::run(args, &state),
        Commands::Stats(args) => {
            let rt = runtime_for_machine(&args.machine_id, &state);
            commands::stats::run(args, rt.as_ref(), &state)
        }
    };

    if let Err(e) = result {
        if let Some(claw_err) = e.downcast_ref::<ClawError>() {
            let mut cli_err = CliError::new(claw_err.code(), claw_err.to_string());
            if let Some(hint) = claw_err.hint() {
                cli_err = cli_err.with_hint(hint);
            }
            cli_err.exit();
        }
        CliError::new("internal_error", format!("{e:#}")).exit();
    }
}

fn make_runtime(name: &str) -> Box<dyn Runtime> {
    match name {
        "firecracker" | "fc" => Box::new(FirecrackerRuntime::new()),
        _ => Box::new(NspawnRuntime::new()),
    }
}

fn runtime_for_machine(machine_id: &str, state: &StateStore) -> Box<dyn Runtime> {
    let runtime_name = state
        .with_read_lock(|s| {
            Ok(s.machines
                .get(machine_id)
                .map(|m| m.runtime.clone())
                .unwrap_or_else(|| "nspawn".to_string()))
        })
        .unwrap_or_else(|_| "nspawn".to_string());

    make_runtime(&runtime_name)
}
