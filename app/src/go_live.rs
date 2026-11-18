//! Step 3 of the window, "Go live / Stop" (docs/specs/desktop-app.md, sections 4, 5
//! and 9.3).
//!
//! [`GoLive`] runs one session at a time. Go live first makes the checks section 4
//! lists, each failure one line from `messages` (section 10): the settings file, at
//! least one input ticked, no other gateway live on this machine (`gateway.pid`, a CLI
//! `up` or another app), camera and microphone access (macOS), and a Strom left behind
//! by a crash reclaimed rather than adopted. Then it starts [`Session`] and turns its
//! events into the page's: one row per input with its state and bitrate, banners for
//! session-wide notices, and teardown progress. Stop runs the session's own teardown
//! ([`SessionHandle::request_stop`]) and the rows show it.
//!
//! [`Rows`] is the mapping from library events to page events, pure apart from the
//! clock it is handed, so every state and notice is tested without a server.

use crate::ipc::{Deliver, Event, Reply};
use crate::media_access::{self, Needs};
use crate::messages::{self, Context, Failure, Media, Message, WarningKind};
use open_live_ingest::api::{self, OscCredential};
use open_live_ingest::config::{self, Config};
use open_live_ingest::gateway::{self, Events, InputState};
use open_live_ingest::keychain::SecretStore;
use open_live_ingest::local_strom;
use open_live_ingest::run::{self, Stop};
use open_live_ingest::session::{Inputs, Session, SessionHandle};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::instrument::WithSubscriber;

/// How long an input may run with nothing reaching the cloud before its row says so.
pub const NOTHING_DELIVERED_AFTER: Duration = Duration::from_secs(30);

/// The `[[input]]` device that stands for the test pattern (#94).
const TEST_PATTERN: &str = "test";

/// An input's state as its row shows it (section 5). Strom starting is session-wide:
/// the `strom` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    Registering,
    Connecting,
    Live,
    Failed,
    Stopping,
    /// Its flow is gone, after Stop.
    Stopped,
    /// A saved device that is not present now; skipped, the others go live.
    NotConnected,
}

#[derive(Debug, Clone)]
struct Row {
    input: String,
    state: RowState,
    message: Option<Message>,
    /// Since when it has been connecting with nothing delivered.
    connecting_since: Option<Instant>,
    bitrate_kbps: Option<u64>,
}

impl Row {
    fn event(&self) -> Event {
        Event::new(
            "input",
            json!({
                "input": self.input,
                "state": self.state,
                "message": self.message.as_ref().map(|m| &m.message),
                "action": self.message.as_ref().and_then(|m| m.action),
            }),
        )
    }
}

/// Session-wide notices, shown in the banner above the rows.
const OPEN_LIVE_UNREACHABLE: &str = "open_live_unreachable";
const HEARTBEAT_REFUSED: &str = "heartbeat_refused";
const SIGN_IN_ENDS: &str = "sign_in_ends";

fn notice(kind: &str, message: &Message) -> Event {
    Event::new(
        "notice",
        json!({ "kind": kind, "message": message.message, "action": message.action }),
    )
}

/// The page's view of one session, built from the library's events.
#[derive(Debug)]
pub struct Rows {
    ctx: Context,
    rows: Vec<Row>,
    notices: Vec<&'static str>,
    /// Open Live could not be reached in the tick now ending.
    unreachable_this_tick: bool,
    stopping: Option<(usize, usize)>,
    left_behind: Vec<String>,
}

impl Rows {
    pub fn new(ctx: Context) -> Self {
        Rows {
            ctx,
            rows: Vec::new(),
            notices: Vec::new(),
            unreachable_this_tick: false,
            stopping: None,
            left_behind: Vec::new(),
        }
    }

    fn row(&mut self, input: &str) -> &mut Row {
        match self.rows.iter().position(|r| r.input == input) {
            Some(at) => &mut self.rows[at],
            None => {
                self.rows.push(Row {
                    input: input.to_string(),
                    state: RowState::Registering,
                    message: None,
                    connecting_since: None,
                    bitrate_kbps: None,
                });
                self.rows.last_mut().expect("just pushed")
            }
        }
    }

    /// Sets a row's state and message; the row's event when that changed anything.
    fn set(
        &mut self,
        input: &str,
        state: RowState,
        message: Option<Message>,
        now: Instant,
    ) -> Option<Event> {
        let new = !self.rows.iter().any(|r| r.input == input);
        let row = self.row(input);
        if state == RowState::Connecting {
            row.connecting_since.get_or_insert(now);
        } else {
            row.connecting_since = None;
        }
        // A connecting row keeps the hint it already has.
        let message = match (state, message) {
            (RowState::Connecting, None) if row.state == RowState::Connecting => {
                row.message.clone()
            }
            (_, message) => message,
        };
        if !new && row.state == state && row.message == message {
            return None;
        }
        row.state = state;
        row.message = message;
        Some(row.event())
    }

    fn add_notice(&mut self, kind: &'static str, message: &Message) -> Option<Event> {
        if self.notices.contains(&kind) {
            return None;
        }
        self.notices.push(kind);
        Some(notice(kind, message))
    }

    /// What the page is told about one library event, at `now`.
    pub fn on_event(&mut self, event: &gateway::Event, now: Instant) -> Vec<Event> {
        use gateway::Event as E;
        let mut out = Vec::new();
        match event {
            E::StromStarting { .. } => {
                out.push(Event::new("strom", json!({ "state": "starting" })))
            }
            E::StromReady { .. } => out.push(Event::new("strom", json!({ "state": "ready" }))),
            E::StromStopped => out.push(Event::new("strom", json!({ "state": "stopped" }))),
            E::Registering { input } => {
                out.extend(self.set(input, RowState::Registering, None, now))
            }
            E::InputFailed { input, reason } => {
                let message = messages::for_input_failure(input, reason);
                out.extend(self.set(input, RowState::Failed, Some(message), now));
            }
            E::Summary { inputs } => {
                for started in inputs {
                    out.extend(self.set(&started.name, RowState::Connecting, None, now));
                }
            }
            E::Input { input, state } => {
                let (state, message) = match state {
                    InputState::OnAir => (RowState::Live, None),
                    InputState::Failed { reason } => (
                        RowState::Failed,
                        Some(messages::for_input_failure(input, reason)),
                    ),
                    InputState::Starting | InputState::Waiting | InputState::Unreachable => {
                        (RowState::Connecting, None)
                    }
                };
                out.extend(self.set(input, state, message, now));
            }
            E::Stats { inputs } => {
                for stats in inputs {
                    let row = self.row(&stats.input);
                    row.bitrate_kbps = stats.uplink.map(|u| u.bitrate_kbps);
                }
                // A row connecting for 30 s with nothing delivered says why that may be.
                let hint = Failure::NothingDelivered.message();
                for row in &mut self.rows {
                    let waited = row
                        .connecting_since
                        .is_some_and(|since| now.duration_since(since) >= NOTHING_DELIVERED_AFTER);
                    if waited && row.message.is_none() {
                        row.message = Some(hint.clone());
                        out.push(row.event());
                    }
                }
                // Open Live reached again this tick: its banner clears itself.
                if !std::mem::take(&mut self.unreachable_this_tick)
                    && self.notices.contains(&OPEN_LIVE_UNREACHABLE)
                {
                    self.notices.retain(|k| *k != OPEN_LIVE_UNREACHABLE);
                    out.push(Event::new(
                        "notice",
                        json!({ "clear": OPEN_LIVE_UNREACHABLE }),
                    ));
                }
                out.push(Event::new(
                    "stats",
                    json!({
                        "inputs": inputs.iter().map(|s| json!({
                            "input": s.input,
                            "bitrate_kbps": s.uplink.map(|u| u.bitrate_kbps),
                            "rtt_ms": s.uplink.map(|u| u.rtt_ms),
                            "dropped": s.uplink.map(|u| u.dropped),
                        })).collect::<Vec<Value>>()
                    }),
                ));
            }
            E::Warning { message } => match messages::for_warning(message) {
                Some((WarningKind::OpenLiveUnreachable, shown)) => {
                    self.unreachable_this_tick = true;
                    out.extend(self.add_notice(OPEN_LIVE_UNREACHABLE, &shown));
                }
                Some((WarningKind::GatewayTokenRefused, shown)) => {
                    out.extend(self.add_notice(HEARTBEAT_REFUSED, &shown));
                }
                Some((WarningKind::NotConnected { device }, shown)) => {
                    out.extend(self.set(&device, RowState::NotConnected, Some(shown), now));
                }
                None => {}
            },
            E::OscSignInEnds { at } => out
                .extend(self.add_notice(SIGN_IN_ENDS, &Failure::SignInEnds { at: *at }.message())),
            E::Stopping => {
                let mut total = 0;
                for at in 0..self.rows.len() {
                    if matches!(
                        self.rows[at].state,
                        RowState::Registering | RowState::Connecting | RowState::Live
                    ) {
                        total += 1;
                        let input = self.rows[at].input.clone();
                        out.extend(self.set(&input, RowState::Stopping, None, now));
                    }
                }
                self.stopping = Some((0, total));
                out.push(Event::new("stopping", json!({ "done": 0, "total": total })));
            }
            E::FlowRemoved { input } => {
                out.extend(self.set(input, RowState::Stopped, None, now));
                if let Some((done, total)) = &mut self.stopping {
                    *done = (*done + 1).min(*total);
                    out.push(Event::new(
                        "stopping",
                        json!({ "done": *done, "total": *total }),
                    ));
                }
            }
            E::SourceKept { input, .. } => {
                if !self.left_behind.contains(input) {
                    self.left_behind.push(input.clone());
                }
            }
            E::Stopped => out.push(self.stopped(None)),
            E::Failed { reason } => {
                let error = messages::for_error(reason, &self.ctx);
                out.push(self.stopped(Some(error)));
            }
            _ => {}
        }
        out
    }

    fn stopped(&self, error: Option<Message>) -> Event {
        Event::new(
            "stopped",
            json!({ "left_behind": self.left_behind, "error": error }),
        )
    }
}

/// What Go live needs from the machine, apart from the settings: a fake in tests.
pub trait Host: Send + Sync {
    /// The pid of another gateway live on this machine.
    fn other_gateway(&self) -> Option<u32> {
        run::running_gateway()
    }
    /// Stops a Strom a crashed session left behind (blocks while it stops).
    fn reclaim_strom(&self) {
        local_strom::reclaim_recorded(&Events::new(|e| {
            tracing::info!(event = ?e, "reclaiming Strom");
        }));
    }
    /// Camera and microphone access (blocks while macOS asks).
    fn media_access(&self, needs: Needs) -> Result<(), Media> {
        media_access::check(needs)
    }
}

/// This machine.
pub struct System;

impl Host for System {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// The checks before the session runs.
    Checking,
    Live,
    Stopping,
}

struct State {
    phase: Phase,
    handle: Option<SessionHandle>,
}

/// One session at a time, over the settings file, with the keychain an Open Source
/// Cloud sign-in is kept in.
pub struct GoLive {
    settings_file: PathBuf,
    keychain: Arc<dyn SecretStore>,
    host: Arc<dyn Host>,
    state: Arc<Mutex<State>>,
}

impl GoLive {
    pub fn new(
        settings_file: PathBuf,
        keychain: Arc<dyn SecretStore>,
        host: Arc<dyn Host>,
    ) -> Self {
        GoLive {
            settings_file,
            keychain,
            host,
            state: Arc::new(Mutex::new(State {
                phase: Phase::Idle,
                handle: None,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Going live, live or stopping: the rest of the window is read-only.
    pub fn is_live(&self) -> bool {
        self.lock().phase != Phase::Idle
    }

    /// Goes live, on `runtime`. The reply to command `id` is delivered once the
    /// checks have passed and the session has started (`ok: null`), or with the
    /// failure's one line and action. Progress follows as events. Asked again while
    /// a session runs, it replies at once and starts nothing.
    pub fn start(&self, id: u32, deliver: Deliver, runtime: &tokio::runtime::Handle) {
        {
            let mut state = self.lock();
            if state.phase != Phase::Idle {
                deliver(Reply::ok(id, Value::Null).script());
                return;
            }
            state.phase = Phase::Checking;
        }
        let settings_file = self.settings_file.clone();
        let keychain = self.keychain.clone();
        let host = self.host.clone();
        let state = self.state.clone();
        let task = async move {
            let checked =
                tokio::task::spawn_blocking(move || checks(&settings_file, host.as_ref())).await;
            let (settings, ctx) = match checked {
                Ok(Ok(checked)) => checked,
                Ok(Err(message)) => {
                    tracing::info!("go live refused: {}", message.message);
                    lock(&state).phase = Phase::Idle;
                    deliver(Reply::failure(id, &message).script());
                    return;
                }
                Err(err) => {
                    tracing::warn!("the checks before going live failed: {err}");
                    lock(&state).phase = Phase::Idle;
                    deliver(Reply::failure(id, &Failure::Unmatched.message()).script());
                    return;
                }
            };
            let mut events = {
                let mut guard = lock(&state);
                // Stop pressed while the checks ran: nothing has started.
                if guard.phase != Phase::Checking {
                    drop(guard);
                    deliver(Reply::ok(id, Value::Null).script());
                    deliver(Rows::new(ctx).stopped(None).script());
                    return;
                }
                let credential = settings
                    .open_live
                    .uses_keychain()
                    .then(|| OscCredential::Keychain(keychain));
                let inputs = Inputs::default();
                let (handle, events) = match credential {
                    Some(credential) => Session::start_with(settings, inputs, credential),
                    None => Session::start(settings, inputs),
                };
                // Under the same lock as the phase, so a Stop from now on reaches it.
                guard.phase = Phase::Live;
                guard.handle = Some(handle);
                events
            };
            tracing::info!("going live");
            deliver(Reply::ok(id, Value::Null).script());

            let mut rows = Rows::new(ctx);
            while let Some(event) = events.recv().await {
                match &event {
                    gateway::Event::Stats { .. } => tracing::debug!(?event, "session event"),
                    _ => tracing::info!(?event, "session event"),
                }
                let last = matches!(
                    event,
                    gateway::Event::Stopped | gateway::Event::Failed { .. }
                );
                if last {
                    // Idle before the page hears it stopped, so Go live works at once.
                    let mut guard = lock(&state);
                    guard.phase = Phase::Idle;
                    guard.handle = None;
                }
                for out in rows.on_event(&event, Instant::now()) {
                    deliver(out.script());
                }
            }
            let mut guard = lock(&state);
            guard.phase = Phase::Idle;
            guard.handle = None;
        };
        runtime.spawn(task.with_current_subscriber());
    }

    /// Stops the session: its teardown runs and the rows show it. Nothing to stop is
    /// not an error.
    pub fn stop(&self) {
        let mut state = self.lock();
        match state.phase {
            Phase::Checking => state.phase = Phase::Idle,
            Phase::Live => {
                state.phase = Phase::Stopping;
                if let Some(handle) = state.handle.as_mut() {
                    handle.request_stop(Stop::Interrupt);
                }
            }
            Phase::Idle | Phase::Stopping => {}
        }
    }

    /// Stops the other gateway live on this machine and removes what it left, as
    /// `open-live-ingest down` does. Refused while this app is live: its own session
    /// is stopped with Stop.
    pub async fn stop_other(&self) -> Result<(), Message> {
        if self.is_live() {
            return Err(Message::new(
                "This window is live. Press Stop to stop it.",
                None,
            ));
        }
        let host = self.host.clone();
        let other = tokio::task::spawn_blocking(move || host.other_gateway())
            .await
            .unwrap_or(None);
        let Some(pid) = other else {
            // Already gone.
            return Ok(());
        };
        let settings = load(&self.settings_file)?;
        tracing::info!(pid, "stopping the other live gateway");
        let events = Events::new(|e| tracing::info!(event = ?e, "stopping the other gateway"));
        run::down(&settings, &events)
            .await
            .map_err(|err| messages::for_error(&format!("{err:#}"), &Context::of(&settings)))
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// The settings, or the settings file's problem as the window shows it.
fn load(settings_file: &std::path::Path) -> Result<Config, Message> {
    api::load_from(settings_file).map_err(|err| {
        let problem = format!("{err:#}").replace(['\r', '\n'], " ");
        Failure::SettingsInvalid { problem }.message()
    })
}

/// The checks before Go live (section 4), in order; the first that fails is the
/// reply. Blocks: reclaiming a Strom waits for it, and macOS may show its prompt.
fn checks(settings_file: &std::path::Path, host: &dyn Host) -> Result<(Config, Context), Message> {
    let settings = load(settings_file)?;
    if let Some(problem) = config::problems(&settings).first() {
        return Err(messages::for_problem(problem));
    }
    if settings.inputs.is_empty() {
        return Err(Failure::NoInputsTicked.message());
    }
    if let Some(pid) = host.other_gateway() {
        tracing::info!(pid, "another gateway is live");
        return Err(Failure::AlreadyLive.message());
    }
    let needs = Needs {
        camera: settings.inputs.iter().any(|i| i.device != TEST_PATTERN),
        microphone: settings.inputs.iter().any(|i| i.audio.is_some()),
    };
    if let Err(media) = host.media_access(needs) {
        return Err(Failure::AccessDenied { media }.message());
    }
    host.reclaim_strom();
    let ctx = Context::of(&settings);
    Ok((settings, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::Action;
    use crate::testing;
    use open_live_ingest::gateway::{Event as E, InputStats, SummaryRow, UplinkStats};
    use open_live_ingest::keychain::MemoryKeychain;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver};

    fn rows() -> Rows {
        Rows::new(Context {
            open_live_host: "venue.example.osaas.io".to_string(),
            strom_port: Some(8080),
        })
    }

    fn json_of(events: &[Event]) -> Vec<Value> {
        events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    fn summary(names: &[&str]) -> E {
        E::Summary {
            inputs: names
                .iter()
                .map(|n| SummaryRow {
                    name: n.to_string(),
                    port: 47110,
                    address: "srt://:47110?mode=listener".to_string(),
                })
                .collect(),
        }
    }

    fn stats(input: &str, kbps: Option<u64>) -> E {
        E::Stats {
            inputs: vec![InputStats {
                input: input.to_string(),
                state: InputState::Waiting,
                uplink: kbps.map(|bitrate_kbps| UplinkStats {
                    bitrate_kbps,
                    rtt_ms: 12.5,
                    dropped: 0,
                }),
            }],
        }
    }

    const CAM: &str = "Venue — Camera";

    /// Strom starting, registering, connecting, live: each change is one `input`
    /// event, and nothing is sent for what did not change.
    #[test]
    fn a_session_starting_reads_as_the_rows_states() {
        let mut rows = rows();
        let t = Instant::now();
        let mut seen = Vec::new();
        for event in [
            E::StromStarting {
                port: 8080,
                log_path: "strom.log".into(),
            },
            E::StromReady { video_sources: 1 },
            E::Registering {
                input: CAM.to_string(),
            },
            E::Registered {
                input: CAM.to_string(),
                source_id: "src-1".to_string(),
            },
            summary(&[CAM]),
            E::Input {
                input: CAM.to_string(),
                state: InputState::Waiting,
            },
            E::Input {
                input: CAM.to_string(),
                state: InputState::OnAir,
            },
        ] {
            seen.extend(rows.on_event(&event, t));
        }
        assert_eq!(
            json_of(&seen),
            vec![
                json!({"event": "strom", "data": {"state": "starting"}}),
                json!({"event": "strom", "data": {"state": "ready"}}),
                json!({"event": "input", "data": {"input": CAM, "state": "registering", "message": null, "action": null}}),
                json!({"event": "input", "data": {"input": CAM, "state": "connecting", "message": null, "action": null}}),
                json!({"event": "input", "data": {"input": CAM, "state": "live", "message": null, "action": null}}),
            ]
        );
    }

    #[test]
    fn stats_carry_each_inputs_bitrate() {
        let mut rows = rows();
        let out = rows.on_event(&stats(CAM, Some(5800)), Instant::now());
        assert_eq!(
            json_of(&out),
            vec![json!({"event": "stats", "data": {"inputs": [
                {"input": CAM, "bitrate_kbps": 5800, "rtt_ms": 12.5, "dropped": 0}
            ]}})]
        );
        let out = rows.on_event(&stats(CAM, None), Instant::now());
        assert_eq!(
            json_of(&out)[0]["data"]["inputs"][0]["bitrate_kbps"],
            Value::Null
        );
    }

    /// A camera that cannot be opened at start, and one failing while live, show
    /// section 10's line on their row; the others carry on.
    #[test]
    fn a_failed_input_shows_the_camera_line() {
        let mut rows = rows();
        let t = Instant::now();
        let out = rows.on_event(
            &E::InputFailed {
                input: CAM.to_string(),
                reason: "POST flow start failed with HTTP 500".to_string(),
            },
            t,
        );
        let out = json_of(&out);
        assert_eq!(out[0]["data"]["state"], "failed");
        assert_eq!(
            out[0]["data"]["message"],
            "Venue — Camera could not be opened. Close other apps that use it, then press Go live."
        );
        assert_eq!(out[0]["data"]["action"], "retry");

        let out = rows.on_event(
            &E::Input {
                input: "Venue — Other".to_string(),
                state: InputState::Failed {
                    reason: "Strom did not report the flow running after start".to_string(),
                },
            },
            t,
        );
        assert_eq!(json_of(&out)[0]["data"]["state"], "failed");
    }

    /// Connecting for 30 s with nothing delivered: the row stays connecting and says
    /// why that may be. It goes when bytes move.
    #[test]
    fn nothing_delivered_for_30_seconds_adds_the_udp_hint() {
        let mut rows = rows();
        let t = Instant::now();
        rows.on_event(&summary(&[CAM]), t);
        let early = rows.on_event(&stats(CAM, None), t + Duration::from_secs(20));
        assert_eq!(json_of(&early).len(), 1, "only stats: {early:?}");
        let late = json_of(&rows.on_event(&stats(CAM, None), t + Duration::from_secs(30)));
        assert_eq!(late[0]["event"], "input");
        assert_eq!(late[0]["data"]["state"], "connecting");
        assert_eq!(
            late[0]["data"]["message"],
            "Nothing has reached the cloud yet. Check that this network allows outgoing UDP."
        );
        // Said once, and kept while it stays connecting.
        let again = rows.on_event(&stats(CAM, None), t + Duration::from_secs(40));
        assert_eq!(json_of(&again).len(), 1);
        let still = rows.on_event(
            &E::Input {
                input: CAM.to_string(),
                state: InputState::Waiting,
            },
            t + Duration::from_secs(41),
        );
        assert!(still.is_empty(), "{still:?}");
        let live = json_of(&rows.on_event(
            &E::Input {
                input: CAM.to_string(),
                state: InputState::OnAir,
            },
            t + Duration::from_secs(50),
        ));
        assert_eq!(live[0]["data"]["state"], "live");
        assert_eq!(live[0]["data"]["message"], Value::Null);
    }

    /// Open Live unreachable while live is a banner that clears itself on the next
    /// tick that reaches it.
    #[test]
    fn open_live_unreachable_while_live_is_a_banner_that_clears_itself() {
        let mut rows = rows();
        let t = Instant::now();
        let warning = E::Warning {
            message: "could not update the Open Live sources (GET sources: timed out); the feeds are unaffected, retrying next tick".to_string(),
        };
        let shown = json_of(&rows.on_event(&warning, t));
        assert_eq!(shown[0]["event"], "notice");
        assert_eq!(shown[0]["data"]["kind"], "open_live_unreachable");
        // Still failing next tick: shown once, not cleared.
        assert_eq!(json_of(&rows.on_event(&stats(CAM, None), t)).len(), 1);
        assert!(rows.on_event(&warning, t).is_empty());
        assert_eq!(json_of(&rows.on_event(&stats(CAM, None), t)).len(), 1);
        // Reached again.
        let cleared = json_of(&rows.on_event(&stats(CAM, None), t));
        assert_eq!(
            cleared[0],
            json!({"event": "notice", "data": {"clear": "open_live_unreachable"}})
        );
    }

    #[test]
    fn a_refused_gateway_token_and_a_sign_in_ending_are_banners() {
        let mut rows = rows();
        let t = Instant::now();
        let refused = json_of(&rows.on_event(
            &E::Warning {
                message: "Open Live refused the token for gateway gw-1; check open_live.gateway_id and open_live.gateway_token, or rotate the token".to_string(),
            },
            t,
        ));
        assert_eq!(refused[0]["data"]["kind"], "heartbeat_refused");
        assert_eq!(refused[0]["data"]["action"], "advanced");
        let ends = json_of(&rows.on_event(&E::OscSignInEnds { at: 1_795_010_700 }, t));
        assert_eq!(ends[0]["data"]["kind"], "sign_in_ends");
        assert_eq!(ends[0]["data"]["action"], "sign_in");
        // Other warnings are the log's.
        assert!(rows
            .on_event(
                &E::Warning {
                    message: "uplink.port_range (1-2) is ignored".to_string()
                },
                t
            )
            .is_empty());
    }

    #[test]
    fn a_saved_device_not_present_is_its_own_row() {
        let mut rows = rows();
        let out = json_of(&rows.on_event(
            &E::Warning {
                message: "[[input]] device \"dev-cam\" is not connected; it is skipped, the others go live".to_string(),
            },
            Instant::now(),
        ));
        assert_eq!(out[0]["data"]["input"], "dev-cam");
        assert_eq!(out[0]["data"]["state"], "not_connected");
    }

    /// Stop: the rows go stopping, then stopped as each flow is removed, with
    /// progress; a source Open Live kept is named at the end.
    #[test]
    fn stop_shows_progress_in_the_rows() {
        let mut rows = rows();
        let t = Instant::now();
        let other = "Venue — Test pattern";
        rows.on_event(&summary(&[CAM, other]), t);
        let mut seen = Vec::new();
        for event in [
            E::Stopping,
            E::FlowRemoved {
                input: CAM.to_string(),
            },
            E::SourceRemoved {
                input: CAM.to_string(),
            },
            E::FlowRemoved {
                input: other.to_string(),
            },
            E::SourceKept {
                input: other.to_string(),
                reason: "in active production \"Morning Show\"".to_string(),
            },
            E::StromStopped,
            E::Stopped,
        ] {
            seen.extend(rows.on_event(&event, t));
        }
        let seen = json_of(&seen);
        let shape: Vec<(String, Value)> = seen
            .iter()
            .map(|e| {
                let detail = match e["event"].as_str().unwrap() {
                    "input" => json!([e["data"]["input"], e["data"]["state"]]),
                    _ => e["data"].clone(),
                };
                (e["event"].as_str().unwrap().to_string(), detail)
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("input".into(), json!([CAM, "stopping"])),
                ("input".into(), json!([other, "stopping"])),
                ("stopping".into(), json!({"done": 0, "total": 2})),
                ("input".into(), json!([CAM, "stopped"])),
                ("stopping".into(), json!({"done": 1, "total": 2})),
                ("input".into(), json!([other, "stopped"])),
                ("stopping".into(), json!({"done": 2, "total": 2})),
                ("strom".into(), json!({"state": "stopped"})),
                (
                    "stopped".into(),
                    json!({"left_behind": [other], "error": null})
                ),
            ]
        );
    }

    /// A session that ends on an error says why with section 10's line.
    #[test]
    fn a_failed_session_ends_with_the_line_for_its_reason() {
        let mut rows = rows();
        let out = json_of(&rows.on_event(
            &E::Failed {
                reason: "Strom did not become ready within 45s (connection refused).".to_string(),
            },
            Instant::now(),
        ));
        assert_eq!(out[0]["event"], "stopped");
        assert_eq!(
            out[0]["data"]["error"],
            json!({
                "message": "The video engine did not start within 45 seconds. Show logs for details.",
                "action": "show_strom_log",
            })
        );
    }

    // -----------------------------------------------------------------------
    // The controller.
    // -----------------------------------------------------------------------

    /// A machine with another gateway live or not, access granted or not, that
    /// remembers whether a Strom was reclaimed.
    #[derive(Default)]
    struct FakeHost {
        other: Option<u32>,
        denied: Option<Media>,
        reclaimed: AtomicBool,
        asked: Mutex<Option<Needs>>,
    }

    impl Host for FakeHost {
        fn other_gateway(&self) -> Option<u32> {
            self.other
        }
        fn reclaim_strom(&self) {
            self.reclaimed.store(true, Ordering::SeqCst);
        }
        fn media_access(&self, needs: Needs) -> Result<(), Media> {
            *self.asked.lock().unwrap() = Some(needs);
            match self.denied {
                Some(media) => Err(media),
                None => Ok(()),
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn deliverer() -> (Deliver, Receiver<Value>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let deliver: Deliver = Arc::new(move |script: String| {
            let json = script
                .strip_prefix("window.oli.receive(")
                .and_then(|s| s.strip_suffix(')'))
                .expect("a receive script");
            let _ = tx.lock().unwrap().send(serde_json::from_str(json).unwrap());
        });
        (deliver, rx)
    }

    fn next(rx: &Receiver<Value>) -> Value {
        rx.recv_timeout(Duration::from_secs(60))
            .expect("something delivered")
    }

    /// Settings that pass the checks: a Strom nobody listens on and may not be
    /// started, so a session gets as far as Strom and ends there.
    fn settings_file(scratch: &testing::Scratch, inputs: &str) -> PathBuf {
        let path = scratch.settings();
        std::fs::write(
            &path,
            format!(
                r#"[gateway]
id = "gw-app"
name = "Venue"

[strom]
url = "http://127.0.0.1:9"
manage = false

[open_live]
register = false

[uplink]
host = "cloud.example.com"
port_range = "47110-47112"
{inputs}"#
            ),
        )
        .unwrap();
        path
    }

    const CAMERA_WITH_MIC: &str = "\n[[input]]\ndevice = \"dev-cam\"\naudio = \"dev-mic\"\n";

    fn go_live(path: PathBuf, host: Arc<FakeHost>) -> GoLive {
        GoLive::new(path, Arc::new(MemoryKeychain::new()), host)
    }

    #[test]
    fn go_live_with_nothing_ticked_says_so() {
        let scratch = testing::Scratch::new("golive-none");
        let host = Arc::new(FakeHost::default());
        let go = go_live(settings_file(&scratch, ""), host.clone());
        let (deliver, rx) = deliverer();
        let rt = runtime();
        go.start(1, deliver, rt.handle());
        assert_eq!(
            next(&rx),
            json!({"id": 1, "error": {"message": "Tick at least one camera or the test pattern.", "action": null}})
        );
        assert!(!go.is_live());
        assert!(!host.reclaimed.load(Ordering::SeqCst));
    }

    /// `gateway.pid` names a live gateway: refused, with Stop it offered, and
    /// nothing reclaimed or started.
    #[test]
    fn go_live_refuses_while_another_gateway_is_live() {
        let scratch = testing::Scratch::new("golive-other");
        let host = Arc::new(FakeHost {
            other: Some(4242),
            ..FakeHost::default()
        });
        let go = go_live(settings_file(&scratch, CAMERA_WITH_MIC), host.clone());
        let (deliver, rx) = deliverer();
        let rt = runtime();
        go.start(2, deliver, rt.handle());
        let reply = next(&rx);
        assert_eq!(
            reply["error"]["message"],
            "Open Live Ingest is already live from another window or a terminal."
        );
        assert_eq!(reply["error"]["action"], "stop_it");
        assert!(!go.is_live());
        assert!(!host.reclaimed.load(Ordering::SeqCst));
    }

    #[test]
    fn go_live_with_the_camera_denied_says_so() {
        let scratch = testing::Scratch::new("golive-denied");
        let host = Arc::new(FakeHost {
            denied: Some(Media::Microphone),
            ..FakeHost::default()
        });
        let go = go_live(settings_file(&scratch, CAMERA_WITH_MIC), host.clone());
        let (deliver, rx) = deliverer();
        let rt = runtime();
        go.start(3, deliver, rt.handle());
        let reply = next(&rx);
        assert_eq!(
            reply["error"]["message"],
            "Open Live Ingest is not allowed to use the microphone. Allow it in System Settings → Privacy & Security, then press Go live."
        );
        assert_eq!(
            *host.asked.lock().unwrap(),
            Some(Needs {
                camera: true,
                microphone: true
            })
        );
    }

    /// The test pattern alone needs neither camera nor microphone.
    #[test]
    fn the_test_pattern_needs_no_camera_access() {
        let scratch = testing::Scratch::new("golive-test");
        let host = Arc::new(FakeHost::default());
        let path = settings_file(&scratch, "\n[[input]]\ndevice = \"test\"\n");
        let (settings, _) = checks(&path, host.as_ref()).unwrap();
        assert_eq!(settings.inputs.len(), 1);
        assert_eq!(*host.asked.lock().unwrap(), Some(Needs::default()));
        assert!(
            host.reclaimed.load(Ordering::SeqCst),
            "a leftover Strom is reclaimed"
        );
    }

    #[test]
    fn a_broken_settings_file_says_so_with_show_file() {
        let scratch = testing::Scratch::new("golive-broken");
        std::fs::write(scratch.settings(), "[gateway\n").unwrap();
        let err = checks(&scratch.settings(), &FakeHost::default()).unwrap_err();
        assert!(
            err.message.starts_with("The settings file has a problem: "),
            "{}",
            err.message
        );
        assert!(!err.message.contains('\n'));
        assert_eq!(err.action, Some(Action::ShowFile));
    }

    /// The checks pass, the session starts and ends at Strom: the reply comes first,
    /// then a `stopped` event with the reason in one line, and Go live works again.
    #[test]
    fn a_session_that_ends_on_an_error_ends_with_stopped() {
        let scratch = testing::Scratch::new("golive-session");
        let host = Arc::new(FakeHost::default());
        let go = go_live(settings_file(&scratch, CAMERA_WITH_MIC), host.clone());
        let (deliver, rx) = deliverer();
        let rt = runtime();
        go.start(4, deliver.clone(), rt.handle());
        assert_eq!(next(&rx), json!({"id": 4, "ok": null}));
        assert!(host.reclaimed.load(Ordering::SeqCst));
        let stopped = loop {
            let message = next(&rx);
            if message["event"] == "stopped" {
                break message;
            }
        };
        // "nothing is listening at …, and [strom] manage is off" has no row of its own.
        assert_eq!(
            stopped["data"]["error"]["message"],
            "Something went wrong. Show logs for details."
        );
        assert_eq!(stopped["data"]["error"]["action"], "show_logs");
        assert!(!go.is_live());
        go.start(5, deliver, rt.handle());
        assert_eq!(next(&rx), json!({"id": 5, "ok": null}));
    }

    #[test]
    fn stop_other_with_nobody_else_live_does_nothing() {
        let scratch = testing::Scratch::new("golive-stop-other");
        let go = go_live(
            settings_file(&scratch, CAMERA_WITH_MIC),
            Arc::new(FakeHost::default()),
        );
        assert_eq!(runtime().block_on(go.stop_other()), Ok(()));
    }

    #[test]
    fn stop_with_nothing_live_is_not_an_error() {
        let scratch = testing::Scratch::new("golive-stop");
        let go = go_live(scratch.settings(), Arc::new(FakeHost::default()));
        go.stop();
        assert!(!go.is_live());
    }
}
