//! The app API between the page and the app core (docs/specs/desktop-app.md, section 9).
//!
//! Page to core: `{"id": <u32>, "cmd": "<name>", "args": {...}}`, posted with
//! `window.ipc.postMessage`. Core to page: a reply `{"id": <u32>, "ok": <value>}` or
//! `{"id": <u32>, "error": {"message": "<one line>", "action": <id or null>}}`, and
//! events `{"event": "<name>", "data": {...}}`, both delivered by evaluating
//! `window.oli.receive(<json>)` in the page.
//!
//! Commands are one serde enum; an unknown command or bad arguments is an error reply,
//! never a panic. Secrets only travel page to core: [`Secret`] has no `Serialize`, so no
//! reply or event can carry one, and its `Debug` is redacted. Only the command name is
//! logged, never the arguments, and an error reply never quotes what the page sent.

use crate::where_to_send::{Opener, WhereToSend, WhereView};
use open_live_ingest::keychain::OsKeychain;
use open_live_ingest::osc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::instrument::WithSubscriber;

/// A credential the page sends: an API key, a token, a passphrase. It can be read in the
/// core, but it cannot be serialised back to the page, and it does not print.
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

/// The values of the Advanced section, as the page sends them. They include the SRT
/// passphrase, the Strom API key and the gateway token, so they are treated like a
/// [`Secret`]: no `Serialize`, and a redacted `Debug`.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct AdvancedArgs(Map<String, Value>);

impl AdvancedArgs {
    pub fn values(&self) -> &Map<String, Value> {
        &self.0
    }
}

impl fmt::Debug for AdvancedArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdvancedArgs(<redacted>)")
    }
}

/// One input to send, as `set_inputs` names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputArgs {
    pub device: String,
    #[serde(default)]
    pub audio: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Which log `show_logs` reveals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFile {
    App,
    Strom,
}

/// Which page `open_url` opens. A name, not a URL: the URLs come from the settings, so
/// the page cannot make the app open an arbitrary address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Strom,
    Studio,
    Help,
}

/// Every command the page can send (section 9.2).
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
    OscPasteToken { token: Secret },
    OscWorkspaces,
    ChooseInstance { url: String },
    SelfHosted { url: String, api_key: Secret },
    Devices { show_all: bool },
    SetInputs { inputs: Vec<InputArgs> },
    SetAdvanced(AdvancedArgs),
    GoLive,
    Stop,
    StopOther,
    Quit { confirmed: bool },
    ShowLogs { which: LogFile },
    OpenUrl { which: Link },
}

impl Command {
    /// The command's name, the one thing about it that is logged.
    pub fn name(&self) -> &'static str {
        match self {
            Command::State => "state",
            Command::OscSignIn => "osc_sign_in",
            Command::OscPasteToken { .. } => "osc_paste_token",
            Command::OscWorkspaces => "osc_workspaces",
            Command::ChooseInstance { .. } => "choose_instance",
            Command::SelfHosted { .. } => "self_hosted",
            Command::Devices { .. } => "devices",
            Command::SetInputs { .. } => "set_inputs",
            Command::SetAdvanced(_) => "set_advanced",
            Command::GoLive => "go_live",
            Command::Stop => "stop",
            Command::StopOther => "stop_other",
            Command::Quit { .. } => "quit",
            Command::ShowLogs { .. } => "show_logs",
            Command::OpenUrl { .. } => "open_url",
        }
    }
}

/// An error reply's body: one line for the person, and what they can do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorReply {
    pub message: String,
    pub action: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ok(Value),
    Error(ErrorReply),
}

/// The answer to one command, matched to it by `id`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reply {
    id: u32,
    #[serde(flatten)]
    outcome: Outcome,
}

impl Reply {
    pub fn ok(id: u32, value: Value) -> Self {
        Reply {
            id,
            outcome: Outcome::Ok(value),
        }
    }

    pub fn error(id: u32, message: &str) -> Self {
        Reply {
            id,
            outcome: Outcome::Error(ErrorReply {
                message: message.to_string(),
                action: None,
            }),
        }
    }

    /// The script that hands this reply to the page.
    pub fn script(&self) -> String {
        receive_script(self)
    }
}

/// Something the core tells the page without being asked: progress, state, notices.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    event: &'static str,
    data: Value,
}

impl Event {
    pub fn new(event: &'static str, data: Value) -> Self {
        Event { event, data }
    }

    /// The script that hands this event to the page.
    pub fn script(&self) -> String {
        receive_script(self)
    }
}

/// `window.oli.receive(<json>)`. JSON is a JavaScript expression; U+2028 and U+2029 are
/// escaped as well, for engines older than ES2019 that end a line at them.
fn receive_script(message: &impl Serialize) -> String {
    let json = serde_json::to_string(message)
        .expect("a reply or event is plain JSON")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!("window.oli.receive({json})")
}

/// What the window has to do after a command, besides delivering the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
}

/// What a message from the page could not be read as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Malformed {
    /// Not a JSON object with a `u32` `id`: there is no one to reply to.
    NoId,
    /// An `id`, but no known command with valid arguments: an error reply.
    BadCommand { id: u32 },
}

/// One message from the page, read as an id and a command.
pub fn parse(raw: &str) -> Result<(u32, Command), Malformed> {
    let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(raw) else {
        return Err(Malformed::NoId);
    };
    let id = message
        .remove("id")
        .and_then(|id| id.as_u64())
        .and_then(|id| u32::try_from(id).ok())
        .ok_or(Malformed::NoId)?;
    let command =
        Command::deserialize(Value::Object(message)).map_err(|_| Malformed::BadCommand { id })?;
    Ok((id, command))
}

const NOT_UNDERSTOOD: &str = "The app did not understand that request. Restart the app.";
const NOT_YET: &str = "This version of Open Live Ingest cannot do that yet.";

/// The view model `state` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewModel {
    pub version: &'static str,
    pub settings_file: String,
    pub live: bool,
    /// Step 1, "Where to send".
    #[serde(rename = "where")]
    pub where_to_send: WhereView,
}

/// Hands a script to the page: how replies that come later, and events, reach it. The
/// window evaluates it in the webview on the main thread.
pub type Deliver = Arc<dyn Fn(String) + Send + Sync>;

/// The app core the dispatcher drives. The window owns one and hands it every message.
///
/// A command that answers from what is at hand (`state`, `choose_instance`, `quit`)
/// is answered by [`dispatch`](Self::dispatch). One that waits on a server or on the
/// person in the browser runs on the core's own runtime, and its reply goes through
/// [`Deliver`] when it is done, so the window never stops answering meanwhile.
pub struct Core {
    settings_file: PathBuf,
    live: bool,
    where_to_send: Arc<WhereToSend>,
    deliver: Deliver,
    runtime: tokio::runtime::Runtime,
}

impl Core {
    /// The core over the settings file, the OS keychain and Open Source Cloud itself.
    pub fn new(settings_file: PathBuf, deliver: Deliver) -> Self {
        let page = deliver.clone();
        let open: Opener = Arc::new(move |url: &str| {
            // Shown in the window too, for when no browser opens. The address holds
            // only public values: the client id, the redirect, the PKCE challenge and
            // the state.
            page(Event::new("osc_sign_in_page", json!({ "url": url })).script());
            if let Err(err) = osc::open_browser(url) {
                tracing::warn!("could not open the system browser for the sign-in: {err:#}");
            }
            Ok(())
        });
        let where_to_send = WhereToSend::new(
            settings_file.clone(),
            Arc::new(OsKeychain::default()),
            osc::Endpoints::production(),
            osc::SIGN_IN_TIMEOUT,
            open,
        );
        Self::with_where_to_send(settings_file, where_to_send, deliver)
    }

    /// The core over a step 1 model of the caller's: fakes, in tests.
    pub fn with_where_to_send(
        settings_file: PathBuf,
        where_to_send: WhereToSend,
        deliver: Deliver,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oli-core")
            .enable_all()
            .build()
            .expect("starting the app core's runtime");
        Core {
            settings_file,
            live: false,
            where_to_send: Arc::new(where_to_send),
            deliver,
            runtime,
        }
    }

    fn view_model(&self) -> ViewModel {
        view_model(&self.settings_file, self.live, self.where_to_send.view())
    }

    /// Handle one message from the page. `None` when the reply comes later through
    /// [`Deliver`], or when there is nothing to reply to (no readable `id`), which is
    /// logged and otherwise ignored.
    pub fn dispatch(&mut self, raw: &str) -> Option<(Reply, Effect)> {
        let (id, command) = match parse(raw) {
            Ok(parsed) => parsed,
            Err(Malformed::NoId) => {
                tracing::warn!("ui message without an id, ignored");
                return None;
            }
            Err(Malformed::BadCommand { id }) => {
                tracing::warn!(id, "ui message with an unknown command or bad arguments");
                return Some((Reply::error(id, NOT_UNDERSTOOD), Effect::None));
            }
        };
        tracing::info!(id, cmd = command.name(), "ui command");
        self.handle(id, command)
    }

    fn handle(&mut self, id: u32, command: Command) -> Option<(Reply, Effect)> {
        let name = command.name();
        let model = self.where_to_send.clone();
        match command {
            Command::State => {
                // What is saved, at once; the workspace and instance names follow as
                // a `where` event once Open Source Cloud has been asked.
                if model.needs_lookup() {
                    let deliver = self.deliver.clone();
                    let task = async move {
                        let view = model.refresh().await;
                        deliver(Event::new("where", to_json(&view)).script());
                    };
                    self.runtime.spawn(task.with_current_subscriber());
                }
                Some((Reply::ok(id, to_json(&self.view_model())), Effect::None))
            }
            Command::OscSignIn | Command::OscWorkspaces => {
                self.later(id, name, Vec::new(), async move {
                    model.sign_in().await.map(|view| to_json(&view))
                });
                None
            }
            Command::OscPasteToken { token } => {
                let secret = token.expose().to_string();
                self.later(id, name, vec![secret], async move {
                    model.paste_token(token.expose()).await.map(|v| to_json(&v))
                });
                None
            }
            Command::ChooseInstance { url } => {
                let reply = match model.choose_instance(&url) {
                    Ok(view) => {
                        let vm = view_model(&self.settings_file, self.live, view);
                        Reply::ok(id, to_json(&vm))
                    }
                    Err(err) => failed(id, name, &err, &[]),
                };
                Some((reply, Effect::None))
            }
            Command::SelfHosted { url, api_key } => {
                let (settings_file, live) = (self.settings_file.clone(), self.live);
                let secret = api_key.expose().to_string();
                self.later(id, name, vec![secret], async move {
                    let view = model.self_hosted(&url, api_key.expose()).await?;
                    Ok(to_json(&view_model(&settings_file, live, view)))
                });
                None
            }
            // Nothing is live yet, so there is nothing to ask about: quit.
            Command::Quit { .. } if !self.live => Some((Reply::ok(id, Value::Null), Effect::Quit)),
            _ => Some((Reply::error(id, NOT_YET), Effect::None)),
        }
    }

    /// Runs `work` on the core's runtime and delivers its reply when it is done. An
    /// error is one line, with every one of `secrets` taken out of it.
    fn later(
        &self,
        id: u32,
        name: &'static str,
        secrets: Vec<String>,
        work: impl std::future::Future<Output = anyhow::Result<Value>> + Send + 'static,
    ) {
        let deliver = self.deliver.clone();
        let task = async move {
            let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
            let reply = match work.await {
                Ok(value) => Reply::ok(id, value),
                Err(err) => failed(id, name, &err, &secrets),
            };
            deliver(reply.script());
        };
        // The work logs to the subscriber of the thread that asked for it.
        self.runtime.spawn(task.with_current_subscriber());
    }
}

fn view_model(settings_file: &std::path::Path, live: bool, where_to_send: WhereView) -> ViewModel {
    ViewModel {
        version: env!("CARGO_PKG_VERSION"),
        settings_file: settings_file.display().to_string(),
        live,
        where_to_send,
    }
}

fn to_json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).expect("plain JSON")
}

/// The error reply for a command that failed, logged by name with the same line.
fn failed(id: u32, name: &'static str, err: &anyhow::Error, secrets: &[&str]) -> Reply {
    let message = one_line(err, secrets);
    tracing::warn!(id, cmd = name, "ui command failed: {message}");
    Reply::error(id, &message)
}

/// An error chain as the one line the page shows. The library builds its errors from
/// status codes and never quotes a credential; this also takes out any of `secrets`
/// the page sent with the command, should one have ended up in it anyway.
fn one_line(err: &anyhow::Error, secrets: &[&str]) -> String {
    let mut line = format!("{err:#}").replace(['\r', '\n'], " ");
    for secret in secrets.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        line = line.replace(secret, "<redacted>");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use open_live_ingest::keychain::MemoryKeychain;
    use std::io::Write;
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Mutex;
    use std::time::Duration;

    /// A core over `settings_file`, the given Open Source Cloud, an in-memory keychain
    /// and a fake browser, with what it delivers later on the receiver.
    fn core_with(settings_file: PathBuf, endpoints: osc::Endpoints) -> (Core, Receiver<String>) {
        core_over(settings_file, endpoints, Arc::new(MemoryKeychain::new()))
    }

    fn core_over(
        settings_file: PathBuf,
        endpoints: osc::Endpoints,
        keychain: Arc<MemoryKeychain>,
    ) -> (Core, Receiver<String>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let deliver: Deliver = Arc::new(move |script| {
            let _ = tx.lock().unwrap().send(script);
        });
        let where_to_send = WhereToSend::new(
            settings_file.clone(),
            keychain,
            endpoints,
            Duration::from_secs(10),
            testing::browser(testing::SIGNED_IN),
        );
        (
            Core::with_where_to_send(settings_file, where_to_send, deliver),
            rx,
        )
    }

    /// Nothing listens on port 9 of the loopback.
    fn nowhere() -> osc::Endpoints {
        osc::Endpoints {
            connect: "http://127.0.0.1:9".into(),
            catalog: "http://127.0.0.1:9/mysubscriptions".into(),
            token_exchange: "http://127.0.0.1:9/servicetoken".into(),
        }
    }

    fn core() -> Core {
        core_with(PathBuf::from("/home/someone/gateway.toml"), nowhere()).0
    }

    /// The next script delivered later, as the JSON handed to `window.oli.receive`.
    fn next_delivered(rx: &Receiver<String>) -> Value {
        let script = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("a reply delivered later");
        let json = script
            .strip_prefix("window.oli.receive(")
            .and_then(|s| s.strip_suffix(')'))
            .expect("a receive script");
        serde_json::from_str(json).unwrap()
    }

    fn reply_json(core: &mut Core, raw: &str) -> Value {
        let (reply, _) = core.dispatch(raw).expect("a reply");
        serde_json::to_value(reply).unwrap()
    }

    #[test]
    fn state_replies_with_the_view_model() {
        let reply = reply_json(&mut core(), r#"{"id": 7, "cmd": "state"}"#);
        assert_eq!(
            reply,
            json!({
                "id": 7,
                "ok": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "settings_file": "/home/someone/gateway.toml",
                    "live": false,
                    "where": {
                        "mode": "none",
                        "ready": false,
                        "url": null,
                        "instance": null,
                        "workspace": null,
                        "instances": [],
                        "signed_in": false,
                        "api_key_set": false,
                        "summary": null,
                        "problem": null,
                    },
                }
            })
        );
    }

    /// Step 1 through the dispatcher: a sign-in replies later with the workspace and
    /// its instances; `choose_instance` replies at once with the view model; a later
    /// `state` shows "Sending to <instance> (<workspace>)".
    #[test]
    fn where_to_send_commands_reply_through_the_dispatcher() {
        let osc = testing::FakeOsc::start(&[
            ("venue", "https://venue.example.osaas.io"),
            ("rehearsal", "https://rehearsal.example.osaas.io"),
        ]);
        let dir = testing::Scratch::new("ipc-where");
        let (mut core, rx) = core_with(dir.settings(), osc.endpoints());

        assert!(core
            .dispatch(r#"{"id": 1, "cmd": "osc_sign_in"}"#)
            .is_none());
        let reply = next_delivered(&rx);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["ok"]["workspace"], testing::WORKSPACE);
        assert_eq!(reply["ok"]["instances"].as_array().unwrap().len(), 2);

        let (reply, _) = core
            .dispatch(r#"{"id": 2, "cmd": "choose_instance", "args": {"url": "https://venue.example.osaas.io"}}"#)
            .unwrap();
        let reply = serde_json::to_value(reply).unwrap();
        assert_eq!(
            reply["ok"]["where"]["summary"],
            "Sending to venue (venue-ws)"
        );

        let (reply, _) = core
            .dispatch(
                r#"{"id": 3, "cmd": "choose_instance", "args": {"url": "https://example.com"}}"#,
            )
            .unwrap();
        let reply = serde_json::to_value(reply).unwrap();
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not in workspace venue-ws"),
            "{reply}"
        );

        // A new launch over the same file and keychain is not possible with a fresh
        // in-memory keychain, so the lookup is checked on the model; here `state`
        // shows what is saved.
        let state = reply_json(&mut core, r#"{"id": 4, "cmd": "state"}"#);
        assert_eq!(
            state["ok"]["where"]["summary"],
            "Sending to venue (venue-ws)"
        );
        assert!(rx.try_recv().is_err(), "nothing to look up: already listed");
    }

    /// A later launch: `state` answers at once with what is saved, and a `where` event
    /// follows with the names Open Source Cloud gives.
    #[test]
    fn state_after_a_launch_is_followed_by_the_looked_up_names() {
        let osc = testing::FakeOsc::start(&[("venue", "https://venue.example.osaas.io")]);
        let dir = testing::Scratch::new("ipc-later");
        let keychain = Arc::new(MemoryKeychain::new());
        let (mut first, rx) = core_over(dir.settings(), osc.endpoints(), keychain.clone());
        assert!(first
            .dispatch(r#"{"id": 1, "cmd": "osc_sign_in"}"#)
            .is_none());
        assert_eq!(next_delivered(&rx)["ok"]["ready"], true);
        drop(first);

        let (mut later, rx) = core_over(dir.settings(), osc.endpoints(), keychain);
        let state = reply_json(&mut later, r#"{"id": 2, "cmd": "state"}"#);
        assert_eq!(
            state["ok"]["where"]["summary"],
            "Sending to venue.example.osaas.io"
        );
        let event = next_delivered(&rx);
        assert_eq!(event["event"], "where");
        assert_eq!(event["data"]["summary"], "Sending to venue (venue-ws)");
    }

    #[test]
    fn a_self_hosted_check_that_fails_replies_with_one_line() {
        let open_live = testing::fake_open_live("the-key");
        let dir = testing::Scratch::new("ipc-self-hosted");
        let (mut core, rx) = core_with(dir.settings(), nowhere());
        let raw = json!({"id": 8, "cmd": "self_hosted",
                         "args": {"url": open_live, "api_key": "wrong"}})
        .to_string();
        assert!(core.dispatch(&raw).is_none());
        let reply = next_delivered(&rx);
        assert_eq!(reply["id"], 8);
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("could not reach Open Live at"),
            "{message}"
        );
        assert!(!message.contains('\n'));
        assert_eq!(reply["error"]["action"], Value::Null);

        let raw = json!({"id": 9, "cmd": "self_hosted",
                         "args": {"url": open_live, "api_key": "the-key"}})
        .to_string();
        assert!(core.dispatch(&raw).is_none());
        let reply = next_delivered(&rx);
        assert_eq!(reply["ok"]["where"]["mode"], "self_hosted");
        assert_eq!(reply["ok"]["where"]["ready"], true);
        assert_eq!(reply["ok"]["where"]["api_key_set"], true);
    }

    /// A failed command is logged by name with the line the page gets, and neither has
    /// the secret in it.
    #[test]
    fn a_failed_command_is_logged_without_its_secret() {
        let log = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish();
        let err = anyhow::anyhow!("HTTP 401 for OLI-SECRET-KEY");
        let reply = tracing::subscriber::with_default(subscriber, || {
            failed(7, "self_hosted", &err, &["OLI-SECRET-KEY"])
        });
        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("ui command failed: HTTP 401 for <redacted>"),
            "{log}"
        );
        assert!(!log.contains("OLI-SECRET-KEY"), "{log}");
        assert!(!reply.script().contains("OLI-SECRET-KEY"));
    }

    #[test]
    fn a_secret_is_taken_out_of_an_error_line() {
        let err = anyhow::anyhow!("refused key s3cret").context("checking\nOpen Live");
        assert_eq!(
            one_line(&err, &[" s3cret ", ""]),
            "checking Open Live: refused key <redacted>"
        );
    }

    #[test]
    fn a_unit_command_takes_no_args_or_null_args() {
        for raw in [
            r#"{"id": 1, "cmd": "state"}"#,
            r#"{"id": 1, "cmd": "state", "args": null}"#,
        ] {
            assert_eq!(parse(raw), Ok((1, Command::State)), "{raw}");
        }
    }

    #[test]
    fn every_command_of_the_spec_parses() {
        let cases = [
            (json!({"cmd": "state"}), "state"),
            (json!({"cmd": "osc_sign_in"}), "osc_sign_in"),
            (
                json!({"cmd": "osc_paste_token", "args": {"token": "t"}}),
                "osc_paste_token",
            ),
            (json!({"cmd": "osc_workspaces"}), "osc_workspaces"),
            (
                json!({"cmd": "choose_instance", "args": {"url": "https://example.com"}}),
                "choose_instance",
            ),
            (
                json!({"cmd": "self_hosted", "args": {"url": "https://example.com", "api_key": "k"}}),
                "self_hosted",
            ),
            (
                json!({"cmd": "devices", "args": {"show_all": true}}),
                "devices",
            ),
            (
                json!({"cmd": "set_inputs", "args": {"inputs": [
                    {"device": "cam1", "audio": "mic1", "label": "Stage left"},
                    {"device": "test"}
                ]}}),
                "set_inputs",
            ),
            (
                json!({"cmd": "set_advanced", "args": {"latency_ms": 200}}),
                "set_advanced",
            ),
            (json!({"cmd": "go_live"}), "go_live"),
            (json!({"cmd": "stop"}), "stop"),
            (json!({"cmd": "stop_other"}), "stop_other"),
            (json!({"cmd": "quit", "args": {"confirmed": false}}), "quit"),
            (
                json!({"cmd": "show_logs", "args": {"which": "strom"}}),
                "show_logs",
            ),
            (
                json!({"cmd": "open_url", "args": {"which": "studio"}}),
                "open_url",
            ),
        ];
        for (mut message, name) in cases {
            message["id"] = json!(3);
            let (id, command) = parse(&message.to_string()).unwrap_or_else(|e| {
                panic!("{name}: {e:?}");
            });
            assert_eq!(id, 3);
            assert_eq!(command.name(), name);
        }
    }

    #[test]
    fn set_inputs_reads_device_audio_and_label() {
        let raw = r#"{"id": 2, "cmd": "set_inputs", "args": {"inputs": [
            {"device": "cam1", "audio": "mic1", "label": "Stage left"}, {"device": "test"}]}}"#;
        assert_eq!(
            parse(raw),
            Ok((
                2,
                Command::SetInputs {
                    inputs: vec![
                        InputArgs {
                            device: "cam1".into(),
                            audio: Some("mic1".into()),
                            label: Some("Stage left".into()),
                        },
                        InputArgs {
                            device: "test".into(),
                            audio: None,
                            label: None,
                        },
                    ]
                }
            ))
        );
    }

    #[test]
    fn an_unknown_command_or_bad_args_is_an_error_reply() {
        let mut core = core();
        for raw in [
            r#"{"id": 4, "cmd": "rm_rf"}"#,
            r#"{"id": 4}"#,
            r#"{"id": 4, "cmd": 12}"#,
            r#"{"id": 4, "cmd": "devices"}"#,
            r#"{"id": 4, "cmd": "devices", "args": {"show_all": "yes"}}"#,
            r#"{"id": 4, "cmd": "open_url", "args": {"which": "https://example.com"}}"#,
            r#"{"id": 4, "cmd": "show_logs", "args": {"which": "/etc/passwd"}}"#,
            r#"{"id": 4, "cmd": "state", "args": {"x": 1}}"#,
            r#"{"id": 4, "cmd": "state", "extra": 1}"#,
            r#"{"id": 4, "cmd": "set_inputs", "args": {"inputs": [{"device": "a", "x": 1}]}}"#,
            r#"{"id": 4, "cmd": "set_advanced", "args": [1, 2]}"#,
        ] {
            let (reply, effect) = core.dispatch(raw).expect("a reply");
            assert_eq!(effect, Effect::None, "{raw}");
            assert_eq!(
                serde_json::to_value(reply).unwrap(),
                json!({"id": 4, "error": {"message": NOT_UNDERSTOOD, "action": null}}),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_message_without_a_usable_id_gets_no_reply() {
        let mut core = core();
        for raw in [
            "",
            "not json",
            "[]",
            "42",
            r#"{"cmd": "state"}"#,
            r#"{"id": "1", "cmd": "state"}"#,
            r#"{"id": -1, "cmd": "state"}"#,
            r#"{"id": 1.5, "cmd": "state"}"#,
            r#"{"id": 4294967296, "cmd": "state"}"#,
        ] {
            assert!(core.dispatch(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn quit_when_not_live_quits() {
        let (reply, effect) = core()
            .dispatch(r#"{"id": 9, "cmd": "quit", "args": {"confirmed": false}}"#)
            .unwrap();
        assert_eq!(effect, Effect::Quit);
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            json!({"id": 9, "ok": null})
        );
    }

    #[test]
    fn commands_not_wired_up_yet_say_so() {
        let (reply, effect) = core().dispatch(r#"{"id": 5, "cmd": "go_live"}"#).unwrap();
        assert_eq!(effect, Effect::None);
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            json!({"id": 5, "error": {"message": NOT_YET, "action": null}})
        );
    }

    #[test]
    fn replies_and_events_are_delivered_through_window_oli_receive() {
        assert_eq!(
            Reply::ok(1, json!({"a": 1})).script(),
            r#"window.oli.receive({"id":1,"ok":{"a":1}})"#
        );
        assert_eq!(
            Event::new("stopping", json!({"done": 1, "total": 2})).script(),
            r#"window.oli.receive({"event":"stopping","data":{"done":1,"total":2}})"#
        );
        let script = Reply::ok(1, json!("a\u{2028}b\u{2029}c</script>")).script();
        assert!(!script.contains('\u{2028}') && !script.contains('\u{2029}'));
        assert!(script.contains("a\\u2028b\\u2029c"));
    }

    /// A `MakeWriter` over a shared buffer, to read what the dispatcher logs.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Credentials typed into the window cross into the core and never come back: not in
    /// a reply, not in the log, not in a `Debug` of the command, and not in the error
    /// reply when the arguments are wrong (serde's own error text would quote them).
    #[test]
    fn no_secret_reaches_a_reply_the_log_or_debug_output() {
        const SECRETS: [&str; 5] = [
            "OLI-SECRET-API-KEY",
            "OLI-SECRET-TOKEN",
            "OLI-SECRET-PASSPHRASE",
            "OLI-SECRET-WRONG-TYPE",
            "OLI-SECRET-UNKNOWN-FIELD",
        ];
        // Both are fakes that refuse: the key and the token cross into the core, are
        // sent to a server, and come back as error replies delivered later.
        let open_live = testing::fake_open_live("the-real-key");
        let osc = testing::FakeOsc::start(&[]);
        let dir = testing::Scratch::new("ipc-secrets");
        let messages = [
            json!({"id": 1, "cmd": "self_hosted",
                   "args": {"url": open_live, "api_key": SECRETS[0]}}),
            json!({"id": 2, "cmd": "osc_paste_token", "args": {"token": SECRETS[1]}}),
            json!({"id": 3, "cmd": "set_advanced",
                   "args": {"uplink": {"passphrase": SECRETS[2]}}}),
            json!({"id": 4, "cmd": "devices", "args": {"show_all": SECRETS[3]}}),
            json!({"id": 5, "cmd": "self_hosted",
                   "args": {"url": "https://example.com", "api_key": "k", SECRETS[4]: 1}}),
            json!({"id": 6, "cmd": SECRETS[0]}),
        ];

        let log = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let mut replies = Vec::new();
        let mut debug = Vec::new();
        let (mut core, rx) = core_with(dir.settings(), osc.endpoints());
        tracing::subscriber::with_default(subscriber, || {
            for message in &messages {
                let raw = message.to_string();
                if let Ok((_, command)) = parse(&raw) {
                    debug.push(format!("{command:?}"));
                }
                if let Some((reply, _)) = core.dispatch(&raw) {
                    replies.push(reply.script());
                }
            }
        });
        let mut later = Vec::new();
        for _ in 0..2 {
            let reply = next_delivered(&rx);
            assert!(reply.get("error").is_some(), "{reply}");
            later.push(reply.to_string());
        }
        drop(core);
        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();

        assert_eq!(replies.len() + later.len(), messages.len());
        assert_eq!(debug.len(), 3, "the first three parse");
        assert!(log.contains("cmd=\"self_hosted\"") || log.contains("cmd=self_hosted"));
        for secret in SECRETS {
            for reply in replies.iter().chain(&later) {
                assert!(!reply.contains(secret), "{secret} in reply {reply}");
            }
            for line in &debug {
                assert!(!line.contains(secret), "{secret} in {line}");
            }
            assert!(!log.contains(secret), "{secret} in the log:\n{log}");
        }
    }

    #[test]
    fn a_secret_is_readable_in_the_core_only() {
        let Ok((_, Command::SelfHosted { api_key, .. })) =
            parse(r#"{"id": 1, "cmd": "self_hosted", "args": {"url": "u", "api_key": "abc"}}"#)
        else {
            panic!("self_hosted parses");
        };
        assert_eq!(api_key.expose(), "abc");
        assert_eq!(format!("{api_key:?}"), "Secret(<redacted>)");
    }
}
