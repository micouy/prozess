mod cli;
mod client;
mod config;
mod daemon_state;
mod pid_identity;
mod ports;
mod protocol;
mod runtime;
mod server;
mod service;
mod store;
mod supervisor;
mod terminate;

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, error::ErrorKind};

use crate::cli::{Cli, Command, DaemonCommand, LogStream, RunArgs};
use crate::client::Client;
use crate::config::Config;
use crate::protocol::{
    EnvVar, OutputChunk, OutputStream, ProcessSelector, ProcessStatus, Request, Response, RunSpec,
    StopSignal,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli();

    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Start => server::start().await,
            DaemonCommand::Run => server::run().await,
            DaemonCommand::Status => {
                print_response(Client::new().send(Request::DaemonStatus).await)
            }
            DaemonCommand::Stop => print_response(Client::new().send(Request::DaemonStop).await),
        },
        Command::Run(args) => print_response(
            Client::new()
                .send(Request::Spawn {
                    spec: run_spec(args)?,
                })
                .await,
        ),
        Command::Stop(args) => print_response(
            Client::new()
                .send(Request::StopProcess {
                    selector: process_selector(&args.process),
                    force: args.force,
                    grace_ms: args.grace.as_deref().map(parse_duration_ms).transpose()?,
                })
                .await,
        ),
        Command::Timeout(args) => print_response(
            Client::new()
                .send(Request::SetTimeout {
                    selector: process_selector(&args.process),
                    timeout_ms: parse_timeout_arg(&args.timeout)?,
                })
                .await,
        ),
        Command::Wait { process } => wait_process(process_selector(&process)).await,
        Command::Restart { process } => print_response(
            Client::new()
                .send(Request::RestartProcess {
                    selector: process_selector(&process),
                })
                .await,
        ),
        Command::Resources { process } => print_response(
            Client::new()
                .send(Request::Resources {
                    selector: process_selector(&process),
                })
                .await,
        ),
        Command::Ports { process } => print_response(
            Client::new()
                .send(Request::Ports {
                    selector: process_selector(&process),
                })
                .await,
        ),
        Command::Ps(args) => print_response(
            Client::new()
                .send(Request::ListProcesses { all: args.all })
                .await,
        ),
        Command::Show { process } => print_response(
            Client::new()
                .send(Request::ShowProcess {
                    selector: process_selector(&process),
                })
                .await,
        ),
        Command::Logs(args) => {
            let selector = args.process.as_deref().map(process_selector);
            let stream = args.channel.into();
            let since_ms = cutoff_ms(args.since.as_deref())?;
            let mut printer = if args.all {
                OutputPrinter::prefixed()
            } else {
                OutputPrinter::raw()
            };

            if args.follow {
                follow_logs(selector, stream, args.tail, since_ms, args.all, printer).await
            } else {
                match Client::new()
                    .send(Request::ReadLogs {
                        selector,
                        stream,
                        after_id: None,
                        since_ms,
                        until_ms: cutoff_ms(args.until.as_deref())?,
                        tail_lines: args.tail.map(|tail| tail as u64),
                    })
                    .await?
                {
                    Response::Output { chunks, .. } => {
                        printer.print(&chunks).await?;
                        printer.flush_partial_lines()?;
                        Ok(())
                    }
                    Response::Error { message } => bail!(message),
                    _ => bail!("daemon returned an unexpected logs response"),
                }
            }
        }
    }
}

fn parse_cli() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let kind = error.kind();
            let _ = error.print();
            if !matches!(
                kind,
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                let mut command = help_command_for_error();
                let mut stderr = std::io::stderr().lock();
                let command_name = command.get_name().to_owned();
                let _ = writeln!(stderr);
                let _ = writeln!(stderr, "Help page for `{command_name}`:");
                let _ = command.write_help(&mut stderr);
                let _ = writeln!(stderr);
            }
            std::process::exit(error.exit_code());
        }
    }
}

fn help_command_for_error() -> clap::Command {
    let mut command = Cli::command();
    let args = std::env::args().collect::<Vec<_>>();
    let Some(subcommand_name) = args.get(1) else {
        return command;
    };

    if let Some(subcommand) = command
        .get_subcommands_mut()
        .find(|subcommand| subcommand.get_name() == subcommand_name)
    {
        return subcommand.clone();
    }

    command
}

async fn wait_process(selector: ProcessSelector) -> Result<()> {
    let response = Client::new()
        .send(Request::WaitProcess { selector })
        .await?;

    match response {
        Response::WaitedProcess(process) => match process.status {
            ProcessStatus::Exited => std::process::exit(process.exit_code.unwrap_or(1)),
            ProcessStatus::Killed
            | ProcessStatus::Failed
            | ProcessStatus::TimedOut
            | ProcessStatus::Lost => std::process::exit(1),
            ProcessStatus::Running => bail!("process is still running"),
        },
        Response::Error { message } => bail!(message),
        _ => bail!("daemon returned an unexpected wait response"),
    }
}

fn run_spec(args: RunArgs) -> Result<RunSpec> {
    let config = Config::load()?;
    let cli_cwd = std::env::current_dir().context("failed to get current directory")?;
    let cwd = absolute_path(args.cwd.as_deref().unwrap_or(Path::new(".")), &cli_cwd)?;
    let mut env_files = config
        .run
        .env_files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    env_files.extend(
        args.env_files
            .iter()
            .map(|path| absolute_path(path, &cli_cwd).map(|path| path.display().to_string()))
            .collect::<Result<Vec<_>>>()?,
    );
    // The config's [env] section is applied by the daemon at spawn time,
    // not merged here: only genuine --env values are irreproducible and
    // must make a process unrestartable.
    let env = args
        .env
        .iter()
        .map(|value| parse_env_var(value))
        .collect::<Result<Vec<_>>>()?;

    Ok(RunSpec {
        name: args.name,
        replace: args.replace,
        timeout_ms: args.timeout.as_deref().map(parse_duration_ms).transpose()?,
        command: args.command,
        cwd: cwd.display().to_string(),
        inherit_env: config.run.inherit_env || args.inherit_env,
        env_files,
        env,
    })
}

fn parse_timeout_arg(value: &str) -> Result<Option<u64>> {
    if value == "clear" {
        Ok(None)
    } else {
        parse_duration_ms(value).map(Some)
    }
}

fn parse_duration_ms(value: &str) -> Result<u64> {
    let Some(unit) = value.chars().last() else {
        bail!("timeout cannot be empty");
    };
    let number = &value[..value.len() - unit.len_utf8()];
    let amount = number
        .parse::<u64>()
        .with_context(|| format!("invalid timeout duration {value:?}"))?;
    let multiplier = match unit {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        _ => bail!("invalid timeout unit {unit:?}: expected s, m, or h"),
    };

    amount
        .checked_mul(multiplier)
        .context("timeout duration is too large")
}

fn cutoff_ms(value: Option<&str>) -> Result<Option<i64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let duration_ms = parse_duration_ms(value)?;
    let now = now_ms()?;
    let duration_ms = i64::try_from(duration_ms).context("duration does not fit in i64")?;

    now.checked_sub(duration_ms)
        .context("duration is too large")
        .map(Some)
}

fn now_ms() -> Result<i64> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;

    i64::try_from(duration.as_millis()).context("current timestamp does not fit in i64")
}

fn process_selector(value: &str) -> ProcessSelector {
    value
        .parse::<i64>()
        .map(ProcessSelector::Id)
        .unwrap_or_else(|_| ProcessSelector::Name(value.to_owned()))
}

fn absolute_path(path: &Path, base: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };

    Ok(path.components().collect::<PathBuf>())
}

fn parse_env_var(value: &str) -> Result<EnvVar> {
    let Some((key, value)) = value.split_once('=') else {
        bail!("invalid --env value {value:?}: expected KEY=VALUE");
    };
    if key.is_empty() || key.contains('\0') {
        bail!("invalid --env key {key:?}");
    }

    Ok(EnvVar {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

async fn follow_logs(
    selector: Option<ProcessSelector>,
    stream: OutputStream,
    tail_lines: Option<usize>,
    since_ms: Option<i64>,
    all: bool,
    mut printer: OutputPrinter,
) -> Result<()> {
    let client = Client::new();
    let mut after_id = None;
    // First read only; later reads resume from the returned cursor.
    let mut tail_lines = tail_lines.map(|tail| tail as u64);
    let mut quiet_polls_after_exit = 0;

    loop {
        let (chunks, resume_after_id) = match client
            .send(Request::ReadLogs {
                selector: selector.clone(),
                stream,
                after_id,
                since_ms,
                until_ms: None,
                tail_lines: tail_lines.take(),
            })
            .await?
        {
            Response::Output {
                chunks,
                resume_after_id,
            } => (chunks, resume_after_id),
            Response::Error { message } => bail!(message),
            _ => bail!("daemon returned an unexpected logs response"),
        };
        let printed_any = !chunks.is_empty();
        printer.print(&chunks).await?;
        after_id = Some(resume_after_id);

        // Following everything has no exit condition: new processes can
        // appear at any time. Runs until interrupted.
        if !all {
            let selector = selector.clone().context("missing process selector")?;
            let is_running = match client.send(Request::ShowProcess { selector }).await? {
                Response::ProcessDetails(process) => process.status == ProcessStatus::Running,
                Response::Error { message } => bail!(message),
                _ => bail!("daemon returned an unexpected process response"),
            };

            if is_running || printed_any {
                quiet_polls_after_exit = 0;
            } else {
                quiet_polls_after_exit += 1;
            }

            if !is_running && quiet_polls_after_exit >= 3 {
                break;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    printer.flush_partial_lines()?;

    Ok(())
}

/// Prints chunks either raw (single-process mode) or line-buffered with a
/// `name | ` prefix (all-processes mode), where buffering prevents a chunk
/// ending mid-line from gluing another process's prefix onto its tail.
struct OutputPrinter {
    prefixed: bool,
    labels: std::collections::HashMap<i64, String>,
    pending: std::collections::HashMap<i64, Vec<u8>>,
}

impl OutputPrinter {
    fn raw() -> Self {
        Self {
            prefixed: false,
            labels: Default::default(),
            pending: Default::default(),
        }
    }

    fn prefixed() -> Self {
        Self {
            prefixed: true,
            ..Self::raw()
        }
    }

    async fn print(&mut self, chunks: &[OutputChunk]) -> Result<()> {
        if !self.prefixed {
            return print_output(chunks);
        }

        let mut stdout = std::io::stdout().lock();

        for chunk in chunks {
            let label = self.label(chunk.process_id).await?;
            let buffer = self.pending.entry(chunk.process_id).or_default();
            buffer.extend_from_slice(&chunk.data);

            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=newline).collect::<Vec<_>>();
                check_stdout_write(
                    stdout.write_all(format!("{label} | ").as_bytes()),
                    "failed to write output",
                )?;
                check_stdout_write(stdout.write_all(&line), "failed to write output")?;
            }
        }
        check_stdout_write(stdout.flush(), "failed to flush output")?;

        Ok(())
    }

    async fn label(&mut self, process_id: i64) -> Result<String> {
        if let Some(label) = self.labels.get(&process_id) {
            return Ok(label.clone());
        }

        let label = match Client::new()
            .send(Request::ShowProcess {
                selector: ProcessSelector::Id(process_id),
            })
            .await?
        {
            Response::ProcessDetails(process) => {
                process.name.unwrap_or_else(|| format!("#{}", process.id))
            }
            _ => format!("#{process_id}"),
        };
        self.labels.insert(process_id, label.clone());

        Ok(label)
    }

    fn flush_partial_lines(&mut self) -> Result<()> {
        let mut stdout = std::io::stdout().lock();

        for (process_id, buffer) in std::mem::take(&mut self.pending) {
            if buffer.is_empty() {
                continue;
            }

            let label = self
                .labels
                .get(&process_id)
                .cloned()
                .unwrap_or_else(|| format!("#{process_id}"));
            check_stdout_write(
                stdout.write_all(format!("{label} | ").as_bytes()),
                "failed to write output",
            )?;
            check_stdout_write(stdout.write_all(&buffer), "failed to write output")?;
            check_stdout_write(stdout.write_all(b"\n"), "failed to write output")?;
        }
        check_stdout_write(stdout.flush(), "failed to flush output")?;

        Ok(())
    }
}

fn print_response(response: Result<Response>) -> Result<()> {
    match response? {
        Response::DaemonStatus {
            pid,
            socket,
            database,
        } => {
            println!("pz daemon running");
            println!("pid: {pid}");
            println!("socket: {socket}");
            println!("db: {database}");
        }
        Response::DaemonStopping => println!("pz daemon stopped"),
        Response::Spawned(process) => {
            println!("spawned process {}", process.id);
            if let Some(name) = process.name {
                println!("name: {name}");
            }
            println!("status: {}", process.status);
            if let Some(timeout_ms) = process.timeout_ms {
                println!("timeout: {}", format_duration_ms(timeout_ms));
            }
            println!("command: {}", process.command.join(" "));
        }
        Response::StoppedProcess { id, signal } => {
            println!("stopped process {id}");
            println!("signal: {signal}");
        }
        Response::TimeoutUpdated { id, timeout_ms } => {
            if let Some(timeout_ms) = timeout_ms {
                println!("timeout set for process {id}");
                println!("timeout: {}", format_duration_ms(timeout_ms));
            } else {
                println!("timeout cleared for process {id}");
            }
        }
        Response::WaitedProcess(process) => print_process_details(&process),
        Response::ProcessList(processes) => print_process_list(&processes),
        Response::ProcessDetails(process) => print_process_details(&process),
        Response::ResourceSnapshot(snapshot) => print_resource_snapshot(&snapshot),
        Response::PortList(ports) => print_ports(&ports),
        Response::Output { chunks, .. } => {
            print_output(&chunks)?;
        }
        Response::Error { message } => bail!(message),
    }

    Ok(())
}

fn print_ports(ports: &crate::protocol::PortList) {
    println!("id: {}", ports.process_id);
    if let Some(name) = &ports.name {
        println!("name: {name}");
    }
    println!("status: {}", ports.status);

    if ports.status != ProcessStatus::Running {
        println!("ports: unavailable for non-running process");
        return;
    }

    if ports.unavailable {
        println!("ports: unavailable");
        return;
    }

    if ports.ports.is_empty() {
        println!("ports: none");
        return;
    }

    println!();
    println!("{:<6} {:<8} {:<22} PIDS", "PROTO", "STATE", "LOCAL");
    for port in &ports.ports {
        println!(
            "{:<6} {:<8} {:<22} {}",
            port.protocol,
            port.state,
            format!("{}:{}", port.local_addr, port.local_port),
            port.pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
}

fn print_resource_snapshot(snapshot: &crate::protocol::ResourceSnapshot) {
    println!("id: {}", snapshot.process_id);
    if let Some(name) = &snapshot.name {
        println!("name: {name}");
    }
    println!("status: {}", snapshot.status);
    println!(
        "pid: {}",
        snapshot
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "pgid: {}",
        snapshot
            .pgid
            .map(|pgid| pgid.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );

    if snapshot.status != ProcessStatus::Running {
        println!("resources: unavailable for non-running process");
        return;
    }

    println!("processes: {}", snapshot.process_count);
    println!("memory: {}", format_bytes(snapshot.total_memory_bytes));
    println!("cpu: {:.1}%", snapshot.total_cpu_percent);

    if snapshot.processes.is_empty() {
        return;
    }

    println!();
    println!("{:<8} {:<8} {:<8} {:<10} NAME", "PID", "PPID", "CPU", "MEM");
    for process in &snapshot.processes {
        println!(
            "{:<8} {:<8} {:<8} {:<10} {}",
            process.pid,
            process
                .parent_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            format!("{:.1}%", process.cpu_percent),
            format_bytes(process.memory_bytes),
            process.name,
        );
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Treats a closed stdout (e.g. `pz logs | head`) as a silent success:
/// the consumer is done, so exit 0 without an error, SIGPIPE-style.
fn check_stdout_write(result: std::io::Result<()>, context: &'static str) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(err) => Err(err).context(context),
    }
}

fn print_output(chunks: &[OutputChunk]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    for chunk in chunks {
        check_stdout_write(stdout.write_all(&chunk.data), "failed to write output")?;
        check_stdout_write(stdout.flush(), "failed to flush output")?;
    }

    Ok(())
}

fn print_process_details(process: &crate::protocol::ProcessDetails) {
    let pid = process
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let exit = process
        .exit_code
        .map(|exit| exit.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let pgid = process
        .pgid
        .map(|pgid| pgid.to_string())
        .unwrap_or_else(|| "-".to_owned());

    println!("id: {}", process.id);
    if let Some(name) = &process.name {
        println!("name: {name}");
    }
    println!("status: {}", process.status);
    println!("pid: {pid}");
    println!("pgid: {pgid}");
    println!("exit: {exit}");
    println!("command: {}", process.command.join(" "));
    println!("cwd: {}", process.cwd);
    if let Some(timeout_ms) = process.timeout_ms {
        println!("timeout: {}", format_duration_ms(timeout_ms));
    }
    println!("inherit env: {}", process.env.inherit_env);

    if !process.env.env_files.is_empty() {
        println!("env files: {}", process.env.env_files.join(", "));
    }

    if !process.env.env_keys.is_empty() {
        println!("env overrides: {}", process.env.env_keys.join(", "));
    }

    if let Some(error) = &process.error_message {
        println!("error: {error}");
    }
}

fn print_process_list(processes: &[crate::protocol::ProcessSummary]) {
    println!(
        "{:<3} {:<16} {:<12} {:<12} {:<6} {:<5} COMMAND / ERROR",
        "ID", "NAME", "STATUS", "PORTS", "PID", "EXIT"
    );

    for process in processes {
        let pid = process
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let exit = process
            .exit_code
            .map(|exit| exit.to_string())
            .unwrap_or_else(|| "-".to_owned());

        let command = if let Some(error) = &process.error_message {
            format!("{} ({error})", process.command.join(" "))
        } else {
            process.command.join(" ")
        };

        println!(
            "{:<3} {:<16} {:<12} {:<12} {:<6} {:<5} {}",
            process.id,
            process.name.as_deref().unwrap_or("-"),
            process.status,
            format_ports(process.ports_unavailable, &process.ports),
            pid,
            exit,
            command
        );
    }
}

fn format_ports(unavailable: bool, ports: &[u16]) -> String {
    if unavailable {
        "?".to_owned()
    } else if ports.is_empty() {
        "-".to_owned()
    } else {
        ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl From<LogStream> for OutputStream {
    fn from(stream: LogStream) -> Self {
        match stream {
            LogStream::All => Self::All,
            LogStream::Stdout => Self::Stdout,
            LogStream::Stderr => Self::Stderr,
        }
    }
}

impl std::fmt::Display for crate::protocol::ProcessStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Lost => "lost",
        };

        formatter.write_str(status)
    }
}

fn format_duration_ms(timeout_ms: u64) -> String {
    if timeout_ms.is_multiple_of(3_600_000) {
        format!("{}h", timeout_ms / 3_600_000)
    } else if timeout_ms.is_multiple_of(60_000) {
        format!("{}m", timeout_ms / 60_000)
    } else if timeout_ms.is_multiple_of(1_000) {
        format!("{}s", timeout_ms / 1_000)
    } else {
        format!("{timeout_ms}ms")
    }
}

impl std::fmt::Display for StopSignal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let signal = match self {
            Self::Term => "TERM",
            Self::Kill => "KILL",
        };

        formatter.write_str(signal)
    }
}
