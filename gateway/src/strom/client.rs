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

/// A flow as Strom holds it: the fields the agent acts on, plus the raw JSON.
///
/// The raw form matters because Strom owns fields the gateway must not overwrite —
/// `properties.auto_restart`, `properties.started_at`, `running`, `gst_state` — so an
/// update has to carry them back rather than let them default away.
#[derive(Debug)]
pub struct FetchedFlow {
    pub state: FlowState,
    pub raw: Value,
}

/// The uplink's SRT connection as Strom reports it.
///
/// `connected` is deliberately not exposed: when the far end disappears, srtsink
/// keeps a stale caller entry with `connected: true` while it retries. What actually
/// distinguishes a delivering uplink is `bytes_sent` advancing, corroborated by a
/// non-null `rtt_ms` — every metric goes null once the socket is broken.
#[derive(Debug, Clone, Default)]
pub struct SrtUplink {
    pub bytes_sent: u64,
    pub rtt_ms: Option<f64>,
    pub send_rate_mbps: Option<f64>,
    pub packets_retransmitted: Option<u64>,
    pub packets_sent_dropped: Option<u64>,
    pub negotiated_latency_ms: Option<u64>,
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

    pub async fn get_flow(&self, id: &str) -> Result<Option<FetchedFlow>> {
        let res = self
            .auth(self.http.get(format!("{}/api/flows/{id}", self.base_url)))
            .send()
            .await
            .context("GET flow")?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = error_for_status(res, "GET flow")?;
        let body: Value = res.json().await.context("decoding flow")?;
        let raw = body
            .get("flow")
            .cloned()
            .context("flow response has no `flow` field")?;
        let state: FlowState = serde_json::from_value(raw.clone()).context("decoding flow")?;
        Ok(Some(FetchedFlow { state, raw }))
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

    /// Reads the flow's SRT statistics, picking out the sink in caller mode — the
    /// gateway's uplink. Returns None when the flow reports no such connection.
    pub async fn srt_uplink(&self, id: &str) -> Result<Option<SrtUplink>> {
        let res = self
            .auth(
                self.http
                    .get(format!("{}/api/flows/{id}/srt-stats", self.base_url)),
            )
            .send()
            .await
            .context("GET srt-stats")?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = error_for_status(res, "GET srt-stats")?;
        let body: Value = res.json().await.context("decoding srt-stats")?;

        let connections = match body
            .pointer("/stats/connections")
            .and_then(Value::as_object)
        {
            Some(connections) => connections,
            None => return Ok(None),
        };

        let uplink = connections.values().find(|c| {
            c.get("role").and_then(Value::as_str) == Some("sink")
                && c.get("mode").and_then(Value::as_str) == Some("caller")
        });

        let Some(caller) = uplink
            .and_then(|c| c.get("callers"))
            .and_then(Value::as_array)
            .and_then(|callers| callers.first())
        else {
            return Ok(None);
        };

        Ok(Some(SrtUplink {
            bytes_sent: caller
                .get("bytes_sent")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            rtt_ms: caller.get("rtt_ms").and_then(Value::as_f64),
            send_rate_mbps: caller.get("send_rate_mbps").and_then(Value::as_f64),
            packets_retransmitted: caller.get("packets_retransmitted").and_then(Value::as_u64),
            packets_sent_dropped: caller.get("packets_sent_dropped").and_then(Value::as_u64),
            negotiated_latency_ms: caller.get("negotiated_latency_ms").and_then(Value::as_u64),
        }))
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
