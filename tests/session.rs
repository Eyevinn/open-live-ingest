//! The session API driven the way an app drives it: `Session::start` with the test
//! pattern, no terminal (stdin is /dev/null), wait for the input to come up from the
//! events alone, then `SessionHandle::stop`, and check the flow is gone from Strom.
//!
//! `start_events_and_stop_against_a_fake_strom` runs in the default `cargo test`
//! against an in-process fake of Strom's REST API: no GStreamer, nothing beyond
//! loopback.
//!
//! The other fake-Strom tests pin the stop contract (a stop asked for during startup,
//! a dropped handle) and, with a fake Open Live, that a failed startup removes the
//! sources it registered, that a stop removes them, and that a session started to
//! keep its sources (the desktop app) leaves them for the next one to adopt with the
//! same id, while its stop (`teardown`, and `teardown_within` when the process is
//! ending) still sweeps the other sources carrying its gateway's name prefix.
//!
//! `start_events_and_stop_against_a_real_strom` (#72 AC2) needs a real Strom, at the
//! release `Cargo.toml` pins, already listening, so it is `#[ignore]`d. Run it with
//!
//! ```text
//! strom --headless --port 8080 &
//! OLI_TEST_STROM_URL=http://127.0.0.1:8080 cargo test --test session -- --ignored
//! ```
//!
//! That Strom needs the GStreamer plugins the test pattern's flow uses: x264 (ugly),
//! `avenc_aac` (libav), `mpegtsmux` and `srtsink` (bad). On a headless Linux box with
//! no GPU and no GL set up, Strom 0.8.2 crashed in its GL probe at start; it ran
//! with Mesa and `GST_GL_PLATFORM=egl GST_GL_WINDOW=surfaceless
//! EGL_PLATFORM=surfaceless`.

use open_live_ingest::config::{Config, UplinkMode};
use open_live_ingest::flow;
use open_live_ingest::gateway::{Event, InputState};
use open_live_ingest::run::Stop;
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
    let state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(state.clone());

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
    assert_eq!(state.lock().unwrap().flows.len(), 1);

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
    assert!(
        state.lock().unwrap().flows.is_empty(),
        "the flow is still there"
    );
}

/// All events until the receiver ends, within `within`.
async fn to_the_end(events: &mut Receiver, within: Duration) -> Vec<Event> {
    let mut seen = Vec::new();
    let all = async {
        while let Some(event) = events.recv().await {
            seen.push(event);
        }
    };
    if tokio::time::timeout(within, all).await.is_err() {
        panic!("the session did not end within {within:?}: {seen:?}");
    }
    seen
}

/// A stop asked for before the session has emitted anything is acted on once the
/// inputs are up: the flow is started, then removed again, and the session ends
/// with `Stopped`.
#[tokio::test]
async fn a_stop_requested_during_startup_is_acted_on_once_the_inputs_are_up() {
    no_terminal_and_a_scratch_user_dir();
    let state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(state.clone());

    let (mut handle, mut events) = Session::start(
        settings(&strom, "gw-session-early-stop", "47220-47229"),
        Inputs::test_pattern(),
    );
    handle.request_stop(Stop::Interrupt);
    let seen = to_the_end(&mut events, Duration::from_secs(60)).await;
    handle.wait().await.expect("the session ended on an error");

    let summary = seen
        .iter()
        .position(|e| matches!(e, Event::Summary { inputs } if inputs.len() == 1))
        .unwrap_or_else(|| panic!("the input never came up: {seen:?}"));
    assert_torn_down(&seen[summary..], "Session test — Test pattern");
    assert!(
        state.lock().unwrap().flows.is_empty(),
        "the flow is still there"
    );
    assert!(
        state.lock().unwrap().created > 0,
        "no flow was ever started"
    );
}

/// Dropping the handle counts as a stop (`Stop::Interrupt`): the session tears down
/// in the background and still reports it to the receiver.
#[tokio::test]
async fn dropping_the_handle_stops_the_session_and_removes_the_flow() {
    no_terminal_and_a_scratch_user_dir();
    let state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(state.clone());

    let (handle, mut events) = Session::start(
        settings(&strom, "gw-session-drop", "47230-47239"),
        Inputs::test_pattern(),
    );
    until_up(&mut events, Duration::from_secs(30)).await;
    assert_eq!(state.lock().unwrap().flows.len(), 1);

    drop(handle);
    let rest = to_the_end(&mut events, Duration::from_secs(60)).await;
    assert_torn_down(&rest, "Session test — Test pattern");
    assert!(
        state.lock().unwrap().flows.is_empty(),
        "the flow is still there"
    );
}

/// Settings that register with a fake Open Live in caller mode: the host and the
/// ports come from its server info, and every input's source is created (and its
/// port assigned) before any flow exists.
fn registering_settings(strom: &str, open_live: &str, gateway_id: &str) -> Config {
    let mut cfg = settings(strom, gateway_id, "47240-47249");
    cfg.uplink.mode = UplinkMode::Caller;
    cfg.uplink.host = None;
    cfg.uplink.public_host = None;
    cfg.uplink.port_range = None;
    cfg.open_live.register = true;
    cfg.open_live.url = Some(open_live.to_string());
    cfg
}

/// #87 review C2: registration on, the input's source is created in Open Live, then
/// Strom refuses the flow, so no input starts. The session fails, and the source it
/// registered is removed again before `Failed`.
#[tokio::test]
async fn a_failed_start_removes_the_sources_it_registered() {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState {
        refuse_flows: true,
        ..StromState::default()
    }));
    let strom = fake_strom(strom_state.clone());
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());

    let (handle, mut events) = Session::start(
        registering_settings(&strom, &open_live, "gw-session-failed-start"),
        Inputs::test_pattern(),
    );
    let seen = to_the_end(&mut events, Duration::from_secs(60)).await;
    let err = handle.wait().await.expect_err("the session started");
    assert_eq!(format!("{err:#}"), "no input could be started");

    let name = "Session test — Test pattern".to_string();
    let at = |wanted: &Event| {
        seen.iter()
            .position(|e| e == wanted)
            .unwrap_or_else(|| panic!("no {wanted:?} in {seen:?}"))
    };
    let registered = seen
        .iter()
        .position(|e| matches!(e, Event::Registered { input, .. } if *input == name))
        .unwrap_or_else(|| panic!("never registered: {seen:?}"));
    let failed = seen
        .iter()
        .position(|e| matches!(e, Event::InputFailed { input, .. } if *input == name))
        .unwrap_or_else(|| panic!("no InputFailed: {seen:?}"));
    let removed = at(&Event::SourceRemoved {
        input: name.clone(),
    });
    assert!(registered < failed && failed < removed, "{seen:?}");
    assert_eq!(
        seen.last(),
        Some(&Event::Failed {
            reason: "no input could be started".to_string()
        }),
        "{seen:?}"
    );
    assert_eq!(open_live_state.lock().unwrap().created, 1);
    assert!(
        open_live_state.lock().unwrap().sources.is_empty(),
        "the source is still in Open Live"
    );
    assert!(strom_state.lock().unwrap().flows.is_empty());
}

/// #87 review C2: registering fails part way, on the second camera. The first
/// camera's source, already created, is removed again before `Failed`.
#[tokio::test]
async fn a_registration_failing_part_way_removes_the_sources_before_it() {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState {
        video: vec![device("cam-a", "Camera A"), device("cam-b", "Camera B")],
        ..StromState::default()
    }));
    let strom = fake_strom(strom_state.clone());
    let open_live_state = Arc::new(Mutex::new(OpenLiveState {
        refuse_create_after: Some(1),
        ..OpenLiveState::default()
    }));
    let open_live = fake_open_live(open_live_state.clone());

    let (handle, mut events) = Session::start(
        registering_settings(&strom, &open_live, "gw-session-partial"),
        Inputs::default(),
    );
    let seen = to_the_end(&mut events, Duration::from_secs(60)).await;
    let err = handle.wait().await.expect_err("the session started");
    let reason = format!("{err:#}");
    assert!(
        reason.starts_with("registering \"Session test — Camera B\" with Open Live"),
        "{reason}"
    );

    assert!(
        seen.contains(&Event::SourceRemoved {
            input: "Session test — Camera A".to_string()
        }),
        "{seen:?}"
    );
    assert!(
        matches!(seen.last(), Some(Event::Failed { reason: r }) if *r == reason),
        "{seen:?}"
    );
    assert_eq!(open_live_state.lock().unwrap().created, 1);
    assert!(
        open_live_state.lock().unwrap().sources.is_empty(),
        "Camera A's source is still in Open Live"
    );
    assert_eq!(strom_state.lock().unwrap().created, 0, "a flow was started");
}

/// Reads events until an input reports `wanted`, and returns them. Any failure event
/// fails the test with its reason.
async fn until_state(events: &mut Receiver, wanted: InputState, within: Duration) -> Vec<Event> {
    let mut seen = Vec::new();
    let wait = async {
        while let Some(event) = events.recv().await {
            if let Event::Failed { reason } | Event::InputFailed { reason, .. } = &event {
                panic!("failed: {reason}");
            }
            let reached = matches!(&event, Event::Input { state, .. } if *state == wanted);
            seen.push(event);
            if reached {
                return;
            }
        }
        panic!("the session ended before an input was {wanted:?}: {seen:?}");
    };
    if tokio::time::timeout(within, wait).await.is_err() {
        panic!("no input was {wanted:?} within {within:?}: {seen:?}");
    }
    seen
}

/// The source the fake Open Live holds under `name`: its id and address.
fn source_named(state: &Mutex<OpenLiveState>, name: &str) -> Option<(String, String)> {
    state
        .lock()
        .unwrap()
        .sources
        .iter()
        .find(|s| s["name"] == name)
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_string(),
                s["address"].as_str().unwrap().to_string(),
            )
        })
}

/// #115: the desktop app's Stop then Go live. Go live registers the source; a
/// production slot takes its id and the feed goes on air; Stop removes the flow but
/// leaves the source, marked inactive; Go live again adopts that same source, with
/// the same id and port, creating none, so the slot still names it and the new flow
/// sends to the port the slot's listener is on.
#[tokio::test]
async fn a_session_keeping_its_sources_is_adopted_with_the_same_id_by_the_next() {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState {
        delivering: true,
        ..StromState::default()
    }));
    let strom = fake_strom(strom_state.clone());
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());
    let settings = registering_settings(&strom, &open_live, "gw-session-keep");
    let name = "Session test — Test pattern";

    // Go live: registered, and on air once bytes move between two polls.
    let (handle, mut events) =
        Session::start_keeping_sources(settings.clone(), Inputs::test_pattern(), None);
    until_state(&mut events, InputState::OnAir, Duration::from_secs(60)).await;
    let (slot, address) = source_named(&open_live_state, name).expect("no source registered");
    assert_eq!(open_live_state.lock().unwrap().created, 1);
    let port = address
        .strip_prefix("srt://:")
        .and_then(|rest| rest.split(['?', '/']).next())
        .unwrap_or_else(|| panic!("no listener port in {address}"))
        .to_string();

    // Stop: the flow goes, the source stays, and is no longer marked active.
    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, name);
    assert!(
        rest.contains(&Event::SourceRetained {
            input: name.to_string()
        }),
        "{rest:?}"
    );
    assert!(
        !rest
            .iter()
            .any(|e| matches!(e, Event::SourceRemoved { .. })),
        "{rest:?}"
    );
    assert!(strom_state.lock().unwrap().flows.is_empty());
    {
        let ol = open_live_state.lock().unwrap();
        assert_eq!(ol.deleted, 0, "a source was deleted");
        assert_eq!(ol.sources.len(), 1);
        assert_eq!(ol.sources[0]["id"], slot.as_str());
        assert_eq!(ol.sources[0]["status"], "inactive");
        assert_eq!(ol.patched.last().map(String::as_str), Some("inactive"));
    }

    // Go live again: the same source, nothing created, and the flow sends to its port.
    strom_state.lock().unwrap().delivering = false;
    let (handle, mut events) =
        Session::start_keeping_sources(settings, Inputs::test_pattern(), None);
    let seen = until_up(&mut events, Duration::from_secs(60)).await;
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Registered { .. })),
        "registered anew: {seen:?}"
    );
    assert_eq!(
        open_live_state.lock().unwrap().created,
        1,
        "a source was created"
    );
    assert_eq!(
        source_named(&open_live_state, name).map(|(id, _)| id),
        Some(slot.clone()),
        "the slot's source id changed"
    );
    let flow = strom_state.lock().unwrap().flows[0].to_string();
    assert!(
        flow.contains(&format!("cloud.example.com:{port}")),
        "the flow does not send to port {port}: {flow}"
    );

    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, name);
    let ol = open_live_state.lock().unwrap();
    assert_eq!(ol.deleted, 0, "a source was deleted");
    assert_eq!(ol.sources.len(), 1);
    assert_eq!(ol.sources[0]["id"], slot.as_str());
}

/// The CLI's `up` (a session not started to keep its sources) still removes them at
/// a stop.
#[tokio::test]
async fn a_stop_removes_the_sources_unless_kept() {
    no_terminal_and_a_scratch_user_dir();
    let strom = fake_strom(Arc::new(Mutex::new(StromState::default())));
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());

    let (handle, mut events) = Session::start(
        registering_settings(&strom, &open_live, "gw-session-remove"),
        Inputs::test_pattern(),
    );
    until_up(&mut events, Duration::from_secs(30)).await;
    assert_eq!(open_live_state.lock().unwrap().sources.len(), 1);
    let rest = stop(handle, &mut events).await;
    let name = "Session test — Test pattern";
    assert_torn_down(&rest, name);
    assert!(
        rest.contains(&Event::SourceRemoved {
            input: name.to_string()
        }),
        "{rest:?}"
    );
    assert!(
        open_live_state.lock().unwrap().sources.is_empty(),
        "the source is still in Open Live"
    );
}

/// #115 review: a session keeping its sources still sweeps the other sources of
/// its gateway at a stop, by name prefix, and leaves its own. `why` picks the
/// teardown: `Stop::Interrupt` (`teardown`, the app's Stop) or `Stop::Ending`
/// (`teardown_within`, Windows ending the process). The stale source is added
/// once the session is on air, so only the teardown's sweep can remove it.
async fn a_kept_session_sweeps_the_other_sources_of_its_gateway(gateway_id: &str, why: Stop) {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState {
        delivering: true,
        ..StromState::default()
    }));
    let strom = fake_strom(strom_state.clone());
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());
    let name = "Session test — Test pattern";
    let stale = "Session test — Old label";
    let foreign = "Another gateway — Test pattern";

    let (mut handle, mut events) = Session::start_keeping_sources(
        registering_settings(&strom, &open_live, gateway_id),
        Inputs::test_pattern(),
        None,
    );
    until_state(&mut events, InputState::OnAir, Duration::from_secs(60)).await;
    let (slot, _) = source_named(&open_live_state, name).expect("no source registered");
    {
        let mut ol = open_live_state.lock().unwrap();
        for (id, source_name) in [("src-stale", stale), ("src-foreign", foreign)] {
            ol.sources.push(json!({
                "id": id,
                "name": source_name,
                "address": "srt://:47259?mode=listener",
                "streamType": "srt",
                "status": "inactive",
            }));
        }
    }

    handle.request_stop(why);
    let rest = to_the_end(&mut events, Duration::from_secs(60)).await;
    tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .expect("the session did not end")
        .expect("the session ended on an error");
    assert_torn_down(&rest, name);
    assert!(
        rest.contains(&Event::SourceRetained {
            input: name.to_string()
        }),
        "{rest:?}"
    );
    assert!(
        rest.contains(&Event::SourceRemoved {
            input: stale.to_string()
        }),
        "the stale source was not swept: {rest:?}"
    );
    assert!(
        !rest.contains(&Event::SourceRemoved {
            input: name.to_string()
        }),
        "{rest:?}"
    );
    let ol = open_live_state.lock().unwrap();
    assert_eq!(ol.deleted, 1, "only the stale source goes");
    let left: Vec<&str> = ol
        .sources
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(left, [slot.as_str(), "src-foreign"]);
    assert_eq!(ol.sources[0]["status"], "inactive");
}

#[tokio::test]
async fn a_kept_session_sweeps_the_other_sources_of_its_gateway_at_stop() {
    a_kept_session_sweeps_the_other_sources_of_its_gateway("gw-session-sweep", Stop::Interrupt)
        .await;
}

#[tokio::test]
async fn a_kept_session_sweeps_the_other_sources_of_its_gateway_when_ending() {
    a_kept_session_sweeps_the_other_sources_of_its_gateway("gw-session-sweep-end", Stop::Ending)
        .await;
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
    // "Up" is the flow running in Strom, which it reports before the pipeline has
    // negotiated anything. Strom keeps a flow `running` even after a pipeline error
    // (a missing AAC encoder, say), so ask it what actually flows: after a moment the
    // muxer must carry H.264 video and AAC audio, and the SRT sink MPEG-TS.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let flows = strom.list_flows().await.unwrap();
    let running = flows
        .iter()
        .find(|f| flow::is_ours(f, gateway_id))
        .expect("the flow is gone from Strom");
    assert!(running.running, "Strom no longer runs the flow");
    let caps = pad_caps(&url, &running.id.to_string()).await;
    let negotiated = |element: &str, direction: &str, media: &str| {
        caps[element].as_array().is_some_and(|pads| {
            pads.iter().any(|p| {
                p["direction"] == direction
                    && p["caps"].as_str().is_some_and(|c| c.starts_with(media))
            })
        })
    };
    for (element, direction, media) in [
        ("uplink:mpegtsmux", "sink", "video/x-h264"),
        ("uplink:mpegtsmux", "sink", "audio/mpeg"),
        ("uplink:srtsink", "sink", "video/mpegts"),
    ] {
        assert!(
            negotiated(element, direction, media),
            "{element} has no {direction} pad carrying {media}; Strom's log names the pipeline error. Pad caps: {caps}"
        );
    }

    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, "Session test — Test pattern");
    assert_eq!(
        ours(&strom.list_flows().await.unwrap()),
        0,
        "the flow is still in Strom"
    );
}

/// The caps negotiated on every pad of a running flow, by element:
/// `GET /api/flows/{id}/pad-caps` on a real Strom.
async fn pad_caps(strom: &str, flow_id: &str) -> Value {
    let mut req = reqwest::Client::new().get(format!("{strom}/api/flows/{flow_id}/pad-caps"));
    if let Ok(key) = std::env::var("OLI_STROM_API_KEY") {
        req = req.bearer_auth(key);
    }
    req.send()
        .await
        .and_then(|r| r.error_for_status())
        .expect("asking Strom for the flow's pad caps")
        .json()
        .await
        .expect("decoding the pad caps")
}

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 server on loopback: one request per connection, routed by a
// closure.
// ---------------------------------------------------------------------------

type Handler = Arc<dyn Fn(&str, &str, Value) -> (u16, Value) + Send + Sync>;

fn serve(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let _ = answer(stream, &handler);
            });
        }
    });
    format!("http://{addr}")
}

fn answer(mut stream: TcpStream, handler: &Handler) -> std::io::Result<()> {
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

    let (status, value) = handler(&method, &path, request);
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

// ---------------------------------------------------------------------------
// A fake Strom: discovery and the flow API, flows kept in memory. Unless told it is
// delivering, nothing receives the uplink, so SRT statistics are absent.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StromState {
    video: Vec<Value>,
    flows: Vec<Value>,
    /// Flows ever created.
    created: usize,
    /// Refuse every new flow, as a Strom missing an encoder does.
    refuse_flows: bool,
    /// Something receives the uplink: SRT statistics with bytes moving on every poll.
    delivering: bool,
    /// SRT statistics polls answered while delivering.
    polls: u64,
}

fn device(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "device_class": "Video/Source",
        "category": "videosource",
        "provider": "fake",
        "properties": {},
        "first_seen_secs_ago": 0,
        "last_seen_secs_ago": 0,
    })
}

fn fake_strom(state: Arc<Mutex<StromState>>) -> String {
    serve(Arc::new(move |method, path, request| {
        route(method, path, request, &mut state.lock().unwrap())
    }))
}

fn route(method: &str, path: &str, request: Value, s: &mut StromState) -> (u16, Value) {
    match (method, path, path.strip_prefix("/api/flows/")) {
        ("GET", "/api/discovery/devices?category=video_source", _) => {
            (200, Value::Array(s.video.clone()))
        }
        ("GET", "/api/discovery/devices?category=audio_source", _) => (200, json!([])),
        ("GET", "/api/flows", _) => (200, json!({ "flows": s.flows })),
        ("POST", "/api/flows", _) if s.refuse_flows => {
            (500, json!({ "error": "no encoder for the test pattern" }))
        }
        ("POST", "/api/flows", _) => {
            s.flows.retain(|f| f["id"] != request["id"]);
            s.flows.push(request.clone());
            s.created += 1;
            (201, json!({ "flow": request }))
        }
        (method, _, Some(rest)) => {
            let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
            let Some(at) = s.flows.iter().position(|f| f["id"] == id) else {
                return (404, json!({ "error": "no such flow" }));
            };
            match (method, action) {
                ("GET", "") => (200, json!({ "flow": s.flows[at] })),
                ("POST", "start") => {
                    s.flows[at]["running"] = json!(true);
                    (200, json!({ "flow": s.flows[at] }))
                }
                ("POST", "stop") => {
                    s.flows[at]["running"] = json!(false);
                    (200, json!({ "flow": s.flows[at] }))
                }
                ("DELETE", "") => {
                    s.flows.remove(at);
                    (204, Value::Null)
                }
                ("GET", "srt-stats") if s.delivering => {
                    s.polls += 1;
                    let stats = json!({
                        "flow_id": id,
                        "stats": { "connections": { "uplink:srtsink": {
                            "role": "sink",
                            "connected": true,
                            "callers": [{ "bytes_sent": s.polls * 100_000, "rtt_ms": 5.0 }],
                        } } },
                    });
                    (200, stats)
                }
                ("GET", "srt-stats") => (404, json!({ "error": "no stats" })),
                _ => (404, Value::Null),
            }
        }
        // An older Strom without the version route; only ever warned about.
        _ => (404, Value::Null),
    }
}

// ---------------------------------------------------------------------------
// A fake Open Live: server info publishing a cloud host and ports, and sources,
// each created source given the lowest published port nobody holds.
// ---------------------------------------------------------------------------

const PUBLISHED_PORTS: [u16; 3] = [47250, 47251, 47252];

#[derive(Default)]
struct OpenLiveState {
    sources: Vec<Value>,
    /// Sources ever created.
    created: usize,
    /// Refuse creating sources once this many have been created.
    refuse_create_after: Option<usize>,
    /// Sources ever deleted.
    deleted: usize,
    /// The `status` of every PATCH, in order.
    patched: Vec<String>,
}

fn fake_open_live(state: Arc<Mutex<OpenLiveState>>) -> String {
    serve(Arc::new(move |method, path, request| {
        let mut s = state.lock().unwrap();
        match (method, path, path.strip_prefix("/api/v1/sources/")) {
            ("GET", "/api/v1/server-info", _) => (
                200,
                json!({
                    "stromHost": "cloud.example.com",
                    "srtPorts": PUBLISHED_PORTS,
                    "srtPortState": "reserved",
                }),
            ),
            ("GET", "/api/v1/sources", _) => (200, Value::Array(s.sources.clone())),
            ("POST", "/api/v1/sources", _) => {
                if s.refuse_create_after.is_some_and(|n| s.created >= n) {
                    return (500, json!({ "error": "database unavailable" }));
                }
                let held: Vec<String> = s
                    .sources
                    .iter()
                    .map(|x| x["address"].as_str().unwrap_or_default().to_string())
                    .collect();
                let port = PUBLISHED_PORTS
                    .iter()
                    .find(|p| !held.iter().any(|a| a.starts_with(&format!("srt://:{p}"))))
                    .expect("a free published port");
                let address = request["address"].as_str().unwrap_or_default().replacen(
                    "srt://:0",
                    &format!("srt://:{port}"),
                    1,
                );
                s.created += 1;
                let source = json!({
                    "id": format!("src-{}", s.created),
                    "name": request["name"],
                    "address": address,
                    "streamType": request["streamType"],
                    "status": request["status"],
                    "latency": request["latency"],
                });
                s.sources.push(source.clone());
                (201, source)
            }
            ("PATCH", _, Some(id)) => {
                let Some(source) = s.sources.iter_mut().find(|x| x["id"] == id) else {
                    return (404, json!({ "error": "Source not found" }));
                };
                for field in ["name", "address", "streamType", "status", "latency"] {
                    if !request[field].is_null() {
                        source[field] = request[field].clone();
                    }
                }
                let source = source.clone();
                s.patched
                    .push(request["status"].as_str().unwrap_or_default().to_string());
                (200, source)
            }
            ("DELETE", _, Some(id)) => {
                s.sources.retain(|x| x["id"] != id);
                s.deleted += 1;
                (204, Value::Null)
            }
            _ => (404, Value::Null),
        }
    }))
}
