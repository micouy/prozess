//! HTTP interface to the daemon. A `pz serve` process is a daemon client
//! (see WEB_NOTES D3): every request becomes a `protocol::Request` sent
//! over the socket via `client::Client`. It never touches daemon internals.

mod api;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{get, post};

use crate::client::Client;

#[derive(Clone)]
struct AppState {
    client: Client,
}

pub async fn serve(host: String, port: u16) -> Result<()> {
    // Fail fast if no daemon is listening, with a clear message.
    let client = Client::new();
    client
        .send(crate::protocol::Request::DaemonStatus)
        .await
        .context("cannot reach the pz daemon; start it with `pz daemon start`")?;

    let state = AppState { client };
    let app = router(state);

    let addr = format!("{host}:{port}")
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid serve address {host}:{port}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!("pz web listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .context("web server error")?;

    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(api::health))
        .route("/api/processes", get(api::list_processes))
        .route("/api/processes/{id}", get(api::show_process))
        .route("/api/processes/{id}/logs", get(api::logs))
        .route("/api/processes/{id}/stop", post(api::stop))
        .route("/api/processes/{id}/restart", post(api::restart))
        .route("/api/run", post(api::run))
        .fallback(api::index)
        .with_state(state)
}
