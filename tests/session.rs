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

use open_live_ingest::config::{Config, InputSettings, UplinkMode};
use open_live_ingest::flow;
use open_live_ingest::gateway::{Event, InputState};
use open_live_ingest::media::{ExternalMedia, MediaRequest};
use open_live_ingest::run::{Sources, Stop};
use open_live_ingest::session::{Inputs, Receiver, Session, SessionHandle};
use open_live_ingest::strom::StromClient;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use strom_types::api::SrtCallerStats;
use tokio::sync::mpsc;

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

/// A production document as Open Live lists it, holding `sources` in slots
/// `video_in_0`, `video_in_1`, ….
fn production(id: &str, name: &str, status: &str, sources: &[&str]) -> Value {
    json!({
        "_id": id,
        "_rev": "1-x",
        "type": "production",
        "name": name,
        "status": status,
        "sources": sources.iter().enumerate()
            .map(|(n, s)| json!({ "sourceId": s, "mixerInput": format!("video_in_{n}") }))
            .collect::<Vec<_>>(),
    })
}

/// Reads events until one matches `wanted`, and returns them all. A failure fails
/// the test.
async fn until_event(
    events: &mut Receiver,
    what: &str,
    wanted: impl Fn(&Event) -> bool,
    within: Duration,
) -> Vec<Event> {
    let mut seen = Vec::new();
    let wait = async {
        while let Some(event) = events.recv().await {
            if let Event::Failed { reason } | Event::InputFailed { reason, .. } = &event {
                panic!("failed: {reason}");
            }
            let found = wanted(&event);
            seen.push(event);
            if found {
                return;
            }
        }
        panic!("the session ended before {what}: {seen:?}");
    };
    if tokio::time::timeout(within, wait).await.is_err() {
        panic!("no {what} within {within:?}: {seen:?}");
    }
    seen
}

/// Waits until `check` holds of the fake Strom, polling, for at most `within`.
async fn until_strom(
    state: &Mutex<StromState>,
    what: &str,
    check: impl Fn(&StromState) -> bool,
    within: Duration,
) {
    let started = tokio::time::Instant::now();
    while !check(&state.lock().unwrap()) {
        assert!(started.elapsed() < within, "{what} not within {within:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// #130 (OL-63c): Go live arms. The source is registered and no flow runs; the
/// uplink starts once a production holding the source is `active` (not while it is
/// inactive or activating), well within 5 s; it stops, the source kept, when the
/// production is deactivated; it starts again when it is reactivated; and Stop
/// while armed removes nothing but its (absent) flow, keeping the source.
#[tokio::test]
async fn an_armed_session_sends_only_while_a_production_holding_its_source_is_live() {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(strom_state.clone());
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());
    let name = "Session test — Test pattern";
    let set = |status: &str, source: &str| {
        open_live_state.lock().unwrap().productions =
            vec![production("prod-1", "Morning Show", status, &[source])];
    };

    let (handle, mut events) = Session::start_armed(
        registering_settings(&strom, &open_live, "gw-session-armed"),
        Inputs::test_pattern(),
        None,
        Sources::Keep,
    );
    let seen = until_event(
        &mut events,
        "the summary",
        |e| matches!(e, Event::Summary { .. }),
        Duration::from_secs(30),
    )
    .await;
    assert!(
        seen.contains(&Event::Input {
            input: name.to_string(),
            state: InputState::Armed
        }),
        "{seen:?}"
    );
    let (source, _) = source_named(&open_live_state, name).expect("no source registered");
    assert_eq!(strom_state.lock().unwrap().created, 0, "a flow was started");

    // Held, but inactive, then activating: still nothing sent.
    set("inactive", &source);
    tokio::time::sleep(Duration::from_secs(3)).await;
    set("activating", &source);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(strom_state.lock().unwrap().created, 0, "sent before active");

    // Active: the uplink starts within 5 s.
    set("active", &source);
    let activated = tokio::time::Instant::now();
    until_strom(
        &strom_state,
        "the flow running",
        |s| s.created == 1 && s.flows.iter().any(|f| f["running"] == true),
        Duration::from_secs(5),
    )
    .await;
    assert!(activated.elapsed() < Duration::from_secs(5));
    until_event(
        &mut events,
        "ProductionLive",
        |e| matches!(e, Event::ProductionLive { input, productions } if input == name && productions == &["Morning Show".to_string()]),
        Duration::from_secs(5),
    )
    .await;

    // The productions unreadable for a while: the uplink is left as it is.
    open_live_state.lock().unwrap().productions_fail = true;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        strom_state.lock().unwrap().flows.len(),
        1,
        "the flow was disturbed"
    );
    open_live_state.lock().unwrap().productions_fail = false;

    // Deactivated: the flow goes, the source stays, and the input is armed again.
    set("inactive", &source);
    until_event(
        &mut events,
        "NoProductionLive",
        |e| matches!(e, Event::NoProductionLive { input } if input == name),
        Duration::from_secs(5),
    )
    .await;
    until_event(
        &mut events,
        "Armed again",
        |e| {
            matches!(
                e,
                Event::Input {
                    state: InputState::Armed,
                    ..
                }
            )
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        strom_state.lock().unwrap().flows.is_empty(),
        "the flow is still there"
    );
    {
        let ol = open_live_state.lock().unwrap();
        assert_eq!(ol.deleted, 0, "the source was deleted");
        assert!(
            ol.sources.iter().any(|s| s["id"] == source.as_str()),
            "the source is gone"
        );
    }

    // Reactivated: a new flow, without restarting the session.
    set("active", &source);
    until_strom(
        &strom_state,
        "the flow again",
        |s| s.created == 2 && s.flows.len() == 1,
        Duration::from_secs(5),
    )
    .await;

    // Deactivated again, then Stop while armed: no flow, the source kept.
    set("inactive", &source);
    until_strom(
        &strom_state,
        "the flow gone",
        |s| s.flows.is_empty(),
        Duration::from_secs(5),
    )
    .await;
    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, name);
    assert!(
        rest.contains(&Event::SourceRetained {
            input: name.to_string()
        }),
        "{rest:?}"
    );
    let strom_now = strom_state.lock().unwrap();
    assert!(strom_now.flows.is_empty());
    assert_eq!(strom_now.created, 2);
    let ol = open_live_state.lock().unwrap();
    assert_eq!(ol.deleted, 0, "the source was deleted");
    assert_eq!(ol.sources.len(), 1);
    assert_eq!(ol.sources[0]["id"], source.as_str());
}

/// Several productions holding the source: one start however many go live, the
/// uplink kept while any still is, and stopped once none is. A production given the
/// source while live is named (#128).
#[tokio::test]
async fn an_armed_session_starts_once_for_several_productions_and_names_a_late_slot() {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(strom_state.clone());
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());
    let name = "Session test — Test pattern";

    let (handle, mut events) = Session::start_armed(
        registering_settings(&strom, &open_live, "gw-session-armed-two"),
        Inputs::test_pattern(),
        None,
        Sources::Keep,
    );
    until_event(
        &mut events,
        "the summary",
        |e| matches!(e, Event::Summary { .. }),
        Duration::from_secs(30),
    )
    .await;
    let (source, _) = source_named(&open_live_state, name).expect("no source registered");
    let set = |a: &str, b: &str, b_holds: bool| {
        let b_sources: &[&str] = if b_holds { &[source.as_str()] } else { &[] };
        open_live_state.lock().unwrap().productions = vec![
            production("prod-a", "Show A", a, &[source.as_str()]),
            production("prod-b", "Show B", b, b_sources),
        ];
    };

    // B live without the source, A inactive: nothing sent.
    set("inactive", "active", false);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(strom_state.lock().unwrap().created, 0);
    // The source is put in B while B is live: sent, and B named.
    set("inactive", "active", true);
    until_event(
        &mut events,
        "SlotAddedWhileLive",
        |e| matches!(e, Event::SlotAddedWhileLive { productions, .. } if productions == &["Show B".to_string()]),
        Duration::from_secs(5),
    )
    .await;
    until_strom(
        &strom_state,
        "the flow",
        |s| s.flows.len() == 1,
        Duration::from_secs(5),
    )
    .await;
    // A goes live too: no second start.
    set("active", "active", true);
    tokio::time::sleep(Duration::from_secs(3)).await;
    // B deactivated: still live in A, the flow kept; B no longer named.
    set("active", "inactive", true);
    until_event(
        &mut events,
        "the late slot cleared",
        |e| matches!(e, Event::SlotAddedWhileLive { productions, .. } if productions.is_empty()),
        Duration::from_secs(5),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    {
        let s = strom_state.lock().unwrap();
        assert_eq!(s.created, 1, "started more than once");
        assert_eq!(s.flows.len(), 1, "stopped while a production was live");
    }
    // Neither live: stopped.
    set("inactive", "inactive", true);
    until_strom(
        &strom_state,
        "the flow gone",
        |s| s.flows.is_empty(),
        Duration::from_secs(5),
    )
    .await;

    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, name);
    let ol = open_live_state.lock().unwrap();
    assert_eq!(ol.deleted, 0);
    assert_eq!(ol.sources.len(), 1);
}

/// Arming needs Open Live's productions: with registration off the session ends at
/// once, starting nothing.
#[tokio::test]
async fn an_armed_session_without_open_live_fails_at_once() {
    no_terminal_and_a_scratch_user_dir();
    let strom_state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(strom_state.clone());
    let (handle, mut events) = Session::start_armed(
        settings(&strom, "gw-session-armed-none", "47320-47329"),
        Inputs::test_pattern(),
        None,
        Sources::Keep,
    );
    let seen = to_the_end(&mut events, Duration::from_secs(30)).await;
    assert!(handle.wait().await.is_err());
    let Some(Event::Failed { reason }) = seen.last() else {
        panic!("{seen:?}");
    };
    assert!(reason.contains("[open_live] register is off"), "{reason}");
    assert_eq!(strom_state.lock().unwrap().created, 0);
}

/// A port where nothing may connect: the Strom URL of a session whose media the host
/// does. `assert_untouched` fails if anything connected to it.
struct NoStrom {
    listener: TcpListener,
}

impl NoStrom {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        NoStrom { listener }
    }

    fn url(&self) -> String {
        format!("http://{}", self.listener.local_addr().unwrap())
    }

    fn assert_untouched(&self) {
        match self.listener.accept() {
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("something asked the Strom: {other:?}"),
        }
    }
}

/// Armed settings for a phone: registering with the fake Open Live, one camera and
/// microphone as the app writes them (AVFoundation ids, no lookup), and a Strom URL
/// nothing may ask.
fn phone_settings(strom: &str, open_live: &str, gateway_id: &str) -> Config {
    let mut cfg = registering_settings(strom, open_live, gateway_id);
    cfg.inputs = vec![InputSettings {
        device: "com.apple.avfoundation.avcapturedevice.built-in_video:0".to_string(),
        audio: Some("BuiltInMicrophoneDevice".to_string()),
        label: Some("iPhone".to_string()),
    }];
    cfg
}

/// The next request the session hands the host, within `within`.
async fn next_request(
    requests: &mut mpsc::UnboundedReceiver<MediaRequest>,
    what: &str,
    within: Duration,
) -> MediaRequest {
    match tokio::time::timeout(within, requests.recv()).await {
        Ok(Some(request)) => request,
        Ok(None) => panic!("the session dropped its media before {what}"),
        Err(_) => panic!("no {what} within {within:?}"),
    }
}

/// OL-64a: an armed session whose media the host does (`ExternalMedia`, as on the
/// phone), with a fake host. The source is registered and nothing is asked of the
/// host while no production holding it is active; `active` hands the host a start
/// for the input, with its camera, microphone and endpoint; leaving `active` a stop,
/// the source kept; `active` again a new start; and Stop a stop, the source kept
/// (#115). No Strom is asked at any point.
#[tokio::test]
async fn an_armed_session_through_external_media_hands_start_and_stop_to_the_host() {
    no_terminal_and_a_scratch_user_dir();
    let no_strom = NoStrom::new();
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());
    let name = "Session test — iPhone";
    let set = |status: &str, source: &str| {
        open_live_state.lock().unwrap().productions =
            vec![production("prod-1", "Morning Show", status, &[source])];
    };

    let (media, mut requests) = ExternalMedia::new();
    let host = media.clone();
    let (handle, mut events) = Session::start_armed_with(
        media,
        phone_settings(&no_strom.url(), &open_live, "gw-session-external"),
        Inputs::default(),
        None,
        Sources::Keep,
    );
    let seen = until_event(
        &mut events,
        "the summary",
        |e| matches!(e, Event::Summary { .. }),
        Duration::from_secs(30),
    )
    .await;
    assert!(
        seen.contains(&Event::Input {
            input: name.to_string(),
            state: InputState::Armed
        }),
        "{seen:?}"
    );
    let (source, address) = source_named(&open_live_state, name).expect("no source registered");

    // Held, but inactive: nothing asked of the host.
    set("inactive", &source);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        requests.try_recv().is_err(),
        "the host was asked before active"
    );

    // Active: the host is asked to start, with what it needs to send.
    set("active", &source);
    let MediaRequest::Start { input, video } =
        next_request(&mut requests, "a start", Duration::from_secs(5)).await
    else {
        panic!("not a start");
    };
    assert_eq!(input.name, name);
    let flow::Source::Device {
        device_id,
        audio_device,
        ..
    } = &input.source
    else {
        panic!("not a camera: {:?}", input.source);
    };
    assert_eq!(
        device_id,
        "com.apple.avfoundation.avcapturedevice.built-in_video:0"
    );
    assert_eq!(audio_device.as_deref(), Some("BuiltInMicrophoneDevice"));
    assert_eq!(input.endpoint.mode, UplinkMode::Caller);
    assert_eq!(input.endpoint.cloud_host, "cloud.example.com");
    assert!(
        address.starts_with(&format!("srt://:{}", input.endpoint.port)),
        "the uplink port {} is not the registered one: {address}",
        input.endpoint.port
    );
    assert_eq!(input.endpoint.passphrase(), None);
    assert_eq!(input.endpoint.pbkeylen(), None);
    assert!(input
        .endpoint
        .venue_uri()
        .starts_with("srt://cloud.example.com:"));
    assert_eq!(video.bitrate_kbps, Config::default().video.bitrate_kbps);
    host.report(&input.id, true, None);
    until_event(
        &mut events,
        "ProductionLive",
        |e| matches!(e, Event::ProductionLive { input, .. } if input == name),
        Duration::from_secs(5),
    )
    .await;

    // Deactivated: the host is asked to stop; the source stays.
    set("inactive", &source);
    let stop_request = next_request(&mut requests, "a stop", Duration::from_secs(5)).await;
    assert!(
        matches!(&stop_request, MediaRequest::Stop { input_id } if *input_id == input.id),
        "{stop_request:?}"
    );
    until_event(
        &mut events,
        "NoProductionLive",
        |e| matches!(e, Event::NoProductionLive { input } if input == name),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        open_live_state.lock().unwrap().deleted,
        0,
        "the source was deleted"
    );

    // Reactivated: started again, without restarting the session.
    set("active", &source);
    let again = next_request(&mut requests, "a start again", Duration::from_secs(5)).await;
    assert!(
        matches!(&again, MediaRequest::Start { input: i, .. } if i.id == input.id),
        "{again:?}"
    );
    host.report(&input.id, true, None);

    // Stop while sending: the host is asked to stop, and the source is kept.
    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, name);
    assert!(
        rest.contains(&Event::SourceRetained {
            input: name.to_string()
        }),
        "{rest:?}"
    );
    let last = next_request(&mut requests, "the stop's stop", Duration::from_secs(5)).await;
    assert!(
        matches!(&last, MediaRequest::Stop { input_id } if *input_id == input.id),
        "{last:?}"
    );
    {
        let ol = open_live_state.lock().unwrap();
        assert_eq!(ol.deleted, 0, "the source was deleted");
        assert_eq!(ol.sources.len(), 1);
        assert_eq!(ol.sources[0]["id"], source.as_str());
    }
    no_strom.assert_untouched();
}

/// OL-64a: the stall restart (#111, #127) through `ExternalMedia`. The host reports
/// an uplink whose sender drops all it sends, the counters moving; two stalled
/// intervals after the first sample, the session asks the host to restart it.
#[tokio::test]
async fn a_stalled_uplink_through_external_media_is_restarted_by_the_host() {
    no_terminal_and_a_scratch_user_dir();
    let no_strom = NoStrom::new();
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(open_live_state.clone());
    let name = "Session test — iPhone";

    let (media, mut requests) = ExternalMedia::new();
    let host = media.clone();
    let (handle, mut events) = Session::start_armed_with(
        media,
        phone_settings(&no_strom.url(), &open_live, "gw-session-external-stall"),
        Inputs::default(),
        None,
        Sources::Keep,
    );
    until_event(
        &mut events,
        "the summary",
        |e| matches!(e, Event::Summary { .. }),
        Duration::from_secs(30),
    )
    .await;
    let (source, _) = source_named(&open_live_state, name).expect("no source registered");
    open_live_state.lock().unwrap().productions =
        vec![production("prod-1", "Morning Show", "active", &[&source])];
    let MediaRequest::Start { input, .. } =
        next_request(&mut requests, "a start", Duration::from_secs(5)).await
    else {
        panic!("not a start");
    };

    // Connected, and dropping all it sends: #111 run B's rate, reported every second.
    let reporting = {
        let host = host.clone();
        let id = input.id.clone();
        tokio::spawn(async move {
            for n in 1u64.. {
                let sample = SrtCallerStats {
                    packets_sent: Some(n * 6_065),
                    packets_sent_dropped: Some(n * 6_065),
                    bytes_sent: Some(n * 6_065 * 1_316),
                    ..SrtCallerStats::default()
                };
                host.report(&id, true, Some(sample));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    };
    // A baseline poll, then two stalled ones: well within four ticks.
    let restart = next_request(&mut requests, "a restart", Duration::from_secs(45)).await;
    reporting.abort();
    assert!(
        matches!(&restart, MediaRequest::Restart { input_id } if *input_id == input.id),
        "{restart:?}"
    );
    until_event(
        &mut events,
        "the restart's warning",
        |e| matches!(e, Event::Warning { message } if message.contains("restart 1 of 4")),
        Duration::from_secs(5),
    )
    .await;
    until_event(
        &mut events,
        "Starting again",
        |e| {
            matches!(
                e,
                Event::Input {
                    state: InputState::Starting,
                    ..
                }
            )
        },
        Duration::from_secs(5),
    )
    .await;

    let rest = stop(handle, &mut events).await;
    assert_torn_down(&rest, name);
    no_strom.assert_untouched();
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
    /// What `GET /api/v1/productions` lists, CouchDB documents as Open Live returns.
    productions: Vec<Value>,
    /// Answer the production list with a 503.
    productions_fail: bool,
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
            ("GET", "/api/v1/productions", _) if s.productions_fail => {
                (503, json!({ "error": "Database unavailable" }))
            }
            ("GET", "/api/v1/productions", _) => (200, Value::Array(s.productions.clone())),
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
