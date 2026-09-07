//! Minimal client for the local Strom flow API.
//!
//! Only what the agent needs: create a flow under an id it generated, update it, read
//! its state, start it, stop it. Auth is a bearer token (`STROM_API_KEY` on the Strom
//! side); Strom running without auth needs none.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The subset of Strom's `Flow` the agent reads back.
#[derive(Debug, Clone, Deserialize)]
pub struct FlowState {
    // Echoed back by Strom and kept for logging and diagnostics; the agent addresses
    // flows by the id it derived, so it never needs to read these to act.
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub name: String,
    /// True when the pipeline is actually running. Strom documents this as the field
    /// callers should use, rather than interpreting `gst_state` themselves.
    #[serde(default)]
    pub running: bool,
    /// Raw GStreamer state, for diagnostics only.
    #[serde(default)]
    pub gst_state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlowResponse {
    flow: FlowState,
}

pub struct StromClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl StromClient {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(str::to_string),
        })
    }

    pub async fn get_flow(&self, id: &str) -> Result<Option<FlowState>> {
        let res = self
            .auth(self.http.get(format!("{}/api/flows/{id}", self.base_url)))
            .send()
            .await
            .context("GET flow")?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = error_for_status(res, "GET flow")?;
        let body: FlowResponse = res.json().await.context("decoding flow")?;
        Ok(Some(body.flow))
    }

    /// Creates a flow under the id carried in `flow`.
    ///
    /// Strom answers 409 when that id already exists, which for a deterministic id is
    /// the normal restart case rather than an error — the caller updates instead.
    pub async fn create_flow(&self, flow: &Value) -> Result<FlowCreateOutcome> {
        let res = self
            .auth(self.http.post(format!("{}/api/flows", self.base_url)))
            .json(flow)
            .send()
            .await
            .context("POST flow")?;

        if res.status() == reqwest::StatusCode::CONFLICT {
            return Ok(FlowCreateOutcome::AlreadyExists);
        }
        let res = error_for_status(res, "POST flow")?;
        let body: FlowResponse = res.json().await.context("decoding created flow")?;
        Ok(FlowCreateOutcome::Created(body.flow))
    }

    /// Replaces an existing flow, so a config change is applied on the next reconcile.
    pub async fn update_flow(&self, id: &str, flow: &Value) -> Result<FlowState> {
        let res = self
            .auth(self.http.post(format!("{}/api/flows/{id}", self.base_url)))
            .json(flow)
            .send()
            .await
            .context("POST flow update")?;

        let res = error_for_status(res, "POST flow update")?;
        let body: FlowResponse = res.json().await.context("decoding updated flow")?;
        Ok(body.flow)
    }

    pub async fn start_flow(&self, id: &str) -> Result<FlowState> {
        let res = self
            .auth(
                self.http
                    .post(format!("{}/api/flows/{id}/start", self.base_url)),
            )
            .send()
            .await
            .context("POST flow start")?;

        let res = error_for_status(res, "POST flow start")?;
        let body: FlowResponse = res.json().await.context("decoding started flow")?;
        Ok(body.flow)
    }

    #[allow(dead_code)] // used by POST /api/v1/inputs/{id}/stop in phase 2
    pub async fn stop_flow(&self, id: &str) -> Result<FlowState> {
        let res = self
            .auth(
                self.http
                    .post(format!("{}/api/flows/{id}/stop", self.base_url)),
            )
            .send()
            .await
            .context("POST flow stop")?;

        let res = error_for_status(res, "POST flow stop")?;
        let body: FlowResponse = res.json().await.context("decoding stopped flow")?;
        Ok(body.flow)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.api_key.as_deref() {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }
}

#[derive(Debug)]
pub enum FlowCreateOutcome {
    Created(FlowState),
    AlreadyExists,
}

/// Turns a non-2xx response into an error carrying the status only — a flow body
/// echoes block properties, and those carry the SRT passphrase.
fn error_for_status(res: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = res.status();
    if !status.is_success() {
        bail!("{what} failed with HTTP {status}");
    }
    Ok(res)
}
