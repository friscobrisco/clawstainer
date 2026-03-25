mod cli;
mod commands;
mod component;
mod error;
mod execlog;
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
use runtime::nspawn::NspawnRuntime;
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

    let runtime = NspawnRuntime::new();

    let result = match cli.command {
        Commands::Create(args) => commands::create::run(args, &runtime, &state),
        Commands::Provision(args) => commands::provision::run(args, &runtime, &state),
        Commands::Exec(args) => commands::exec::run(args, &runtime, &state),
        Commands::Shell(args) => commands::shell::run(args, &runtime, &state),
        Commands::Destroy(args) => commands::destroy::run(args, &runtime, &state),
        Commands::List(args) => commands::list::run(args, &state),
        Commands::Logs(args) => commands::logs::run(args),
        Commands::Stats(args) => commands::stats::run(args, &runtime, &state),
    };

    if let Err(e) = result {
        // Try to downcast to our typed error
        if let Some(claw_err) = e.downcast_ref::<ClawError>() {
            let mut cli_err = CliError::new(claw_err.code(), claw_err.to_string());
            if let Some(hint) = claw_err.hint() {
                cli_err = cli_err.with_hint(hint);
            }
            cli_err.exit();
        }
        // Fallback for unexpected errors
        CliError::new("internal_error", format!("{e:#}")).exit();
    }
}
