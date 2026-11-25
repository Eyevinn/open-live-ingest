//! Audio levels from the local Strom's event WebSocket (`/api/ws`), for the desktop
//! app's preview (docs/specs/desktop-app.md §5.2).
//!
//! Strom's `builtin.meter` block has no REST read: its values exist only as
//! `MeterData` events, which Strom sends to every subscriber of `/api/ws` along with
//! everything else it has to say (flows, system statistics once a second, and so on).
//! The reader keeps one connection open, decodes only text frames whose `type` is
//! `MeterData`, into its own [`MeterReading`], and drops the rest.
//!
//! It deliberately does not decode `strom_types::StromEvent` as a whole: the
//! WebSocket is not a versioned API, and a variant another Strom version adds must
//! never break the stream. What it cannot read it ignores (no levels, a `debug` log).

use crate::flow::BLOCK_SOUND;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use std::time::Duration;
use strom_types::FlowId;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Readings held for a caller that is behind; newer ones are dropped meanwhile.
const BUFFER: usize = 256;

/// One `MeterData` event of a `sound` block: per channel, in dBFS. Digital silence
/// is -inf, which Strom's JSON writes as `null`: `None` here, to be read as -inf.
#[derive(Debug, Clone, PartialEq)]
pub struct MeterReading {
    pub flow_id: FlowId,
    /// The meter's block id in the flow: always [`BLOCK_SOUND`], as the reader drops
    /// the others.
    pub element_id: String,
    pub rms_db: Vec<Option<f64>>,
    pub peak_db: Vec<Option<f64>>,
}

/// The readings of every flow's `sound` block, for as long as this lives. Which
/// flows matter is the caller's business: their ids change, the connection does not.
/// Dropping it closes the connection.
pub struct MeterReadings {
    readings: mpsc::Receiver<MeterReading>,
    task: JoinHandle<()>,
}

impl MeterReadings {
    /// The next reading. Never `None` while this lives: a lost connection is retried
    /// behind it.
    pub async fn next(&mut self) -> Option<MeterReading> {
        self.readings.recv().await
    }

    /// A reading already received, if there is one.
    pub fn try_next(&mut self) -> Option<MeterReading> {
        self.readings.try_recv().ok()
    }
}

impl Drop for MeterReadings {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Opens the event WebSocket of the Strom at `strom_url` (its HTTP base URL, as in the
/// settings) and keeps it open: `Authorization: Bearer <api_key>` on the upgrade
/// when a key is set, never `?auth_token=`, so the key sits in no URL. A lost or
/// refused connection is retried after 1 s, 2 s, then every 5 s. Strom pings every
/// 15 s; tungstenite answers. Must be called within a Tokio runtime.
pub fn meter_readings(strom_url: &str, api_key: Option<&str>) -> Result<MeterReadings> {
    let request = upgrade_request(strom_url, api_key)?;
    let (tx, readings) = mpsc::channel(BUFFER);
    let task = tokio::spawn(run(request, tx));
    Ok(MeterReadings { readings, task })
}

fn upgrade_request(strom_url: &str, api_key: Option<&str>) -> Result<Request> {
    let base = strom_url.trim_end_matches('/');
    let url = if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/api/ws")
    } else if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/api/ws")
    } else {
        bail!("the Strom URL {base:?} is not http:// or https://");
    };
    let mut request = url
        .into_client_request()
        .context("building the Strom event request")?;
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        let mut bearer = HeaderValue::from_str(&format!("Bearer {key}"))
            .context("the Strom API key is not a valid header value")?;
        bearer.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, bearer);
    }
    Ok(request)
}

/// The wait before connection attempt `failures + 1`: 1 s, 2 s, then 5 s.
fn retry_delay(failures: u32) -> Duration {
    match failures {
        0 => Duration::from_secs(1),
        1 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

async fn run(request: Request, tx: mpsc::Sender<MeterReading>) {
    let mut failures = 0;
    loop {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request.clone()),
        )
        .await
        {
            Ok(Ok((socket, _))) => {
                debug!("Strom event socket open");
                failures = 0;
                if !forward(socket, &tx).await {
                    return;
                }
                debug!("Strom event socket closed; reconnecting");
            }
            Ok(Err(err)) => debug!("Strom event socket: {err}"),
            Err(_) => debug!("Strom event socket: timed out connecting"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(retry_delay(failures)).await;
        failures = failures.saturating_add(1);
    }
}

/// Hands on the `sound` readings until the socket closes (true: reconnect) or the
/// receiver is gone (false: stop).
async fn forward<S>(mut socket: S, tx: &mpsc::Sender<MeterReading>) -> bool
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut unreadable_logged = false;
    while let Some(message) = socket.next().await {
        let text = match message {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(err) => {
                debug!("Strom event socket: {err}");
                break;
            }
        };
        let reading = match decode(&text) {
            Ok(Some(reading)) if reading.element_id == BLOCK_SOUND => reading,
            Ok(_) => continue,
            Err(err) => {
                if !unreadable_logged {
                    debug!("ignoring a MeterData event this version cannot read: {err}");
                    unreadable_logged = true;
                }
                continue;
            }
        };
        match tx.try_send(reading) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
    !tx.is_closed()
}

/// A text frame as a reading: `Ok(None)` when it is no `MeterData` event (another
/// event, a variant this version does not know, Strom's `pong`), Err when it is one
/// whose shape cannot be read.
fn decode(text: &str) -> Result<Option<MeterReading>> {
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct Event {
        data: Data,
    }
    // `decay` is not read.
    #[derive(Deserialize)]
    struct Data {
        flow_id: FlowId,
        element_id: String,
        rms: Vec<Option<f64>>,
        peak: Vec<Option<f64>>,
    }

    match serde_json::from_str::<Kind>(text) {
        Ok(Kind { kind }) if kind == "MeterData" => {}
        _ => return Ok(None),
    }
    let Event { data } = serde_json::from_str(text).context("decoding MeterData")?;
    Ok(Some(MeterReading {
        flow_id: data.flow_id,
        element_id: data.element_id,
        rms_db: data.rms,
        peak_db: data.peak,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::SinkExt;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use strom_types::StromEvent;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use uuid::Uuid;

    const FLOW: Uuid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);

    /// What Strom v0.8.4 sends, from its own type: -inf (a muted channel) is written
    /// as `null`, and `decay` rides along.
    fn meter_event(element_id: &str, rms: Vec<f64>, peak: Vec<f64>) -> String {
        serde_json::to_string(&StromEvent::MeterData {
            flow_id: FLOW,
            element_id: element_id.to_string(),
            decay: peak.clone(),
            rms,
            peak,
        })
        .unwrap()
    }

    #[test]
    fn meter_data_from_the_pinned_strom_decodes_with_silence_as_none() {
        let text = meter_event("sound", vec![f64::NEG_INFINITY, -20.5], vec![-60.0, -3.0]);
        assert!(text.contains("null"), "{text}");
        assert_eq!(
            decode(&text).unwrap(),
            Some(MeterReading {
                flow_id: FLOW,
                element_id: "sound".to_string(),
                rms_db: vec![None, Some(-20.5)],
                peak_db: vec![Some(-60.0), Some(-3.0)],
            })
        );
    }

    #[test]
    fn other_events_and_unknown_variants_are_not_readings() {
        let others = [
            serde_json::to_string(&StromEvent::Ping).unwrap(),
            serde_json::to_string(&StromEvent::FlowStarted { flow_id: FLOW }).unwrap(),
            json!({ "type": "SystemStats", "data": { "cpu_usage": 3.5 } }).to_string(),
            json!({ "type": "SomethingStromAddsLater", "data": { "x": [1, 2] } }).to_string(),
            "pong".to_string(),
            "{}".to_string(),
        ];
        for text in others {
            assert_eq!(decode(&text).unwrap(), None, "{text}");
        }
    }

    /// A later Strom reshaping `MeterData` costs the levels, not the stream.
    #[test]
    fn a_meter_event_of_another_shape_is_an_error_not_a_panic() {
        let text = json!({ "type": "MeterData", "data": { "flow_id": "not-a-uuid" } }).to_string();
        assert!(decode(&text).is_err());
    }

    #[test]
    fn retries_wait_one_then_two_then_five_seconds() {
        let waits: Vec<u64> = (0..5).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 5, 5, 5]);
    }

    #[test]
    fn the_socket_url_follows_the_strom_url_and_the_key_goes_in_a_header() {
        let request = upgrade_request("http://127.0.0.1:8080/", Some("k3y")).unwrap();
        assert_eq!(request.uri().to_string(), "ws://127.0.0.1:8080/api/ws");
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer k3y");
        assert!(request.headers()[AUTHORIZATION].is_sensitive());

        let request = upgrade_request("https://strom.example.com", None).unwrap();
        assert_eq!(request.uri().to_string(), "wss://strom.example.com/api/ws");
        assert!(request.headers().get(AUTHORIZATION).is_none());

        assert!(upgrade_request("strom.example.com:8080", None).is_err());
    }

    /// A stand-in for Strom's `/api/ws`: records each upgrade's path and
    /// `Authorization` header, sends connection `n`'s frames from `script[n]`, then
    /// closes. Past the script it holds the connection open.
    #[allow(clippy::result_large_err)]
    async fn fake_strom(script: Vec<Vec<Message>>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upgrades = Arc::new(Mutex::new(Vec::new()));
        let seen = upgrades.clone();
        tokio::spawn(async move {
            let mut script = script.into_iter();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                let frames = script.next();
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_hdr_async(
                        stream,
                        |req: &Request, res: Response| {
                            let auth = req
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            seen.lock().unwrap().push((req.uri().to_string(), auth));
                            Ok(res)
                        },
                    )
                    .await
                    .unwrap();
                    match frames {
                        Some(frames) => {
                            for frame in frames {
                                socket.send(frame).await.unwrap();
                            }
                            socket.close(None).await.ok();
                        }
                        None => while socket.next().await.is_some() {},
                    }
                });
            }
        });
        (format!("http://{addr}"), upgrades)
    }

    async fn next(readings: &mut MeterReadings) -> MeterReading {
        tokio::time::timeout(Duration::from_secs(10), readings.next())
            .await
            .expect("a reading in time")
            .expect("the reader keeps going")
    }

    /// Everything Strom sends goes to every subscriber; only the `sound` blocks'
    /// `MeterData` comes out, nulls as `None`, and a dropped connection is dialled
    /// again with the same header.
    #[tokio::test]
    async fn the_reader_passes_on_sound_levels_only_and_reconnects() {
        let first = vec![
            Message::Text(serde_json::to_string(&StromEvent::Ping).unwrap().into()),
            Message::Text(
                json!({ "type": "SystemStats", "data": { "cpu_usage": 3.5 } })
                    .to_string()
                    .into(),
            ),
            Message::Text(
                json!({ "type": "SomethingStromAddsLater", "data": {} })
                    .to_string()
                    .into(),
            ),
            Message::Binary(vec![1, 2, 3].into()),
            Message::Text(meter_event("mixer", vec![-10.0], vec![-5.0]).into()),
            Message::Text(
                json!({ "type": "MeterData", "data": { "flow_id": 7 } })
                    .to_string()
                    .into(),
            ),
            Message::Text(
                meter_event(
                    "sound",
                    vec![f64::NEG_INFINITY, f64::NEG_INFINITY],
                    vec![f64::NEG_INFINITY, -42.0],
                )
                .into(),
            ),
        ];
        let second = vec![Message::Text(
            meter_event("sound", vec![-18.0], vec![-6.0]).into(),
        )];
        let (url, upgrades) = fake_strom(vec![first, second]).await;

        let mut readings = meter_readings(&url, Some("secret-key")).unwrap();
        let reading = next(&mut readings).await;
        assert_eq!(reading.flow_id, FLOW);
        assert_eq!(reading.element_id, "sound");
        assert_eq!(reading.rms_db, [None, None]);
        assert_eq!(reading.peak_db, [None, Some(-42.0)]);

        // After the first connection closes: retried after 1 s.
        let reading = next(&mut readings).await;
        assert_eq!(reading.rms_db, [Some(-18.0)]);
        assert_eq!(reading.peak_db, [Some(-6.0)]);
        assert!(readings.try_next().is_none());

        let upgrades = upgrades.lock().unwrap().clone();
        assert!(upgrades.len() >= 2, "{upgrades:?}");
        for (path, auth) in &upgrades {
            assert_eq!(path, "/api/ws");
            assert_eq!(auth, "Bearer secret-key");
        }
    }

    /// No key, no header: Strom without authentication takes the upgrade as is.
    #[tokio::test]
    async fn without_a_key_the_upgrade_carries_no_authorization() {
        let (url, upgrades) = fake_strom(vec![vec![Message::Text(
            meter_event("sound", vec![-30.0], vec![-20.0]).into(),
        )]])
        .await;
        let mut readings = meter_readings(&url, None).unwrap();
        next(&mut readings).await;
        assert_eq!(upgrades.lock().unwrap()[0].1, "");
    }
}
