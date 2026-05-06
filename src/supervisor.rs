use std::{env, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::{io::AsyncReadExt, process::Command};

use crate::{
    protocol::{OutputStream, ProcessSummary},
    store::Store,
};

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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
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
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process = store.insert_process(&command, &cwd, pid, pid)?;
        let process_id = process.id;

        if let Some(stdout) = stdout {
            tokio::spawn(capture_output(
                store.clone(),
                process_id,
                OutputStream::Stdout,
                stdout,
            ));
        }

        if let Some(stderr) = stderr {
            tokio::spawn(capture_output(
                store.clone(),
                process_id,
                OutputStream::Stderr,
                stderr,
            ));
        }

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

async fn capture_output<R>(store: Store, process_id: i64, stream: OutputStream, mut reader: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0; 8192];

    loop {
        let bytes_read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };

        let _ = store.insert_output_chunk(process_id, stream, &buffer[..bytes_read]);
    }
}
