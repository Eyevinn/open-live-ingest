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

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::path::PathBuf;

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
}

/// The app core the dispatcher drives. The window owns one and hands it every message.
#[derive(Debug)]
pub struct Core {
    settings_file: PathBuf,
    live: bool,
}

impl Core {
    pub fn new(settings_file: PathBuf) -> Self {
        Core {
            settings_file,
            live: false,
        }
    }

    fn view_model(&self) -> ViewModel {
        ViewModel {
            version: env!("CARGO_PKG_VERSION"),
            settings_file: self.settings_file.display().to_string(),
            live: self.live,
        }
    }

    /// Handle one message from the page. `None` when there is nothing to reply to (no
    /// readable `id`), which is logged and otherwise ignored.
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
        Some(self.handle(id, command))
    }

    fn handle(&mut self, id: u32, command: Command) -> (Reply, Effect) {
        match command {
            Command::State => (
                Reply::ok(
                    id,
                    serde_json::to_value(self.view_model()).expect("plain JSON"),
                ),
                Effect::None,
            ),
            // Nothing is live yet, so there is nothing to ask about: quit.
            Command::Quit { .. } if !self.live => (Reply::ok(id, Value::Null), Effect::Quit),
            _ => (Reply::error(id, NOT_YET), Effect::None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut replies = Vec::new();
        let mut debug = Vec::new();
        tracing::subscriber::with_default(subscriber, || {
            let mut core = core();
            for message in &messages {
                let raw = message.to_string();
                if let Ok((_, command)) = parse(&raw) {
                    debug.push(format!("{command:?}"));
                }
                let (reply, _) = core.dispatch(&raw).expect("a reply");
                replies.push(reply.script());
            }
        });
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
