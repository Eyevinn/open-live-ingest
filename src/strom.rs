//! Client for the local Strom's flow, discovery, and statistics API, speaking
//! Strom's own request and response types.
//!
//! Errors carry the HTTP status only. A flow body echoes block properties, and those
//! carry the SRT passphrase.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::api::{
    ElementPropertiesResponse, FlowListResponse, FlowResponse, SrtRole, SrtStats, SrtStatsResponse,
    SystemInfo,
};
use strom_types::discovery::DeviceResponse;

/// Strom's types a caller of this client handles without depending on `strom-types`
/// itself: a flow and its id, an element property's value
/// ([`StromClient::element_properties`]), and the uplink peer
/// [`StromClient::srt_uplink`] answers with.
pub use strom_types::api::SrtCallerStats;
pub use strom_types::{Flow, FlowId, PropertyValue};

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

/// What [`StromClient::thumbnail`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thumbnail {
    /// The tap's latest frame, as Strom serves it (`image/jpeg`, 320x180 by
    /// default). Not checked: the caller checks the JPEG marker and the size.
    Jpeg(Vec<u8>),
    /// No fresh frame: the branch was just attached, or the source sends nothing
    /// (504). `capture_failed` when Strom says the capture itself failed (500), which
    /// a caller treats the same way, and may log.
    NoFrame { capture_failed: bool },
    /// The flow does not run, or has no such block (404).
    Gone,
}

#[derive(Clone)]
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
    pub async fn video_devices(&self) -> Result<Vec<DeviceResponse>> {
        self.devices_in("video_source").await
    }

    /// Audio sources (microphones, line inputs), as Strom's discovery sees them.
    pub async fn audio_devices(&self) -> Result<Vec<DeviceResponse>> {
        self.devices_in("audio_source").await
    }

    /// `category` is one of the names Strom's discovery route accepts.
    async fn devices_in(&self, category: &str) -> Result<Vec<DeviceResponse>> {
        let res = self
            .get(&format!("/api/discovery/devices?category={category}"))
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
        Ok(self.srt_stats(id).await?.as_ref().and_then(uplink_peer))
    }

    /// Every SRT element's statistics in a flow, sending and receiving. None when the
    /// flow is not running.
    pub async fn srt_stats(&self, id: &FlowId) -> Result<Option<SrtStats>> {
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
        Ok(Some(body.stats))
    }

    /// The current values of a running element's readable properties, by the
    /// element's id in the flow. None when the flow is not running or has no such
    /// element.
    pub async fn element_properties(
        &self,
        id: &FlowId,
        element_id: &str,
    ) -> Result<Option<HashMap<String, PropertyValue>>> {
        let res = self
            .get(&format!("/api/flows/{id}/elements/{element_id}/properties"))
            .send()
            .await
            .context("GET element properties")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: ElementPropertiesResponse = ok(res, "GET element properties")?
            .json()
            .await
            .context("decoding element properties")?;
        Ok(Some(body.properties))
    }

    /// The latest picture from a block's thumbnail tap (`builtin.thumbnail`):
    /// `GET /api/flows/{id}/blocks/{block_id}/thumbnail`. The first request attaches
    /// the tap's branch; Strom detaches it again after 10 s without one. Err only when
    /// Strom does not answer, or answers something else than these.
    pub async fn thumbnail(&self, id: &FlowId, block_id: &str) -> Result<Thumbnail> {
        let res = self
            .get(&format!("/api/flows/{id}/blocks/{block_id}/thumbnail"))
            .send()
            .await
            .context("GET thumbnail")?;
        match res.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(Thumbnail::Gone),
            reqwest::StatusCode::GATEWAY_TIMEOUT => Ok(Thumbnail::NoFrame {
                capture_failed: false,
            }),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR => Ok(Thumbnail::NoFrame {
                capture_failed: true,
            }),
            _ => Ok(Thumbnail::Jpeg(
                ok(res, "GET thumbnail")?
                    .bytes()
                    .await
                    .context("reading thumbnail")?
                    .to_vec(),
            )),
        }
    }

    /// The ids of the blocks this Strom can build (`GET /api/blocks`), such as
    /// `builtin.thumbnail`. Only the ids are read, so a block definition another
    /// Strom version shapes differently does not fail the list.
    pub async fn block_ids(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Blocks {
            blocks: Vec<Block>,
        }
        #[derive(Deserialize)]
        struct Block {
            id: String,
        }
        let res = self.get("/api/blocks").send().await.context("GET blocks")?;
        let body: Blocks = ok(res, "GET blocks")?
            .json()
            .await
            .context("decoding blocks")?;
        Ok(body.blocks.into_iter().map(|b| b.id).collect())
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

    /// A running element's properties, in the shape `GET
    /// /api/flows/<id>/elements/<element>/properties` answers with.
    #[test]
    fn element_properties_in_stroms_shape_decode() {
        let body: ElementPropertiesResponse = serde_json::from_value(json!({
            "element_id": "videosink",
            "properties": { "sync": true, "silent": false, "max-lateness": -1,
                            "last-message": "chain   ******* (videosink:sink) (3110400 bytes, pts: 0:00:12.300000000)" }
        }))
        .unwrap();
        assert!(matches!(
            body.properties.get("sync"),
            Some(PropertyValue::Bool(true))
        ));
        assert!(matches!(
            body.properties.get("last-message"),
            Some(PropertyValue::String(m)) if m.starts_with("chain")
        ));
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

        let audio: Vec<DeviceResponse> = serde_json::from_value(json!([{
            "id": "dev-5e1a0c3f9b2d7e41", "name": "USB Audio Device",
            "device_class": "Audio/Source", "category": "audiosource", "provider": "avfprovider",
            "properties": {}, "first_seen_secs_ago": 1, "last_seen_secs_ago": 1
        }]))
        .unwrap();
        assert_eq!(
            audio[0].category,
            strom_types::discovery::DeviceCategory::AudioSource
        );
    }

    /// A stand-in for Strom on a local port: answers each request with what `route`
    /// gives for its path (status, content type, body), and records the path and the
    /// `Authorization` header.
    async fn fake_strom(
        route: fn(&str) -> (u16, &'static str, Vec<u8>),
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let auth = head
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_string())
                    })
                    .unwrap_or_default();
                log.lock().unwrap().push(format!("{path} {auth}"));
                let (status, content_type, body) = route(&path);
                let mut response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];

    fn error(status: u16, message: &str) -> (u16, &'static str, Vec<u8>) {
        (
            status,
            "application/json",
            json!({ "error": message }).to_string().into_bytes(),
        )
    }

    /// Strom v0.8.4's `get_block_thumbnail`: 200 with the JPEG, 504 while there is no
    /// fresh frame, 404 for a flow that does not run, 500 for a capture that failed.
    #[tokio::test]
    async fn thumbnail_statuses_map_to_what_the_preview_shows() {
        let (url, seen) = fake_strom(|path| {
            let flow = path
                .strip_prefix("/api/flows/")
                .and_then(|r| r.strip_suffix("/blocks/picture/thumbnail"));
            match flow {
                Some("00000000-0000-0000-0000-000000000001") => (200, "image/jpeg", JPEG.to_vec()),
                Some("00000000-0000-0000-0000-000000000002") => {
                    error(504, "Frame capture timed out")
                }
                Some("00000000-0000-0000-0000-000000000003") => {
                    error(404, "Flow not running or block not found")
                }
                Some("00000000-0000-0000-0000-000000000004") => {
                    error(500, "Thumbnail capture failed")
                }
                Some("00000000-0000-0000-0000-000000000005") => error(401, "unauthorized"),
                _ => error(404, "no route"),
            }
        })
        .await;
        let strom = StromClient::new(&url, Some("k3y")).unwrap();
        let id = |n: u128| uuid::Uuid::from_u128(n);

        assert_eq!(
            strom.thumbnail(&id(1), "picture").await.unwrap(),
            Thumbnail::Jpeg(JPEG.to_vec())
        );
        assert_eq!(
            strom.thumbnail(&id(2), "picture").await.unwrap(),
            Thumbnail::NoFrame {
                capture_failed: false
            }
        );
        assert_eq!(
            strom.thumbnail(&id(3), "picture").await.unwrap(),
            Thumbnail::Gone
        );
        assert_eq!(
            strom.thumbnail(&id(4), "picture").await.unwrap(),
            Thumbnail::NoFrame {
                capture_failed: true
            }
        );
        let err = strom.thumbnail(&id(5), "picture").await.unwrap_err();
        assert!(format!("{err:#}").contains("401"), "{err:#}");
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.ends_with(" Bearer k3y")));
    }

    /// No answer at all is an error: Strom is gone, which is not a row's state.
    #[tokio::test]
    async fn a_thumbnail_from_a_strom_that_is_gone_is_an_error() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let strom = StromClient::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        assert!(strom
            .thumbnail(&uuid::Uuid::from_u128(1), "picture")
            .await
            .is_err());
    }

    /// Only the ids are read, so a definition shaped by another Strom version does
    /// not fail the list; the pinned Strom's own definitions decode too.
    #[tokio::test]
    async fn block_ids_read_the_ids_of_get_api_blocks() {
        let (url, seen) = fake_strom(|path| match path {
            "/api/blocks" => (
                200,
                "application/json",
                json!({ "blocks": [
                    { "id": "builtin.local_input", "name": "Local Input", "category": "Inputs" },
                    { "id": "builtin.thumbnail", "something_new": [1, 2] },
                    { "id": "builtin.meter" }
                ]})
                .to_string()
                .into_bytes(),
            ),
            _ => error(404, "no route"),
        })
        .await;
        let strom = StromClient::new(&url, None).unwrap();
        let ids = strom.block_ids().await.unwrap();
        assert_eq!(
            ids,
            ["builtin.local_input", "builtin.thumbnail", "builtin.meter"]
        );
        assert!(crate::flow::has_preview_blocks(&ids));
        assert_eq!(seen.lock().unwrap().as_slice(), ["/api/blocks "]);
    }

    /// An older Strom without the blocks: listed, but not both.
    #[tokio::test]
    async fn an_older_strom_lists_no_preview_blocks() {
        let (url, _) = fake_strom(|path| match path {
            "/api/blocks" => (
                200,
                "application/json",
                json!({ "blocks": [{ "id": "builtin.local_input" }, { "id": "builtin.meter" }] })
                    .to_string()
                    .into_bytes(),
            ),
            _ => error(404, "no route"),
        })
        .await;
        let ids = StromClient::new(&url, None)
            .unwrap()
            .block_ids()
            .await
            .unwrap();
        assert!(!crate::flow::has_preview_blocks(&ids));
    }

    #[tokio::test]
    async fn an_unreadable_block_list_is_an_error() {
        let (url, _) = fake_strom(|_| error(500, "boom")).await;
        let strom = StromClient::new(&url, None).unwrap();
        assert!(strom.block_ids().await.is_err());
    }
}
