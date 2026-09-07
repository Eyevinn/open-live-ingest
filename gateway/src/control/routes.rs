//! Control API routes. See docs/DESIGN.md §6 for the endpoint table.

use crate::state::SharedState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use open_live_gateway_types::status::GatewayStatus;
use std::sync::Arc;

#[derive(Clone)]
struct ApiState {
    shared: Arc<SharedState>,
    token: Option<String>,
}

pub fn router(shared: Arc<SharedState>, token: Option<String>) -> Router {
    let state = ApiState { shared, token };

    Router::new()
        // Liveness is deliberately unauthenticated so a probe needs no credentials.
        .route("/healthz", get(healthz))
        .route("/api/v1/status", get(status))
        .route("/api/v1/inputs/{id}/start", post(start_input))
        .route("/api/v1/inputs/{id}/stop", post(stop_input))
        .route("/metrics", get(metrics))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<GatewayStatus>, StatusCode> {
    authorize(&state, &headers)?;
    Ok(Json(state.shared.snapshot()))
}

async fn start_input(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    authorize(&state, &headers)?;
    // TODO(phase 2): stop/start the input's flow in Strom on demand.
    Err(StatusCode::NOT_IMPLEMENTED)
}

async fn stop_input(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    authorize(&state, &headers)?;
    // TODO(phase 2): stop/start the input's flow in Strom on demand.
    Err(StatusCode::NOT_IMPLEMENTED)
}

async fn metrics(State(state): State<ApiState>, headers: HeaderMap) -> Result<String, StatusCode> {
    authorize(&state, &headers)?;
    // TODO(phase 2): Prometheus exposition of per-input state, plus SRT stats read
    // from Strom's own stats API.
    Ok(String::new())
}

/// Bearer check. When no token is configured the API is loopback-only, which
/// `config::validate` guarantees, so an absent token is not a hole.
fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(());
    };
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match presented {
        Some(token) if token == expected => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}
