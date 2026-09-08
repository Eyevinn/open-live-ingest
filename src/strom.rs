//! Client for the local Strom's flow, discovery, and statistics API.
//!
//! Errors carry the HTTP status only. A flow body echoes block properties, and those
//! carry the SRT passphrase.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A flow as Strom holds it.
#[derive(Debug, Clone)]
pub struct Flow {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub raw: Value,
}

impl Flow {
    fn from_raw(raw: Value) -> Option<Self> {
        Some(Self {
            id: raw.get("id")?.as_str()?.to_string(),
            name: raw
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            running: raw.get("running").and_then(Value::as_bool).unwrap_or(false),
            raw,
        })
    }
}

/// A capture device Strom can see on this host.
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    /// Strom's discovery id, which is what `builtin.local_input.video_device` takes.
    pub id: String,
    #[serde(alias = "name")]
    pub display_name: String,
}

/// The uplink's SRT connection as Strom reports it. `connected` is deliberately not
/// read: srtsink keeps a stale entry reporting connected while it retries a vanished
/// peer. Bytes moving between polls is what proves delivery.
#[derive(Debug, Clone, Default)]
pub struct SrtSample {
    pub bytes_sent: u64,
    pub rtt_ms: Option<f64>,
    pub send_rate_mbps: Option<f64>,
}

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
                    .json::<Value>()
                    .await
                    .ok()
                    .map(|body| device_items(body).as_array().map_or(0, Vec::len))
                    .unwrap_or(0),
            },
        }
    }

    /// Video sources, as Strom's discovery sees them.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let res = self
            .get("/api/discovery/devices?category=video_source")
            .send()
            .await
            .context("GET devices")?;
        let body: Value = ok(res, "GET devices")?
            .json()
            .await
            .context("decoding devices")?;
        Ok(serde_json::from_value(device_items(body)).unwrap_or_default())
    }

    pub async fn list_flows(&self) -> Result<Vec<Flow>> {
        let res = self.get("/api/flows").send().await.context("GET flows")?;
        let body: Value = ok(res, "GET flows")?
            .json()
            .await
            .context("decoding flows")?;
        Ok(body
            .get("flows")
            .and_then(Value::as_array)
            .map(|flows| flows.iter().cloned().filter_map(Flow::from_raw).collect())
            .unwrap_or_default())
    }

    pub async fn get_flow(&self, id: &str) -> Result<Option<Flow>> {
        let res = self
            .get(&format!("/api/flows/{id}"))
            .send()
            .await
            .context("GET flow")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: Value = ok(res, "GET flow")?.json().await.context("decoding flow")?;
        Ok(body.get("flow").cloned().and_then(Flow::from_raw))
    }

    pub async fn create_flow(&self, flow: &Value) -> Result<()> {
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
    pub async fn start_flow(&self, id: &str) -> Result<bool> {
        let res = self
            .post(&format!("/api/flows/{id}/start"))
            .send()
            .await
            .context("POST flow start")?;
        let body: Value = ok(res, "POST flow start")?
            .json()
            .await
            .context("decoding started flow")?;
        Ok(body
            .pointer("/flow/running")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub async fn stop_flow(&self, id: &str) -> Result<()> {
        let res = self
            .post(&format!("/api/flows/{id}/stop"))
            .send()
            .await
            .context("POST flow stop")?;
        ok(res, "POST flow stop")?;
        Ok(())
    }

    pub async fn delete_flow(&self, id: &str) -> Result<()> {
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

    /// The uplink's SRT statistics: the sink connection's first peer, whichever end
    /// dialled. None when the flow reports no such connection yet.
    pub async fn srt_uplink(&self, id: &str) -> Result<Option<SrtSample>> {
        let res = self
            .get(&format!("/api/flows/{id}/srt-stats"))
            .send()
            .await
            .context("GET srt-stats")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: Value = ok(res, "GET srt-stats")?
            .json()
            .await
            .context("decoding srt-stats")?;
        Ok(uplink_sample(&body))
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

/// Picks the uplink's peer out of an srt-stats body.
fn uplink_sample(body: &Value) -> Option<SrtSample> {
    let connections = body.pointer("/stats/connections")?.as_object()?;
    let sink = connections
        .values()
        .find(|c| c.get("role").and_then(Value::as_str) == Some("sink"))?;
    let peer = sink.get("callers")?.as_array()?.first()?;
    Some(SrtSample {
        bytes_sent: peer.get("bytes_sent").and_then(Value::as_u64).unwrap_or(0),
        rtt_ms: peer.get("rtt_ms").and_then(Value::as_f64),
        send_rate_mbps: peer.get("send_rate_mbps").and_then(Value::as_f64),
    })
}

/// Accepts a bare array or an object wrapping one, so a shape change upstream
/// degrades to an empty list rather than an error.
fn device_items(body: Value) -> Value {
    if body.is_array() {
        return body;
    }
    ["devices", "items", "sources"]
        .iter()
        .find_map(|k| body.get(*k).cloned())
        .unwrap_or(Value::Array(vec![]))
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
    fn the_uplink_sample_is_the_sink_connections_first_peer() {
        let body = json!({ "stats": { "connections": {
            "uplink": { "role": "sink", "mode": "caller",
                        "callers": [{ "bytes_sent": 1234, "rtt_ms": 31.5, "send_rate_mbps": 5.9 }] },
            "monitor": { "role": "source", "mode": "listener", "callers": [{ "bytes_sent": 9 }] }
        }}});
        let s = uplink_sample(&body).expect("a sink connection");
        assert_eq!(s.bytes_sent, 1234);
        assert_eq!(s.rtt_ms, Some(31.5));
    }

    /// Before the far end answers there is no peer, which is "waiting", not an error.
    #[test]
    fn no_peer_yet_is_no_sample() {
        let body = json!({ "stats": { "connections": {
            "uplink": { "role": "sink", "mode": "caller", "callers": [] }
        }}});
        assert!(uplink_sample(&body).is_none());
        assert!(uplink_sample(&json!({})).is_none());
    }

    #[test]
    fn device_lists_may_be_bare_or_wrapped() {
        let bare = json!([{ "id": "dev-1", "name": "Cam" }]);
        let wrapped = json!({ "devices": [{ "id": "dev-1", "display_name": "Cam" }] });
        for body in [bare, wrapped] {
            let devices: Vec<Device> = serde_json::from_value(device_items(body)).unwrap();
            assert_eq!(devices[0].display_name, "Cam");
        }
        assert!(device_items(json!({ "other": 1 }))
            .as_array()
            .unwrap()
            .is_empty());
    }
}
