use std::io::Write;

use clap::{CommandFactory, Parser, error::ErrorKind};

use pz::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pz::client::run(parse_cli()).await
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
