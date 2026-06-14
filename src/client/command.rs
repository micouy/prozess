use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::{Cli, Command, DaemonCommand, RunArgs};
use crate::client::Client;
use crate::client::output::{OutputPrinter, print_response};
use crate::config::Config;
use crate::protocol::{EnvVar, ProcessSelector, ProcessStatus, Request, Response, RunSpec};

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Start => crate::daemon::start().await,
            DaemonCommand::Run => crate::daemon::run().await,
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
                crate::client::output::follow_logs(
                    selector, stream, args.tail, since_ms, args.all, printer,
                )
                .await
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
