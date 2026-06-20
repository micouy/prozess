use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::protocol::{OutputStream, ProcessSelector, Request, Response as PzResponse, RunSpec};

const INDEX_HTML: &str = include_str!("static/index.html");

pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    all: bool,
}

pub async fn list_processes(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let processes = match send(&state, Request::ListProcesses { all: query.all }).await? {
        PzResponse::ProcessList(processes) => processes,
        other => return Err(ApiError::unexpected(other)),
    };

    // Resources are a separate per-process request (each a system scan),
    // so fetch them only for running processes and merge into the row.
    let mut rows = Vec::with_capacity(processes.len());
    for process in processes {
        let mut row = serde_json::to_value(&process).unwrap();
        if process.status == crate::protocol::ProcessStatus::Running {
            if let Ok(PzResponse::ResourceSnapshot(snapshot)) = state
                .client
                .send(Request::Resources {
                    selector: ProcessSelector::Id(process.id),
                })
                .await
            {
                row["memory_bytes"] = snapshot.total_memory_bytes.into();
                row["cpu_percent"] = snapshot.total_cpu_percent.into();
            }
        }
        rows.push(row);
    }

    Ok(Json(serde_json::json!({ "processes": rows })))
}

pub async fn show_process(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match send(
        &state,
        Request::ShowProcess {
            selector: ProcessSelector::Id(id),
        },
    )
    .await?
    {
        PzResponse::ProcessDetails(details) => Ok(Json(serde_json::to_value(details).unwrap())),
        other => Err(ApiError::unexpected(other)),
    }
}

#[derive(Deserialize)]
pub struct LogsQuery {
    after_id: Option<i64>,
    tail: Option<u64>,
}

#[derive(Serialize)]
struct LogLine {
    process_id: i64,
    stream: OutputStream,
    text: String,
}

pub async fn logs(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request = Request::ReadLogs {
        selector: Some(ProcessSelector::Id(id)),
        stream: OutputStream::All,
        after_id: query.after_id,
        since_ms: None,
        until_ms: None,
        tail_lines: query.tail,
    };

    match send(&state, request).await? {
        PzResponse::Output {
            chunks,
            resume_after_id,
        } => {
            let lines = chunks
                .into_iter()
                .map(|chunk| LogLine {
                    process_id: chunk.process_id,
                    stream: chunk.stream,
                    text: String::from_utf8_lossy(&chunk.data).into_owned(),
                })
                .collect::<Vec<_>>();
            Ok(Json(serde_json::json!({
                "lines": lines,
                "resume_after_id": resume_after_id,
            })))
        }
        other => Err(ApiError::unexpected(other)),
    }
}

#[derive(Deserialize)]
pub struct StopBody {
    #[serde(default)]
    force: bool,
}

pub async fn stop(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<StopBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let force = body.map(|b| b.force).unwrap_or(false);
    match send(
        &state,
        Request::StopProcess {
            selector: ProcessSelector::Id(id),
            force,
            grace_ms: None,
        },
    )
    .await?
    {
        PzResponse::StoppedProcess { id, signal } => Ok(Json(serde_json::json!({
            "id": id,
            "signal": format!("{signal:?}"),
        }))),
        other => Err(ApiError::unexpected(other)),
    }
}

pub async fn restart(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match send(
        &state,
        Request::RestartProcess {
            selector: ProcessSelector::Id(id),
        },
    )
    .await?
    {
        PzResponse::Spawned(summary) => Ok(Json(serde_json::to_value(summary).unwrap())),
        other => Err(ApiError::unexpected(other)),
    }
}

#[derive(Deserialize)]
pub struct RunBody {
    name: Option<String>,
    command: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    replace: bool,
}

pub async fn run(
    State(state): State<AppState>,
    Json(body): Json<RunBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if body.command.is_empty() {
        return Err(ApiError::bad_request("command must not be empty"));
    }

    let cwd = body.cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });
    let spec = RunSpec {
        name: body.name,
        replace: body.replace,
        timeout_ms: None,
        command: body.command,
        cwd,
        inherit_env: false,
        env_files: Vec::new(),
        env: Vec::new(),
    };

    match send(&state, Request::Spawn { spec }).await? {
        PzResponse::Spawned(summary) => Ok(Json(serde_json::to_value(summary).unwrap())),
        other => Err(ApiError::unexpected(other)),
    }
}

/// Sends a request and turns a daemon-level `Error` response into a 400,
/// so handlers only match the responses they expect.
async fn send(state: &AppState, request: Request) -> Result<PzResponse, ApiError> {
    let response = state
        .client
        .send(request)
        .await
        .map_err(|error| ApiError::bad_gateway(error.to_string()))?;

    match response {
        PzResponse::Error { message } => Err(ApiError::bad_request(message)),
        other => Ok(other),
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }

    fn unexpected(response: PzResponse) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("unexpected daemon response: {response:?}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
