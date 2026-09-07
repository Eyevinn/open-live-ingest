//! Minimal client for the Open Live `/api/v1/sources` API.
//!
//! Only the calls the gateway needs. The API key is a bearer token and must never
//! reach the logs — construct errors from status codes, not from request dumps.

use crate::openlive::token::Auth;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Payload for creating a source. `streamType` is always `srt` in phase 1; `efp`
/// arrives with multi-track audio in phase 3.
#[derive(Debug, Clone, Serialize)]
pub struct SourcePayload {
    pub name: String,
    /// The *listener* form of the SRT URI — what Strom binds on the cloud side.
    pub address: String,
    #[serde(rename = "streamType")]
    pub stream_type: String,
    pub status: String,
    /// SRT receiver latency in ms. Open Live takes the maximum across a production's
    /// sources when it generates the flow, so both ends agree on the buffer.
    pub latency: u32,
    #[serde(rename = "liveCamera", skip_serializing_if = "Option::is_none")]
    pub live_camera: Option<bool>,
}

// Fields beyond `id` are deserialized so a drift check can compare them against the
// desired payload; that comparison lands with phase 2.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct SourceResponse {
    pub id: String,
    pub name: String,
    /// Returned with any `passphrase=` value masked, which is why the gateway treats
    /// its own config as the source of truth and never reads the passphrase back.
    pub address: String,
    #[serde(rename = "streamType")]
    pub stream_type: String,
    pub status: String,
    #[serde(default)]
    pub latency: Option<u32>,
}

pub struct OpenLiveClient {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
}

impl OpenLiveClient {
    pub fn new(base_url: &str, auth_mode: &str, api_key: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth: Auth::new(auth_mode, api_key)?,
        })
    }

    /// Attaches the bearer token. In OSC mode this may perform a token exchange, so
    /// it is awaited per request rather than cached on the client.
    async fn auth_req(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match self.auth.bearer(&self.http).await? {
            Some(token) => req.bearer_auth(token),
            None => req,
        })
    }

    /// Lists all sources.
    ///
    /// Deliberately used instead of `GET /api/v1/sources/{id}`: a 200 with a JSON
    /// array proves the API is up and serving, so our id being absent from it is real
    /// evidence the source was deleted. A 404 on a single resource proves nothing —
    /// a restarting Open Live 404s every route, and acting on that would recreate the
    /// source on every tick.
    pub async fn list_sources(&self) -> Result<Vec<SourceResponse>> {
        let res = self
            .auth_req(self.http.get(format!("{}/api/v1/sources", self.base_url)))
            .await?
            .send()
            .await
            .context("GET sources")?;

        let res = error_for_status(res, "GET sources")?;
        res.json().await.context("decoding sources")
    }

    pub async fn create_source(&self, payload: &SourcePayload) -> Result<SourceResponse> {
        let res = self
            .auth_req(self.http.post(format!("{}/api/v1/sources", self.base_url)))
            .await?
            .json(payload)
            .send()
            .await
            .context("POST source")?;

        let res = error_for_status(res, "POST source")?;
        res.json().await.context("decoding created source")
    }

    pub async fn patch_source(&self, id: &str, payload: &SourcePayload) -> Result<SourceResponse> {
        let res = self
            .auth_req(
                self.http
                    .patch(format!("{}/api/v1/sources/{id}", self.base_url)),
            )
            .await?
            .json(payload)
            .send()
            .await
            .context("PATCH source")?;

        let res = error_for_status(res, "PATCH source")?;
        res.json().await.context("decoding patched source")
    }

    #[allow(dead_code)] // used for immediate status pushes on state change (phase 2)
    pub async fn set_status(&self, id: &str, status: &str) -> Result<()> {
        let res = self
            .auth_req(
                self.http
                    .patch(format!("{}/api/v1/sources/{id}", self.base_url)),
            )
            .await?
            .json(&serde_json::json!({ "status": status }))
            .send()
            .await
            .context("PATCH source status")?;

        error_for_status(res, "PATCH source status")?;
        Ok(())
    }
}

/// Turns a non-2xx response into an error carrying the status only — response bodies
/// can echo request content, and request content carries the SRT passphrase.
fn error_for_status(res: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = res.status();
    if !status.is_success() {
        bail!("{what} failed with HTTP {status}");
    }
    Ok(res)
}
