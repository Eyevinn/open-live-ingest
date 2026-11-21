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
use crate::status::{self, Poller, Target};
use open_live_ingest::api::{self, OscCredential};
use open_live_ingest::config::{self, Config};
use open_live_ingest::gateway::{self, Events, InputState};
use open_live_ingest::keychain::SecretStore;
use open_live_ingest::local_strom;
use open_live_ingest::run::{self, Stop};
use open_live_ingest::session::{Inputs, Session, SessionHandle};
use open_live_ingest::strom::StromClient;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
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
    /// The flow waits for a receiver (`InputState::Waiting`).
    waiting: bool,
    /// `message` is the waiting hint from the productions (`status::waiting_hint`).
    hinted: bool,
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
    /// Per input, what its row says while waiting, from the last status poll.
    hints: HashMap<String, Message>,
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
            hints: HashMap::new(),
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
                    waiting: false,
                    hinted: false,
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
        if row.message != message {
            row.hinted = false;
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
                let waiting = *state == InputState::Waiting;
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
                out.extend(self.show_hint(input, waiting));
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

    /// What the page is told about one look at the productions and the links: the
    /// `status` event, and the hint of each row that waits for Open Live.
    pub fn on_status(&mut self, statuses: &[status::InputStatus]) -> Vec<Event> {
        let mut out = Vec::new();
        for s in statuses {
            match status::waiting_hint(&s.productions) {
                Some(hint) => self.hints.insert(s.input.clone(), hint),
                None => self.hints.remove(&s.input),
            };
        }
        let waiting: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.waiting)
            .map(|r| r.input.clone())
            .collect();
        for input in waiting {
            out.extend(self.show_hint(&input, true));
        }
        out.push(status::event(statuses));
        out
    }

    /// A row that waits for Open Live says what it waits for, when the productions
    /// tell; that replaces the 30 s "nothing delivered" line, which is about the
    /// network. Once it no longer waits, or the hint no longer applies, the hint goes.
    fn show_hint(&mut self, input: &str, waiting: bool) -> Option<Event> {
        let hint = self.hints.get(input).cloned();
        let row = self.row(input);
        row.waiting = waiting;
        let want = hint.filter(|_| waiting && row.state == RowState::Connecting);
        match want {
            Some(hint) if row.message.as_ref() != Some(&hint) => {
                row.message = Some(hint);
                row.hinted = true;
                Some(row.event())
            }
            Some(_) => None,
            None if row.hinted => {
                row.message = None;
                row.hinted = false;
                Some(row.event())
            }
            None => None,
        }
    }

    /// The inputs still being sent: registering, connecting or live. What the quit
    /// question counts (section 8).
    pub fn sending(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r.state,
                    RowState::Registering | RowState::Connecting | RowState::Live
                )
            })
            .count()
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
    /// Counts Go lives. A session's task changes the state only while this is still
    /// its own number, so a late end of an earlier session (its checks finishing after
    /// a Stop, its channel closing after its last event) cannot reset a newer one.
    session: u64,
    /// Woken whenever the phase goes back to Idle.
    idle: Arc<Notify>,
}

impl State {
    /// Back to Idle, if `session` is still the current one.
    fn end(&mut self, session: u64) {
        if self.session == session {
            self.set_idle();
        }
    }

    fn set_idle(&mut self) {
        self.phase = Phase::Idle;
        self.handle = None;
        self.idle.notify_waiters();
    }
}

/// One session at a time, over the settings file, with the keychain an Open Source
/// Cloud sign-in is kept in.
pub struct GoLive {
    settings_file: PathBuf,
    keychain: Arc<dyn SecretStore>,
    host: Arc<dyn Host>,
    state: Arc<Mutex<State>>,
    /// The inputs the current session is sending, for the quit question.
    sending: Arc<AtomicUsize>,
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
                session: 0,
                idle: Arc::new(Notify::new()),
            })),
            sending: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Going live, live or stopping: the rest of the window is read-only.
    pub fn is_live(&self) -> bool {
        self.lock().phase != Phase::Idle
    }

    /// How many inputs the session is sending: registering, connecting or live.
    pub fn inputs_live(&self) -> usize {
        self.sending.load(Ordering::SeqCst)
    }

    /// Returns once nothing is live: at once when nothing is, else when the session
    /// has ended (its teardown done) or a Go live still in its checks was stopped.
    pub async fn until_idle(&self) {
        let idle = self.lock().idle.clone();
        loop {
            let notified = idle.notified();
            tokio::pin!(notified);
            // Registered before the check, so an end in between is not missed.
            notified.as_mut().enable();
            if !self.is_live() {
                return;
            }
            notified.await;
        }
    }

    /// Goes live, on `runtime`. The reply to command `id` is delivered once the
    /// checks have passed and the session has started (`ok: null`), or with the
    /// failure's one line and action. Progress follows as events. Asked again while
    /// a session runs, it replies at once and starts nothing.
    pub fn start(&self, id: u32, deliver: Deliver, runtime: &tokio::runtime::Handle) {
        let session = {
            let mut state = self.lock();
            if state.phase != Phase::Idle {
                deliver(Reply::ok(id, Value::Null).script());
                return;
            }
            state.phase = Phase::Checking;
            state.session += 1;
            state.session
        };
        let settings_file = self.settings_file.clone();
        let keychain = self.keychain.clone();
        let host = self.host.clone();
        let state = self.state.clone();
        let sending = self.sending.clone();
        sending.store(0, Ordering::SeqCst);
        let task = async move {
            let checked =
                tokio::task::spawn_blocking(move || checks(&settings_file, host.as_ref())).await;
            let (settings, ctx) = match checked {
                Ok(Ok(checked)) => checked,
                Ok(Err(message)) => {
                    tracing::info!("go live refused: {}", message.message);
                    lock(&state).end(session);
                    deliver(Reply::failure(id, &message).script());
                    return;
                }
                Err(err) => {
                    tracing::warn!("the checks before going live failed: {err}");
                    lock(&state).end(session);
                    deliver(Reply::failure(id, &Failure::Unmatched.message()).script());
                    return;
                }
            };
            let (mut events, poller) = {
                let mut guard = lock(&state);
                // Stop pressed while the checks ran, and maybe Go live again since:
                // nothing has started for this one.
                if guard.session != session || guard.phase != Phase::Checking {
                    let superseded = guard.session != session;
                    drop(guard);
                    deliver(Reply::ok(id, Value::Null).script());
                    // The page's `stopped` is the newer session's to send.
                    if !superseded {
                        deliver(Rows::new(ctx).stopped(None).script());
                    }
                    return;
                }
                let credential = settings
                    .open_live
                    .uses_keychain()
                    .then(|| OscCredential::Keychain(keychain));
                let poller = status_poller(&settings, credential.as_ref());
                // Stop (and quitting while live) leaves the sources in Open Live, so
                // the next Go live adopts them with the same id and a production slot
                // holding one keeps working (#115).
                let (handle, events) =
                    Session::start_keeping_sources(settings, Inputs::default(), credential);
                // Under the same lock as the phase, so a Stop from now on reaches it.
                guard.phase = Phase::Live;
                guard.handle = Some(handle);
                (events, poller)
            };
            tracing::info!("going live");
            deliver(Reply::ok(id, Value::Null).script());

            // The session's events and the status polls, in one queue.
            let (incoming_tx, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            let forward = incoming_tx.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if forward.send(Incoming::Session(event)).is_err() {
                        return;
                    }
                }
                let _ = forward.send(Incoming::Ended);
            });
            let targets: Arc<Mutex<Vec<Target>>> = Arc::default();
            let mut polling = poller
                .map(|poller| tokio::spawn(poll_status(poller, targets.clone(), incoming_tx)));

            let mut rows = Rows::new(ctx);
            while let Some(incoming) = incoming.recv().await {
                let event = match incoming {
                    Incoming::Session(event) => event,
                    Incoming::Status(statuses) => {
                        for out in rows.on_status(&statuses) {
                            deliver(out.script());
                        }
                        continue;
                    }
                    Incoming::Ended => break,
                };
                if let gateway::Event::Stats { inputs } = &event {
                    *lock(&targets) = inputs
                        .iter()
                        .map(|s| Target {
                            input: s.input.clone(),
                            flow_id: s.flow_id,
                            source_id: s.source_id.clone(),
                        })
                        .collect();
                }
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
                    lock(&state).end(session);
                    // Nothing to show status for once the page hears it stopped.
                    if let Some(polling) = polling.take() {
                        polling.abort();
                    }
                }
                let out = rows.on_event(&event, Instant::now());
                sending.store(rows.sending(), Ordering::SeqCst);
                for out in out {
                    deliver(out.script());
                }
            }
            if let Some(polling) = polling {
                polling.abort();
            }
            lock(&state).end(session);
        };
        runtime.spawn(task.with_current_subscriber());
    }

    /// Stops the session: its teardown runs and the rows show it. Nothing to stop is
    /// not an error.
    pub fn stop(&self) {
        self.stop_for(Stop::Interrupt);
    }

    /// Stops the session because the process is about to be ended from outside (a
    /// Windows logout or shutdown): the teardown is bounded to the library's
    /// `ENDING_TEARDOWN` (`teardown_within`). A teardown already running is left to
    /// run as it is.
    pub fn stop_ending(&self) {
        self.stop_for(Stop::Ending);
    }

    fn stop_for(&self, why: Stop) {
        let mut state = self.lock();
        match state.phase {
            Phase::Checking => state.set_idle(),
            Phase::Live => {
                state.phase = Phase::Stopping;
                if let Some(handle) = state.handle.as_mut() {
                    handle.request_stop(why);
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

fn lock<T>(state: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the loop of one session hears: the session's events and the status polls.
enum Incoming {
    Session(gateway::Event),
    Status(Vec<status::InputStatus>),
    /// The session's events have ended.
    Ended,
}

/// The poller for the status lines, with clients as the session builds them. None,
/// and a warning in the log, when even the local Strom's client cannot be built: the
/// session goes live all the same, without the status lines.
fn status_poller(settings: &Config, credential: Option<&OscCredential>) -> Option<Poller> {
    let strom = match StromClient::new(&settings.strom.url, settings.strom.api_key.as_deref()) {
        Ok(strom) => strom,
        Err(err) => {
            tracing::warn!(err = %format_args!("{err:#}"), "no status lines: the local Strom's client");
            return None;
        }
    };
    let open_live = run::open_live_client(settings, credential).unwrap_or_else(|err| {
        tracing::warn!(err = %format_args!("{err:#}"), "the productions are shown as unknown");
        None
    });
    Some(Poller::new(open_live, strom))
}

/// Looks every [`status::POLL`] at the inputs the session's last stats named, until
/// the loop stops listening or the task is aborted.
async fn poll_status(
    mut poller: Poller,
    targets: Arc<Mutex<Vec<Target>>>,
    to: tokio::sync::mpsc::UnboundedSender<Incoming>,
) {
    let mut ticker = tokio::time::interval(status::POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let targets = lock(&targets).clone();
        if targets.is_empty() {
            continue;
        }
        let statuses = poller.poll(&targets).await;
        if to.send(Incoming::Status(statuses)).is_err() {
            return;
        }
    }
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
    // Again: macOS may have asked for minutes, and a gateway started meanwhile is
    // refused, not reclaimed. `reclaim_strom` checks once more itself, after it has
    // read `strom.pid`, which closes the gap between these two calls.
    if let Some(pid) = host.other_gateway() {
        tracing::info!(pid, "another gateway started during the checks");
        return Err(Failure::AlreadyLive.message());
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
    use open_live_ingest::openlive::ProductionState;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
                flow_id: Default::default(),
                source_id: Some("src-1".to_string()),
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

    fn status_of(input: &str, productions: status::Productions) -> status::InputStatus {
        status::InputStatus {
            input: input.to_string(),
            productions,
            link: status::Link {
                state: status::LinkState::Connecting,
                dropped_pct: None,
                rtt_ms: Some(20.0),
            },
        }
    }

    fn held(state: ProductionState) -> status::Productions {
        status::Productions::Held(vec![status::Holder {
            name: "Morning Show".to_string(),
            state,
            slots: vec!["video_in_1".to_string()],
        }])
    }

    fn input(state: InputState) -> E {
        E::Input {
            input: CAM.to_string(),
            state,
        }
    }

    /// The status event carries each input's productions and link as the poll found
    /// them; a row that is not waiting gets no hint.
    #[test]
    fn a_status_poll_is_one_status_event() {
        let mut rows = rows();
        let t = Instant::now();
        rows.on_event(&input(InputState::OnAir), t);
        let out = json_of(&rows.on_status(&[status_of(CAM, held(ProductionState::Active))]));
        assert_eq!(
            out,
            vec![json!({"event": "status", "data": {"inputs": [{
                "input": CAM,
                "productions": [{"name": "Morning Show", "state": "active", "slots": ["video_in_1"]}],
                "link": {"state": "connecting", "dropped_pct": null, "rtt_ms": 20.0},
            }]}})]
        );
    }

    /// #116 item 1: Waiting names what is missing, and Starting and Unreachable keep
    /// their own (empty) wording.
    #[test]
    fn a_waiting_row_names_what_open_live_lacks() {
        let mut rows = rows();
        let t = Instant::now();
        rows.on_event(&input(InputState::Waiting), t);

        let none = json_of(&rows.on_status(&[status_of(CAM, status::Productions::Held(vec![]))]));
        assert_eq!(none[0]["event"], "input");
        assert_eq!(none[0]["data"]["state"], "connecting");
        assert_eq!(
            none[0]["data"]["message"],
            "Waiting for Open Live: no production holds this source. Put it in a production in Studio and activate it."
        );
        assert_eq!(none[0]["data"]["action"], "open_studio");
        assert_eq!(none[1]["event"], "status");
        // Said once while it stays so.
        let same = rows.on_status(&[status_of(CAM, status::Productions::Held(vec![]))]);
        assert_eq!(json_of(&same).len(), 1);

        let inactive = json_of(&rows.on_status(&[status_of(CAM, held(ProductionState::Inactive))]));
        assert_eq!(
            inactive[0]["data"]["message"],
            "Waiting for Open Live: production \"Morning Show\" is inactive. Activate it in Studio."
        );
        // The next Waiting from the session keeps it.
        assert!(rows.on_event(&input(InputState::Waiting), t).is_empty());

        // Starting and Unreachable are not waiting for Open Live: the hint goes.
        for other in [InputState::Starting, InputState::Unreachable] {
            rows.on_event(&input(InputState::Waiting), t);
            rows.on_status(&[status_of(CAM, held(ProductionState::Inactive))]);
            let out = json_of(&rows.on_event(&input(other), t));
            assert_eq!(out.last().unwrap()["data"]["state"], "connecting");
            assert_eq!(out.last().unwrap()["data"]["message"], Value::Null);
        }

        // Waiting again: the last poll's hint is back at once.
        let back = json_of(&rows.on_event(&input(InputState::Waiting), t));
        assert!(back[0]["data"]["message"]
            .as_str()
            .unwrap()
            .contains("is inactive"));

        // The production runs now: the hint goes, the row stays connecting.
        let running = json_of(&rows.on_status(&[status_of(CAM, held(ProductionState::Active))]));
        assert_eq!(running[0]["data"]["message"], Value::Null);
        assert_eq!(running[0]["data"]["state"], "connecting");
    }

    /// Open Live not answering is unknown: no hint, nothing replaced, nothing fails.
    #[test]
    fn unknown_productions_leave_a_waiting_row_as_it_is() {
        let mut rows = rows();
        let t = Instant::now();
        rows.on_event(&summary(&[CAM]), t);
        rows.on_event(&input(InputState::Waiting), t);
        // The 30 s line is there first.
        let late = json_of(&rows.on_event(&stats(CAM, None), t + Duration::from_secs(30)));
        assert_eq!(late[0]["event"], "input");
        let out = json_of(&rows.on_status(&[status_of(CAM, status::Productions::Unknown)]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["event"], "status");
        assert_eq!(out[0]["data"]["inputs"][0]["productions"], Value::Null);
        // A hint, when it comes, replaces the network line.
        let hinted = json_of(&rows.on_status(&[status_of(CAM, status::Productions::Held(vec![]))]));
        assert!(hinted[0]["data"]["message"]
            .as_str()
            .unwrap()
            .contains("no production holds this source"));
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
        /// A gateway that starts while the camera check runs (a CLI `up` started
        /// while macOS shows its prompt).
        starts_during_check: Option<u32>,
        checked: AtomicBool,
        denied: Option<Media>,
        reclaimed: AtomicBool,
        asked: Mutex<Option<Needs>>,
        /// When set, each camera check waits for one message on it.
        gate: Option<Mutex<Receiver<()>>>,
        /// Camera checks begun.
        checking: AtomicUsize,
    }

    impl Host for FakeHost {
        fn other_gateway(&self) -> Option<u32> {
            self.other.or(self
                .starts_during_check
                .filter(|_| self.checked.load(Ordering::SeqCst)))
        }
        fn reclaim_strom(&self) {
            self.reclaimed.store(true, Ordering::SeqCst);
        }
        fn media_access(&self, needs: Needs) -> Result<(), Media> {
            *self.asked.lock().unwrap() = Some(needs);
            self.checked.store(true, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                let gate = gate.lock().unwrap();
                self.checking.fetch_add(1, Ordering::SeqCst);
                // A closed gate (the test has ended) lets the check go.
                let _ = gate.recv();
            }
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

    /// A gateway that starts while the checks run (here, during the camera check,
    /// which on macOS can wait minutes on its prompt) is refused like one that was
    /// already live, and its Strom is not reclaimed.
    #[test]
    fn a_gateway_started_during_the_checks_is_refused_not_reclaimed() {
        let scratch = testing::Scratch::new("golive-other-late");
        let host = FakeHost {
            starts_during_check: Some(4343),
            ..FakeHost::default()
        };
        let err = checks(&settings_file(&scratch, CAMERA_WITH_MIC), &host).unwrap_err();
        assert_eq!(
            err.message,
            "Open Live Ingest is already live from another window or a terminal."
        );
        assert_eq!(err.action, Some(Action::StopIt));
        assert!(
            host.asked.lock().unwrap().is_some(),
            "the camera was checked"
        );
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
        assert_eq!(reply["error"]["action"], "open_microphone_settings");
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

    /// #95 W3: Go live, Stop while the checks run, Go live again. When the first
    /// checks finish late they start nothing and leave the second Go live alone: no
    /// session for the first, no `stopped` for the page, still going live.
    #[test]
    fn a_late_end_of_an_earlier_go_live_leaves_the_newer_one_alone() {
        let scratch = testing::Scratch::new("golive-generation");
        // Before the gate: dropped after it, so a failing assert lets a waiting check
        // go and the runtime's drop does not wait on it forever.
        let rt = runtime();
        let (release, gate) = channel();
        let host = Arc::new(FakeHost {
            gate: Some(Mutex::new(gate)),
            ..FakeHost::default()
        });
        let go = go_live(settings_file(&scratch, CAMERA_WITH_MIC), host.clone());
        let (deliver, rx) = deliverer();
        let until_checking = |n: usize| {
            let started = Instant::now();
            while host.checking.load(Ordering::SeqCst) < n {
                assert!(started.elapsed() < Duration::from_secs(10), "no check {n}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        go.start(1, deliver.clone(), rt.handle());
        until_checking(1);
        go.stop();
        assert!(!go.is_live());
        go.start(2, deliver, rt.handle());
        assert!(go.is_live());

        // The first checks pass now: their reply only, and the second still checking.
        release.send(()).unwrap();
        assert_eq!(next(&rx), json!({"id": 1, "ok": null}));
        until_checking(2);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            go.lock().phase,
            Phase::Checking,
            "the first Go live started"
        );
        assert!(go.lock().handle.is_none());
        assert!(rx.try_recv().is_err(), "nothing else for the first Go live");

        // The second is stopped during its checks as well: it ends as Stop says.
        go.stop();
        release.send(()).unwrap();
        assert_eq!(next(&rx), json!({"id": 2, "ok": null}));
        assert_eq!(next(&rx)["event"], "stopped");
        assert!(!go.is_live());
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

    /// What the quit question counts: the inputs registering, connecting or live, not
    /// the failed, stopped or absent ones.
    #[test]
    fn the_inputs_being_sent_are_counted_for_the_quit_question() {
        let mut rows = rows();
        let t = Instant::now();
        assert_eq!(rows.sending(), 0);
        rows.on_event(&summary(&[CAM, "Venue — Two", "Venue — Three"]), t);
        rows.on_event(
            &E::Input {
                input: CAM.to_string(),
                state: InputState::OnAir,
            },
            t,
        );
        rows.on_event(
            &E::InputFailed {
                input: "Venue — Three".to_string(),
                reason: "POST flow start failed with HTTP 500".to_string(),
            },
            t,
        );
        assert_eq!(rows.sending(), 2);
        rows.on_event(&E::Stopping, t);
        assert_eq!(rows.sending(), 0, "stopping is not sending");
    }

    /// Stop and quit waits on this: at once when idle, and as soon as a Go live in
    /// its checks is stopped (here by the bounded stop of a Windows logout).
    #[test]
    fn until_idle_returns_once_nothing_is_live() {
        let scratch = testing::Scratch::new("golive-idle");
        let rt = runtime();
        let (release, gate) = channel();
        let host = Arc::new(FakeHost {
            gate: Some(Mutex::new(gate)),
            ..FakeHost::default()
        });
        let go = Arc::new(go_live(
            settings_file(&scratch, CAMERA_WITH_MIC),
            host.clone(),
        ));
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(1), go.until_idle()).await })
            .expect("idle at once");

        let (deliver, _rx) = deliverer();
        go.start(1, deliver, rt.handle());
        assert!(go.is_live());
        let waiting = {
            let go = go.clone();
            rt.spawn(async move { go.until_idle().await })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiting.is_finished(), "still going live");
        go.stop_ending();
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), waiting).await })
            .expect("woken when stopped")
            .unwrap();
        assert!(!go.is_live());
        release.send(()).unwrap();
    }

    #[test]
    fn stop_with_nothing_live_is_not_an_error() {
        let scratch = testing::Scratch::new("golive-stop");
        let go = go_live(scratch.settings(), Arc::new(FakeHost::default()));
        go.stop();
        assert!(!go.is_live());
    }
}
