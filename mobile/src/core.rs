//! The command dispatcher and view model behind the C ABI (docs/specs/ios-app.md,
//! section 8.2). Plain Rust: everything but the `extern "C"` shims in `lib.rs` is
//! here, and tested on the host against fake Open Source Cloud and Open Live servers.
//!
//! The desktop app's message envelope (desktop spec, section 9.1), unchanged:
//!
//! - Swift to core: `{"id": <u32>, "cmd": "<name>", "args": {...}}`.
//! - Core to Swift: a reply `{"id": <u32>, "ok": <value>}` or
//!   `{"id": <u32>, "error": {"message": "<one line>", "action": null}}`, and events
//!   `{"event": "<name>", "data": {...}}`. A message with no readable `id` gets a
//!   reply with `"id": null`.
//!
//! Commands are one serde enum ([`Command`]); an unknown command or bad arguments is
//! an error reply, never a panic. A command that answers from what is at hand
//! replies at once; one that waits on a server runs on the core's own runtime and
//! replies through [`Deliver`] when it is done, so [`Core::dispatch`] never waits on
//! the network.
//!
//! The Rust core decides when to send (the gate, from the productions), where (from
//! Open Live's `server-info` and the settings) and what is registered; Swift decides
//! how, carrying out the `media` events and reporting back with `media_report`
//! (section 8.3). Only the command and event names are logged, never a body: the
//! `media` start event's `uplink.passphrase`, the one secret that crosses to Swift,
//! never reaches the log. Every other secret travels only Swift to core ([`Secret`]
//! has no `Serialize`).

use anyhow::{anyhow, bail, Context as _, Result};
use open_live_ingest::api::{self, OscCredential, SaveError, Workspace};
use open_live_ingest::config::{self, AuthMode, Config, CredentialStore, InputSettings, Problem};
use open_live_ingest::devices;
use open_live_ingest::flow::{Input, Source};
use open_live_ingest::gateway::{self, InputState};
use open_live_ingest::keychain::{OsKeychain, SecretStore};
use open_live_ingest::media::{ExternalMedia, MediaRequest, SrtCallerStats};
use open_live_ingest::osc;
use open_live_ingest::run::{Sources, Stop};
use open_live_ingest::session::{self, Inputs, Session, SessionHandle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::instrument::WithSubscriber;

/// How a reply or event reaches Swift: one JSON message, on a core thread.
pub type Deliver = Arc<dyn Fn(String) + Send + Sync>;

/// Shows the Open Source Cloud sign-in page, after the core has told Swift with the
/// `osc_sign_in_page` event: in tests, a fake browser that comes back to the loopback
/// listener. On the phone Swift shows the page itself, from the event.
pub type Opener = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// How long the teardown on [`Core::shutdown`] may take before the core is freed
/// anyway: the CLI's bound for a process about to be ended (`run::ENDING_TEARDOWN`),
/// which is also what iOS allows a background task.
pub const SHUTDOWN_WITHIN: Duration = Duration::from_secs(4);

/// How long a lookup of the workspace and its instances may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

const NOT_UNDERSTOOD: &str = "The app did not understand that request. Restart the app.";
const STOP_FIRST: &str = "Press Stop before changing what or where to send.";
const SIGN_IN_WAITING: &str = "A sign-in is already open. Finish it, or close it and try again.";
const SIGN_IN_CANCELLED: &str = "The sign-in was closed before it finished.";
const NO_INPUT: &str = "Pick a camera first.";
const NOT_SAVED: &str = "Could not save the settings. Share logs for details.";

/// A credential Swift sends: an API key, a token, a passphrase. It can be read in the
/// core, but it cannot be serialised back, and it does not print.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The Advanced values (section 5). Each one absent is left as it is; an empty
/// passphrase, gateway id or gateway token, and a `pbkeylen` of 0, clear it.
#[derive(Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advanced {
    /// `gateway.name`, the source prefix.
    pub name: Option<String>,
    pub latency_ms: Option<u32>,
    pub passphrase: Option<Secret>,
    pub pbkeylen: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    /// `WxH`.
    pub resolution: Option<String>,
    /// `N/D`.
    pub framerate: Option<String>,
    pub log_level: Option<String>,
    pub gateway_id: Option<String>,
    pub gateway_token: Option<Secret>,
}

impl fmt::Debug for Advanced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Advanced(<redacted>)")
    }
}

/// What Swift's media is doing for one input, as `media_report` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    Starting,
    Running,
    Failed,
    Stopped,
}

/// The SRT sender's cumulative counters, from libsrt's `srt_bstats`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedStats {
    pub bytes_sent: Option<u64>,
    pub packets_sent: Option<u64>,
    pub packets_sent_dropped: Option<u64>,
    pub rtt_ms: Option<f64>,
    pub send_rate_mbps: Option<f64>,
}

/// One `media_report`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaReport {
    /// The `input` of the `media` start event: the input's id.
    pub input: String,
    pub state: MediaState,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub stats: Option<ReportedStats>,
}

/// The network the phone is on, as `NWPathMonitor` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Wifi,
    Cellular,
    Wired,
    #[default]
    Other,
    None,
}

/// One `phone` command: the app's state as iOS reports it (section 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneReport {
    pub foreground: bool,
    pub interrupted: bool,
    pub network: Network,
}

/// Every command Swift can send (section 8.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "cmd",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    State,
    OscSignIn,
    OscSignInCancel,
    OscPasteToken {
        token: Secret,
    },
    ChooseInstance {
        url: String,
    },
    SelfHosted {
        url: String,
        #[serde(default)]
        api_key: Option<Secret>,
    },
    SetInput {
        device: String,
        #[serde(default)]
        audio: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    SetAdvanced(Advanced),
    GoLive,
    Stop,
    MediaReport(MediaReport),
    Phone(PhoneReport),
    ForgetSignIn,
    /// Panics, in the command itself or in a task it starts: debug builds only, so
    /// `tests/abi.rs` can check that a panic becomes the `fatal` event and never
    /// unwinds into Swift.
    #[cfg(debug_assertions)]
    DebugPanic {
        #[serde(default)]
        in_task: bool,
    },
}

impl Command {
    /// The command's name, the one thing about it that is logged.
    pub fn name(&self) -> &'static str {
        match self {
            Command::State => "state",
            Command::OscSignIn => "osc_sign_in",
            Command::OscSignInCancel => "osc_sign_in_cancel",
            Command::OscPasteToken { .. } => "osc_paste_token",
            Command::ChooseInstance { .. } => "choose_instance",
            Command::SelfHosted { .. } => "self_hosted",
            Command::SetInput { .. } => "set_input",
            Command::SetAdvanced(_) => "set_advanced",
            Command::GoLive => "go_live",
            Command::Stop => "stop",
            Command::MediaReport(_) => "media_report",
            Command::Phone(_) => "phone",
            Command::ForgetSignIn => "forget_sign_in",
            #[cfg(debug_assertions)]
            Command::DebugPanic { .. } => "debug_panic",
        }
    }

    /// The commands that change where or what to send, refused while live.
    fn changes_settings(&self) -> bool {
        matches!(
            self,
            Command::OscSignIn
                | Command::OscPasteToken { .. }
                | Command::ChooseInstance { .. }
                | Command::SelfHosted { .. }
                | Command::SetInput { .. }
                | Command::SetAdvanced(_)
                | Command::ForgetSignIn
        )
    }
}

/// A reply, matched to its command by `id`; `None` for a message with no readable id.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reply {
    id: Option<u32>,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ok(Value),
    Error(ErrorReply),
}

/// An error reply's body: one line for the person. No action id yet; the phone's
/// screens (OL-64e) add them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ErrorReply {
    message: String,
    action: Option<&'static str>,
}

impl Reply {
    pub fn ok(id: u32, value: Value) -> Self {
        Reply {
            id: Some(id),
            outcome: Outcome::Ok(value),
        }
    }

    pub fn error(id: Option<u32>, message: &str) -> Self {
        Reply {
            id,
            outcome: Outcome::Error(ErrorReply {
                message: message.replace(['\r', '\n'], " "),
                action: None,
            }),
        }
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).expect("a reply is plain JSON")
    }
}

/// `{"event": "<name>", "data": {...}}`.
pub fn event_json(event: &str, data: Value) -> String {
    serde_json::to_string(&json!({ "event": event, "data": data })).expect("plain JSON")
}

/// One message from Swift, read as an id and a command; the reply to send instead
/// when it cannot be.
pub fn parse(raw: &str) -> Result<(u32, Command), Reply> {
    let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(raw) else {
        return Err(Reply::error(None, NOT_UNDERSTOOD));
    };
    let id = message
        .remove("id")
        .and_then(|id| id.as_u64())
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| Reply::error(None, NOT_UNDERSTOOD))?;
    let command = Command::deserialize(Value::Object(message))
        .map_err(|_| Reply::error(Some(id), NOT_UNDERSTOOD))?;
    Ok((id, command))
}

/// What the core is built over: the settings file, the keychain, Open Source Cloud.
/// Production's are [`Options::new`]'s; tests point them at fakes.
pub struct Options {
    pub settings_file: PathBuf,
    pub keychain: Arc<dyn SecretStore>,
    pub endpoints: osc::Endpoints,
    pub sign_in_wait: Duration,
    /// Opens the sign-in page besides telling Swift: a fake browser in tests.
    pub browser: Option<Opener>,
}

impl Options {
    /// The phone's: the settings at
    /// `<app_support_dir>/open-live-ingest/gateway.toml` (`config::user_dir()` on iOS),
    /// the iOS Keychain, and Open Source Cloud itself.
    pub fn new(app_support_dir: &Path) -> Self {
        Options {
            settings_file: app_support_dir
                .join("open-live-ingest")
                .join("gateway.toml"),
            keychain: Arc::new(OsKeychain::default()),
            endpoints: osc::Endpoints::production(),
            sign_in_wait: osc::SIGN_IN_TIMEOUT,
            browser: None,
        }
    }
}

/// The core Swift drives through the C ABI: one per app.
pub struct Core {
    shared: Arc<Shared>,
    runtime: Option<tokio::runtime::Runtime>,
    gate: Arc<Gate>,
}

/// What every delivery to Swift passes through, so that none follows
/// [`Core::shutdown`]: once it is closed, a delivery is dropped, and closing waits for
/// the ones already under way to return.
#[derive(Default)]
struct Gate {
    closed: AtomicBool,
    /// Held shared by each delivery for the length of Swift's callback, and taken
    /// exclusively by [`Gate::close`] to wait for those still running.
    in_flight: RwLock<()>,
}

impl Gate {
    /// Runs `deliver` unless the gate is closed.
    fn pass(&self, deliver: impl FnOnce()) {
        let _in_flight = self.in_flight.read().unwrap_or_else(|e| e.into_inner());
        if !self.closed.load(Ordering::Acquire) {
            deliver();
        }
    }

    /// Closes the gate for good. With `wait`, returns only once no delivery is under
    /// way any more; never wait from within a delivery, which would wait on itself.
    fn close(&self, wait: bool) {
        self.closed.store(true, Ordering::Release);
        if wait {
            drop(self.in_flight.write().unwrap_or_else(|e| e.into_inner()));
        }
    }
}

struct Shared {
    settings_file: PathBuf,
    keychain: Arc<dyn SecretStore>,
    endpoints: osc::Endpoints,
    sign_in_wait: Duration,
    browser: Option<Opener>,
    deliver: Deliver,
    /// The workspace last listed with the kept credential, in this run only.
    listed: Mutex<Option<Workspace>>,
    /// The sign-in waiting for the browser: its command's id and its task.
    sign_in: Mutex<Option<(u32, AbortHandle)>>,
    live: Mutex<Live>,
    phone: Mutex<Phone>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Live,
    Stopping,
}

/// The session, while there is one, and what the status lines are made of.
struct Live {
    phase: Phase,
    /// Counts Go lives, so a late end of an earlier one leaves a newer one alone.
    session: u64,
    handle: Option<SessionHandle>,
    media: Option<ExternalMedia>,
    board: Board,
}

/// What iOS last said about the app (section 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Phone {
    foreground: bool,
    interrupted: bool,
    network: Network,
}

impl Phone {
    /// Whether the camera and the uplink are paused: in the background, or taken by a
    /// call or another app.
    fn held(&self) -> bool {
        !self.foreground || self.interrupted
    }
}

impl Default for Phone {
    fn default() -> Self {
        Phone {
            foreground: true,
            interrupted: false,
            network: Network::Other,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panic elsewhere while holding it (caught at the ABI) leaves it usable.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Core {
    /// The core over `options`, delivering replies and events with `deliver`. On the
    /// first launch (no settings file yet) it writes the phone's own settings: a new
    /// gateway id and name and the phone's defaults (section 7.1).
    pub fn new(options: Options, deliver: Deliver) -> Result<Self> {
        let Options {
            settings_file,
            keychain,
            endpoints,
            sign_in_wait,
            browser,
        } = options;
        if !settings_file.exists() {
            let first = config::phone_settings()?;
            config::save_unfinished(&settings_file, &first)
                .context("writing the first settings")?;
            tracing::info!("first launch: settings written");
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oli-core")
            .enable_all()
            .build()
            .context("starting the core's runtime")?;
        let gate = Arc::new(Gate::default());
        let deliver: Deliver = {
            let gate = gate.clone();
            Arc::new(move |json: String| gate.pass(|| deliver(json)))
        };
        Ok(Core {
            gate,
            shared: Arc::new(Shared {
                settings_file,
                keychain,
                endpoints,
                sign_in_wait,
                browser,
                deliver,
                listed: Mutex::new(None),
                sign_in: Mutex::new(None),
                live: Mutex::new(Live {
                    phase: Phase::Idle,
                    session: 0,
                    handle: None,
                    media: None,
                    board: Board::default(),
                }),
                phone: Mutex::default(),
            }),
            runtime: Some(runtime),
        })
    }

    /// Going live, live or stopping.
    pub fn live(&self) -> bool {
        self.shared.live()
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime
            .as_ref()
            .expect("the runtime lives as long as the core")
    }

    /// Handles one message from Swift. The reply goes through [`Deliver`], at once or,
    /// for a command that waits on a server, when it is done.
    pub fn send(&self, raw: &str) {
        let (id, command) = match parse(raw) {
            Ok(parsed) => parsed,
            Err(reply) => {
                tracing::warn!("message with no readable id, unknown command or bad arguments");
                self.shared.reply(&reply);
                return;
            }
        };
        tracing::info!(id, cmd = command.name(), "command");
        if let Some(reply) = self.handle(id, command) {
            self.shared.reply(&reply);
        }
    }

    /// Runs `work` on the core's runtime. A panic in it is the `fatal` event: the
    /// task ends there, and nothing unwinds into Swift.
    fn spawn(&self, work: impl std::future::Future<Output = ()> + Send + 'static) -> AbortHandle {
        spawn_on(self.runtime().handle(), &self.shared, work)
    }

    /// Runs `work` and delivers the reply it makes.
    fn later(&self, work: impl std::future::Future<Output = Reply> + Send + 'static) {
        let shared = self.shared.clone();
        self.spawn(async move { shared.reply(&work.await) });
    }

    fn handle(&self, id: u32, command: Command) -> Option<Reply> {
        let shared = self.shared.clone();
        if command.changes_settings() && shared.live() {
            return Some(Reply::error(Some(id), STOP_FIRST));
        }
        let ok = |value: Value| Some(Reply::ok(id, value));
        match command {
            Command::State => {
                if shared.needs_lookup() {
                    let shared = shared.clone();
                    self.spawn(async move {
                        shared.refresh().await;
                        let view = shared.view_model();
                        shared.emit("where", view["where"].clone());
                    });
                }
                ok(shared.view_model())
            }
            Command::OscSignIn => {
                let mut waiting = lock(&shared.sign_in);
                if waiting.is_some() {
                    return Some(Reply::error(Some(id), SIGN_IN_WAITING));
                }
                let task = {
                    let shared = shared.clone();
                    async move {
                        let reply = match shared.sign_in().await {
                            Ok(value) => Reply::ok(id, value),
                            Err(err) => shared.failed(id, "osc_sign_in", &err, &[]),
                        };
                        lock(&shared.sign_in).take();
                        shared.reply(&reply);
                    }
                };
                *waiting = Some((id, self.spawn(task)));
                None
            }
            Command::OscSignInCancel => {
                if let Some((sign_in, task)) = lock(&shared.sign_in).take() {
                    task.abort();
                    tracing::info!("sign-in closed before it finished");
                    shared.reply(&Reply::error(Some(sign_in), SIGN_IN_CANCELLED));
                }
                ok(Value::Null)
            }
            Command::OscPasteToken { token } => {
                self.later(async move {
                    match shared.paste_token(token.expose()).await {
                        Ok(value) => Reply::ok(id, value),
                        Err(err) => shared.failed(id, "osc_paste_token", &err, &[token.expose()]),
                    }
                });
                None
            }
            Command::ChooseInstance { url } => Some(match shared.choose_instance(&url) {
                Ok(()) => Reply::ok(id, shared.view_model()),
                Err(err) => shared.failed(id, "choose_instance", &err, &[]),
            }),
            Command::SelfHosted { url, api_key } => {
                self.later(async move {
                    let key = api_key.as_ref().map(Secret::expose).unwrap_or_default();
                    match shared.self_hosted(&url, key).await {
                        Ok(()) => Reply::ok(id, shared.view_model()),
                        Err(err) => shared.failed(id, "self_hosted", &err, &[key]),
                    }
                });
                None
            }
            Command::SetInput {
                device,
                audio,
                label,
            } => Some(match shared.set_input(device, audio, label) {
                Ok(()) => Reply::ok(id, shared.view_model()),
                Err(err) => shared.failed(id, "set_input", &err, &[]),
            }),
            Command::SetAdvanced(advanced) => {
                self.later(async move {
                    let secrets: Vec<String> = [&advanced.passphrase, &advanced.gateway_token]
                        .into_iter()
                        .flatten()
                        .map(|s| s.expose().to_string())
                        .collect();
                    match shared.set_advanced(&advanced).await {
                        Ok(value) => Reply::ok(id, value),
                        Err(err) => {
                            let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
                            shared.failed(id, "set_advanced", &err, &secrets)
                        }
                    }
                });
                None
            }
            Command::GoLive => Some(self.go_live(id)),
            Command::Stop => {
                shared.stop(Stop::Interrupt);
                ok(Value::Null)
            }
            Command::MediaReport(report) => {
                shared.media_report(report);
                ok(Value::Null)
            }
            Command::Phone(report) => {
                shared.phone(report);
                ok(Value::Null)
            }
            Command::ForgetSignIn => Some(match shared.forget_sign_in() {
                Ok(()) => Reply::ok(id, shared.view_model()),
                Err(err) => shared.failed(id, "forget_sign_in", &err, &[]),
            }),
            #[cfg(debug_assertions)]
            Command::DebugPanic { in_task } => {
                if in_task {
                    self.spawn(async { panic!("debug_panic in a task") });
                    return ok(Value::Null);
                }
                panic!("debug_panic in the command")
            }
        }
    }

    /// Go live: checks the settings, then starts an armed session whose media Swift
    /// does ([`ExternalMedia`]), keeping its source at Stop (section 4). Replies once
    /// the session has started; progress follows as events.
    fn go_live(&self, id: u32) -> Reply {
        let shared = &self.shared;
        let settings = match shared.ready_to_go_live() {
            Ok(settings) => settings,
            Err(err) => return shared.failed(id, "go_live", &err, &[]),
        };
        let mut live = lock(&shared.live);
        if live.phase != Phase::Idle {
            return Reply::ok(id, Value::Null);
        }
        let credential = settings
            .open_live
            .uses_keychain()
            .then(|| OscCredential::KeychainAt(shared.keychain.clone(), shared.endpoints.clone()));
        let (media, requests) = ExternalMedia::new();
        media.hold(lock(&shared.phone).held());
        let capture = Capture::of(&settings);
        let (handle, events) = {
            // The session's tasks go on the core's runtime.
            let _runtime = self.runtime().enter();
            Session::start_armed_with(
                media.clone(),
                settings,
                Inputs::default(),
                credential,
                Sources::Keep,
            )
        };
        live.session += 1;
        live.phase = Phase::Live;
        live.handle = Some(handle);
        live.media = Some(media);
        live.board = Board::default();
        let session = live.session;
        drop(live);
        tracing::info!("going live");
        let task = forward(shared.clone(), session, events, requests, capture);
        self.spawn(task);
        Reply::ok(id, Value::Null)
    }

    /// Stops what is live and waits for its teardown, at most `within`, then stops the
    /// runtime. What `oli_core_free` does before the core is freed. The teardown's own
    /// events (`stopping`, `stopped`) are still delivered; once it is over, delivery
    /// is closed ([`Gate`]), waiting for a delivery still under way to return, so
    /// no delivery follows once this returns, even from a worker the runtime's
    /// shutdown leaves running.
    ///
    /// Called from a thread of the core's own runtime (inside a delivery), it cannot
    /// wait: the teardown is asked for, delivery is closed without waiting (only the
    /// delivery it is called from is still under way), and the runtime is left to end
    /// in the background.
    pub fn shutdown(&mut self, within: Duration) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        if let Some((_, task)) = lock(&self.shared.sign_in).take() {
            task.abort();
        }
        let handle = lock(&self.shared.live).handle.take();
        if tokio::runtime::Handle::try_current().is_ok() {
            if let Some(mut handle) = handle {
                handle.request_stop(Stop::Ending);
            }
            self.gate.close(false);
            runtime.shutdown_background();
            return;
        }
        if let Some(handle) = handle {
            tracing::info!("stopping what is live before the core is freed");
            let stopped = runtime.block_on(async {
                tokio::time::timeout(within, handle.stop_with(Stop::Ending)).await
            });
            if stopped.is_err() {
                tracing::warn!("the teardown did not finish within {within:?}; freed anyway");
            }
        }
        self.gate.close(true);
        runtime.shutdown_timeout(Duration::from_millis(250));
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.shutdown(SHUTDOWN_WITHIN);
    }
}

/// Runs `work` on `runtime`, with the caller's subscriber; a panic in it is the
/// `fatal` event.
fn spawn_on(
    runtime: &tokio::runtime::Handle,
    shared: &Arc<Shared>,
    work: impl std::future::Future<Output = ()> + Send + 'static,
) -> AbortHandle {
    let task = runtime.spawn(work.with_current_subscriber());
    let abort = task.abort_handle();
    let shared = shared.clone();
    runtime.spawn(
        async move {
            if let Err(err) = task.await {
                if err.is_panic() {
                    tracing::error!("a core task panicked");
                    shared.fatal();
                }
            }
        }
        .with_current_subscriber(),
    );
    abort
}

/// The video Swift is to capture, from the settings: what `media` start says.
#[derive(Debug, Clone, Copy)]
struct Capture {
    width: u32,
    height: u32,
    fps: f64,
}

impl Capture {
    fn of(settings: &Config) -> Self {
        let (width, height) = settings
            .capture
            .video_resolution
            .as_deref()
            .and_then(|r| config::parse_resolution(r).ok())
            .unwrap_or((1280, 720));
        let fps = settings
            .capture
            .video_framerate
            .as_deref()
            .and_then(|r| r.split_once('/'))
            .and_then(|(n, d)| Some((n.parse::<f64>().ok()?, d.parse::<f64>().ok()?)))
            .filter(|(_, d)| *d > 0.0)
            .map_or(30.0, |(n, d)| n / d);
        Capture { width, height, fps }
    }
}

/// The `media` event for one request of the session's (section 8.2).
fn media_event(request: &MediaRequest, capture: &Capture) -> Value {
    match request {
        MediaRequest::Start { input, video } => {
            let (device, audio) = match &input.source {
                Source::Device {
                    device_id,
                    audio_device,
                    ..
                } => (Some(device_id.as_str()), audio_device.as_deref()),
                Source::Test { .. } => (None, None),
            };
            let endpoint = &input.endpoint;
            let mut uplink = Map::new();
            uplink.insert("host".into(), json!(endpoint.cloud_host));
            uplink.insert("port".into(), json!(endpoint.port));
            uplink.insert("latency_ms".into(), json!(endpoint.latency_ms));
            if let Some(passphrase) = endpoint.passphrase() {
                uplink.insert("passphrase".into(), json!(passphrase));
            }
            if let Some(len) = endpoint.pbkeylen() {
                uplink.insert("pbkeylen".into(), json!(len));
            }
            json!({
                "input": input.id,
                "name": input.name,
                "action": "start",
                "device": device,
                "microphone": audio,
                "test_pattern": device.is_none(),
                "uplink": uplink,
                "video": {
                    "width": capture.width,
                    "height": capture.height,
                    "fps": capture.fps,
                    "bitrate_kbps": video.bitrate_kbps,
                    "keyframe_interval": video.keyframe_interval,
                },
                "audio": {"sample_rate": 48000, "channels": 2, "bitrate_kbps": 128},
            })
        }
        MediaRequest::Stop { input_id } => json!({"input": input_id, "action": "stop"}),
        MediaRequest::Restart { input_id } => json!({"input": input_id, "action": "restart"}),
    }
}

/// Hands the session's events and its media requests to Swift until it ends.
async fn forward(
    shared: Arc<Shared>,
    session: u64,
    mut events: session::Receiver,
    mut requests: mpsc::UnboundedReceiver<MediaRequest>,
    capture: Capture,
) {
    let mut requests_open = true;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                let last = matches!(event, gateway::Event::Stopped | gateway::Event::Failed { .. });
                let out = {
                    let mut live = lock(&shared.live);
                    if live.session != session {
                        break;
                    }
                    let mut out = live.board.on_event(&event);
                    if matches!(event, gateway::Event::Stats { .. }) {
                        out.push(("status", live.board.status(lock(&shared.phone).network)));
                    }
                    if last {
                        live.phase = Phase::Idle;
                        live.handle = None;
                        live.media = None;
                    }
                    out
                };
                for (name, data) in out {
                    shared.emit(name, data);
                }
                if last {
                    break;
                }
            }
            request = requests.recv(), if requests_open => match request {
                Some(request) => {
                    if let MediaRequest::Start { input, .. } = &request {
                        lock(&shared.live).board.started(input);
                    }
                    shared.emit("media", media_event(&request, &capture));
                }
                None => requests_open = false,
            },
        }
    }
    // Requests sent as the session ended (the stop of its teardown).
    while let Ok(request) = requests.try_recv() {
        shared.emit("media", media_event(&request, &capture));
    }
}

impl Shared {
    fn live(&self) -> bool {
        lock(&self.live).phase != Phase::Idle
    }

    fn reply(&self, reply: &Reply) {
        (self.deliver)(reply.json());
    }

    /// Hands Swift one event. Only its name is logged: `media` carries the passphrase.
    fn emit(&self, event: &'static str, data: Value) {
        match event {
            "stats" | "status" => tracing::debug!(event, "event"),
            _ => tracing::info!(event, "event"),
        }
        (self.deliver)(event_json(event, data));
    }

    /// A panic was caught: the app shows "Something went wrong" and offers Share logs.
    pub(crate) fn fatal(&self) {
        (self.deliver)(fatal_json());
    }

    /// The error reply for a command that failed, logged by name with the same line.
    /// An error chain never quotes a credential; `secrets` sent with the command are
    /// taken out all the same, should one have ended up in it.
    fn failed(&self, id: u32, name: &'static str, err: &anyhow::Error, secrets: &[&str]) -> Reply {
        let mut line = format!("{err:#}").replace(['\r', '\n'], " ");
        for secret in secrets.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            line = line.replace(secret, "<redacted>");
        }
        tracing::warn!(id, cmd = name, "command failed: {line}");
        Reply::error(Some(id), &line)
    }

    fn load(&self) -> Result<Config> {
        // The parse error is not quoted further: toml quotes the offending line.
        api::load_from(&self.settings_file).map_err(|err| {
            anyhow!(
                "The settings file has a problem: {}",
                err.root_cause()
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
            )
        })
    }

    fn save(&self, settings: &Config) -> Result<()> {
        match api::save_to(&self.settings_file, settings) {
            Ok(()) => Ok(()),
            Err(SaveError::Invalid(problems)) => Err(anyhow!(
                "{}",
                problems.first().map_or("", |p| p.message.as_str())
            )),
            Err(SaveError::Write(err)) => {
                tracing::warn!("saving the settings: {err:#}");
                Err(anyhow!(NOT_SAVED))
            }
        }
    }

    /// Saves settings that may not say where to send yet (section 7).
    fn save_unfinished(&self, settings: &Config) -> Result<()> {
        config::save_unfinished(&self.settings_file, settings)
    }

    fn listed(&self) -> Option<Workspace> {
        lock(&self.listed).clone()
    }

    fn kept(&self) -> OscCredential {
        OscCredential::KeychainAt(self.keychain.clone(), self.endpoints.clone())
    }

    /// A sign-in is kept but its workspace has not been listed this run.
    fn needs_lookup(&self) -> bool {
        self.listed().is_none()
            && self
                .load()
                .is_ok_and(|settings| settings.open_live.uses_keychain())
    }

    /// Lists the kept sign-in's workspace and instances, for the names in the view.
    async fn refresh(&self) {
        match lookup(api::workspace_at(&self.endpoints, &self.kept())).await {
            Ok(workspace) => *lock(&self.listed) = Some(workspace),
            Err(err) => tracing::warn!("looking up the workspace: {err:#}"),
        }
    }

    /// The library's loopback sign-in (section 6): the page goes to Swift as
    /// `osc_sign_in_page`, `osc_signed_in` follows the moment the browser comes back,
    /// and the reply lists the workspace.
    async fn sign_in(&self) -> Result<Value> {
        let browser = self.browser.clone();
        let deliver = self.deliver.clone();
        let signed_in = osc::sign_in(&self.endpoints.connect, self.sign_in_wait, |url| {
            // Public values only: the client id, the redirect, the PKCE challenge, state.
            tracing::info!(event = "osc_sign_in_page", "event");
            deliver(event_json("osc_sign_in_page", json!({ "url": url })));
            browser.map_or(Ok(()), |open| open(url))
        })
        .await?;
        self.emit("osc_signed_in", json!({}));
        let mut scratch = Config::default();
        api::keep_osc_sign_in(&mut scratch, self.keychain.as_ref(), &signed_in)?;
        let workspace = lookup(api::workspace_at(
            &self.endpoints,
            &OscCredential::from(&signed_in),
        ))
        .await?;
        self.signed_in(workspace)
    }

    /// Keeps a pasted personal access token, once Open Source Cloud has listed its
    /// workspace with it: a token that does not work is not kept.
    async fn paste_token(&self, token: &str) -> Result<Value> {
        let token = token.trim();
        if token.is_empty() {
            bail!("Paste a personal access token from the Open Source Cloud console.");
        }
        let workspace = lookup(api::workspace_at(
            &self.endpoints,
            &OscCredential::Token(token.to_string()),
        ))
        .await
        .context("Open Source Cloud did not accept that token")?;
        let mut scratch = Config::default();
        api::keep_osc_token(&mut scratch, self.keychain.as_ref(), token)?;
        self.signed_in(workspace)
    }

    /// After a sign-in or a pasted token: the workspace is listed, and the instance
    /// is saved when there is no choice to make (the one saved is in the workspace,
    /// or the workspace has exactly one).
    fn signed_in(&self, workspace: Workspace) -> Result<Value> {
        let mut settings = self.load()?;
        let saved = settings
            .open_live
            .url
            .clone()
            .filter(|url| workspace.instances.iter().any(|i| &i.url == url));
        let pick = match (saved, workspace.instances.as_slice()) {
            (Some(url), _) => Some(url),
            (None, [only]) => Some(only.url.clone()),
            (None, _) => None,
        };
        *lock(&self.listed) = Some(workspace.clone());
        if let Some(url) = pick {
            settings.open_live.url = Some(url);
            point_at_keychain(&mut settings);
            self.save(&settings)?;
        }
        Ok(json!({
            "workspace": workspace.id,
            "instances": workspace.instances,
            "where": self.view_model()["where"],
        }))
    }

    fn choose_instance(&self, url: &str) -> Result<()> {
        let listed = self
            .listed()
            .ok_or_else(|| anyhow!("Sign in with Open Source Cloud first."))?;
        if !listed.instances.iter().any(|i| i.url == url) {
            bail!("That instance is not in the signed-in workspace. Pick one from the list.");
        }
        let mut settings = self.load()?;
        settings.open_live.url = Some(url.to_string());
        point_at_keychain(&mut settings);
        self.save(&settings)
    }

    /// A self-hosted Open Live, once it has answered with `api_key` (blank for none).
    /// A kept Open Source Cloud sign-in is removed: one Open Live at a time.
    async fn self_hosted(&self, url: &str, api_key: &str) -> Result<()> {
        let url = url.trim();
        let url = if url.contains("://") || url.is_empty() {
            url.to_string()
        } else {
            format!("https://{url}")
        };
        let api_key = Some(api_key.trim()).filter(|k| !k.is_empty());
        api::check_self_hosted(&url, api_key)
            .await
            .map_err(|problem| anyhow!(problem.message))?;
        let mut settings = self.load()?;
        if settings.open_live.credential.is_some() || self.listed().is_some() {
            api::forget_osc_credential(&mut settings, self.keychain.as_ref())
                .context("removing the Open Source Cloud sign-in from the keychain")?;
        }
        settings.open_live.url = Some(url);
        settings.open_live.auth_mode = AuthMode::Direct;
        settings.open_live.api_key = api_key.map(str::to_string);
        settings.open_live.credential = None;
        self.save(&settings)?;
        *lock(&self.listed) = None;
        Ok(())
    }

    /// The one camera and microphone to send, as `[[input]]` (section 7): `device` the
    /// AVFoundation unique id of the lens, `audio` the microphone's port UID.
    fn set_input(
        &self,
        device: String,
        audio: Option<String>,
        label: Option<String>,
    ) -> Result<()> {
        let entry = InputSettings {
            device: device.trim().to_string(),
            audio: audio
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty()),
            label: label
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
        };
        if let Some(problem) = config::input_problems(std::slice::from_ref(&entry)).first() {
            bail!("{}", problem.message);
        }
        let mut settings = self.load()?;
        settings.inputs = vec![entry];
        self.save_unfinished(&settings)
    }

    /// Advanced (section 5): applied, checked with the phone's rules, saved. Problems
    /// are the reply (`{"problems": [...]}`) and nothing is saved; a new venue name
    /// whose prefix Open Live already has sources under is saved with a warning
    /// (`warnings`), since a Stop sweeps that prefix (section 7.1).
    async fn set_advanced(&self, advanced: &Advanced) -> Result<Value> {
        let mut settings = self.load()?;
        let renamed = apply(&mut settings, advanced);
        let problems: Vec<Problem> = phone_problems(&settings)
            .into_iter()
            .filter(|p| !unfinished(p))
            .collect();
        if !problems.is_empty() {
            return Ok(json!({ "problems": problems }));
        }
        let mut warnings = Vec::new();
        if renamed && settings.open_live.url.is_some() {
            let credential = settings.open_live.uses_keychain().then(|| self.kept());
            match api::sources_named_like(&settings, credential.as_ref()).await {
                Ok(names) if !names.is_empty() => warnings.push(Problem::new(
                    "gateway.name",
                    format!(
                        "Open Live already has {} source(s) named \"{}…\" that this phone did not register. Stop removes the other sources of this name, so pick a name of its own.",
                        names.len(),
                        devices::name_prefix(&settings.gateway.name)
                    ),
                )),
                Ok(_) => {}
                Err(err) => tracing::warn!("could not check the new name against Open Live: {err:#}"),
            }
        }
        self.save_unfinished(&settings)?;
        let mut view = self.view_model();
        view["warnings"] = json!(warnings);
        Ok(view)
    }

    fn forget_sign_in(&self) -> Result<()> {
        let mut settings = self.load()?;
        api::forget_osc_credential(&mut settings, self.keychain.as_ref())?;
        *lock(&self.listed) = None;
        self.save_unfinished(&settings)
    }

    /// The settings, if they are ready to go live with.
    fn ready_to_go_live(&self) -> Result<Config> {
        let settings = self.load()?;
        if settings.inputs.is_empty() {
            bail!(NO_INPUT);
        }
        if let Some(problem) = phone_problems(&settings).first() {
            bail!("The settings have a problem: {}", problem.message);
        }
        Ok(settings)
    }

    fn stop(&self, why: Stop) {
        let mut live = lock(&self.live);
        if live.phase == Phase::Live {
            live.phase = Phase::Stopping;
            if let Some(handle) = live.handle.as_mut() {
                handle.request_stop(why);
            }
        }
    }

    fn media_report(&self, report: MediaReport) {
        let MediaReport {
            input,
            state,
            reason,
            stats,
        } = report;
        let mut live = lock(&self.live);
        let Some(media) = live.media.clone() else {
            return;
        };
        let running = matches!(state, MediaState::Starting | MediaState::Running);
        let stats = stats.filter(|_| running).map(|s| SrtCallerStats {
            bytes_sent: s.bytes_sent,
            packets_sent: s.packets_sent,
            packets_sent_dropped: s.packets_sent_dropped,
            rtt_ms: s.rtt_ms,
            send_rate_mbps: s.send_rate_mbps,
            ..SrtCallerStats::default()
        });
        media.report(&input, running, stats.clone());
        let failed = live.board.reported(&input, running, stats);
        drop(live);
        if state == MediaState::Failed {
            // From Swift's own code, never a credential.
            let reason = reason.unwrap_or_else(|| "the camera or the uplink failed".into());
            tracing::warn!(input = %input, "media failed: {reason}");
            if let Some(name) = failed {
                self.emit(
                    "input",
                    json!({"input": name, "state": "failed", "message": reason}),
                );
            }
        }
    }

    /// `phone` (section 9): the uplink is held while the app is in the background or
    /// interrupted; the network is noted in the status.
    fn phone(&self, report: PhoneReport) {
        let phone = Phone {
            foreground: report.foreground,
            interrupted: report.interrupted,
            network: report.network,
        };
        let before = std::mem::replace(&mut *lock(&self.phone), phone);
        let live = lock(&self.live);
        if let Some(media) = &live.media {
            if before.held() != phone.held() {
                tracing::info!(
                    held = phone.held(),
                    "the phone holds the uplink back, or lets it go"
                );
            }
            media.hold(phone.held());
        }
        let status = live
            .media
            .is_some()
            .then(|| live.board.status(phone.network));
        drop(live);
        if let Some(status) = status.filter(|_| before.network != phone.network) {
            self.emit("status", status);
        }
    }

    /// The view model `state` returns.
    fn view_model(&self) -> Value {
        let live = self.live();
        let network = lock(&self.phone).network;
        let settings = match self.load() {
            Ok(settings) => settings,
            Err(err) => {
                return json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "live": live,
                    "network": network,
                    "problem": format!("{err:#}"),
                })
            }
        };
        let input = settings.inputs.first().map(|i| {
            json!({
                "device": i.device,
                "audio": i.audio,
                "label": i.label,
                "studio_name": format!(
                    "{}{}",
                    devices::name_prefix(&settings.gateway.name),
                    i.label().unwrap_or("iPhone")
                ),
            })
        });
        let uplink = &settings.uplink;
        let open_live = &settings.open_live;
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "live": live,
            "network": network,
            "where": self.where_view(&settings),
            "input": input,
            "advanced": {
                "name": settings.gateway.name,
                "latency_ms": uplink.latency_ms,
                "passphrase_set": uplink.passphrase.as_deref().is_some_and(|p| !p.is_empty()),
                "pbkeylen": uplink.pbkeylen,
                "bitrate_kbps": settings.video.bitrate_kbps,
                "resolution": settings.capture.video_resolution,
                "framerate": settings.capture.video_framerate,
                "log_level": settings.log.level,
                "gateway_id": open_live.gateway_id,
                "gateway_token_set": open_live.gateway_token.as_deref().is_some_and(|t| !t.is_empty()),
            },
            "problems": phone_problems(&settings)
                .into_iter()
                .filter(|p| !unfinished(p))
                .collect::<Vec<_>>(),
        })
    }

    /// Step 1, "Where to send". Holds no secret: whether an API key is set, never the
    /// key; the Open Source Cloud credential is in the keychain and not read here.
    fn where_view(&self, settings: &Config) -> Value {
        let open_live = &settings.open_live;
        let listed = self.listed();
        let url = open_live
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty());
        let signed_in = open_live.uses_keychain();
        let mode = match (open_live.auth_mode, url) {
            (AuthMode::Osc, _) => "osc",
            (AuthMode::Direct, Some(_)) => "self_hosted",
            (AuthMode::Direct, None) => "none",
        };
        let named = url.and_then(|url| {
            listed
                .as_ref()?
                .instances
                .iter()
                .find(|i| i.url == url)
                .map(|i| i.name.clone())
        });
        let instance = named.or_else(|| url.map(host));
        let ready = match mode {
            "osc" => signed_in && url.is_some(),
            "self_hosted" => true,
            _ => false,
        };
        let workspace = listed.as_ref().and_then(|l| l.id.clone());
        json!({
            "mode": mode,
            "ready": ready,
            "url": url,
            "instance": instance,
            "workspace": workspace,
            "instances": listed.map(|l| l.instances).unwrap_or_default(),
            "signed_in": signed_in || self.listed().is_some(),
            "api_key_set": mode == "self_hosted"
                && open_live.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
            "summary": match (ready, &instance, &workspace) {
                (true, Some(i), Some(w)) if mode == "osc" => Some(format!("Sending to {i} ({w})")),
                (true, Some(i), _) => Some(format!("Sending to {i}")),
                _ => None,
            },
        })
    }
}

/// `{"event":"fatal",...}`, for a panic caught at the ABI or in a core task.
pub fn fatal_json() -> String {
    event_json(
        "fatal",
        json!({"message": "Something went wrong. Share logs for details."}),
    )
}

/// Applies the Advanced values to `settings`; whether the venue name changed.
fn apply(settings: &mut Config, advanced: &Advanced) -> bool {
    let set = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    let before = settings.gateway.name.clone();
    if let Some(name) = &advanced.name {
        settings.gateway.name = name.trim().to_string();
    }
    if let Some(ms) = advanced.latency_ms {
        settings.uplink.latency_ms = ms;
    }
    if let Some(passphrase) = &advanced.passphrase {
        settings.uplink.passphrase = set(passphrase.expose());
    }
    if let Some(len) = advanced.pbkeylen {
        settings.uplink.pbkeylen = (len != 0).then_some(len);
    }
    if let Some(kbps) = advanced.bitrate_kbps {
        settings.video.bitrate_kbps = kbps;
    }
    if let Some(resolution) = &advanced.resolution {
        settings.capture.video_resolution = set(resolution);
    }
    if let Some(rate) = &advanced.framerate {
        settings.capture.video_framerate = set(rate);
    }
    if let Some(level) = &advanced.log_level {
        settings.log.level = set(level).unwrap_or_else(|| "info".to_string());
    }
    if let Some(id) = &advanced.gateway_id {
        settings.open_live.gateway_id = set(id);
    }
    if let Some(token) = &advanced.gateway_token {
        settings.open_live.gateway_token = set(token.expose());
    }
    settings.gateway.name != before
}

/// Every problem the settings have for the phone, except a missing address or
/// credential (not chosen yet): the library's checks and the phone's own
/// (`config::phone_problems`, part of the library's on iOS).
fn phone_problems(settings: &Config) -> Vec<Problem> {
    // The phone's own first: a desktop's file copied over is what they catch.
    let mut found = config::phone_problems(settings);
    for problem in config::problems(settings) {
        if !found.contains(&problem) {
            found.push(problem);
        }
    }
    if settings.gateway.name.trim().is_empty() {
        found.push(Problem::new(
            "gateway.name",
            "the venue name is empty; it prefixes the source name in Studio",
        ));
    }
    found
}

/// A problem that only says where to send has not been chosen yet.
fn unfinished(problem: &Problem) -> bool {
    matches!(problem.setting, Some("open_live.url" | "open_live.api_key"))
}

/// The settings point at the Open Source Cloud credential in the keychain, with no
/// token of their own, as `api::keep_osc_sign_in` leaves them.
fn point_at_keychain(settings: &mut Config) {
    settings.open_live.auth_mode = AuthMode::Osc;
    settings.open_live.credential = Some(CredentialStore::Keychain);
    settings.open_live.api_key = None;
}

async fn lookup(work: impl std::future::Future<Output = Result<Workspace>>) -> Result<Workspace> {
    tokio::time::timeout(LOOKUP_TIMEOUT, work)
        .await
        .map_err(|_| anyhow!("Open Source Cloud did not answer within {LOOKUP_TIMEOUT:?}"))?
}

/// The host of an address, for naming an Open Live that has no listed name.
fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_string()
}

// ---------------------------------------------------------------------------
// From the session's events to Swift's
// ---------------------------------------------------------------------------

/// What Swift has reported for one started input.
#[derive(Debug, Clone, Default)]
struct Link {
    name: String,
    running: bool,
    stats: Option<SrtCallerStats>,
}

/// The session as Swift sees it: the library's events turned into the desktop's
/// (desktop spec, section 9.3) and the status lines' parts.
#[derive(Debug, Default)]
struct Board {
    /// Per input id, what Swift last reported.
    links: HashMap<String, Link>,
    /// Per input name, the live productions holding its source; absent while armed.
    productions: BTreeMap<String, Vec<String>>,
    /// Every input by name, in the order they were heard of.
    inputs: Vec<String>,
    stopping: Option<(usize, usize)>,
    left_behind: Vec<String>,
}

impl Board {
    fn input(&mut self, name: &str) {
        if !self.inputs.iter().any(|i| i == name) {
            self.inputs.push(name.to_string());
        }
    }

    fn started(&mut self, input: &Input) {
        self.links.insert(
            input.id.clone(),
            Link {
                name: input.name.clone(),
                running: true,
                stats: None,
            },
        );
    }

    /// A report for an input started this session; its name when it is one.
    fn reported(
        &mut self,
        id: &str,
        running: bool,
        stats: Option<SrtCallerStats>,
    ) -> Option<String> {
        let link = self.links.get_mut(id)?;
        link.running = running;
        link.stats = stats;
        Some(link.name.clone())
    }

    fn on_event(&mut self, event: &gateway::Event) -> Vec<(&'static str, Value)> {
        use gateway::Event as E;
        let row = |input: &str, state: &str, message: Option<&str>| {
            (
                "input",
                json!({"input": input, "state": state, "message": message}),
            )
        };
        let mut out = Vec::new();
        match event {
            E::Registering { input } => {
                self.input(input);
                out.push(row(input, "registering", None));
            }
            E::InputFailed { input, reason } => {
                self.input(input);
                out.push(row(input, "failed", Some(reason)));
            }
            E::Input { input, state } => {
                self.input(input);
                let (state, message) = match state {
                    InputState::Armed => {
                        self.productions.remove(input);
                        (
                            "armed",
                            Some("Ready. Nothing is sent until a production that holds this source is activated in Studio."),
                        )
                    }
                    InputState::Starting | InputState::Waiting | InputState::Unreachable => {
                        ("connecting", None)
                    }
                    InputState::OnAir => ("live", None),
                    InputState::Failed { reason } => ("failed", Some(reason.as_str())),
                };
                out.push(row(input, state, message));
            }
            E::ProductionLive { input, productions } => {
                self.productions.insert(input.clone(), productions.clone());
            }
            E::NoProductionLive { input } => {
                self.productions.remove(input);
            }
            E::SlotAddedWhileLive { input, productions } if !productions.is_empty() => {
                out.push((
                    "notice",
                    json!({
                        "kind": "slot_added_while_live",
                        "input": input,
                        "productions": productions,
                        "message": "A production got this source while live, so Open Live has no receiver for it yet. The director has to deactivate and reactivate the production in Studio.",
                    }),
                ));
            }
            E::Summary { inputs } => out.push((
                "summary",
                json!({
                    "inputs": inputs.iter().map(|r| json!({
                        "input": r.name,
                        "port": r.port,
                        "uplink": r.uplink,
                        "registered": r.address,
                    })).collect::<Vec<Value>>()
                }),
            )),
            E::Stats { inputs } => out.push((
                "stats",
                json!({
                    "inputs": inputs.iter().map(|s| json!({
                        "input": s.input,
                        "bitrate_kbps": s.uplink.map(|u| u.bitrate_kbps),
                        "rtt_ms": s.uplink.map(|u| u.rtt_ms),
                        "dropped": s.uplink.map(|u| u.dropped),
                    })).collect::<Vec<Value>>()
                }),
            )),
            E::Warning { message } => out.push((
                "notice",
                json!({"kind": "warning", "message": message}),
            )),
            E::ConnectWarning { input, warning } => out.push((
                "notice",
                json!({"kind": "connect_warning", "input": input, "message": warning}),
            )),
            E::OscSignInEnds { at } => out.push((
                "notice",
                json!({
                    "kind": "sign_in_ends",
                    "at": at,
                    "message": "Your sign-in ends during this session. Sign in again, or paste a personal access token under Advanced.",
                }),
            )),
            E::Stopping => {
                let total = self.inputs.len();
                self.stopping = Some((0, total));
                for input in &self.inputs {
                    out.push(row(input, "stopping", None));
                }
                out.push(("stopping", json!({"done": 0, "total": total})));
            }
            E::FlowRemoved { input } => {
                out.push(row(input, "stopped", None));
                if let Some((done, total)) = &mut self.stopping {
                    *done = (*done + 1).min(*total);
                    out.push(("stopping", json!({"done": *done, "total": *total})));
                }
            }
            E::SourceKept { input, .. } => {
                if !self.left_behind.contains(input) {
                    self.left_behind.push(input.clone());
                }
            }
            E::Stopped => out.push((
                "stopped",
                json!({"left_behind": self.left_behind, "error": null}),
            )),
            E::Failed { reason } => out.push((
                "stopped",
                json!({
                    "left_behind": self.left_behind,
                    "error": {"message": reason.replace(['\r', '\n'], " "), "action": null},
                }),
            )),
            _ => {}
        }
        out
    }

    /// The status lines (desktop spec, section 5.1): per input, the live productions
    /// holding its source and the link as Swift reports it; and the network.
    fn status(&self, network: Network) -> Value {
        let inputs: Vec<Value> = self
            .inputs
            .iter()
            .map(|name| {
                let link = self.links.values().find(|l| &l.name == name);
                let (state, dropped_pct, rtt_ms) = match link {
                    Some(Link {
                        running: true,
                        stats: Some(s),
                        ..
                    }) => {
                        let dropped = match (s.packets_sent, s.packets_sent_dropped) {
                            (Some(sent), Some(dropped)) if sent > 0 => {
                                Some(dropped as f64 * 100.0 / sent as f64)
                            }
                            _ => None,
                        };
                        ("connected", dropped, s.rtt_ms)
                    }
                    Some(Link { running: true, .. }) => ("connecting", None, None),
                    _ => ("not_sending", None, None),
                };
                json!({
                    "input": name,
                    "productions": self.productions.get(name),
                    "link": {"state": state, "dropped_pct": dropped_pct, "rtt_ms": rtt_ms},
                })
            })
            .collect();
        json!({"inputs": inputs, "network": network})
    }
}

#[cfg(test)]
mod tests;
