//! Status pushed to Open Live, so a Studio operator can see a venue without shelling
//! into it.
//!
//! One outbound WebSocket to Open Live's gateway heartbeat endpoint, opened with the
//! per-gateway token Open Live minted for this box. Nothing new is measured here: the
//! loop in `run` already knows each flow's state and its uplink statistics, and hands
//! a snapshot over after every tick; this re-sends the latest one at the interval Open
//! Live asks for in its `HELLO`. Nothing inbound is acted on: a frame from Open Live
//! is a greeting, an acknowledgement, or an error, and anything else is ignored.
//!
//! Best effort, like registration. A socket that cannot be opened is retried with
//! backoff and never touches a flow. The token never reaches the logs: it travels in a
//! header, and errors are built from status codes and close codes.

use crate::config::OpenLive;
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use strom_types::api::SrtCallerStats;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Open Live's advice when its `HELLO` names no interval.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// Bounds on the interval Open Live may ask for: a tick a second is the most a
/// venue's link should carry, and past a minute the gateway would read as down.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the greeting once the socket is open.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the shutdown notice may hold up the rest of the teardown.
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Open Live's application close code for a refused token.
const CLOSE_UNAUTHORIZED: u16 = 4401;

/// Where to push, and as whom.
#[derive(Clone)]
pub struct Target {
    pub socket_url: String,
    pub gateway_id: String,
    token: String,
}

impl Target {
    /// `None` when the settings name no gateway: the heartbeat is optional, and a box
    /// nobody has registered as a gateway in Open Live simply does not push. An id
    /// without its token, or the reverse, is a mistake worth refusing at startup.
    pub fn from_config(cfg: &OpenLive) -> Result<Option<Self>> {
        let id = trimmed(cfg.gateway_id.as_deref());
        let token = trimmed(cfg.gateway_token.as_deref());
        let (id, token) = match (id, token) {
            (None, None) => return Ok(None),
            (Some(_), None) => bail!(
                "open_live.gateway_id is set but open_live.gateway_token is not; set it, or OLI_OPEN_LIVE_GATEWAY_TOKEN"
            ),
            (None, Some(_)) => bail!(
                "open_live.gateway_token is set but open_live.gateway_id is not; set it to the id Open Live gave the gateway"
            ),
            (Some(id), Some(token)) => (id, token),
        };
        let url = trimmed(cfg.url.as_deref())
            .context("open_live.gateway_id is set but open_live.url is not")?;
        Ok(Some(Self {
            socket_url: socket_url(url, id)?,
            gateway_id: id.to_string(),
            token: token.to_string(),
        }))
    }
}

/// The token is the one thing here that must not be printed, so `Debug` masks it.
impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("socket_url", &self.socket_url)
            .field("gateway_id", &self.gateway_id)
            .field("token", &"***")
            .finish()
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// The heartbeat endpoint for an Open Live address: the same host, over WebSocket.
pub fn socket_url(base: &str, gateway_id: &str) -> Result<String> {
    if !gateway_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("open_live.gateway_id may only contain letters, digits, - and _, got {gateway_id:?}");
    }
    let base = base.trim().trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("open_live.url must start with http:// or https://, got {base:?}");
    };
    Ok(format!("{ws}/ws/gateways/{gateway_id}/heartbeat"))
}

/// What one heartbeat says: the same facts `status --json` reports, in the field
/// names Open Live's gateway resource stores.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strom_version: Option<String>,
    /// Not in Open Live's schema; by contract it ignores fields it does not know, and
    /// a version is the first thing to want when a venue misbehaves.
    pub ingest_version: String,
    /// Inputs this gateway drives.
    pub device_count: usize,
    /// Inputs whose uplink is delivering.
    pub streaming_count: usize,
    pub inputs: Vec<InputStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputStatus {
    pub input_id: String,
    pub name: String,
    pub flow_state: FlowState,
    /// The Open Live source this input registered as, once known.
    pub source_id: Option<String>,
    /// Absent until something receives the feed.
    pub uplink: Option<Uplink>,
}

/// Strom's flow state vocabulary, which is what Open Live stores. Strom also has
/// `paused`, which a gateway flow is never left in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowState {
    Idle,
    Playing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Uplink {
    #[serde(rename = "bitrateKbps")]
    pub bitrate_kbps: u64,
    pub rtt_ms: f64,
    /// Packets dropped before transmission.
    pub dropped: u64,
}

impl Uplink {
    pub fn from_srt(stats: &SrtCallerStats) -> Self {
        Self {
            bitrate_kbps: (stats.send_rate_mbps.unwrap_or(0.0).max(0.0) * 1000.0).round() as u64,
            rtt_ms: stats.rtt_ms.unwrap_or(0.0),
            dropped: stats.packets_sent_dropped.unwrap_or(0),
        }
    }
}

/// The running heartbeat. `update` hands it the latest snapshot; `stop` tells Open
/// Live the gateway is going down.
pub struct Heartbeat {
    snapshot: watch::Sender<Snapshot>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Heartbeat {
    pub fn start(target: Target, initial: Snapshot) -> Self {
        let (snapshot, latest) = watch::channel(initial);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(target, latest, stopped));
        Self {
            snapshot,
            stop: Some(stop),
            task,
        }
    }

    pub fn update(&self, snapshot: Snapshot) {
        self.snapshot.send_replace(snapshot);
    }

    /// Announces the shutdown and ends. Bounded, so a dead socket cannot hold up the
    /// teardown of the feeds.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).ok();
        }
        if tokio::time::timeout(OFFLINE_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

/// Opens the socket once and waits for Open Live's greeting, so setup can tell a
/// wrong token from a working one before the show.
pub async fn probe(target: &Target) -> Result<()> {
    let mut socket = connect(target).await?;
    let outcome = greeting(&mut socket).await;
    socket.close(None).await.ok();
    match outcome? {
        Greeting::Hello { .. } => Ok(()),
        Greeting::Unauthorized => bail!(
            "Open Live refused the token for gateway {}; check open_live.gateway_id and open_live.gateway_token, or rotate the token",
            target.gateway_id
        ),
        Greeting::Closed => bail!("Open Live closed the heartbeat socket without a greeting"),
    }
}

/// Connects, pushes, reconnects, until told to stop. Each failure is reported once,
/// not on every retry, and recovery is reported too, so a log read after the show
/// says when Studio could and could not see the venue.
async fn run(target: Target, latest: watch::Receiver<Snapshot>, mut stop: oneshot::Receiver<()>) {
    let mut seq = 0u64;
    let mut failures = 0u32;
    let mut reported: Option<String> = None;
    loop {
        let ending = session(&target, &latest, &mut stop, &mut seq).await;
        let (delay, why) = match ending {
            Ok(Ending::Stopped) => return,
            Ok(Ending::Unauthorized) => (
                MAX_BACKOFF,
                format!(
                    "Open Live refused the token for gateway {}; check open_live.gateway_id and open_live.gateway_token, or rotate the token",
                    target.gateway_id
                ),
            ),
            Ok(Ending::Dropped { was_connected }) => {
                if was_connected {
                    failures = 0;
                    reported = None;
                }
                failures += 1;
                (
                    backoff(failures),
                    "the heartbeat socket to Open Live closed".to_string(),
                )
            }
            Err(err) => {
                failures += 1;
                (
                    backoff(failures),
                    format!("could not push status to Open Live: {err:#}"),
                )
            }
        };
        if reported.as_deref() != Some(why.as_str()) {
            warn!("{why}; retrying every {}s", delay.as_secs());
            reported = Some(why);
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = &mut stop => return,
        }
    }
}

/// Exponential, from a second to a minute.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(6)).min(MAX_BACKOFF)
}

/// How one connection ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// Told to stop; the shutdown notice went out.
    Stopped,
    /// Open Live refused the token. Retrying quickly will not help.
    Unauthorized,
    /// The socket closed or failed. `was_connected` says whether the greeting had
    /// arrived, which separates a working link that dropped from one that never came up.
    Dropped { was_connected: bool },
}

/// One connection, from dial to drop.
async fn session(
    target: &Target,
    latest: &watch::Receiver<Snapshot>,
    stop: &mut oneshot::Receiver<()>,
    seq: &mut u64,
) -> Result<Ending> {
    let mut socket = connect(target).await?;
    let interval = match greeting(&mut socket).await? {
        Greeting::Hello { interval } => interval,
        Greeting::Unauthorized => return Ok(Ending::Unauthorized),
        Greeting::Closed => {
            return Ok(Ending::Dropped {
                was_connected: false,
            })
        }
    };
    info!(
        gateway = %target.gateway_id,
        every_secs = interval.as_secs(),
        "pushing status to Open Live"
    );

    // Cloned out of the watch before each send, so its read lock is never held
    // across an await.
    let snapshot = latest.borrow().clone();
    send(&mut socket, seq, "GATEWAY_ONLINE", &snapshot).await?;
    let mut ticker = tokio::time::interval(interval);
    // A stall must not be followed by a burst of stale heartbeats.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let snapshot = latest.borrow().clone();
                send(&mut socket, seq, "HEARTBEAT", &snapshot).await?;
            }
            incoming = socket.next() => match incoming {
                Some(Ok(message)) => match classify(&message) {
                    Inbound::Unauthorized => return Ok(Ending::Unauthorized),
                    Inbound::Closed => return Ok(Ending::Dropped { was_connected: true }),
                    Inbound::Rejected(code) => warn!(code, "Open Live rejected a heartbeat frame"),
                    Inbound::Hello { .. } | Inbound::Ignore => {}
                },
                Some(Err(err)) => return Err(describe(err)),
                None => return Ok(Ending::Dropped { was_connected: true }),
            },
            _ = &mut *stop => {
                send(&mut socket, seq, "GATEWAY_OFFLINE", &Offline { reason: "operator-stop" }).await.ok();
                socket.close(None).await.ok();
                return Ok(Ending::Stopped);
            }
        }
    }
}

/// Dials the socket with the token in the `Authorization` header, never in the URL,
/// so it appears in no address anyone logs.
async fn connect(target: &Target) -> Result<Socket> {
    let mut request = target
        .socket_url
        .as_str()
        .into_client_request()
        .context("building the heartbeat request")?;
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", target.token))
        .context("the gateway token is not a valid header value")?;
    bearer.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let (socket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .context("timed out opening the heartbeat socket")?
            .map_err(describe)?;
    Ok(socket)
}

/// Open Live's first word.
#[derive(Debug, PartialEq, Eq)]
enum Greeting {
    Hello { interval: Duration },
    Unauthorized,
    Closed,
}

/// Waits for the `HELLO`. Open Live completes the upgrade even for a bad token, so
/// that it can say so in a frame before closing; that is why a refusal arrives here
/// and not from the connect.
async fn greeting(socket: &mut Socket) -> Result<Greeting> {
    let deadline = tokio::time::sleep(HELLO_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(message)) => match classify(&message) {
                    Inbound::Hello { interval } => return Ok(Greeting::Hello { interval }),
                    Inbound::Unauthorized => return Ok(Greeting::Unauthorized),
                    Inbound::Closed => return Ok(Greeting::Closed),
                    Inbound::Rejected(_) | Inbound::Ignore => {}
                },
                Some(Err(err)) => return Err(describe(err)),
                None => return Ok(Greeting::Closed),
            },
            _ = &mut deadline => bail!("Open Live did not greet the heartbeat socket within {}s", HELLO_TIMEOUT.as_secs()),
        }
    }
}

/// What a frame from Open Live means to this end.
#[derive(Debug, PartialEq, Eq)]
enum Inbound {
    Hello {
        interval: Duration,
    },
    Unauthorized,
    /// An in-band error other than a refused token, by its code.
    Rejected(String),
    Closed,
    /// An acknowledgement, the end of the empty snapshot, a ping, or something this
    /// version does not know. By contract unknown frames are ignored.
    Ignore,
}

fn classify(message: &Message) -> Inbound {
    match message {
        Message::Text(text) => classify_text(text),
        Message::Close(Some(frame)) if u16::from(frame.code) == CLOSE_UNAUTHORIZED => {
            Inbound::Unauthorized
        }
        Message::Close(_) => Inbound::Closed,
        _ => Inbound::Ignore,
    }
}

fn classify_text(text: &str) -> Inbound {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Inbound::Ignore;
    };
    match value.get("type").and_then(|t| t.as_str()) {
        Some("HELLO") => Inbound::Hello {
            interval: value
                .get("heartbeatIntervalSeconds")
                .and_then(|s| s.as_f64())
                .filter(|s| s.is_finite() && *s > 0.0)
                .map(|s| Duration::from_secs_f64(s).clamp(MIN_INTERVAL, MAX_INTERVAL))
                .unwrap_or(DEFAULT_INTERVAL),
        },
        Some("ERROR") => match value.get("code").and_then(|c| c.as_str()) {
            Some("unauthorized") => Inbound::Unauthorized,
            Some(code) => Inbound::Rejected(code.to_string()),
            None => Inbound::Rejected("unknown".to_string()),
        },
        Some("ACK") => {
            debug!(ack = %value.get("ackSeq").unwrap_or(&serde_json::Value::Null), "Open Live acknowledged a heartbeat");
            Inbound::Ignore
        }
        _ => Inbound::Ignore,
    }
}

/// The envelope every frame carries: its type, a sequence number that only grows
/// while this process lives, and when it was sent.
#[derive(Serialize)]
struct Frame<'a, T: Serialize> {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    ts: String,
    #[serde(flatten)]
    body: &'a T,
}

#[derive(Serialize)]
struct Offline {
    reason: &'static str,
}

fn frame<T: Serialize>(seq: &mut u64, kind: &'static str, body: &T) -> Result<String> {
    *seq += 1;
    serde_json::to_string(&Frame {
        kind,
        seq: *seq,
        ts: iso8601(SystemTime::now()),
        body,
    })
    .context("encoding a heartbeat frame")
}

async fn send<T: Serialize>(
    socket: &mut Socket,
    seq: &mut u64,
    kind: &'static str,
    body: &T,
) -> Result<()> {
    let text = frame(seq, kind, body)?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(describe)
}

/// An error worth logging: the status or close code, never a request or response
/// body, since the request carries the token.
fn describe(err: WsError) -> anyhow::Error {
    match err {
        WsError::Http(response) => anyhow::anyhow!("HTTP {}", response.status()),
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            anyhow::anyhow!("the socket is closed")
        }
        other => anyhow::anyhow!("{other}"),
    }
}

/// ISO 8601 UTC with milliseconds, as the envelope wants, without a date crate.
fn iso8601(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (h, m, s) = (secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{:03}Z",
        since.subsec_millis()
    )
}

/// Howard Hinnant's algorithm, days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    fn open_live(url: Option<&str>, id: Option<&str>, token: Option<&str>) -> OpenLive {
        OpenLive {
            url: url.map(str::to_string),
            gateway_id: id.map(str::to_string),
            gateway_token: token.map(str::to_string),
            ..OpenLive::default()
        }
    }

    #[test]
    fn the_heartbeat_is_off_without_a_gateway_and_refuses_half_a_credential() {
        let none = open_live(Some("https://open-live.example.com"), None, None);
        assert!(Target::from_config(&none).unwrap().is_none());

        let both = open_live(
            Some("https://open-live.example.com/"),
            Some("gw-1"),
            Some("olgw_v1_secret"),
        );
        let target = Target::from_config(&both).unwrap().expect("configured");
        assert_eq!(target.gateway_id, "gw-1");
        assert_eq!(
            target.socket_url,
            "wss://open-live.example.com/ws/gateways/gw-1/heartbeat"
        );

        let err = Target::from_config(&open_live(Some("https://x"), Some("gw-1"), None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("OLI_OPEN_LIVE_GATEWAY_TOKEN"), "{err}");
        let err = Target::from_config(&open_live(Some("https://x"), None, Some("t")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("gateway_id"), "{err}");
        assert!(Target::from_config(&open_live(None, Some("gw-1"), Some("t"))).is_err());
    }

    #[test]
    fn the_target_masks_its_token_when_printed() {
        let target = Target::from_config(&open_live(
            Some("https://open-live.example.com"),
            Some("gw-1"),
            Some("olgw_v1_secret"),
        ))
        .unwrap()
        .unwrap();
        let printed = format!("{target:?}");
        assert!(printed.contains("gw-1"), "{printed}");
        assert!(!printed.contains("olgw_v1_secret"), "{printed}");
    }

    /// A plain http Open Live, as in local development, gets a plain ws socket.
    #[test]
    fn the_socket_address_follows_the_scheme_and_checks_the_id() {
        assert_eq!(
            socket_url("http://127.0.0.1:3000", "gw-1").unwrap(),
            "ws://127.0.0.1:3000/ws/gateways/gw-1/heartbeat"
        );
        assert!(socket_url("open-live.example.com", "gw-1").is_err());
        assert!(socket_url("https://open-live.example.com", "gw/1").is_err());
        assert!(socket_url("https://open-live.example.com", "gw 1").is_err());
    }

    #[test]
    fn the_greeting_sets_the_interval_and_a_refusal_is_recognised() {
        assert_eq!(
            classify_text(
                r#"{"type":"HELLO","contractVersion":"1.0.0","gatewayId":"gw-1","heartbeatIntervalSeconds":7,"seq":0,"ts":"t"}"#
            ),
            Inbound::Hello {
                interval: Duration::from_secs(7)
            }
        );
        assert_eq!(
            classify_text(r#"{"type":"HELLO","seq":0,"ts":"t"}"#),
            Inbound::Hello {
                interval: DEFAULT_INTERVAL
            }
        );
        assert_eq!(
            classify_text(r#"{"type":"HELLO","heartbeatIntervalSeconds":0.1}"#),
            Inbound::Hello {
                interval: MIN_INTERVAL
            }
        );
        assert_eq!(
            classify_text(r#"{"type":"HELLO","heartbeatIntervalSeconds":3600}"#),
            Inbound::Hello {
                interval: MAX_INTERVAL
            }
        );
        assert_eq!(
            classify_text(r#"{"type":"ERROR","code":"unauthorized"}"#),
            Inbound::Unauthorized
        );
        assert_eq!(
            classify_text(r#"{"type":"ERROR","code":"invalid_frame"}"#),
            Inbound::Rejected("invalid_frame".into())
        );
        assert_eq!(
            classify_text(r#"{"type":"SNAPSHOT_END","seq":1}"#),
            Inbound::Ignore
        );
        assert_eq!(
            classify_text(r#"{"type":"ACK","ackSeq":2}"#),
            Inbound::Ignore
        );
        assert_eq!(
            classify_text(r#"{"type":"START","inputId":"x"}"#),
            Inbound::Ignore,
            "a command is Phase 2 and is ignored, not acted on"
        );
        assert_eq!(classify_text("not json"), Inbound::Ignore);
    }

    #[test]
    fn a_close_with_open_lives_code_is_a_refusal() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;
        let refused = Message::Close(Some(CloseFrame {
            code: CloseCode::from(CLOSE_UNAUTHORIZED),
            reason: "unauthorized".into(),
        }));
        assert_eq!(classify(&refused), Inbound::Unauthorized);
        let normal = Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }));
        assert_eq!(classify(&normal), Inbound::Closed);
        assert_eq!(classify(&Message::Close(None)), Inbound::Closed);
        assert_eq!(classify(&Message::Ping(vec![].into())), Inbound::Ignore);
    }

    fn sample() -> Snapshot {
        Snapshot {
            host: "venue-box".into(),
            strom_version: Some("0.6.8".into()),
            ingest_version: "0.3.0".into(),
            device_count: 2,
            streaming_count: 1,
            inputs: vec![
                InputStatus {
                    input_id: "dev-1".into(),
                    name: "Venue — Camera 1".into(),
                    flow_state: FlowState::Playing,
                    source_id: Some("src-1".into()),
                    uplink: Some(Uplink {
                        bitrate_kbps: 6200,
                        rtt_ms: 18.0,
                        dropped: 0,
                    }),
                },
                InputStatus {
                    input_id: "dev-2".into(),
                    name: "Venue — Camera 2".into(),
                    flow_state: FlowState::Idle,
                    source_id: None,
                    uplink: None,
                },
            ],
        }
    }

    /// The frame is the contract Open Live validates, so its shape is pinned here in
    /// Open Live's own field names.
    #[test]
    fn a_heartbeat_frame_has_the_documented_shape() {
        let mut seq = 0;
        let text = frame(&mut seq, "HEARTBEAT", &sample()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "HEARTBEAT");
        assert_eq!(v["seq"], 1);
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["host"], "venue-box");
        assert_eq!(v["stromVersion"], "0.6.8");
        assert_eq!(v["ingestVersion"], "0.3.0");
        assert_eq!(v["deviceCount"], 2);
        assert_eq!(v["streamingCount"], 1);
        assert_eq!(v["inputs"][0]["inputId"], "dev-1");
        assert_eq!(v["inputs"][0]["flowState"], "playing");
        assert_eq!(v["inputs"][0]["sourceId"], "src-1");
        assert_eq!(v["inputs"][0]["uplink"]["bitrateKbps"], 6200);
        assert_eq!(v["inputs"][0]["uplink"]["rtt_ms"], 18.0);
        assert_eq!(v["inputs"][0]["uplink"]["dropped"], 0);
        assert_eq!(v["inputs"][1]["flowState"], "idle");
        assert_eq!(v["inputs"][1]["sourceId"], serde_json::Value::Null);
        assert_eq!(v["inputs"][1]["uplink"], serde_json::Value::Null);

        let text = frame(
            &mut seq,
            "GATEWAY_OFFLINE",
            &Offline {
                reason: "operator-stop",
            },
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "GATEWAY_OFFLINE");
        assert_eq!(v["seq"], 2, "the sequence keeps counting across frames");
        assert_eq!(v["reason"], "operator-stop");
    }

    #[test]
    fn uplink_figures_come_from_stroms_srt_statistics() {
        let stats: SrtCallerStats = serde_json::from_value(serde_json::json!({
            "send_rate_mbps": 5.987, "rtt_ms": 31.4, "packets_sent_dropped": 3
        }))
        .unwrap();
        assert_eq!(
            Uplink::from_srt(&stats),
            Uplink {
                bitrate_kbps: 5987,
                rtt_ms: 31.4,
                dropped: 3
            }
        );
        assert_eq!(
            Uplink::from_srt(&SrtCallerStats::default()),
            Uplink {
                bitrate_kbps: 0,
                rtt_ms: 0.0,
                dropped: 0
            }
        );
    }

    #[test]
    fn timestamps_are_iso_8601_utc_with_milliseconds() {
        let at = |secs: u64, millis: u32| {
            iso8601(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis.into()))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(1_789_497_072, 400), "2026-09-15T18:31:12.400Z");
        assert_eq!(at(4_102_444_799, 999), "2099-12-31T23:59:59.999Z");
    }

    #[test]
    fn backoff_doubles_from_a_second_and_stops_at_a_minute() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(7), MAX_BACKOFF);
        assert_eq!(backoff(40), MAX_BACKOFF);
    }

    /// A stand-in for Open Live's endpoint on a local port: checks the token the way
    /// Open Live does, greets, and hands every frame it receives to the test.
    #[allow(clippy::result_large_err)]
    async fn fake_open_live(
        expect_token: &'static str,
        interval_secs: f64,
    ) -> (
        Target,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frames, received) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let frames = frames.clone();
                tokio::spawn(async move {
                    let mut authorized = false;
                    let mut socket = tokio_tungstenite::accept_hdr_async(
                        stream,
                        |req: &Request, res: Response| {
                            authorized = req
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                == Some(&format!("Bearer {expect_token}"));
                            assert!(
                                !req.uri().to_string().contains(expect_token),
                                "the token must not travel in the URL"
                            );
                            Ok(res)
                        },
                    )
                    .await
                    .unwrap();
                    let mut seq = 0;
                    let mut reply = |kind: &str, extra: serde_json::Value| {
                        let mut v = serde_json::json!({ "type": kind, "seq": seq, "ts": "2026-09-15T18:31:12.400Z" });
                        v.as_object_mut()
                            .unwrap()
                            .extend(extra.as_object().unwrap().clone());
                        seq += 1;
                        Message::Text(v.to_string().into())
                    };
                    if !authorized {
                        socket
                            .send(reply(
                                "ERROR",
                                serde_json::json!({ "code": "unauthorized" }),
                            ))
                            .await
                            .unwrap();
                        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
                        use tokio_tungstenite::tungstenite::protocol::CloseFrame;
                        socket
                            .close(Some(CloseFrame {
                                code: CloseCode::from(CLOSE_UNAUTHORIZED),
                                reason: "unauthorized".into(),
                            }))
                            .await
                            .ok();
                        return;
                    }
                    socket
                        .send(reply(
                            "HELLO",
                            serde_json::json!({
                                "contractVersion": "1.0.0", "gatewayId": "gw-1",
                                "heartbeatIntervalSeconds": interval_secs
                            }),
                        ))
                        .await
                        .unwrap();
                    socket
                        .send(reply("SNAPSHOT_END", serde_json::json!({})))
                        .await
                        .unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        if let Message::Text(text) = message {
                            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                            let ack = reply("ACK", serde_json::json!({ "ackSeq": v["seq"] }));
                            frames.send(v).unwrap();
                            socket.send(ack).await.ok();
                        }
                    }
                });
            }
        });
        let target = Target::from_config(&open_live(
            Some(&format!("http://127.0.0.1:{port}")),
            Some("gw-1"),
            Some("olgw_v1_secret"),
        ))
        .unwrap()
        .unwrap();
        (target, received)
    }

    async fn next_frame(
        received: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    ) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .expect("a frame within 5s")
            .expect("the fake Open Live is still up")
    }

    /// End to end against a local stand-in: the token goes in the header, the first
    /// frame after the greeting is the online notice, heartbeats follow at the
    /// interval the greeting named and carry the latest snapshot, and a stop sends
    /// the offline notice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_socket_pushes_online_heartbeats_and_offline_in_order() {
        let (target, mut received) = fake_open_live("olgw_v1_secret", 1.0).await;
        let heartbeat = Heartbeat::start(target, sample());

        let online = next_frame(&mut received).await;
        assert_eq!(online["type"], "GATEWAY_ONLINE");
        assert_eq!(online["seq"], 1);
        assert_eq!(online["deviceCount"], 2);
        assert_eq!(online["inputs"][0]["flowState"], "playing");

        let mut changed = sample();
        changed.streaming_count = 0;
        changed.inputs[0].flow_state = FlowState::Idle;
        heartbeat.update(changed);

        let beat = next_frame(&mut received).await;
        assert_eq!(beat["type"], "HEARTBEAT");
        assert_eq!(beat["seq"], 2);
        assert_eq!(
            beat["streamingCount"], 0,
            "a heartbeat carries the latest snapshot"
        );
        assert_eq!(beat["inputs"][0]["flowState"], "idle");

        heartbeat.stop().await;
        let mut last = next_frame(&mut received).await;
        // A heartbeat may have slipped out just before the stop; the offline notice
        // is the last frame either way.
        while last["type"] == "HEARTBEAT" {
            last = next_frame(&mut received).await;
        }
        assert_eq!(last["type"], "GATEWAY_OFFLINE");
        assert_eq!(last["reason"], "operator-stop");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), received.recv())
                .await
                .is_err(),
            "nothing follows the offline notice"
        );
    }

    /// Setup uses this to tell a wrong token from a working one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_probe_passes_a_greeting_and_names_a_refused_token() {
        let (target, _received) = fake_open_live("olgw_v1_secret", 5.0).await;
        probe(&target).await.expect("the right token is greeted");

        let (wrong, _received) = fake_open_live("olgw_v1_other", 5.0).await;
        let err = probe(&wrong).await.unwrap_err().to_string();
        assert!(err.contains("refused"), "{err}");
        assert!(err.contains("gw-1"), "{err}");
        assert!(
            !err.contains("olgw_v1_secret"),
            "the token must not be in the message"
        );
    }
}
