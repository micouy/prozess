use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::protocol::{Request, Response};
use crate::runtime::RuntimePaths;

#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
}

impl Client {
    pub fn new() -> Self {
        Self::for_socket(RuntimePaths::default().socket)
    }

    pub fn for_socket(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub async fn send(&self, request: Request) -> Result<Response> {
        send_to_socket(&self.socket_path, request).await
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn send_to_socket(socket_path: &Path, request: Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to pz daemon at {}",
            socket_path.display()
        )
    })?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut response = String::new();
    let request = serde_json::to_vec(&request).context("failed to encode request")?;

    writer
        .write_all(&request)
        .await
        .context("failed to send request")?;
    writer
        .write_all(b"\n")
        .await
        .context("failed to finish request")?;

    reader
        .read_line(&mut response)
        .await
        .context("failed to read daemon response")?;

    serde_json::from_str(&response).context("failed to decode daemon response")
}
