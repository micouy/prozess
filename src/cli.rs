use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "pz",
    version,
    about = "Local process manager",
    after_help = "Common usage:\n  pz run --name my-app -- npm run dev   Start a managed process that survives this shell\n  pz ps                                 Find process names, ids, ports, and failures\n  pz logs my-app --tail 100             Read recent captured logs\n  pz stop my-app                        Stop the tracked process group safely\n  pz daemon status                      Check whether the daemon is reachable\n\nAgent note:\n  Use pz for dev servers, watchers, and other long-running commands. Give each process a stable name. Do not use &, nohup, disown, or pkill -f.\n\nSee also:\n  pz run --help\n  pz logs --help\n  pz stop --help"
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

    /// Spawn a process through the daemon.
    Run(RunArgs),

    /// Stop a running process.
    Stop(StopArgs),

    /// Add, replace, or clear a process timeout.
    Timeout(TimeoutArgs),

    /// Wait for a process to finish.
    Wait { process: String },

    /// Restart a process from stored command/cwd/env-file metadata.
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
