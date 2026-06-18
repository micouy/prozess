use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use super::daemon_state::DaemonState;
use super::service::Service;
use super::store::{Store, StoreConfig};
use super::supervisor::Supervisor;
use crate::client::Client;
use crate::protocol::{Request, Response};
use crate::runtime::RuntimePaths;

pub async fn start() -> Result<()> {
    if let Ok(Response::DaemonStatus {
        pid,
        socket,
        database,
    }) = Client::new().send(Request::DaemonStatus).await
    {
        println!("pz daemon already running");
        println!("pid: {pid}");
        println!("socket: {socket}");
        println!("db: {database}");
        return Ok(());
    }

    let paths = RuntimePaths::default();
    // A file, not a pipe: a pipe's read end dies with this process and
    // would EPIPE the daemon's later stderr writes.
    let stderr_path = paths.socket.with_file_name("daemon.stderr");
    create_runtime_dir(&paths.socket)?;
    let stderr_file = std::fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("failed to start pz daemon")?;

    for _ in 0..100 {
        if let Some(status) = child.try_wait().context("failed to check daemon startup")? {
            let detail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            let detail = detail.trim();
            if detail.is_empty() {
                bail!("pz daemon exited during startup with status {status}");
            }
            bail!("pz daemon exited during startup with status {status}:\n{detail}");
        }

        if let Ok(Response::DaemonStatus {
            pid,
            socket,
            database,
        }) = Client::new().send(Request::DaemonStatus).await
        {
            println!("pz daemon started");
            println!("pid: {pid}");
            println!("socket: {socket}");
            println!("db: {database}");
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    bail!(
        "timed out waiting for pz daemon socket at {}",
        paths.socket.display()
    )
}

pub async fn run() -> Result<()> {
    let paths = RuntimePaths::default();
    start_with_paths(paths.socket, paths.database).await
}

pub async fn start_with_paths(socket_path: PathBuf, database_path: PathBuf) -> Result<()> {
    prepare_socket(&socket_path).await?;
    let store = Store::open(StoreConfig { database_path })?;
    store.mark_running_processes_lost()?;
    let state = DaemonState::default();
    let service = Service::new(
        store,
        Supervisor::new(state.clone()),
        state,
        socket_path.display().to_string(),
    );

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {}", socket_path.display()))?;

    println!("pz daemon listening");
    println!("socket: {}", socket_path.display());

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("failed to accept client")?;
                let service = service.clone();
                let shutdown_tx = shutdown_tx.clone();

                tokio::spawn(async move {
                    match handle_connection(stream, &service).await {
                        Ok(true) => {
                            let _ = shutdown_tx.send(()).await;
                        }
                        Ok(false) => {}
                        Err(error) => eprintln!("client connection failed: {error}"),
                    }
                });
            }
        }
    }

    let _ = tokio::fs::remove_file(&socket_path).await;

    Ok(())
}

// Mode 0700 so the socket is not exposed to other users in a shared
// tmpdir. Existing directories are left as-is.
fn create_runtime_dir(socket_path: &Path) -> Result<()> {
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("failed to create runtime directory {}", parent.display()))
}

async fn prepare_socket(socket_path: &Path) -> Result<()> {
    create_runtime_dir(socket_path)?;

    if tokio::fs::try_exists(socket_path).await? {
        if UnixStream::connect(socket_path).await.is_ok() {
            bail!("pz daemon is already running at {}", socket_path.display());
        }

        tokio::fs::remove_file(socket_path)
            .await
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    Ok(())
}

async fn handle_connection(stream: UnixStream, service: &Service) -> Result<bool> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    reader
        .read_line(&mut line)
        .await
        .context("failed to read client request")?;

    let request: Request =
        serde_json::from_str(&line).context("failed to decode client request")?;
    let should_stop = matches!(request, Request::DaemonStop);
    let response = match service.handle(request).await {
        Ok(response) => response,
        Err(error) => Response::Error {
            message: error.to_string(),
        },
    };
    let response = serde_json::to_vec(&response).context("failed to encode response")?;

    writer
        .write_all(&response)
        .await
        .context("failed to send response")?;
    writer
        .write_all(b"\n")
        .await
        .context("failed to finish response")?;

    Ok(should_stop)
}

#[cfg(test)]
mod tests;
