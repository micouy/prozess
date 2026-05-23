use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "pz",
    version,
    about = "Local process manager",
    after_help = "Process ownership:\n  Use pz run --name <name> -- <command> for long-running commands. pz keeps the process, logs, ports, and stop/restart handle after this shell exits. Do not use &, nohup, disown, or pkill -f.\n\nEnvironment:\n  pz uses a controlled environment by default. Use pz run --inherit-env when the command depends on the current shell PATH or environment."
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
    #[command(
        after_help = "Examples:\n  pz run --name my-app -- npm run dev\n  pz run --name web --cwd /path/to/project -- python3 -m http.server 8000\n  pz run --name test --timeout 5m -- cargo test\n\nNotes:\n  Use -- before the command. pz runs the command directly, not through a shell. Use --name for long-running processes so they can be inspected, logged, stopped, and restarted. Use --inherit-env if command lookup depends on the current shell environment.\n\nSee also:\n  pz ps\n  pz logs --help\n  pz stop --help"
    )]
    Run(RunArgs),

    /// Stop a running process.
    #[command(
        after_help = "Examples:\n  pz stop my-app\n  pz stop my-app --force\n\nNotes:\n  pz stop signals the tracked process group. Use this instead of pkill -f.\n\nSee also:\n  pz ps\n  pz show --help"
    )]
    Stop(StopArgs),

    /// Add, replace, or clear a process timeout.
    #[command(
        after_help = "Examples:\n  pz timeout my-app 5m\n  pz timeout my-app clear\n\nNotes:\n  Timeouts are enforced by the daemon even if the client exits. Timed out processes get status timed_out.\n\nSee also:\n  pz show --help"
    )]
    Timeout(TimeoutArgs),

    /// Wait for a process to finish.
    #[command(
        after_help = "Examples:\n  pz wait my-app\n\nNotes:\n  Blocks until the process exits and returns the process exit code. The process remains daemon-owned if the waiting client exits.\n\nSee also:\n  pz run --help\n  pz show --help"
    )]
    Wait { process: String },

    /// Restart a process from stored command/cwd/env-file metadata.
    #[command(
        after_help = "Examples:\n  pz restart my-app\n\nNotes:\n  Restarts from stored command, cwd, env files, and timeout metadata. Inline --env values are not stored, so processes using them cannot be restarted exactly.\n\nSee also:\n  pz run --help\n  pz show --help"
    )]
    Restart { process: String },

    /// Show current CPU and memory usage for a running process group.
    #[command(
        after_help = "Examples:\n  pz resources my-app\n\nNotes:\n  Shows current CPU and memory for the tracked process group. Only running, supervised processes have live resources.\n\nSee also:\n  pz ps\n  pz ports --help"
    )]
    Resources { process: String },

    /// Show listening TCP ports owned by a running process group.
    #[command(
        after_help = "Examples:\n  pz ports my-app\n\nNotes:\n  Shows listening TCP ports owned by the tracked process group. If port discovery is unavailable, pz reports that instead of failing process listing.\n\nSee also:\n  pz ps\n  pz resources --help"
    )]
    Ports { process: String },

    /// List tracked processes.
    #[command(
        after_help = "Examples:\n  pz ps\n\nNotes:\n  Lists tracked processes with names, ids, status, ports, exit codes, and commands. Use this to find the name or id for logs, show, stop, restart, resources, and ports.\n\nSee also:\n  pz show --help\n  pz logs --help"
    )]
    Ps,

    /// Show details for one process.
    #[command(
        after_help = "Examples:\n  pz show my-app\n  pz show 12\n\nNotes:\n  Shows stored metadata for one process: status, command, cwd, env sources, pid, pgid, and timeout.\n\nSee also:\n  pz logs --help\n  pz resources --help\n  pz ports --help"
    )]
    Show { process: String },

    /// Print captured process output.
    #[command(
        after_help = "Examples:\n  pz logs my-app\n  pz logs my-app stdout\n  pz logs my-app --tail 100\n  pz logs my-app --since 10m\n  pz logs my-app -f\n\nNotes:\n  Logs are captured by pz after the process starts. -f blocks until the process exits.\n\nSee also:\n  pz ps\n  pz show --help"
    )]
    Logs(LogsArgs),
}

#[derive(Debug, Subcommand)]
#[command(
    after_help = "Examples:\n  pz daemon start\n  pz daemon status\n  pz daemon stop\n\nNotes:\n  The daemon owns process supervision, logs, timeouts, and SQLite state. Most users should use pz run, pz ps, pz logs, and pz stop.\n\nSee also:\n  pz run --help\n  pz ps"
)]
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
