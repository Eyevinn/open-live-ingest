//! Local control API. Loopback by default; a non-loopback bind requires a token,
//! which `config::validate` enforces at startup rather than at first request.

mod routes;

use crate::state::SharedState;
use anyhow::{Context, Result};
use open_live_gateway_types::config::ControlConfig;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

pub async fn serve(state: Arc<SharedState>, cfg: &ControlConfig) -> Result<()> {
    let addr: SocketAddr = cfg.bind.parse().context("parsing control bind address")?;
    let app = routes::router(state, cfg.token.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding control API to {addr}"))?;
    info!(%addr, "control API listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("control API server error")
}

/// Shuts down on SIGINT/SIGTERM so systemd restarts are clean.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
