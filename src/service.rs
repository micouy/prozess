use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::{
    daemon_state::{DaemonState, ProcessLifecycle},
    protocol::{
        PortInfo, PortList, ProcessDetails, ProcessStatus, Request, ResourceProcess,
        ResourceSnapshot, Response, StopSignal,
    },
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
            Request::Spawn { spec } => Response::Spawned(self.spawn_process(spec).await?),
            Request::StopProcess {
                selector,
                force,
                grace_ms,
            } => self.stop_process(&selector, force, grace_ms).await?,
            Request::SetTimeout {
                selector,
                timeout_ms,
            } => self.set_timeout(&selector, timeout_ms)?,
            Request::WaitProcess { selector } => {
                Response::WaitedProcess(self.wait_process(&selector).await?)
            }
            Request::RestartProcess { selector } => {
                Response::Spawned(self.restart_process(&selector).await?)
            }
            Request::Resources { selector } => {
                Response::ResourceSnapshot(self.resources(&selector)?)
            }
            Request::Ports { selector } => Response::PortList(self.ports(&selector)?),
            Request::ListProcesses { all } => Response::ProcessList(self.list_processes(all)?),
            Request::ShowProcess { selector } => Response::ProcessDetails(
                self.store
                    .get_process_details(self.store.resolve_process_id(&selector)?)?,
            ),
            Request::ReadLogs {
                selector,
                stream,
                after_id,
                since_ms,
                until_ms,
                tail_lines,
            } => {
                let (chunks, resume_after_id) = self.store.read_output(
                    self.store.resolve_process_id(&selector)?,
                    stream,
                    after_id,
                    since_ms,
                    until_ms,
                    tail_lines,
                )?;

                Response::Output {
                    chunks,
                    resume_after_id,
                }
            }
        };

        Ok(response)
    }

    async fn spawn_process(
        &self,
        spec: crate::protocol::RunSpec,
    ) -> Result<crate::protocol::ProcessSummary> {
        if spec.replace
            && let Some(name) = spec.name.as_deref()
        {
            self.clear_name(name).await?;
        }

        let timeout_ms = spec.timeout_ms;
        let process = self.supervisor.spawn(self.store.clone(), spec)?;

        if timeout_ms.is_some() {
            self.set_timeout_for_id(process.id, timeout_ms)?;
        }

        Ok(process)
    }

    /// Confirmed-kills every live holder of `name` — the running
    /// generation and any lost-but-alive ones.
    async fn clear_name(&self, name: &str) -> Result<()> {
        if let Some(id) = self.store.find_running_by_name(name)? {
            self.stop_process(&crate::protocol::ProcessSelector::Id(id), false, None)
                .await?;
        }

        for (id, pid, token) in self.store.lost_generations(name)? {
            if crate::pid_identity::is_alive(pid, token) {
                self.stop_process(&crate::protocol::ProcessSelector::Id(id), false, None)
                    .await?;
            }
        }

        Ok(())
    }

    fn list_processes(&self, all: bool) -> Result<Vec<crate::protocol::ProcessSummary>> {
        let mut processes = self.store.list_processes(!all)?;

        if !all {
            // A lost row whose pid no longer matches its identity token is
            // history with an unknown exit code, not a live process.
            let mut keep = Vec::with_capacity(processes.len());
            for process in processes {
                if process.status == ProcessStatus::Lost {
                    let token = self.store.pid_identity(process.id)?;
                    let alive = process
                        .pid
                        .is_some_and(|pid| crate::pid_identity::is_alive(pid, token));
                    if !alive {
                        continue;
                    }
                }
                keep.push(process);
            }
            processes = keep;
        }

        for process in &mut processes {
            if process.status != ProcessStatus::Running {
                continue;
            }

            let details = self.store.get_process_details(process.id)?;
            let ports = self.ports_for_details(&details)?;
            process.ports_unavailable = ports.is_none();
            process.ports = ports
                .unwrap_or_default()
                .into_iter()
                .map(|port| port.local_port)
                .collect();
            process.ports.sort_unstable();
            process.ports.dedup();
        }

        Ok(processes)
    }

    async fn restart_process(
        &self,
        selector: &crate::protocol::ProcessSelector,
    ) -> Result<crate::protocol::ProcessSummary> {
        let id = self.store.resolve_process_id(selector)?;
        let spec = self.store.restart_spec(id)?;
        let details = self.store.get_process_details(id)?;

        // Lost-but-alive included: restart is explicit intent to replace
        // it, and the successor needs its ports.
        if matches!(details.status, ProcessStatus::Running | ProcessStatus::Lost) {
            self.stop_process(selector, false, None).await?;
        }

        self.spawn_process(spec).await
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

    fn resources(&self, selector: &crate::protocol::ProcessSelector) -> Result<ResourceSnapshot> {
        let id = self.store.resolve_process_id(selector)?;
        let details = self.store.get_process_details(id)?;
        let Some(pgid) = details.pgid.or(details.pid) else {
            return Ok(empty_resource_snapshot(details));
        };

        if details.status != ProcessStatus::Running {
            return Ok(empty_resource_snapshot(details));
        }

        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let mut processes = Vec::new();

        for (pid, process) in system.processes() {
            let pid_u32 = pid.as_u32();
            // On Linux, sysinfo can surface task/thread entries. Threads share
            // their process address space, so count only thread-group leaders
            // to avoid summing the same RSS once per thread.
            if !is_thread_group_leader(pid_u32) {
                continue;
            }

            if process_group_id(pid_u32) != Some(pgid) {
                continue;
            }

            processes.push(ResourceProcess {
                pid: pid_u32,
                parent_pid: process.parent().map(Pid::as_u32),
                name: process.name().to_string_lossy().into_owned(),
                memory_bytes: process.memory(),
                cpu_percent: process.cpu_usage(),
            });
        }

        processes.sort_by_key(|process| process.pid);
        let total_memory_bytes = processes.iter().map(|process| process.memory_bytes).sum();
        let total_cpu_percent = processes.iter().map(|process| process.cpu_percent).sum();

        Ok(ResourceSnapshot {
            process_id: details.id,
            name: details.name,
            status: details.status,
            pid: details.pid,
            pgid: details.pgid,
            process_count: processes.len(),
            total_memory_bytes,
            total_cpu_percent,
            processes,
        })
    }

    fn ports(&self, selector: &crate::protocol::ProcessSelector) -> Result<PortList> {
        let id = self.store.resolve_process_id(selector)?;
        let details = self.store.get_process_details(id)?;
        if details.status != ProcessStatus::Running {
            return Ok(PortList {
                process_id: details.id,
                name: details.name,
                status: details.status,
                unavailable: false,
                ports: Vec::new(),
            });
        }

        let ports = self.ports_for_details(&details)?;
        let unavailable = ports.is_none();

        Ok(PortList {
            process_id: details.id,
            name: details.name,
            status: details.status,
            unavailable,
            ports: ports.unwrap_or_default(),
        })
    }

    fn ports_for_details(&self, details: &ProcessDetails) -> Result<Option<Vec<PortInfo>>> {
        if details.status != ProcessStatus::Running {
            return Ok(Some(Vec::new()));
        }

        let pids = self.process_group_pids(details)?;
        let sockets = match netstat2::get_sockets_info(
            AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
            ProtocolFlags::TCP,
        ) {
            Ok(sockets) => sockets,
            Err(error) => {
                eprintln!("port discovery unavailable: {error}");
                return Ok(None);
            }
        };
        let mut ports = Vec::new();

        for socket in sockets {
            let ProtocolSocketInfo::Tcp(tcp) = socket.protocol_socket_info else {
                continue;
            };
            if tcp.state != TcpState::Listen {
                continue;
            }

            let associated_pids = socket
                .associated_pids
                .into_iter()
                .filter(|pid| pids.contains(pid))
                .collect::<Vec<_>>();
            if associated_pids.is_empty() {
                continue;
            }

            ports.push(PortInfo {
                protocol: "tcp".to_owned(),
                state: "listen".to_owned(),
                local_addr: tcp.local_addr.to_string(),
                local_port: tcp.local_port,
                pids: associated_pids,
            });
        }

        ports.sort_by_key(|port| (port.local_port, port.local_addr.clone()));

        Ok(Some(ports))
    }

    fn process_group_pids(&self, details: &ProcessDetails) -> Result<Vec<u32>> {
        let Some(pgid) = details.pgid.or(details.pid) else {
            return Ok(Vec::new());
        };
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let mut pids = system
            .processes()
            .keys()
            .map(|pid| pid.as_u32())
            .filter(|pid| is_thread_group_leader(*pid))
            .filter(|pid| process_group_id(*pid) == Some(pgid))
            .collect::<Vec<_>>();

        pids.sort_unstable();
        Ok(pids)
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
                // Marked first so the reaper records timed_out, not exited.
                let _ = store.mark_process_timed_out(id);
                state.finish_process(id, None);
                // Detached: finish_process (ours above, or the reaper's
                // when a group member exits) aborts this timeout task, and
                // the escalation must survive that to kill the whole group.
                tokio::spawn(async move {
                    let _ = crate::terminate::kill_group_confirmed(
                        process.pgid,
                        false,
                        crate::terminate::DEFAULT_GRACE,
                    )
                    .await;
                });
            }
        });
        self.state.set_timeout(id, Some(timeout));

        Ok(())
    }

    async fn stop_process(
        &self,
        selector: &crate::protocol::ProcessSelector,
        force: bool,
        grace_ms: Option<u64>,
    ) -> Result<Response> {
        let id = self.store.resolve_process_id(selector)?;
        let process = self.store.get_process(id)?;
        if !matches!(process.status, ProcessStatus::Running | ProcessStatus::Lost) {
            anyhow::bail!("process {id} is not running (status: {})", process.status);
        }

        // A lost row's pgid may have been recycled since the previous
        // daemon generation recorded it; signaling it would hit an
        // unrelated process group. The identity token decides (see
        // DESIGN.md, "Pid identity"). Running rows need no check: the
        // daemon holds their unreaped children, pinning the pids.
        if process.status == ProcessStatus::Lost {
            let token = self.store.pid_identity(id)?;
            let alive = process
                .pid
                .is_some_and(|pid| crate::pid_identity::is_alive(pid, token));

            if !alive {
                self.store.mark_process_killed(id)?;
                return Ok(Response::StoppedProcess {
                    id,
                    signal: StopSignal::Term,
                });
            }
        }

        let pgid = self
            .state
            .process(id)
            .map(|process| process.pgid)
            .or(process.pgid)
            .or(process.pid)
            .context("process has no pid or process group to stop")?;
        let grace = grace_ms
            .map(Duration::from_millis)
            .unwrap_or(crate::terminate::DEFAULT_GRACE);

        let signal = crate::terminate::kill_group_confirmed(pgid, force, grace).await?;
        // Death is confirmed; killed overwrites the reaper's exited.
        self.store.mark_process_killed(id)?;

        Ok(Response::StoppedProcess { id, signal })
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

fn empty_resource_snapshot(details: ProcessDetails) -> ResourceSnapshot {
    ResourceSnapshot {
        process_id: details.id,
        name: details.name,
        status: details.status,
        pid: details.pid,
        pgid: details.pgid,
        process_count: 0,
        total_memory_bytes: 0,
        total_cpu_percent: 0.0,
        processes: Vec::new(),
    }
}

fn process_group_id(pid: u32) -> Option<u32> {
    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid as i32)))
        .ok()
        .and_then(|pgid| u32::try_from(pgid.as_raw()).ok())
}

fn is_thread_group_leader(pid: u32) -> bool {
    thread_group_id(pid).is_none_or(|tgid| tgid == pid)
}

#[cfg(target_os = "linux")]
fn thread_group_id(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("Tgid:")
            .and_then(|value| value.trim().parse().ok())
    })
}

#[cfg(not(target_os = "linux"))]
fn thread_group_id(_pid: u32) -> Option<u32> {
    None
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
