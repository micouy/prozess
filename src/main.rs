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
        Response::NotImplemented { command } => println!("{command} is not implemented yet"),
    }

    Ok(())
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
