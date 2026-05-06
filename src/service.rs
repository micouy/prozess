use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::{
    daemon_state::{DaemonState, ProcessLifecycle},
    protocol::{ProcessDetails, ProcessStatus, Request, Response, StopSignal},
    store::{Store, TimeoutSpec},
    supervisor::Supervisor,
};

#[derive(Debug, Clone)]
pub struct Service {
    store: Store,
    supervisor: Supervisor,
    state: DaemonState,
    socket_path: String,
}

impl Service {
    pub fn new(
        store: Store,
        supervisor: Supervisor,
        state: DaemonState,
        socket_path: String,
    ) -> Self {
        Self {
            store,
            supervisor,
            state,
            socket_path,
        }
    }

    pub async fn handle(&self, request: Request) -> Result<Response> {
        let response = match request {
            Request::DaemonStatus => Response::DaemonStatus {
                pid: std::process::id(),
                socket: self.socket_path.clone(),
                database: self.store.database_path().display().to_string(),
            },
            Request::DaemonStop => Response::DaemonStopping,
            Request::Spawn { spec } => Response::Spawned(self.spawn_process(spec)?),
            Request::StopProcess { selector, force } => self.stop_process(&selector, force)?,
            Request::SetTimeout {
                selector,
                timeout_ms,
            } => self.set_timeout(&selector, timeout_ms)?,
            Request::WaitProcess { selector } => {
                Response::WaitedProcess(self.wait_process(&selector).await?)
            }
            Request::RestartProcess { selector } => {
                Response::Spawned(self.restart_process(&selector)?)
            }
            Request::ListProcesses => Response::ProcessList(self.store.list_processes()?),
            Request::ShowProcess { selector } => Response::ProcessDetails(
                self.store
                    .get_process_details(self.store.resolve_process_id(&selector)?)?,
            ),
            Request::ReadLogs {
                selector,
                stream,
                after_id,
            } => Response::Output(self.store.read_output(
                self.store.resolve_process_id(&selector)?,
                stream,
                after_id,
            )?),
        };

        Ok(response)
    }

    fn spawn_process(
        &self,
        spec: crate::protocol::RunSpec,
    ) -> Result<crate::protocol::ProcessSummary> {
        let timeout_ms = spec.timeout_ms;
        let process = self.supervisor.spawn(self.store.clone(), spec)?;

        if timeout_ms.is_some() {
            self.set_timeout_for_id(process.id, timeout_ms)?;
        }

        Ok(process)
    }

    fn restart_process(
        &self,
        selector: &crate::protocol::ProcessSelector,
    ) -> Result<crate::protocol::ProcessSummary> {
        let id = self.store.resolve_process_id(selector)?;
        let spec = self.store.restart_spec(id)?;
        let details = self.store.get_process_details(id)?;

        if details.status == ProcessStatus::Running {
            self.stop_process(selector, false)?;
        }

        self.spawn_process(spec)
    }

    fn set_timeout(
        &self,
        selector: &crate::protocol::ProcessSelector,
        timeout_ms: Option<u64>,
    ) -> Result<Response> {
        let id = self.store.resolve_process_id(selector)?;
        self.set_timeout_for_id(id, timeout_ms)?;

        Ok(Response::TimeoutUpdated { id, timeout_ms })
    }

    fn set_timeout_for_id(&self, id: i64, timeout_ms: Option<u64>) -> Result<()> {
        let timeout = timeout_ms
            .map(|duration_ms| {
                now_ms().and_then(|now| {
                    Ok(TimeoutSpec {
                        duration_ms,
                        deadline_ms: checked_i64_add(now, duration_ms)?,
                    })
                })
            })
            .transpose()?;
        self.store.set_timeout(id, timeout)?;

        let Some(timeout) = timeout else {
            self.state.set_timeout(id, None);
            return Ok(());
        };

        let store = self.store.clone();
        let state = self.state.clone();
        let timeout = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout.duration_ms)).await;

            if let Some(process) = state.process(id) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(-(process.pgid as i32)),
                    nix::sys::signal::Signal::SIGTERM,
                );
                let _ = store.mark_process_timed_out(id);
                state.finish_process(id, None);
            }
        });
        self.state.set_timeout(id, Some(timeout));

        Ok(())
    }

    fn stop_process(
        &self,
        selector: &crate::protocol::ProcessSelector,
        force: bool,
    ) -> Result<Response> {
        let id = self.store.resolve_process_id(selector)?;
        let process = self.store.get_process(id)?;
        let pgid = self
            .state
            .process(id)
            .map(|process| process.pgid)
            .or(process.pgid)
            .or(process.pid)
            .context("process has no pid or process group to stop")?;
        let signal = if force {
            nix::sys::signal::Signal::SIGKILL
        } else {
            nix::sys::signal::Signal::SIGTERM
        };

        nix::sys::signal::kill(nix::unistd::Pid::from_raw(-(pgid as i32)), signal)
            .with_context(|| format!("failed to send {signal} to process group {pgid}"))?;
        self.store.mark_process_killed(id)?;

        Ok(Response::StoppedProcess {
            id,
            signal: if force {
                StopSignal::Kill
            } else {
                StopSignal::Term
            },
        })
    }

    async fn wait_process(
        &self,
        selector: &crate::protocol::ProcessSelector,
    ) -> Result<ProcessDetails> {
        let id = self.store.resolve_process_id(selector)?;
        let details = self.store.get_process_details(id)?;
        if details.status != ProcessStatus::Running {
            return Ok(details);
        }

        if let Some(mut lifecycle) = self.state.subscribe(id) {
            loop {
                if matches!(&*lifecycle.borrow(), ProcessLifecycle::Finished { .. }) {
                    break;
                }

                if lifecycle.changed().await.is_err() {
                    break;
                }
            }
        }

        self.store.get_process_details(id)
    }
}

fn now_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;

    i64::try_from(duration.as_millis()).context("current timestamp does not fit in i64")
}

fn checked_i64_add(value: i64, add_ms: u64) -> Result<i64> {
    let add_ms = i64::try_from(add_ms).context("timeout duration does not fit in i64")?;
    value
        .checked_add(add_ms)
        .context("timeout timestamp overflowed")
}
