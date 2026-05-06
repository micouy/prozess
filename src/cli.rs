use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "pz", version, about = "Local process manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the local pz daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Spawn a process through the daemon.
    Run(RunArgs),

    /// Stop a running process.
    Stop(StopArgs),

    /// List tracked processes.
    Ps,

    /// Show details for one process.
    Show { process: String },

    /// Print captured process output.
    Logs(LogsArgs),
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the local daemon in the background.
    Start,

    /// Run the local daemon in the foreground.
    Run,

    /// Check whether the local daemon is running.
    Status,

    /// Stop the local daemon.
    Stop,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Human-friendly process name for later show/logs/stop commands.
    #[arg(long)]
    pub name: Option<String>,

    /// Working directory for the process. Defaults to the current directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Inherit the CLI environment. By default, processes start with an empty environment.
    #[arg(long)]
    pub inherit_env: bool,

    /// Read environment variables from a KEY=VALUE file. Repeatable.
    #[arg(long = "env-file")]
    pub env_files: Vec<PathBuf>,

    /// Set an environment variable as KEY=VALUE. Repeatable and takes precedence over env files.
    #[arg(long = "env")]
    pub env: Vec<String>,

    /// Command and arguments to run. Use `--` before commands with flags.
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    pub process: String,

    /// Send SIGKILL instead of SIGTERM.
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    pub process: String,

    /// Output channel to show.
    #[arg(value_enum, default_value_t = LogStream::All)]
    pub channel: LogStream,

    /// Continue printing new output as it arrives.
    #[arg(short, long)]
    pub follow: bool,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum LogStream {
    All,
    Stdout,
    Stderr,
}
