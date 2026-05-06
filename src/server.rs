use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::client::Client;
use crate::protocol::{Request, Response, StopSignal};
use crate::runtime::RuntimePaths;
use crate::store::{Store, StoreConfig};
use crate::supervisor::Supervisor;

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
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start pz daemon")?;

    for _ in 0..100 {
        if let Some(status) = child.try_wait().context("failed to check daemon startup")? {
            bail!("pz daemon exited during startup with status {status}");
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
    let supervisor = Supervisor::new();

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {}", socket_path.display()))?;

    println!("pz daemon listening");
    println!("socket: {}", socket_path.display());

    loop {
        let (stream, _) = listener.accept().await.context("failed to accept client")?;
        let should_stop = handle_connection(stream, &socket_path, &store, &supervisor).await?;

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

async fn handle_connection(
    stream: UnixStream,
    socket_path: &Path,
    store: &Store,
    supervisor: &Supervisor,
) -> Result<bool> {
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
    let response = match response_for(request, socket_path, store, supervisor) {
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

fn response_for(
    request: Request,
    socket_path: &Path,
    store: &Store,
    supervisor: &Supervisor,
) -> Result<Response> {
    let response = match request {
        Request::DaemonStatus => Response::DaemonStatus {
            pid: std::process::id(),
            socket: socket_path.display().to_string(),
            database: store.database_path().display().to_string(),
        },
        Request::DaemonStop => Response::DaemonStopping,
        Request::Spawn { command } => Response::Spawned(supervisor.spawn(store.clone(), command)?),
        Request::StopProcess { id, force } => stop_process(store, id, force)?,
        Request::ListProcesses => Response::ProcessList(store.list_processes()?),
        Request::ShowProcess { id } => Response::ProcessDetails(store.get_process_details(id)?),
        Request::ReadLogs {
            id,
            stream,
            after_id,
        } => Response::Output(store.read_output(id, stream, after_id)?),
    };

    Ok(response)
}

fn stop_process(store: &Store, id: i64, force: bool) -> Result<Response> {
    let process = store.get_process(id)?;
    let pid = process.pid.context("process has no pid to stop")?;
    let signal = if force {
        nix::sys::signal::Signal::SIGKILL
    } else {
        nix::sys::signal::Signal::SIGTERM
    };

    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal)
        .with_context(|| format!("failed to send {signal} to pid {pid}"))?;
    store.mark_process_killed(id)?;

    Ok(Response::StoppedProcess {
        id,
        signal: if force {
            StopSignal::Kill
        } else {
            StopSignal::Term
        },
    })
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
        let database = dir.path().join("pz.sqlite");
        let expected_database = database.display().to_string();
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let status = client.send(Request::DaemonStatus).await?;
        assert!(
            matches!(status, Response::DaemonStatus { socket: ref path, database: ref db, .. } if path == &socket.display().to_string() && db == &expected_database)
        );

        let stopping = client.send(Request::DaemonStop).await?;
        assert!(matches!(stopping, Response::DaemonStopping));

        server.await??;
        assert!(!socket.exists());

        Ok(())
    }

    #[tokio::test]
    async fn daemon_lists_processes() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let server_socket = socket.clone();
        let database = dir.path().join("pz.sqlite");
        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        store.insert_process(&["first".to_owned()], dir.path(), 111)?;
        store.insert_process(&["second".to_owned()], dir.path(), 222)?;
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client.send(Request::ListProcesses).await?;
        let Response::ProcessList(processes) = response else {
            bail!("expected process list response");
        };
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].id, 2);
        assert_eq!(processes[0].pid, Some(222));

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_spawns_process_and_records_exit() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let command = vec!["/usr/bin/env".to_owned(), "false".to_owned()];
        let response = client.send(Request::Spawn { command }).await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };
        assert_eq!(process.id, 1);
        assert_eq!(process.status, crate::protocol::ProcessStatus::Running);

        wait_for_process_exit(&database, process.id, Some(1)).await?;

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_spawns_process_and_captures_stdout() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let command = vec!["/bin/echo".to_owned(), "hello".to_owned()];
        let response = client.send(Request::Spawn { command }).await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        wait_for_process_exit(&database, process.id, Some(0)).await?;
        wait_for_output(
            &database,
            process.id,
            crate::protocol::OutputStream::Stdout,
            b"hello\n",
        )
        .await?;

        let response = client
            .send(Request::ReadLogs {
                id: process.id,
                stream: crate::protocol::OutputStream::Stdout,
                after_id: None,
            })
            .await?;
        let Response::Output(chunks) = response else {
            bail!("expected output response");
        };
        let output = chunks
            .into_iter()
            .flat_map(|chunk| chunk.data)
            .collect::<Vec<_>>();
        assert_eq!(output, b"hello\n");

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_stops_process_with_sigterm() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client
            .send(Request::Spawn {
                command: vec!["/bin/sleep".to_owned(), "30".to_owned()],
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let response = client
            .send(Request::StopProcess {
                id: process.id,
                force: false,
            })
            .await?;
        assert!(
            matches!(response, Response::StoppedProcess { id, signal: StopSignal::Term } if id == process.id)
        );

        let store = Store::open(StoreConfig {
            database_path: database,
        })?;
        let process = store.get_process(process.id)?;
        assert_eq!(process.status, crate::protocol::ProcessStatus::Killed);

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_survives_failed_spawn() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client
            .send(Request::Spawn {
                command: vec!["/definitely/not/a/real/pz-test-command".to_owned()],
            })
            .await?;
        assert!(
            matches!(response, Response::Error { message } if message.contains("failed to spawn"))
        );

        let response = client.send(Request::ListProcesses).await?;
        let Response::ProcessList(processes) = response else {
            bail!("expected process list response");
        };
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].status, crate::protocol::ProcessStatus::Failed);
        assert!(processes[0].error_message.is_some());

        let status = client.send(Request::DaemonStatus).await?;
        assert!(matches!(status, Response::DaemonStatus { .. }));

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_shows_process_details() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let server_socket = socket.clone();
        let database = dir.path().join("pz.sqlite");
        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let process =
            store.insert_process(&["echo".to_owned(), "hello".to_owned()], dir.path(), 111)?;
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client.send(Request::ShowProcess { id: process.id }).await?;
        let Response::ProcessDetails(details) = response else {
            bail!("expected process details response");
        };
        assert_eq!(details.id, process.id);
        assert_eq!(details.pid, Some(111));
        assert_eq!(details.cwd, dir.path().display().to_string());

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_reads_process_logs() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let server_socket = socket.clone();
        let database = dir.path().join("pz.sqlite");
        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let process = store.insert_process(&["echo".to_owned()], dir.path(), 111)?;
        store.insert_output_chunk(
            process.id,
            crate::protocol::OutputStream::Stdout,
            b"hello\n",
        )?;
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client
            .send(Request::ReadLogs {
                id: process.id,
                stream: crate::protocol::OutputStream::Stdout,
                after_id: None,
            })
            .await?;
        let Response::Output(chunks) = response else {
            bail!("expected output response");
        };
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, b"hello\n");

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

    async fn wait_for_process_exit(
        database: &Path,
        id: i64,
        expected_exit_code: Option<i32>,
    ) -> Result<()> {
        let store = Store::open(StoreConfig {
            database_path: database.to_path_buf(),
        })?;

        for _ in 0..100 {
            let process = store.get_process(id)?;
            if process.status == crate::protocol::ProcessStatus::Exited {
                assert_eq!(process.exit_code, expected_exit_code);
                return Ok(());
            }

            sleep(Duration::from_millis(10)).await;
        }

        bail!("process {id} did not exit")
    }

    async fn wait_for_output(
        database: &Path,
        id: i64,
        stream: crate::protocol::OutputStream,
        expected: &[u8],
    ) -> Result<()> {
        let store = Store::open(StoreConfig {
            database_path: database.to_path_buf(),
        })?;

        for _ in 0..100 {
            let output = store
                .read_output(id, stream, None)?
                .into_iter()
                .flat_map(|chunk| chunk.data)
                .collect::<Vec<_>>();
            if output == expected {
                return Ok(());
            }

            sleep(Duration::from_millis(10)).await;
        }

        bail!("process {id} did not produce expected output")
    }
}
