#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    DaemonStatus,
    DaemonStop,
    Spawn {
        command: Vec<String>,
    },
    StopProcess {
        id: i64,
        force: bool,
    },
    ListProcesses,
    ShowProcess {
        id: i64,
    },
    ReadLogs {
        id: i64,
        stream: OutputStream,
        after_id: Option<i64>,
    },
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DaemonStatus => "daemon status",
            Self::DaemonStop => "daemon stop",
            Self::Spawn { .. } => "run",
            Self::StopProcess { .. } => "stop",
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub id: i64,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error_message: Option<String>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessDetails {
    pub id: i64,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error_message: Option<String>,
    pub command: Vec<String>,
    pub cwd: String,
}
