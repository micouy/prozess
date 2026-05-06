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
    ListProcesses,
    ShowProcess {
        selector: ProcessSelector,
    },
    ReadLogs {
        selector: ProcessSelector,
        stream: OutputStream,
        after_id: Option<i64>,
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
            Self::ListProcesses => "ps",
            Self::ShowProcess { .. } => "show",
            Self::ReadLogs { .. } => "logs",
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
    Output(Vec<OutputChunk>),
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
