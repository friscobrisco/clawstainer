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

    /// List all sandboxes (use --watch N for live refresh)
    List(ListArgs),

    /// Show command execution history for a sandbox
    Logs(LogsArgs),

    /// Show resource usage and disk stats (use --watch N for live refresh)
    Stats(StatsArgs),

    /// Forward a port from the host to a sandbox
    #[command(name = "port-forward")]
    PortForward(PortForwardArgs),

    /// Manage fleets of sandboxes from a YAML definition
    Fleet(FleetArgs),
}

#[derive(clap::Args)]
pub struct FleetArgs {
    #[command(subcommand)]
    pub command: FleetCommands,
}

#[derive(Subcommand)]
pub enum FleetCommands {
    /// Create all machines defined in a fleet YAML file
    Create(FleetCreateArgs),

    /// Destroy fleet machines
    Destroy(FleetDestroyArgs),
}

#[derive(clap::Args)]
pub struct FleetCreateArgs {
    /// Path to fleet YAML definition file
    #[arg(long, short, default_value = "fleet.yaml")]
    pub file: String,

    /// Runtime backend: "nspawn" | "firecracker"
    #[arg(long, default_value = "nspawn")]
    pub runtime: String,

    /// Network mode: "nat" | "none"
    #[arg(long, default_value = "nat")]
    pub network: String,

    /// Max parallel provisioning jobs (0 = sequential)
    #[arg(long, default_value = "3")]
    pub parallel: usize,
}

#[derive(clap::Args)]
pub struct FleetDestroyArgs {
    /// Destroy all fleet machines
    #[arg(long)]
    pub all: bool,

    /// Destroy machines belonging to a specific fleet group name
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(clap::Args)]
pub struct PortForwardArgs {
    /// Machine ID
    pub machine_id: String,

    /// Port mapping: HOST_PORT:SANDBOX_PORT (e.g. 8080:8080 or just 8080 for same port)
    pub port: String,
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

    /// Runtime backend: "nspawn" | "firecracker"
    #[arg(long, default_value = "nspawn")]
    pub runtime: String,
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

    /// Watch mode: refresh every N seconds (0 = one-shot)
    #[arg(long, default_value = "0")]
    pub watch: u64,
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
    /// Machine ID (omit for global stats)
    pub machine_id: Option<String>,

    /// Watch mode: refresh every N seconds (0 = one-shot)
    #[arg(long, default_value = "0")]
    pub watch: u64,

    /// Output format: "table" | "json"
    #[arg(long, default_value = "table")]
    pub format: String,
}
