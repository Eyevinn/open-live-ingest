//! The session API driven the way an app drives it: `Session::start` with the test
//! pattern, no terminal (stdin is /dev/null), wait for the input to come up from the
//! events alone, then `SessionHandle::stop`, and check the flow is gone from Strom.
//!
//! `start_events_and_stop_against_a_fake_strom` runs in the default `cargo test`
//! against an in-process fake of Strom's REST API: no GStreamer, nothing beyond
//! loopback.
//!
//! `start_events_and_stop_against_a_real_strom` (#72 AC2) needs a real Strom, at the
//! release `Cargo.toml` pins, already listening, so it is `#[ignore]`d. Run it with
//!
//! ```text
//! strom --headless --port 8080 &
//! OLI_TEST_STROM_URL=http://127.0.0.1:8080 cargo test --test session -- --ignored
//! ```

use open_live_ingest::config::{Config, UplinkMode};
use open_live_ingest::flow;
use open_live_ingest::gateway::{Event, InputState};
use open_live_ingest::session::{Inputs, Receiver, Session, SessionHandle};
use open_live_ingest::strom::StromClient;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

/// Runs once per test binary, before any session: a scratch user directory, so
/// the pidfile and settings lookups never touch the real one, and stdin on
/// /dev/null, so nothing can be waiting on a terminal.
fn no_terminal_and_a_scratch_user_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("oli-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for var in ["HOME", "XDG_CONFIG_HOME", "APPDATA"] {
            std::env::set_var(var, &dir);
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let null = std::fs::File::open("/dev/null").unwrap();
            // SAFETY: points fd 0 at /dev/null; nothing in this process holds a
            // reference into the old stdin.
            assert_eq!(unsafe { libc::dup2(null.as_raw_fd(), 0) }, 0);
        }
    });
}

/// Settings for a test-pattern session against `strom`, with no Open Live: the
/// feed listens on a local SRT port, which is all a Strom needs to run the flow.
fn settings(strom: &str, gateway_id: &str, ports: &str) -> Config {
    let mut cfg = Config::default();
    cfg.gateway.id = Some(gateway_id.to_string());
    cfg.gateway.name = "Session test".to_string();
    cfg.strom.url = strom.to_string();
    cfg.strom.manage = false;
    cfg.open_live.register = false;
    cfg.uplink.mode = UplinkMode::Listener;
    cfg.uplink.host = Some("127.0.0.1".to_string());
    cfg.uplink.public_host = Some("127.0.0.1".to_string());
    cfg.uplink.port_range = Some(ports.to_string());
    cfg
}

/// Reads events until the test pattern's flow is up in Strom (waiting for a
/// receiver, or on air if something receives it), and returns them. Any failure
/// event fails the test with its reason.
async fn until_up(events: &mut Receiver, within: Duration) -> Vec<Event> {
    let mut seen = Vec::new();
    let wait = async {
        while let Some(event) = events.recv().await {
            match &event {
                Event::Failed { reason } => panic!("the session failed: {reason}"),
                Event::InputFailed { input, reason } => panic!("{input} failed: {reason}"),
                Event::Input {
                    state: InputState::Failed { reason },
                    input,
                } => panic!("{input} failed: {reason}"),
                _ => {}
            }
            let up = matches!(
                event,
                Event::Input {
                    state: InputState::Waiting | InputState::OnAir,
                    ..
                }
            );
            seen.push(event);
            if up {
                return;
            }
        }
        panic!("the session ended before the input came up: {seen:?}");
    };
    if tokio::time::timeout(within, wait).await.is_err() {
        panic!("the input did not come up within {within:?}");
    }
    seen
}

/// Stops the session and returns the events of the teardown, through the last.
async fn stop(handle: SessionHandle, events: &mut Receiver) -> Vec<Event> {
    tokio::time::timeout(Duration::from_secs(60), handle.stop())
        .await
        .expect("stop did not finish within a minute")
        .expect("the session ended on an error");
    let mut rest = Vec::new();
    while let Some(event) = events.recv().await {
        rest.push(event);
    }
    rest
}

/// What every stop must report, in order: stopping, the flow removed, stopped.
fn assert_torn_down(rest: &[Event], input: &str) {
    let at = |wanted: &Event| {
        rest.iter()
            .position(|e| e == wanted)
            .unwrap_or_else(|| panic!("no {wanted:?} in {rest:?}"))
    };
    let stopping = at(&Event::Stopping);
    let removed = at(&Event::FlowRemoved {
        input: input.to_string(),
    });
    assert!(stopping < removed, "{rest:?}");
    assert_eq!(rest.last(), Some(&Event::Stopped), "{rest:?}");
}

#[tokio::test]
async fn start_events_and_stop_against_a_fake_strom() {
    no_terminal_and_a_scratch_user_dir();
    let flows = Arc::new(Mutex::new(Vec::new()));
    let strom = fake_strom(flows.clone());

    let (handle, mut events) = Session::start(
        settings(&strom, "gw-session-fake", "47210-47219"),
        Inputs::test_pattern(),
    );
    let seen = until_up(&mut events, Duration::from_secs(30)).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Summary { inputs } if inputs.len() == 1)),
        "{seen:?}"
    );
    assert_eq!(flows.lock().unwrap().len(), 1);

    // The stats of the same tick follow the state change.
    let stats = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("no stats")
        .expect("ended");
    let Event::Stats { inputs } = stats else {
        panic!("not stats: {stats:?}");
    };
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].input, "Session test — Test pattern");
    assert_eq!(inputs[0].state, InputState::Waiting);
    assert_eq!(inputs[0].uplink, None);

    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, "Session test — Test pattern");
    assert!(flows.lock().unwrap().is_empty(), "the flow is still there");
}

/// #72 AC2, against a real Strom; see the module documentation.
#[tokio::test]
#[ignore = "needs a real Strom at OLI_TEST_STROM_URL (default http://127.0.0.1:8080)"]
async fn start_events_and_stop_against_a_real_strom() {
    no_terminal_and_a_scratch_user_dir();
    let url =
        std::env::var("OLI_TEST_STROM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let gateway_id = "gw-session-real";
    let strom = StromClient::new(&url, std::env::var("OLI_STROM_API_KEY").ok().as_deref())
        .expect("a Strom client");

    let (handle, mut events) = Session::start(
        settings(&url, gateway_id, "47310-47319"),
        Inputs::test_pattern(),
    );
    // A cold Strom builds its first pipeline slowly.
    until_up(&mut events, Duration::from_secs(120)).await;
    let ours = |flows: &[strom_types::Flow]| {
        flows
            .iter()
            .filter(|f| flow::is_ours(f, gateway_id))
            .count()
    };
    assert_eq!(ours(&strom.list_flows().await.unwrap()), 1);

    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, "Session test — Test pattern");
    assert_eq!(
        ours(&strom.list_flows().await.unwrap()),
        0,
        "the flow is still in Strom"
    );
}

// ---------------------------------------------------------------------------
// A fake Strom: discovery and the flow API, flows kept in memory. Nothing ever
// receives the uplink, so SRT statistics are always absent.
// ---------------------------------------------------------------------------

fn fake_strom(flows: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let flows = flows.clone();
            std::thread::spawn(move || {
                let _ = answer(stream, &flows);
            });
        }
    });
    format!("http://{addr}")
}

fn answer(mut stream: TcpStream, flows: &Mutex<Vec<Value>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    let (status, value) = route(&method, &path, request, &mut flows.lock().unwrap());
    let body = if value.is_null() {
        String::new()
    } else {
        value.to_string()
    };
    write!(
        stream,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn route(method: &str, path: &str, request: Value, flows: &mut Vec<Value>) -> (u16, Value) {
    match (method, path, path.strip_prefix("/api/flows/")) {
        ("GET", "/api/discovery/devices?category=video_source", _)
        | ("GET", "/api/discovery/devices?category=audio_source", _) => (200, json!([])),
        ("GET", "/api/flows", _) => (200, json!({ "flows": flows })),
        ("POST", "/api/flows", _) => {
            flows.retain(|f| f["id"] != request["id"]);
            flows.push(request.clone());
            (201, json!({ "flow": request }))
        }
        (method, _, Some(rest)) => {
            let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
            let Some(at) = flows.iter().position(|f| f["id"] == id) else {
                return (404, json!({ "error": "no such flow" }));
            };
            match (method, action) {
                ("GET", "") => (200, json!({ "flow": flows[at] })),
                ("POST", "start") => {
                    flows[at]["running"] = json!(true);
                    (200, json!({ "flow": flows[at] }))
                }
                ("POST", "stop") => {
                    flows[at]["running"] = json!(false);
                    (200, json!({ "flow": flows[at] }))
                }
                ("DELETE", "") => {
                    flows.remove(at);
                    (204, Value::Null)
                }
                ("GET", "srt-stats") => (404, json!({ "error": "no stats" })),
                _ => (404, Value::Null),
            }
        }
        // An older Strom without the version route; only ever warned about.
        _ => (404, Value::Null),
    }
}
