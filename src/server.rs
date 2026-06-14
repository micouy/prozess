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

use crate::client::Client;
use crate::daemon_state::DaemonState;
use crate::protocol::{Request, Response};
use crate::runtime::RuntimePaths;
use crate::service::Service;
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
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;
    use tokio::time::sleep;

    use super::*;
    use crate::client::Client;
    use crate::protocol::{EnvVar, ProcessSelector, RunSpec, StopSignal};

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
        store.insert_process(
            None,
            &["first".to_owned()],
            dir.path(),
            111,
            111,
            false,
            &[],
            &[],
        )?;
        store.insert_process(
            Some("worker"),
            &["second".to_owned()],
            dir.path(),
            222,
            222,
            false,
            &[],
            &[],
        )?;
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client.send(Request::ListProcesses { all: true }).await?;
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
        let response = client
            .send(Request::Spawn {
                spec: test_run_spec(command, dir.path()),
            })
            .await?;
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
    async fn daemon_records_pid_identity_on_spawn() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let command = vec!["/bin/sleep".to_owned(), "5".to_owned()];
        let response = client
            .send(Request::Spawn {
                spec: test_run_spec(command, dir.path()),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let token = store.pid_identity(process.id)?;
        assert!(token.is_some(), "spawn should record a pid identity token");
        assert!(crate::pid_identity::is_alive(
            process.pid.context("spawned process should have a pid")?,
            token,
        ));

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: true,
                grace_ms: None,
            })
            .await?;
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
        let response = client
            .send(Request::Spawn {
                spec: test_run_spec(command, dir.path()),
            })
            .await?;
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
                selector: Some(ProcessSelector::Id(process.id)),
                stream: crate::protocol::OutputStream::Stdout,
                after_id: None,
                since_ms: None,
                until_ms: None,
                tail_lines: None,
            })
            .await?;
        let Response::Output { chunks, .. } = response else {
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
    async fn daemon_waits_for_running_process_completion() -> Result<()> {
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
                spec: test_run_spec(
                    vec!["/usr/bin/env".to_owned(), "false".to_owned()],
                    dir.path(),
                ),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let response = client
            .send(Request::WaitProcess {
                selector: ProcessSelector::Id(process.id),
            })
            .await?;
        let Response::WaitedProcess(process) = response else {
            bail!("expected waited process response");
        };
        assert_eq!(process.status, crate::protocol::ProcessStatus::Exited);
        assert_eq!(process.exit_code, Some(1));

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_wait_returns_finished_process_immediately() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let process = store.insert_process(
            Some("finished"),
            &["true".to_owned()],
            dir.path(),
            111,
            111,
            false,
            &[],
            &[],
        )?;
        store.mark_process_finished(process.id, Some(0))?;
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client
            .send(Request::WaitProcess {
                selector: ProcessSelector::Name("finished".to_owned()),
            })
            .await?;
        let Response::WaitedProcess(process) = response else {
            bail!("expected waited process response");
        };
        assert_eq!(process.status, crate::protocol::ProcessStatus::Exited);
        assert_eq!(process.exit_code, Some(0));

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_applies_cwd_and_env_without_storing_values() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let env_file = dir.path().join("test.env");
        std::fs::write(&env_file, "FOO=file\nSECRET=file-secret\n")?;
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let spec = RunSpec {
            replace: false,
            name: Some("env-test".to_owned()),
            timeout_ms: None,
            command: vec!["/usr/bin/env".to_owned()],
            cwd: dir.path().display().to_string(),
            inherit_env: false,
            env_files: vec![env_file.display().to_string()],
            env: vec![EnvVar {
                key: "FOO".to_owned(),
                value: "inline".to_owned(),
            }],
        };
        let response = client.send(Request::Spawn { spec }).await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        wait_for_process_exit(&database, process.id, Some(0)).await?;
        let output =
            wait_for_any_output(&database, process.id, crate::protocol::OutputStream::Stdout)
                .await?;
        let output = String::from_utf8(output)?;
        assert!(output.contains("FOO=inline\n"));
        assert!(output.contains("SECRET=file-secret\n"));

        let store = Store::open(StoreConfig {
            database_path: database,
        })?;
        let details = store.get_process_details(process.id)?;
        assert!(!details.env.inherit_env);
        assert_eq!(details.env.env_files, vec![env_file.display().to_string()]);
        assert_eq!(details.env.env_keys, vec!["FOO"]);

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
                spec: test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path()),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let response = client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: false,
                grace_ms: None,
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
    async fn daemon_stop_escalates_stubborn_process_and_confirms_death() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let ready = dir.path().join("ready");
        let script = format!(
            "trap '' TERM; touch {}; while :; do sleep 0.1; done",
            ready.display()
        );
        let response = client
            .send(Request::Spawn {
                spec: test_run_spec(
                    vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
                    dir.path(),
                ),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };
        let pid = process.pid.context("process should have a pid")?;
        while !ready.exists() {
            sleep(Duration::from_millis(10)).await;
        }

        let response = client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: false,
                grace_ms: Some(300),
            })
            .await?;
        assert!(
            matches!(response, Response::StoppedProcess { id, signal: StopSignal::Kill } if id == process.id),
            "expected escalation to SIGKILL, got {response:?}"
        );
        // The response means confirmed death, immediately assertable
        // with the same liveness definition the primitive uses.
        assert!(!crate::terminate::group_alive(pid));

        let store = Store::open(StoreConfig {
            database_path: database,
        })?;
        assert_eq!(
            store.get_process(process.id)?.status,
            crate::protocol::ProcessStatus::Killed
        );

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_restart_confirms_death_before_respawn() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path());
        spec.name = Some("svc".to_owned());
        let Response::Spawned(old) = client.send(Request::Spawn { spec }).await? else {
            bail!("expected spawned response");
        };
        let old_pid = old.pid.context("old generation should have a pid")?;

        let Response::Spawned(new) = client
            .send(Request::RestartProcess {
                selector: ProcessSelector::Name("svc".to_owned()),
            })
            .await?
        else {
            bail!("expected spawned response");
        };
        // The old generation must be dead before its successor spawns.
        assert!(!crate::terminate::group_alive(old_pid));
        assert_ne!(new.pid, old.pid);

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(new.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_rejects_stopping_finished_process() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let Response::Spawned(process) = client
            .send(Request::Spawn {
                spec: test_run_spec(vec!["/bin/echo".to_owned(), "hi".to_owned()], dir.path()),
            })
            .await?
        else {
            bail!("expected spawned response");
        };
        wait_for_process_exit(&database, process.id, Some(0)).await?;

        let Response::Error { message } = client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: false,
                grace_ms: None,
            })
            .await?
        else {
            bail!("expected error response");
        };
        assert!(message.contains("is not running"), "{message}");

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_never_signals_a_recycled_lost_pgid() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");

        // A lost row whose pid now belongs to a stranger: this very test
        // process, with a token that cannot match it. If the daemon
        // signals the pgid anyway, the canary dies and the test aborts.
        let canary = std::process::id();
        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let id = store.reserve_process(
            Some("stale"),
            &["/bin/sleep".to_owned(), "30".to_owned()],
            dir.path(),
            false,
            &[],
            &[],
        )?;
        store.activate_process(id, canary, canary, Some(1))?;

        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());
        wait_for_socket(&socket).await?;

        let response = client
            .send(Request::StopProcess {
                selector: ProcessSelector::Name("stale".to_owned()),
                force: false,
                grace_ms: None,
            })
            .await?;
        assert!(
            matches!(response, Response::StoppedProcess { id: stopped, signal: StopSignal::Term } if stopped == id),
            "expected silent success, got {response:?}"
        );
        assert_eq!(
            store.get_process(id)?.status,
            crate::protocol::ProcessStatus::Killed
        );

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_times_out_running_process() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path());
        spec.timeout_ms = Some(50);
        let response = client.send(Request::Spawn { spec }).await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let response = client
            .send(Request::WaitProcess {
                selector: ProcessSelector::Id(process.id),
            })
            .await?;
        let Response::WaitedProcess(process) = response else {
            bail!("expected waited process response");
        };
        assert_eq!(process.status, crate::protocol::ProcessStatus::TimedOut);

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    // Slow (~5s): waits out the real TERM grace before the KILL lands.
    #[tokio::test]
    async fn daemon_timeout_escalates_stubborn_process() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let ready = dir.path().join("ready");
        let script = format!(
            "trap '' TERM; touch {}; while :; do sleep 0.1; done",
            ready.display()
        );
        let mut spec = test_run_spec(
            vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            dir.path(),
        );
        spec.timeout_ms = Some(100);
        let Response::Spawned(process) = client.send(Request::Spawn { spec }).await? else {
            bail!("expected spawned response");
        };
        let pid = process.pid.context("process should have a pid")?;
        while !ready.exists() {
            sleep(Duration::from_millis(10)).await;
        }

        let store = Store::open(StoreConfig {
            database_path: database,
        })?;
        // The status flips at the deadline, before the group is dead.
        for _ in 0..100 {
            if store.get_process(process.id)?.status == crate::protocol::ProcessStatus::TimedOut {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            store.get_process(process.id)?.status,
            crate::protocol::ProcessStatus::TimedOut
        );

        // TERM is ignored; the escalation must still end the group.
        for _ in 0..800 {
            if !crate::terminate::group_alive(pid) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !crate::terminate::group_alive(pid),
            "timed-out process survived escalation"
        );

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_can_set_and_clear_timeout() -> Result<()> {
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
                spec: test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path()),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let response = client
            .send(Request::SetTimeout {
                selector: ProcessSelector::Id(process.id),
                timeout_ms: Some(10_000),
            })
            .await?;
        assert!(
            matches!(response, Response::TimeoutUpdated { id, timeout_ms: Some(10_000) } if id == process.id)
        );

        let response = client
            .send(Request::SetTimeout {
                selector: ProcessSelector::Id(process.id),
                timeout_ms: None,
            })
            .await?;
        assert!(
            matches!(response, Response::TimeoutUpdated { id, timeout_ms: None } if id == process.id)
        );

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_reports_process_group_resources() -> Result<()> {
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
                spec: test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path()),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };
        let pid = process.pid.expect("spawned process should have pid");

        let response = client
            .send(Request::Resources {
                selector: ProcessSelector::Id(process.id),
            })
            .await?;
        let Response::ResourceSnapshot(resources) = response else {
            bail!("expected resource snapshot response");
        };
        assert_eq!(resources.process_id, process.id);
        assert_eq!(resources.status, crate::protocol::ProcessStatus::Running);
        assert!(resources.processes.iter().any(|process| process.pid == pid));

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_reports_listening_ports() -> Result<()> {
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
                spec: test_run_spec(
                    vec![
                        "/usr/bin/python3".to_owned(),
                        "-m".to_owned(),
                        "http.server".to_owned(),
                        "0".to_owned(),
                        "--bind".to_owned(),
                        "127.0.0.1".to_owned(),
                    ],
                    dir.path(),
                ),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };

        let ports = wait_for_ports(&client, process.id).await?;
        assert!(ports.iter().any(|port| port.local_addr == "127.0.0.1"));

        let response = client.send(Request::ListProcesses { all: true }).await?;
        let Response::ProcessList(processes) = response else {
            bail!("expected process list response");
        };
        let process = processes
            .into_iter()
            .find(|listed| listed.id == process.id)
            .context("spawned process should be listed")?;
        assert!(!process.ports.is_empty());

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_restarts_process_from_env_files() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let env_file = dir.path().join("restart.env");
        std::fs::write(&env_file, "RESTART_VALUE=from-file\n")?;
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/usr/bin/env".to_owned()], dir.path());
        spec.name = Some("restartable".to_owned());
        spec.env_files = vec![env_file.display().to_string()];
        let response = client.send(Request::Spawn { spec }).await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };
        wait_for_process_exit(&database, process.id, Some(0)).await?;

        let response = client
            .send(Request::RestartProcess {
                selector: ProcessSelector::Name("restartable".to_owned()),
            })
            .await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };
        assert_eq!(process.name, Some("restartable".to_owned()));
        wait_for_process_exit(&database, process.id, Some(0)).await?;
        let output =
            wait_for_any_output(&database, process.id, crate::protocol::OutputStream::Stdout)
                .await?;
        assert!(String::from_utf8(output)?.contains("RESTART_VALUE=from-file\n"));

        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_refuses_restart_with_inline_env_keys() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/usr/bin/env".to_owned()], dir.path());
        spec.name = Some("not-restartable".to_owned());
        spec.env = vec![EnvVar {
            key: "SECRET".to_owned(),
            value: "hidden".to_owned(),
        }];
        let response = client.send(Request::Spawn { spec }).await?;
        let Response::Spawned(process) = response else {
            bail!("expected spawned response");
        };
        wait_for_process_exit(&database, process.id, Some(0)).await?;

        let response = client
            .send(Request::RestartProcess {
                selector: ProcessSelector::Name("not-restartable".to_owned()),
            })
            .await?;
        assert!(
            matches!(response, Response::Error { message } if message.contains("inline env values were not stored"))
        );

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
                spec: test_run_spec(
                    vec!["/definitely/not/a/real/pz-test-command".to_owned()],
                    dir.path(),
                ),
            })
            .await?;
        assert!(
            matches!(response, Response::Error { message } if message.contains("failed to spawn"))
        );

        let response = client.send(Request::ListProcesses { all: true }).await?;
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
        let process = store.insert_process(
            Some("details"),
            &["echo".to_owned(), "hello".to_owned()],
            dir.path(),
            111,
            111,
            false,
            &[],
            &[],
        )?;
        let server = tokio::spawn(async move { start_with_paths(server_socket, database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let response = client
            .send(Request::ShowProcess {
                selector: ProcessSelector::Name("details".to_owned()),
            })
            .await?;
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
        let process = store.insert_process(
            Some("logs"),
            &["echo".to_owned()],
            dir.path(),
            111,
            111,
            false,
            &[],
            &[],
        )?;
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
                selector: Some(ProcessSelector::Name("logs".to_owned())),
                stream: crate::protocol::OutputStream::Stdout,
                after_id: None,
                since_ms: None,
                until_ms: None,
                tail_lines: None,
            })
            .await?;
        let Response::Output { chunks, .. } = response else {
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

    #[tokio::test]
    async fn daemon_replace_takes_over_running_name() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path());
        spec.name = Some("svc".to_owned());
        let Response::Spawned(old) = client.send(Request::Spawn { spec: spec.clone() }).await?
        else {
            bail!("expected spawned response");
        };
        let old_pid = old.pid.context("old generation should have a pid")?;

        spec.replace = true;
        let Response::Spawned(new) = client.send(Request::Spawn { spec }).await? else {
            bail!("expected spawned response");
        };
        // The old generation is confirmed dead before the successor spawns.
        assert!(!crate::terminate::group_alive(old_pid));
        assert_ne!(new.id, old.id);

        let store = Store::open(StoreConfig {
            database_path: database,
        })?;
        assert_eq!(
            store.get_process(old.id)?.status,
            crate::protocol::ProcessStatus::Killed
        );
        assert_eq!(
            store.get_process(new.id)?.status,
            crate::protocol::ProcessStatus::Running
        );

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(new.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_replace_reaps_lost_but_alive_generation() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");

        use std::os::unix::process::CommandExt;
        // Its own group, like a real pz-spawned orphan: the replace kill
        // targets the group, not the pid.
        let mut orphan = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .context("failed to spawn orphan")?;
        let orphan_pid = orphan.id();
        let token = crate::pid_identity::current_token(orphan_pid);
        assert!(token.is_some());

        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let id = store.reserve_process(
            Some("svc"),
            &["/bin/sleep".to_owned(), "30".to_owned()],
            dir.path(),
            false,
            &[],
            &[],
        )?;
        store.activate_process(id, orphan_pid, orphan_pid, token)?;

        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());
        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path());
        spec.name = Some("svc".to_owned());
        spec.replace = true;
        let Response::Spawned(new) = client.send(Request::Spawn { spec }).await? else {
            bail!("expected spawned response");
        };
        assert!(!crate::terminate::group_alive(orphan_pid));
        orphan.wait().context("failed to reap orphan")?;

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(new.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_replace_without_conflict_just_spawns() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database;
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "30".to_owned()], dir.path());
        spec.name = Some("svc".to_owned());
        spec.replace = true;
        let Response::Spawned(process) = client.send(Request::Spawn { spec }).await? else {
            bail!("expected spawned response");
        };

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_rejects_duplicate_running_name_without_leaking() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());

        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "5".to_owned()], dir.path());
        spec.name = Some("api".to_owned());
        let Response::Spawned(_) = client.send(Request::Spawn { spec: spec.clone() }).await? else {
            bail!("expected spawned response");
        };

        let Response::Error { message } = client.send(Request::Spawn { spec }).await? else {
            bail!("expected error response");
        };
        assert!(
            message.contains("a running process named \"api\" already exists"),
            "{message}"
        );

        // The conflict happened before anything spawned: exactly one row.
        let Response::ProcessList(processes) =
            client.send(Request::ListProcesses { all: true }).await?
        else {
            bail!("expected process list");
        };
        assert_eq!(processes.len(), 1);

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Name("api".to_owned()),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    #[tokio::test]
    async fn daemon_rejects_name_of_lost_but_alive_process() -> Result<()> {
        let dir = tempdir()?;
        let socket = dir.path().join("pz.sock");
        let database = dir.path().join("pz.sqlite");

        // A stand-in for the orphan of a previous daemon generation.
        let mut orphan = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .spawn()
            .context("failed to spawn orphan")?;
        let orphan_pid = orphan.id();
        let token = crate::pid_identity::current_token(orphan_pid);
        assert!(token.is_some());

        let store = Store::open(StoreConfig {
            database_path: database.clone(),
        })?;
        let id = store.reserve_process(
            Some("api"),
            &["/bin/sleep".to_owned(), "30".to_owned()],
            dir.path(),
            false,
            &[],
            &[],
        )?;
        store.activate_process(id, orphan_pid, orphan_pid, token)?;

        // Daemon startup marks it lost.
        let server_socket = socket.clone();
        let server_database = database.clone();
        let server =
            tokio::spawn(async move { start_with_paths(server_socket, server_database).await });
        let client = Client::for_socket(socket.clone());
        wait_for_socket(&socket).await?;

        let mut spec = test_run_spec(vec!["/bin/sleep".to_owned(), "5".to_owned()], dir.path());
        spec.name = Some("api".to_owned());
        let Response::Error { message } =
            client.send(Request::Spawn { spec: spec.clone() }).await?
        else {
            bail!("expected error response");
        };
        assert!(message.contains("lost but still running"), "{message}");
        assert!(message.contains(&orphan_pid.to_string()), "{message}");

        // Once the orphan dies the name is free again.
        orphan.kill().context("failed to kill orphan")?;
        orphan.wait().context("failed to reap orphan")?;
        let Response::Spawned(process) = client.send(Request::Spawn { spec }).await? else {
            bail!("expected spawned response");
        };

        client
            .send(Request::StopProcess {
                selector: ProcessSelector::Id(process.id),
                force: true,
                grace_ms: None,
            })
            .await?;
        client.send(Request::DaemonStop).await?;
        server.await??;

        Ok(())
    }

    fn test_run_spec(command: Vec<String>, cwd: &Path) -> RunSpec {
        RunSpec {
            name: None,
            replace: false,
            timeout_ms: None,
            command,
            cwd: cwd.display().to_string(),
            inherit_env: false,
            env_files: Vec::new(),
            env: Vec::new(),
        }
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
            let (chunks, _) = store.read_output(Some(id), stream, None, None, None, None)?;
            let output = chunks
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

    async fn wait_for_any_output(
        database: &Path,
        id: i64,
        stream: crate::protocol::OutputStream,
    ) -> Result<Vec<u8>> {
        let store = Store::open(StoreConfig {
            database_path: database.to_path_buf(),
        })?;

        for _ in 0..100 {
            let (chunks, _) = store.read_output(Some(id), stream, None, None, None, None)?;
            let output = chunks
                .into_iter()
                .flat_map(|chunk| chunk.data)
                .collect::<Vec<_>>();
            if !output.is_empty() {
                return Ok(output);
            }

            sleep(Duration::from_millis(10)).await;
        }

        bail!("process {id} did not produce output")
    }

    async fn wait_for_ports(client: &Client, id: i64) -> Result<Vec<crate::protocol::PortInfo>> {
        for _ in 0..100 {
            let response = client
                .send(Request::Ports {
                    selector: ProcessSelector::Id(id),
                })
                .await?;
            let Response::PortList(ports) = response else {
                bail!("expected port list response");
            };
            if !ports.ports.is_empty() {
                return Ok(ports.ports);
            }

            sleep(Duration::from_millis(50)).await;
        }

        bail!("process {id} did not expose listening ports")
    }
}
