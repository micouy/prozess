mod cli;
mod client;
mod config;
mod daemon_state;
mod protocol;
mod runtime;
mod server;
mod service;
mod store;
mod supervisor;

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::{Cli, Command, DaemonCommand, LogStream, RunArgs};
use crate::client::Client;
use crate::config::Config;
use crate::protocol::{
    EnvVar, OutputChunk, OutputStream, ProcessSelector, ProcessStatus, Request, Response, RunSpec,
    StopSignal,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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
        Command::Ps => print_response(Client::new().send(Request::ListProcesses).await),
        Command::Show { process } => print_response(
            Client::new()
                .send(Request::ShowProcess {
                    selector: process_selector(&process),
                })
                .await,
        ),
        Command::Logs(args) => {
            if args.follow {
                follow_logs(process_selector(&args.process), args.channel.into()).await
            } else {
                print_response(
                    Client::new()
                        .send(Request::ReadLogs {
                            selector: process_selector(&args.process),
                            stream: args.channel.into(),
                            after_id: None,
                        })
                        .await,
                )
            }
        }
    }
}

async fn wait_process(selector: ProcessSelector) -> Result<()> {
    let response = Client::new()
        .send(Request::WaitProcess { selector })
        .await?;

    match response {
        Response::WaitedProcess(process) => match process.status {
            ProcessStatus::Exited => std::process::exit(process.exit_code.unwrap_or(1)),
            ProcessStatus::Killed | ProcessStatus::Failed | ProcessStatus::TimedOut => {
                std::process::exit(1)
            }
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
    let mut env = config
        .env
        .into_iter()
        .map(|env| (env.key, env.value))
        .collect::<BTreeMap<_, _>>();

    for env_var in args
        .env
        .iter()
        .map(|value| parse_env_var(value))
        .collect::<Result<Vec<_>>>()?
    {
        env.insert(env_var.key, env_var.value);
    }

    let env = env
        .into_iter()
        .map(|(key, value)| EnvVar { key, value })
        .collect();

    Ok(RunSpec {
        name: args.name,
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

async fn follow_logs(selector: ProcessSelector, stream: OutputStream) -> Result<()> {
    let client = Client::new();
    let mut after_id = None;

    loop {
        let chunks = match client
            .send(Request::ReadLogs {
                selector: selector.clone(),
                stream,
                after_id,
            })
            .await?
        {
            Response::Output(chunks) => chunks,
            Response::Error { message } => bail!(message),
            _ => bail!("daemon returned an unexpected logs response"),
        };
        let printed_any = !chunks.is_empty();
        after_id = print_output(&chunks)?.or(after_id);

        let is_running = match client
            .send(Request::ShowProcess {
                selector: selector.clone(),
            })
            .await?
        {
            Response::ProcessDetails(process) => process.status == ProcessStatus::Running,
            Response::Error { message } => bail!(message),
            _ => bail!("daemon returned an unexpected process response"),
        };

        if !is_running && !printed_any {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Ok(())
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
        Response::Output(chunks) => {
            print_output(&chunks)?;
        }
        Response::Error { message } => bail!(message),
    }

    Ok(())
}

fn print_output(chunks: &[OutputChunk]) -> Result<Option<i64>> {
    let mut stdout = std::io::stdout().lock();
    let mut last_id = None;

    for chunk in chunks {
        stdout
            .write_all(&chunk.data)
            .context("failed to write output")?;
        stdout.flush().context("failed to flush output")?;
        last_id = Some(chunk.id);
    }

    Ok(last_id)
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
        "{:<3} {:<16} {:<9} {:<6} {:<5} COMMAND / ERROR",
        "ID", "NAME", "STATUS", "PID", "EXIT"
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
            "{:<3} {:<16} {:<9} {:<6} {:<5} {}",
            process.id,
            process.name.as_deref().unwrap_or("-"),
            process.status,
            pid,
            exit,
            command
        );
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
        };

        formatter.write_str(status)
    }
}

fn format_duration_ms(timeout_ms: u64) -> String {
    if timeout_ms % 3_600_000 == 0 {
        format!("{}h", timeout_ms / 3_600_000)
    } else if timeout_ms % 60_000 == 0 {
        format!("{}m", timeout_ms / 60_000)
    } else if timeout_ms % 1_000 == 0 {
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
