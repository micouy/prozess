use std::{collections::BTreeMap, path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::{io::AsyncReadExt, process::Command};

use crate::{
    protocol::{OutputStream, ProcessSummary, RunSpec},
    store::Store,
};

#[derive(Debug, Default)]
pub struct Supervisor;

impl Supervisor {
    pub fn new() -> Self {
        Self
    }

    pub fn spawn(&self, store: Store, spec: RunSpec) -> Result<ProcessSummary> {
        let (program, args) = spec
            .command
            .split_first()
            .context("missing command to run")?;
        if program.is_empty() {
            bail!("missing command to run");
        }

        let cwd = PathBuf::from(&spec.cwd);
        let env = effective_env(&spec)?;
        let env_keys = spec
            .env
            .iter()
            .map(|env| env.key.clone())
            .collect::<Vec<_>>();
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&cwd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let spawn_result = command.spawn();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                let message = format!("failed to spawn {}: {error}", spec.command.join(" "));
                let _ = store.insert_failed_process(
                    &spec.command,
                    &cwd,
                    &message,
                    spec.inherit_env,
                    &spec.env_files,
                    &env_keys,
                );
                bail!(message);
            }
        };
        let pid = child.id().context("spawned process did not expose a pid")?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process = store.insert_process(
            &spec.command,
            &cwd,
            pid,
            pid,
            spec.inherit_env,
            &spec.env_files,
            &env_keys,
        )?;
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

fn effective_env(spec: &RunSpec) -> Result<BTreeMap<String, String>> {
    let mut env = if spec.inherit_env {
        std::env::vars().collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };

    for env_file in &spec.env_files {
        for (key, value) in read_env_file(env_file)? {
            env.insert(key, value);
        }
    }

    for env_var in &spec.env {
        env.insert(env_var.key.clone(), env_var.value.clone());
    }

    Ok(env)
}

fn read_env_file(path: &str) -> Result<Vec<(String, String)>> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("failed to read env file {path}"))?;
    let mut values = Vec::new();

    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            bail!(
                "invalid env file line {} in {path}: expected KEY=VALUE",
                index + 1
            );
        };
        if key.is_empty() || key.contains('\0') {
            bail!("invalid env key on line {} in {path}", index + 1);
        }

        values.push((key.to_owned(), value.to_owned()));
    }

    Ok(values)
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
