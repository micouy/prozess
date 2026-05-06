#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    DaemonStatus,
    DaemonStop,
    Spawn { command: Vec<String> },
    ListProcesses,
    ShowProcess { id: i64 },
    ReadLogs { id: i64, stream: OutputStream },
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DaemonStatus => "daemon status",
            Self::DaemonStop => "daemon stop",
            Self::Spawn { .. } => "run",
            Self::ListProcesses => "ps",
            Self::ShowProcess { .. } => "show",
            Self::ReadLogs { .. } => "logs",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    DaemonStatus { socket: String, database: String },
    DaemonStopping,
    Spawned(ProcessSummary),
    ProcessList(Vec<ProcessSummary>),
    NotImplemented { command: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OutputStream {
    All,
    Stdout,
    Stderr,
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
    pub exit_code: Option<i32>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessDetails {
    pub id: i64,
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub command: Vec<String>,
    pub cwd: String,
}
