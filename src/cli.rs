use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "pz",
    version,
    about = "A daemon-backed process manager with persistent logs, explicit environment control, and resource usage tracking."
)]
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

    /// Start a process through the daemon and return immediately.
    #[command(
        after_help = "Examples:\n  pz run --name my-app -- npm run dev\n  pz run --name web --cwd /path/to/project -- python3 -m http.server 8000\n  pz run --name test --timeout 5m -- cargo test\n\nNotes:\n  pz runs the command directly, not through a shell. Use --name for long-running processes. Use pz logs <name> --tail 100 instead of redirecting output, pz logs <name> -f instead of foregrounding, and pz stop <name> instead of pkill -f. Do not use &, nohup, or disown."
    )]
    Run(RunArgs),

    /// Stop a running process.
    #[command(
        after_help = "Notes:\n  pz stop signals the tracked process group. Use this instead of pkill -f."
    )]
    Stop(StopArgs),

    /// Add, replace, or clear a process timeout.
    Timeout(TimeoutArgs),

    /// Wait for a process to finish.
    #[command(
        after_help = "Notes:\n  Blocks until the process exits and returns the process exit code."
    )]
    Wait { process: String },

    /// Restart a process from stored command/cwd/env-file metadata.
    #[command(
        after_help = "Notes:\n  Inline --env values are not stored, so processes using them cannot be restarted exactly."
    )]
    Restart { process: String },

    /// Show current CPU and memory usage for a running process group.
    Resources { process: String },

    /// Show listening TCP ports owned by a running process group.
    Ports { process: String },

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

    /// Stop the process if it is still running after this duration, e.g. 30s, 5m, 1h.
    #[arg(long)]
    pub timeout: Option<String>,

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
pub struct TimeoutArgs {
    pub process: String,

    /// Duration like 30s, 5m, 1h, or `clear` to remove the timeout.
    pub timeout: String,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    pub process: String,

    /// Output channel to show.
    #[arg(value_enum, default_value_t = LogStream::All)]
    pub channel: LogStream,

    /// Continue printing new output as it arrives. Blocks until the process exits.
    #[arg(short, long)]
    pub follow: bool,

    /// Print only the last N lines.
    #[arg(long)]
    pub tail: Option<usize>,

    /// Show chunks captured in the last duration, e.g. 10s, 5m, 1h.
    #[arg(long)]
    pub since: Option<String>,

    /// Show chunks captured until this duration ago, e.g. 10s, 5m, 1h.
    #[arg(long)]
    pub until: Option<String>,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum LogStream {
    All,
    Stdout,
    Stderr,
}
