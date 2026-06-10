#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    DaemonStatus,
    DaemonStop,
    Spawn {
        spec: RunSpec,
    },
    StopProcess {
        selector: ProcessSelector,
        force: bool,
    },
    SetTimeout {
        selector: ProcessSelector,
        timeout_ms: Option<u64>,
    },
    WaitProcess {
        selector: ProcessSelector,
    },
    RestartProcess {
        selector: ProcessSelector,
    },
    Resources {
        selector: ProcessSelector,
    },
    Ports {
        selector: ProcessSelector,
    },
    ListProcesses,
    ShowProcess {
        selector: ProcessSelector,
    },
    ReadLogs {
        selector: ProcessSelector,
        stream: OutputStream,
        after_id: Option<i64>,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    },
    /// Returns the `after_id` cursor from which reading covers (at least)
    /// the last `tail_lines` lines. `tail_lines: 0` points past everything
    /// stored so far, i.e. "follow from now".
    LogCursor {
        selector: ProcessSelector,
        stream: OutputStream,
        tail_lines: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessSelector {
    Id(i64),
    Name(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    pub name: Option<String>,
    pub timeout_ms: Option<u64>,
    pub command: Vec<String>,
    pub cwd: String,
    pub inherit_env: bool,
    pub env_files: Vec<String>,
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEnvSummary {
    pub inherit_env: bool,
    pub env_files: Vec<String>,
    pub env_keys: Vec<String>,
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DaemonStatus => "daemon status",
            Self::DaemonStop => "daemon stop",
            Self::Spawn { .. } => "run",
            Self::StopProcess { .. } => "stop",
            Self::SetTimeout { .. } => "timeout",
            Self::WaitProcess { .. } => "wait",
            Self::RestartProcess { .. } => "restart",
            Self::Resources { .. } => "resources",
            Self::Ports { .. } => "ports",
            Self::ListProcesses => "ps",
            Self::ShowProcess { .. } => "show",
            Self::ReadLogs { .. } => "logs",
            Self::LogCursor { .. } => "logs",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    DaemonStatus {
        pid: u32,
        socket: String,
        database: String,
    },
    DaemonStopping,
    Spawned(ProcessSummary),
    StoppedProcess {
        id: i64,
        signal: StopSignal,
    },
    TimeoutUpdated {
        id: i64,
        timeout_ms: Option<u64>,
    },
    WaitedProcess(ProcessDetails),
    ProcessList(Vec<ProcessSummary>),
    ProcessDetails(ProcessDetails),
    ResourceSnapshot(ResourceSnapshot),
    PortList(PortList),
    Output(Vec<OutputChunk>),
    LogCursor {
        after_id: Option<i64>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum StopSignal {
    Term,
    Kill,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OutputStream {
    All,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputChunk {
    pub id: i64,
    pub stream: OutputStream,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessStatus {
    Running,
    Exited,
    Failed,
    Killed,
    TimedOut,
    Lost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub id: i64,
    pub name: Option<String>,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error_message: Option<String>,
    pub timeout_ms: Option<u64>,
    pub timeout_at_ms: Option<i64>,
    pub ports_unavailable: bool,
    pub ports: Vec<u16>,
    pub command: Vec<String>,
    pub env: ProcessEnvSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessDetails {
    pub id: i64,
    pub name: Option<String>,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error_message: Option<String>,
    pub timeout_ms: Option<u64>,
    pub timeout_at_ms: Option<i64>,
    pub command: Vec<String>,
    pub cwd: String,
    pub env: ProcessEnvSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub process_id: i64,
    pub name: Option<String>,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub process_count: usize,
    pub total_memory_bytes: u64,
    pub total_cpu_percent: f32,
    pub processes: Vec<ResourceProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceProcess {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortList {
    pub process_id: i64,
    pub name: Option<String>,
    pub status: ProcessStatus,
    pub unavailable: bool,
    pub ports: Vec<PortInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortInfo {
    pub protocol: String,
    pub state: String,
    pub local_addr: String,
    pub local_port: u16,
    pub pids: Vec<u32>,
}
