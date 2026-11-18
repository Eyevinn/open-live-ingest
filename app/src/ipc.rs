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

use crate::what_to_send::{self, DeviceSource};
use open_live_ingest::api::{self, Devices, SaveError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
const NO_DEVICE_LIST: &str =
    "Could not get the list of cameras from the video engine. Show logs for details.";
const NOT_SAVED: &str = "Could not save the settings. Show logs for details.";
const READ_ONLY_WHILE_LIVE: &str = "Stop first: what is sent cannot change while live.";

/// The view model `state` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewModel {
    pub version: &'static str,
    pub settings_file: String,
    pub live: bool,
}

/// Delivers a reply that was not ready when its command arrived. Called from another
/// thread; the window hands the reply to its event loop.
pub type LateReply = Arc<dyn Fn(Reply) + Send + Sync>;

/// The app core the dispatcher drives. The window owns one and hands it every message.
pub struct Core {
    settings_file: PathBuf,
    live: bool,
    device_source: Arc<dyn DeviceSource>,
    /// Where a reply goes that is computed off the window's thread. Without one, such
    /// commands run on the caller's thread and reply at once (tests).
    late: Option<LateReply>,
    /// The last device list Strom gave, for the rows and the names in messages.
    listed: Arc<Mutex<Option<Devices>>>,
    /// Whether the What to send step lists virtual devices.
    show_all: bool,
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("settings_file", &self.settings_file)
            .field("live", &self.live)
            .field("show_all", &self.show_all)
            .finish_non_exhaustive()
    }
}

impl Core {
    pub fn new(settings_file: PathBuf) -> Self {
        Core {
            settings_file,
            live: false,
            device_source: Arc::new(what_to_send::Strom),
            late: None,
            listed: Arc::default(),
            show_all: false,
        }
    }

    /// Lists devices from `source` instead of the local Strom.
    pub fn with_device_source(mut self, source: Arc<dyn DeviceSource>) -> Self {
        self.device_source = source;
        self
    }

    /// Runs slow commands (listing devices, which may start a Strom) on a thread of
    /// their own and hands their replies to `late`, so the window stays responsive.
    pub fn with_late_replies(mut self, late: LateReply) -> Self {
        self.late = Some(late);
        self
    }

    fn view_model(&self) -> ViewModel {
        ViewModel {
            version: env!("CARGO_PKG_VERSION"),
            settings_file: self.settings_file.display().to_string(),
            live: self.live,
        }
    }

    /// Handle one message from the page. `None` when there is nothing to reply to now:
    /// no readable `id` (logged and otherwise ignored), or a reply that arrives later
    /// through [`with_late_replies`](Self::with_late_replies).
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
        let reply = match command {
            Command::State => Reply::ok(
                id,
                serde_json::to_value(self.view_model()).expect("plain JSON"),
            ),
            // Nothing is live yet, so there is nothing to ask about: quit.
            Command::Quit { .. } if !self.live => {
                return Some((Reply::ok(id, Value::Null), Effect::Quit))
            }
            Command::Devices { show_all } => return self.devices(id, show_all),
            Command::SetInputs { inputs } => self.set_inputs(id, &inputs),
            _ => Reply::error(id, NOT_YET),
        };
        Some((reply, Effect::None))
    }

    /// The settings as the CLI reads them, or the reply saying why they cannot be read.
    fn settings(&self, id: u32) -> Result<open_live_ingest::config::Config, Reply> {
        api::load_from(&self.settings_file).map_err(|err| {
            tracing::warn!("reading the settings: {err:#}");
            Reply::error(
                id,
                &format!(
                    "The settings file has a problem: {}.",
                    err.root_cause()
                        .to_string()
                        .lines()
                        .next()
                        .unwrap_or_default()
                ),
            )
        })
    }

    /// `devices`: the What to send rows for what Strom reports now.
    fn devices(&mut self, id: u32, show_all: bool) -> Option<(Reply, Effect)> {
        self.show_all = show_all;
        let settings = match self.settings(id) {
            Ok(settings) => settings,
            Err(reply) => return Some((reply, Effect::None)),
        };
        let source = self.device_source.clone();
        let listed = self.listed.clone();
        let work = move || match source.devices(&settings) {
            Ok(devices) => {
                let model = what_to_send::model(Some(&devices), &settings, show_all);
                *listed.lock().unwrap_or_else(|e| e.into_inner()) = Some(devices);
                Reply::ok(id, serde_json::to_value(model).expect("plain JSON"))
            }
            Err(err) => {
                tracing::warn!("listing devices: {err:#}");
                Reply::error(id, NO_DEVICE_LIST)
            }
        };
        match &self.late {
            None => Some((work(), Effect::None)),
            Some(late) => {
                let late = late.clone();
                std::thread::spawn(move || late(work()));
                None
            }
        }
    }

    /// `set_inputs`: the ticked rows, checked and saved as `[[input]]`.
    fn set_inputs(&mut self, id: u32, inputs: &[InputArgs]) -> Reply {
        if self.live {
            return Reply::error(id, READ_ONLY_WHILE_LIVE);
        }
        let mut settings = match self.settings(id) {
            Ok(settings) => settings,
            Err(reply) => return reply,
        };
        let listed = self
            .listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let entries = what_to_send::entries(inputs, listed.as_ref());
        if let Err(message) = what_to_send::check(&entries, listed.as_ref()) {
            return Reply::error(id, &message);
        }
        settings.inputs = entries;
        match api::save_to(&self.settings_file, &settings) {
            Ok(()) => {
                let model = what_to_send::model(listed.as_ref(), &settings, self.show_all);
                Reply::ok(id, serde_json::to_value(model).expect("plain JSON"))
            }
            Err(SaveError::Invalid(problems)) => Reply::error(
                id,
                &format!(
                    "The settings file has a problem: {}.",
                    problems
                        .first()
                        .map_or("", |p| p.message.as_str())
                        .trim_end_matches('.')
                ),
            ),
            Err(SaveError::Write(err)) => {
                tracing::warn!("saving the settings: {err:#}");
                Reply::error(id, NOT_SAVED)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_live_ingest::config::InputSettings;
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    fn core() -> Core {
        Core::new(PathBuf::from("/home/someone/gateway.toml"))
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
                }
            })
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

    struct FakeStrom(Option<Devices>);

    impl DeviceSource for FakeStrom {
        fn devices(&self, _settings: &open_live_ingest::config::Config) -> anyhow::Result<Devices> {
            self.0
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Strom at http://127.0.0.1:8080 is not answering"))
        }
    }

    /// A settings file in a directory of its own, as the Where to send step leaves it.
    struct SettingsFile(PathBuf);

    impl SettingsFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("oli-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("gateway.toml");
            std::fs::write(
                &path,
                "[gateway]\nname = \"Venue\"\n\n[open_live]\nurl = \"https://open-live.example.com\"\nauth_mode = \"direct\"\n",
            )
            .unwrap();
            SettingsFile(path)
        }

        fn core(&self, devices: Option<Devices>) -> Core {
            Core::new(self.0.clone()).with_device_source(Arc::new(FakeStrom(devices)))
        }

        fn read(&self) -> open_live_ingest::config::Config {
            toml::from_str(&std::fs::read_to_string(&self.0).unwrap()).unwrap()
        }
    }

    impl Drop for SettingsFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn devices() -> Devices {
        crate::what_to_send::tests::devices()
    }

    #[test]
    fn devices_replies_with_the_rows() {
        let file = SettingsFile::new("devices");
        let reply = reply_json(
            &mut file.core(Some(devices())),
            r#"{"id": 1, "cmd": "devices", "args": {"show_all": false}}"#,
        );
        let ok = &reply["ok"];
        assert_eq!(ok["name_prefix"], "Venue — ");
        let rows: Vec<&str> = ok["video"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["device"].as_str().unwrap())
            .collect();
        assert_eq!(rows, ["test", "dev-cam"]);
        assert_eq!(ok["video"][1]["label"], "FaceTime HD Camera");
        assert_eq!(ok["audio"][0]["device"], "dev-mic");
    }

    /// A Strom that cannot be asked is one line for the person; the details go to the
    /// log only.
    #[test]
    fn devices_that_cannot_be_listed_say_so_in_one_line() {
        let file = SettingsFile::new("no-strom");
        let reply = reply_json(
            &mut file.core(None),
            r#"{"id": 2, "cmd": "devices", "args": {"show_all": true}}"#,
        );
        assert_eq!(
            reply,
            json!({"id": 2, "error": {"message": NO_DEVICE_LIST, "action": null}})
        );
    }

    /// With late replies, listing runs off the caller's thread and the reply arrives
    /// through the hook, not from `dispatch`.
    #[test]
    fn devices_reply_late_when_the_window_asks_for_it() {
        let file = SettingsFile::new("late");
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let mut core = file
            .core(Some(devices()))
            .with_late_replies(Arc::new(move |reply| {
                let _ = tx.lock().unwrap().send(reply);
            }));
        assert!(core
            .dispatch(r#"{"id": 3, "cmd": "devices", "args": {"show_all": false}}"#)
            .is_none());
        let reply = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a late reply");
        let reply = serde_json::to_value(reply).unwrap();
        assert_eq!(reply["id"], 3);
        assert_eq!(reply["ok"]["video"][0]["device"], "test");
    }

    /// The selection is saved as `[[input]]` on every change, in the CLI's format, and
    /// the reply shows it ticked.
    #[test]
    fn set_inputs_saves_the_selection_as_input_entries() {
        let file = SettingsFile::new("save");
        let mut core = file.core(Some(devices()));
        reply_json(
            &mut core,
            r#"{"id": 1, "cmd": "devices", "args": {"show_all": true}}"#,
        );
        let reply = reply_json(
            &mut core,
            r#"{"id": 2, "cmd": "set_inputs", "args": {"inputs": [
                {"device": "test"},
                {"device": "dev-obs", "audio": "dev-mic", "label": "Stage left"},
                {"device": "dev-cam", "label": "FaceTime HD Camera"}]}}"#,
        );
        let rows = reply["ok"]["video"].as_array().expect("the rows");
        assert!(rows.iter().all(|r| r["ticked"] == true), "{reply}");
        assert_eq!(reply["ok"]["audio"][0]["paired_with"], "dev-obs");

        let saved = file.read();
        assert_eq!(
            saved.inputs,
            [
                InputSettings {
                    device: "test".into(),
                    audio: None,
                    label: None,
                },
                InputSettings {
                    device: "dev-obs".into(),
                    audio: Some("dev-mic".into()),
                    label: Some("Stage left".into()),
                },
                InputSettings {
                    device: "dev-cam".into(),
                    audio: None,
                    label: None,
                },
            ]
        );
        open_live_ingest::config::validate(&saved).expect("the CLI accepts it");

        // Unticking everything removes the entries: `up` is back to every device.
        reply_json(
            &mut core,
            r#"{"id": 3, "cmd": "set_inputs", "args": {"inputs": []}}"#,
        );
        let text = std::fs::read_to_string(&file.0).unwrap();
        assert!(!text.contains("[[input]]"), "{text}");
    }

    /// #64 AC5: the second camera cannot take an audio source the first one has, and
    /// the file is left as it was.
    #[test]
    fn set_inputs_refuses_an_audio_source_paired_twice() {
        let file = SettingsFile::new("pair-twice");
        let mut core = file.core(Some(devices()));
        reply_json(
            &mut core,
            r#"{"id": 1, "cmd": "devices", "args": {"show_all": true}}"#,
        );
        let before = std::fs::read_to_string(&file.0).unwrap();
        let reply = reply_json(
            &mut core,
            r#"{"id": 2, "cmd": "set_inputs", "args": {"inputs": [
                {"device": "dev-cam", "audio": "dev-mic"},
                {"device": "dev-obs", "audio": "dev-mic"}]}}"#,
        );
        assert_eq!(
            reply["error"]["message"],
            "USB Microphone is already used with FaceTime HD Camera. Pick another audio input."
        );
        assert_eq!(std::fs::read_to_string(&file.0).unwrap(), before);
    }

    /// Until Where to send is done the file cannot be saved, and the reply says why.
    #[test]
    fn set_inputs_on_unfinished_settings_names_the_problem() {
        let file = SettingsFile::new("unfinished");
        std::fs::write(&file.0, "[gateway]\nname = \"Venue\"\n").unwrap();
        let reply = reply_json(
            &mut file.core(Some(devices())),
            r#"{"id": 2, "cmd": "set_inputs", "args": {"inputs": [{"device": "test"}]}}"#,
        );
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("The settings file has a problem: "),
            "{message}"
        );
        assert!(message.contains("open_live.url"), "{message}");
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
        let messages = [
            json!({"id": 1, "cmd": "self_hosted",
                   "args": {"url": "https://example.com", "api_key": SECRETS[0]}}),
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
        // The global subscriber, not a scoped one: with tests dispatching on other
        // threads, tracing's level and interest caches can leave a scoped subscriber
        // hearing nothing. Other tests' lines land in this log too; none carries these
        // secrets, and none may.
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other test sets a global subscriber");
        tracing::callsite::rebuild_interest_cache();
        let mut replies = Vec::new();
        let mut debug = Vec::new();
        let mut core = core();
        for message in &messages {
            let raw = message.to_string();
            if let Ok((_, command)) = parse(&raw) {
                debug.push(format!("{command:?}"));
            }
            let (reply, _) = core.dispatch(&raw).expect("a reply");
            replies.push(reply.script());
        }
        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();

        assert_eq!(replies.len(), messages.len());
        assert_eq!(debug.len(), 3, "the first three parse");
        assert!(log.contains("cmd=\"self_hosted\"") || log.contains("cmd=self_hosted"));
        for secret in SECRETS {
            for reply in &replies {
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
