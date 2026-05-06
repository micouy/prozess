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
    Show { id: i64 },

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
    /// Command and arguments to run. Use `--` before commands with flags.
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    pub id: i64,

    /// Send SIGKILL instead of SIGTERM.
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    pub id: i64,

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
