mod cli;
mod client;
mod protocol;
mod runtime;
mod server;
mod store;
mod supervisor;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command, DaemonCommand, LogStream};
use crate::client::Client;
use crate::protocol::{OutputStream, Request, Response};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Start => server::start().await,
            DaemonCommand::Status => {
                print_response(Client::new().send(Request::DaemonStatus).await)
            }
            DaemonCommand::Stop => print_response(Client::new().send(Request::DaemonStop).await),
        },
        Command::Run(args) => print_response(
            Client::new()
                .send(Request::Spawn {
                    command: args.command,
                })
                .await,
        ),
        Command::Ps => print_response(Client::new().send(Request::ListProcesses).await),
        Command::Show { id } => {
            print_response(Client::new().send(Request::ShowProcess { id }).await)
        }
        Command::Logs(args) => {
            if args.follow {
                println!("log following is not implemented yet");
            }

            print_response(
                Client::new()
                    .send(Request::ReadLogs {
                        id: args.id,
                        stream: args.stream.into(),
                    })
                    .await,
            )
        }
    }
}

fn print_response(response: Result<Response>) -> Result<()> {
    match response? {
        Response::DaemonStatus { socket, database } => {
            println!("pz daemon running");
            println!("socket: {socket}");
            println!("db: {database}");
        }
        Response::DaemonStopping => println!("pz daemon stopped"),
        Response::Spawned(process) => {
            println!("spawned process {}", process.id);
            println!("status: {}", process.status);
            println!("command: {}", process.command.join(" "));
        }
        Response::ProcessList(processes) => print_process_list(&processes),
        Response::NotImplemented { command } => println!("{command} is not implemented yet"),
    }

    Ok(())
}

fn print_process_list(processes: &[crate::protocol::ProcessSummary]) {
    println!(
        "{:<3} {:<8} {:<6} {:<5} COMMAND",
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

        println!(
            "{:<3} {:<8} {:<6} {:<5} {}",
            process.id,
            process.status,
            pid,
            exit,
            process.command.join(" ")
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
