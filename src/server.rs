use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::protocol::{Request, Response};
use crate::runtime::RuntimePaths;

pub async fn start() -> Result<()> {
    let paths = RuntimePaths::default();
    start_with_socket(paths.socket).await
}

pub async fn start_with_socket(socket_path: PathBuf) -> Result<()> {
    prepare_socket(&socket_path).await?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {}", socket_path.display()))?;

    println!("pz daemon listening");
    println!("socket: {}", socket_path.display());

    loop {
        let (stream, _) = listener.accept().await.context("failed to accept client")?;
        let should_stop = handle_connection(stream, &socket_path).await?;

        if should_stop {
            break;
        }
    }

    let _ = tokio::fs::remove_file(&socket_path).await;

    Ok(())
}

async fn prepare_socket(socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create runtime directory {}", parent.display()))?;
    }

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

async fn handle_connection(stream: UnixStream, socket_path: &Path) -> Result<bool> {
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
    let response = response_for(request, socket_path);
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

fn response_for(request: Request, socket_path: &Path) -> Response {
    match request {
        Request::DaemonStatus => Response::DaemonStatus {
            socket: socket_path.display().to_string(),
        },
        Request::DaemonStop => Response::DaemonStopping,
        other => Response::NotImplemented {
            command: other.name().to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;
    use tokio::time::sleep;

    use super::*;
    use crate::client::Client;

    #[tokio::test]
    async fn daemon_reports_status_and_stops() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { start_with_socket(server_socket).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let status = client.send(Request::DaemonStatus).await?;
        assert!(
            matches!(status, Response::DaemonStatus { socket: ref path } if path == &socket.display().to_string())
        );

        let stopping = client.send(Request::DaemonStop).await?;
        assert!(matches!(stopping, Response::DaemonStopping));

        server.await??;
        assert!(!socket.exists());

        Ok(())
    }

    #[tokio::test]
    async fn daemon_returns_not_implemented_for_future_requests() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { start_with_socket(server_socket).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client.send(Request::ListProcesses).await?;
        assert!(matches!(response, Response::NotImplemented { command } if command == "ps"));

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    async fn wait_for_socket(socket: &Path) -> Result<()> {
        for _ in 0..100 {
            if socket.exists() {
                return Ok(());
            }

            sleep(Duration::from_millis(10)).await;
        }

        bail!("daemon socket was not created at {}", socket.display())
    }
}
