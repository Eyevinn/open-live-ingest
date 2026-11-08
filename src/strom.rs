//! Client for the local Strom's flow, discovery, and statistics API, speaking
//! Strom's own request and response types.
//!
//! Errors carry the HTTP status only. A flow body echoes block properties, and those
//! carry the SRT passphrase.

use anyhow::{bail, Context, Result};
use std::time::Duration;
use strom_types::api::{
    FlowListResponse, FlowResponse, SrtCallerStats, SrtRole, SrtStats, SrtStatsResponse, SystemInfo,
};
use strom_types::discovery::DeviceResponse;
use strom_types::{Flow, FlowId};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The Strom release this gateway is built against: the `strom-types` tag in
/// Cargo.toml, handed over by build.rs.
pub const PINNED_VERSION: &str = env!("STROM_PINNED_VERSION");

pub enum Reachability {
    Ok {
        video_sources: usize,
    },
    /// Reached, but it wants a credential. Distinct from unreachable because the two
    /// send an operator to completely different places.
    NeedsCredential,
    Unreachable(String),
}

pub struct StromClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl StromClient {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .context("building HTTP client")?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(str::to_string),
        })
    }

    pub async fn probe(&self) -> Reachability {
        let res = match self
            .get("/api/discovery/devices?category=video_source")
            .send()
            .await
        {
            Ok(res) => res,
            Err(err) => return Reachability::Unreachable(err.to_string()),
        };
        match res.status() {
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                Reachability::NeedsCredential
            }
            status if !status.is_success() => Reachability::Unreachable(format!("HTTP {status}")),
            _ => Reachability::Ok {
                video_sources: res
                    .json::<Vec<DeviceResponse>>()
                    .await
                    .map(|d| d.len())
                    .unwrap_or(0),
            },
        }
    }

    /// Video sources, as Strom's discovery sees them.
    pub async fn devices(&self) -> Result<Vec<DeviceResponse>> {
        let res = self
            .get("/api/discovery/devices?category=video_source")
            .send()
            .await
            .context("GET devices")?;
        ok(res, "GET devices")?
            .json()
            .await
            .context("decoding devices")
    }

    pub async fn list_flows(&self) -> Result<Vec<Flow>> {
        let res = self.get("/api/flows").send().await.context("GET flows")?;
        let body: FlowListResponse = ok(res, "GET flows")?
            .json()
            .await
            .context("decoding flows")?;
        Ok(body.flows)
    }

    pub async fn get_flow(&self, id: &FlowId) -> Result<Option<Flow>> {
        let res = self
            .get(&format!("/api/flows/{id}"))
            .send()
            .await
            .context("GET flow")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: FlowResponse = ok(res, "GET flow")?.json().await.context("decoding flow")?;
        Ok(Some(body.flow))
    }

    pub async fn create_flow(&self, flow: &Flow) -> Result<()> {
        let res = self
            .post("/api/flows")
            .json(flow)
            .send()
            .await
            .context("POST flow")?;
        ok(res, "POST flow")?;
        Ok(())
    }

    /// Starts a flow and reports whether Strom says it is running.
    pub async fn start_flow(&self, id: &FlowId) -> Result<bool> {
        let res = self
            .post(&format!("/api/flows/{id}/start"))
            .send()
            .await
            .context("POST flow start")?;
        let body: FlowResponse = ok(res, "POST flow start")?
            .json()
            .await
            .context("decoding started flow")?;
        Ok(body.flow.running)
    }

    pub async fn stop_flow(&self, id: &FlowId) -> Result<()> {
        let res = self
            .post(&format!("/api/flows/{id}/stop"))
            .send()
            .await
            .context("POST flow stop")?;
        ok(res, "POST flow stop")?;
        Ok(())
    }

    pub async fn delete_flow(&self, id: &FlowId) -> Result<()> {
        let res = self
            .delete(&format!("/api/flows/{id}"))
            .send()
            .await
            .context("DELETE flow")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        ok(res, "DELETE flow")?;
        Ok(())
    }

    /// Strom's own version, checked against [`PINNED_VERSION`] at startup and reported
    /// in the heartbeat to Open Live.
    pub async fn version(&self) -> Result<String> {
        let res = self
            .get("/api/version")
            .send()
            .await
            .context("GET version")?;
        let body: SystemInfo = ok(res, "GET version")?
            .json()
            .await
            .context("decoding version")?;
        Ok(body.version)
    }

    /// The uplink's SRT peer statistics. None when the flow reports no peer yet.
    pub async fn srt_uplink(&self, id: &FlowId) -> Result<Option<SrtCallerStats>> {
        let res = self
            .get(&format!("/api/flows/{id}/srt-stats"))
            .send()
            .await
            .context("GET srt-stats")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: SrtStatsResponse = ok(res, "GET srt-stats")?
            .json()
            .await
            .context("decoding srt-stats")?;
        Ok(uplink_peer(&body.stats))
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.get(format!("{}{path}", self.base_url)))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.post(format!("{}{path}", self.base_url)))
    }

    fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.delete(format!("{}{path}", self.base_url)))
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }
}

/// The sending element's first peer, whichever end dialled. `connected` is
/// deliberately not read: srtsink keeps a stale entry reporting connected while it
/// retries a vanished peer. Bytes moving between polls is what proves delivery.
fn uplink_peer(stats: &SrtStats) -> Option<SrtCallerStats> {
    stats
        .connections
        .values()
        .find(|c| c.role == SrtRole::Sink)?
        .callers
        .first()
        .cloned()
}

/// The warning to print when the running Strom is not the release this gateway is
/// built against, or None when it is. One leading `v` is ignored on either side; any
/// other difference, a prerelease or build suffix included, is a mismatch. `running`
/// is None when the version could not be read.
pub fn version_warning(pinned: &str, running: Option<&str>) -> Option<String> {
    let pinned = strip_v(pinned);
    match running.map(strip_v) {
        Some(running) if running == pinned => None,
        Some(running) => Some(format!(
            "Strom {running} is running, but this gateway is built against Strom {pinned}; flows may fail. Install Strom v{pinned} or a gateway release built for {running}."
        )),
        None => Some(format!(
            "Could not read the running Strom's version, but this gateway is built against Strom {pinned}; flows may fail on another version. Install Strom v{pinned} if they do."
        )),
    }
}

fn strip_v(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

fn ok(res: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = res.status();
    if !status.is_success() {
        bail!("{what} failed with HTTP {status}");
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_same_strom_gives_no_warning() {
        assert_eq!(version_warning("v0.8.0", Some("v0.8.0")), None);
        assert_eq!(version_warning("0.8.0", Some("0.8.0")), None);
    }

    #[test]
    fn one_leading_v_is_ignored_on_either_side() {
        assert_eq!(version_warning("v0.8.0", Some("0.8.0")), None);
        assert_eq!(version_warning("0.8.0", Some("v0.8.0")), None);
    }

    #[test]
    fn another_strom_is_warned_about_by_both_versions() {
        let warning = version_warning("v0.8.0", Some("0.7.2")).expect("a warning");
        assert!(warning.contains("Strom 0.7.2 is running"), "{warning}");
        assert!(warning.contains("built against Strom 0.8.0"), "{warning}");
    }

    #[test]
    fn a_prerelease_of_the_pinned_strom_is_a_mismatch() {
        let warning = version_warning("v0.8.0", Some("0.8.0-alpha.5")).expect("a warning");
        assert!(warning.contains("0.8.0-alpha.5"), "{warning}");
        assert!(version_warning("v0.8.0", Some("0.8.0+build.1")).is_some());
        assert!(version_warning("v0.8.0", Some("vv0.8.0")).is_some());
    }

    #[test]
    fn an_unreadable_version_is_warned_about_by_the_pinned_one() {
        let warning = version_warning("v0.8.0", None).expect("a warning");
        assert!(warning.contains("Strom 0.8.0"), "{warning}");
        assert!(warning.contains("Could not read"), "{warning}");
    }

    fn stats(body: serde_json::Value) -> SrtStats {
        serde_json::from_value(body).expect("srt-stats body in Strom's shape")
    }

    #[test]
    fn the_uplink_peer_is_the_sending_elements_first_caller() {
        let s = stats(json!({ "connections": {
            "uplink:srtsink": { "role": "sink", "mode": "caller", "connected": true,
                                "callers": [{ "bytes_sent": 1234, "rtt_ms": 31.5, "send_rate_mbps": 5.9 }] },
            "monitor:srtsrc": { "role": "source", "mode": "listener", "connected": true,
                                "callers": [{ "bytes_received": 9 }] }
        }}));
        let peer = uplink_peer(&s).expect("a sink connection");
        assert_eq!(peer.bytes_sent, Some(1234));
        assert_eq!(peer.rtt_ms, Some(31.5));
    }

    /// Before the far end answers there is no peer, which is "waiting", not an error.
    #[test]
    fn no_peer_yet_is_no_sample() {
        let s = stats(json!({ "connections": {
            "uplink:srtsink": { "role": "sink", "mode": "caller", "connected": false, "callers": [] }
        }}));
        assert!(uplink_peer(&s).is_none());
        assert!(uplink_peer(&SrtStats::default()).is_none());
    }

    /// The device list is what `GET /api/discovery/devices` returns today, verbatim.
    #[test]
    fn a_device_list_in_stroms_shape_decodes() {
        let devices: Vec<DeviceResponse> = serde_json::from_value(json!([{
            "id": "dev-d24eb9fb6733c220", "name": "FaceTime HD Camera",
            "device_class": "Video/Source", "category": "videosource", "provider": "avfprovider",
            "properties": { "device.api": "avf" }, "first_seen_secs_ago": 1, "last_seen_secs_ago": 1
        }]))
        .unwrap();
        assert_eq!(devices[0].name, "FaceTime HD Camera");
    }
}
