use anyhow::{Context, Result};

use crate::{
    daemon_state::{DaemonState, ProcessLifecycle},
    protocol::{ProcessDetails, ProcessStatus, Request, Response, StopSignal},
    store::Store,
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
            Request::Spawn { spec } => {
                Response::Spawned(self.supervisor.spawn(self.store.clone(), spec)?)
            }
            Request::StopProcess { selector, force } => self.stop_process(&selector, force)?,
            Request::WaitProcess { selector } => {
                Response::WaitedProcess(self.wait_process(&selector).await?)
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
