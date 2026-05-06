mod cli;
mod client;
mod config;
mod protocol;
mod runtime;
mod server;
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
    EnvVar, OutputChunk, OutputStream, ProcessStatus, Request, Response, RunSpec, StopSignal,
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
                    id: args.id,
                    force: args.force,
                })
                .await,
        ),
        Command::Ps => print_response(Client::new().send(Request::ListProcesses).await),
        Command::Show { id } => {
            print_response(Client::new().send(Request::ShowProcess { id }).await)
        }
        Command::Logs(args) => {
            if args.follow {
                follow_logs(args.id, args.channel.into()).await
            } else {
                print_response(
                    Client::new()
                        .send(Request::ReadLogs {
                            id: args.id,
                            stream: args.channel.into(),
                            after_id: None,
                        })
                        .await,
                )
            }
        }
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
        command: args.command,
        cwd: cwd.display().to_string(),
        inherit_env: config.run.inherit_env || args.inherit_env,
        env_files,
        env,
    })
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

async fn follow_logs(id: i64, stream: OutputStream) -> Result<()> {
    let client = Client::new();
    let mut after_id = None;

    loop {
        let chunks = match client
            .send(Request::ReadLogs {
                id,
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

        let is_running = match client.send(Request::ShowProcess { id }).await? {
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
            println!("status: {}", process.status);
            println!("command: {}", process.command.join(" "));
        }
        Response::StoppedProcess { id, signal } => {
            println!("stopped process {id}");
            println!("signal: {signal}");
        }
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
    println!("status: {}", process.status);
    println!("pid: {pid}");
    println!("pgid: {pgid}");
    println!("exit: {exit}");
    println!("command: {}", process.command.join(" "));
    println!("cwd: {}", process.cwd);
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
        "{:<3} {:<8} {:<6} {:<5} COMMAND / ERROR",
        "ID", "STATUS", "PID", "EXIT"
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
            "{:<3} {:<8} {:<6} {:<5} {}",
            process.id, process.status, pid, exit, command
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
        };

        formatter.write_str(status)
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
