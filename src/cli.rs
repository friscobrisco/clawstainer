use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "clawstainer")]
#[command(about = "Lightweight isolated Linux environments for AI agents")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Spin up a new sandbox from the base image
    Create(CreateArgs),

    /// Install components inside a running sandbox
    Provision(ProvisionArgs),

    /// Run a command inside a sandbox
    Exec(ExecArgs),

    /// Open an interactive shell inside a sandbox
    Shell(ShellArgs),

    /// Tear down a sandbox and clean up all resources
    Destroy(DestroyArgs),

    /// List all sandboxes with their current status
    List(ListArgs),

    /// Show command execution history for a sandbox
    Logs(LogsArgs),

    /// Show live resource usage for a sandbox
    Stats(StatsArgs),
}

#[derive(clap::Args)]
pub struct CreateArgs {
    /// Human-readable name (auto-generated if omitted)
    #[arg(long)]
    pub name: Option<String>,

    /// Memory limit in MB
    #[arg(long, default_value = "512")]
    pub memory: u32,

    /// CPU limit
    #[arg(long, default_value = "1")]
    pub cpus: u32,

    /// Network mode: "nat" | "none"
    #[arg(long, default_value = "nat")]
    pub network: String,

    /// Auto-destroy after N seconds (0 = no timeout)
    #[arg(long, default_value = "0")]
    pub timeout: u64,
}

#[derive(clap::Args)]
pub struct ProvisionArgs {
    /// Machine ID
    pub machine_id: String,

    /// Comma-separated component names
    #[arg(long)]
    pub components: Option<String>,

    /// Path to a YAML file listing components
    #[arg(long)]
    pub file: Option<String>,

    /// Per-component timeout in seconds
    #[arg(long, default_value = "120")]
    pub timeout: u64,
}

#[derive(clap::Args)]
pub struct ExecArgs {
    /// Machine ID
    pub machine_id: String,

    /// Command to execute
    pub command: String,

    /// Command timeout in seconds
    #[arg(long, default_value = "30")]
    pub timeout: u64,

    /// Working directory inside sandbox
    #[arg(long, default_value = "/root")]
    pub workdir: String,

    /// Environment variable (repeatable)
    #[arg(long = "env", value_name = "KEY=VAL")]
    pub envs: Vec<String>,

    /// Run as user
    #[arg(long, default_value = "root")]
    pub user: String,
}

#[derive(clap::Args)]
pub struct ShellArgs {
    /// Machine ID
    pub machine_id: String,

    /// Shell user
    #[arg(long, default_value = "root")]
    pub user: String,
}

#[derive(clap::Args)]
pub struct DestroyArgs {
    /// Machine ID (omit if using --all)
    pub machine_id: Option<String>,

    /// Destroy all sandboxes
    #[arg(long)]
    pub all: bool,
}

#[derive(clap::Args)]
pub struct ListArgs {
    /// Output format: "table" | "json"
    #[arg(long, default_value = "table")]
    pub format: String,

    /// Filter by status: "running" | "stopped" | "all"
    #[arg(long, default_value = "all")]
    pub status: String,
}

#[derive(clap::Args)]
pub struct LogsArgs {
    /// Machine ID
    pub machine_id: String,

    /// Stream new entries as they happen
    #[arg(long)]
    pub follow: bool,

    /// Show last N entries
    #[arg(long, default_value = "20")]
    pub last: usize,

    /// Output format: "table" | "json"
    #[arg(long, default_value = "table")]
    pub format: String,
}

#[derive(clap::Args)]
pub struct StatsArgs {
    /// Machine ID
    pub machine_id: String,

    /// Watch mode: refresh every N seconds (0 = one-shot)
    #[arg(long, default_value = "0")]
    pub watch: u64,

    /// Output format: "table" | "json"
    #[arg(long, default_value = "table")]
    pub format: String,
}
