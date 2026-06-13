use std::{collections::BTreeMap, path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::{io::AsyncReadExt, process::Command};

use crate::{
    daemon_state::{DaemonState, RuntimeProcessMetadata},
    protocol::{OutputStream, ProcessSummary, RunSpec},
    store::Store,
};

#[derive(Debug, Clone)]
pub struct Supervisor {
    state: DaemonState,
}

impl Supervisor {
    pub fn new(state: DaemonState) -> Self {
        Self { state }
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

        if let Some(name) = spec.name.as_deref() {
            ensure_name_not_lost_alive(&store, name)?;
        }
        // Reserved before spawning: a name conflict must fail while there
        // is still nothing to leak.
        let process_id = store.reserve_process(
            spec.name.as_deref(),
            &spec.command,
            &cwd,
            spec.inherit_env,
            &spec.env_files,
            &env_keys,
        )?;

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

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("failed to spawn {}: {error}", spec.command.join(" "));
                let _ = store.mark_spawn_failed(process_id, &message);
                bail!(message);
            }
        };
        let pid = match child.id() {
            Some(pid) => pid,
            None => {
                let message = "spawned process did not expose a pid";
                let _ = store.mark_spawn_failed(process_id, message);
                bail!(message);
            }
        };
        // Captured immediately so later liveness checks can tell this
        // process apart from an unrelated one that recycled its pid.
        let pid_started_at = crate::pid_identity::current_token(pid);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process = store.activate_process(process_id, pid, pid, pid_started_at)?;
        let state = self.state.clone();

        state.insert_process(RuntimeProcessMetadata {
            id: process_id,
            pid,
            pgid: pid,
        });

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
            state.finish_process(process_id, exit_code);
        });

        Ok(process)
    }
}

fn ensure_name_not_lost_alive(store: &Store, name: &str) -> Result<()> {
    for (_, pid, token) in store.lost_generations(name)? {
        if crate::pid_identity::is_alive(pid, token) {
            bail!(
                "process {name:?} is lost but still running (pid {pid}); \
                 stop it with `pz stop {name}` or pass --replace"
            );
        }
    }

    Ok(())
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

    // The config's [env] section is resolved here, at spawn time, so
    // restart reproduces it from the current config the same way it
    // re-reads env files.
    for env_var in crate::config::Config::load()?.env {
        env.insert(env_var.key, env_var.value);
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
