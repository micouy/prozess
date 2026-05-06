use std::{env, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::{protocol::ProcessSummary, store::Store};

#[derive(Debug, Default)]
pub struct Supervisor;

impl Supervisor {
    pub fn new() -> Self {
        Self
    }

    pub fn spawn(&self, store: Store, command: Vec<String>) -> Result<ProcessSummary> {
        let (program, args) = command.split_first().context("missing command to run")?;
        if program.is_empty() {
            bail!("missing command to run");
        }

        let cwd = env::current_dir().context("failed to get current directory")?;
        let spawn_result = Command::new(program)
            .args(args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                let message = format!("failed to spawn {}: {error}", command.join(" "));
                let _ = store.insert_failed_process(&command, &cwd, &message);
                bail!(message);
            }
        };
        let pid = child.id().context("spawned process did not expose a pid")?;
        let process = store.insert_process(&command, &cwd, pid)?;
        let process_id = process.id;

        tokio::spawn(async move {
            let exit_code = match child.wait().await {
                Ok(status) => status.code(),
                Err(_) => None,
            };

            let _ = store.mark_process_finished(process_id, exit_code);
        });

        Ok(process)
    }
}
