//! Failures in one line and what to do about them (docs/specs/desktop-app.md, section
//! 10; #62 AC3).
//!
//! The library reports a failure as an error chain or an event, in words written for
//! the log and the terminal. The window never shows those: each failure the spec lists
//! is a [`Failure`] here, with the one line the person reads and the [`Action`] that
//! goes with it. [`for_error`], [`for_input_failure`] and [`for_warning`] recognise
//! the library's errors and events; anything they do not recognise is
//! [`Failure::Unmatched`], "Something went wrong. Show logs for details.", logged at
//! `warn` so that a row can be added for it.

use open_live_ingest::config::Problem;
use serde::{Serialize, Serializer};

/// What the person can do about a failure: a button next to the message. The page
/// knows what each one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Press Go live again.
    Retry,
    /// Sign in with Open Source Cloud again.
    SignIn,
    /// Change where to send.
    Change,
    /// Open Studio in the browser.
    OpenStudio,
    /// Open the privacy settings for the camera (macOS).
    OpenSettings,
    /// Open the privacy settings for the microphone (macOS).
    OpenMicrophoneSettings,
    /// Reveal Strom's log.
    ShowStromLog,
    /// Reveal the app's log.
    ShowLogs,
    /// Open the Advanced section.
    Advanced,
    /// Open the help page.
    Help,
    /// Stop the other live gateway.
    StopIt,
    /// Reveal the settings file.
    ShowFile,
}

impl Action {
    /// The id the page knows the action by.
    pub fn id(self) -> &'static str {
        match self {
            Action::Retry => "retry",
            Action::SignIn => "sign_in",
            Action::Change => "change",
            Action::OpenStudio => "open_studio",
            Action::OpenSettings => "open_settings",
            Action::OpenMicrophoneSettings => "open_microphone_settings",
            Action::ShowStromLog => "show_strom_log",
            Action::ShowLogs => "show_logs",
            Action::Advanced => "advanced",
            Action::Help => "help",
            Action::StopIt => "stop_it",
            Action::ShowFile => "show_file",
        }
    }
}

impl Serialize for Action {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.id())
    }
}

/// One line for the person, and what they can do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    pub message: String,
    pub action: Option<Action>,
}

impl Message {
    pub(crate) fn new(message: impl Into<String>, action: Option<Action>) -> Self {
        Message {
            message: message.into(),
            action,
        }
    }
}

/// Camera or microphone, for the macOS access check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Camera,
    Microphone,
}

impl Media {
    fn noun(self) -> &'static str {
        match self {
            Media::Camera => "camera",
            Media::Microphone => "microphone",
        }
    }
}

/// Every failure of section 10, one variant per row, in the spec's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Open Live cannot be reached at Go live.
    OpenLiveUnreachable { host: String },
    /// Open Live cannot be reached while live; the feeds are unaffected. A banner.
    OpenLiveUnreachableWhileLive,
    /// The Open Source Cloud sign-in has expired or was refused.
    SignInExpired,
    /// The Open Source Cloud sign-in cannot be renewed and ends at `at` (Unix
    /// seconds), during the session. A banner.
    SignInEnds { at: u64 },
    /// A self-hosted Open Live refused the API key.
    ApiKeyRefused,
    /// Open Live has no free SRT port for `input`.
    NoFreePort { input: String },
    /// Port `port` is held by `source`, a source that is not this gateway's.
    PortHeldByOther { port: u16, source: String },
    /// `source`, this gateway's own from an earlier session, is held by an active
    /// production.
    PortHeldByLeftover { source: String },
    /// macOS refuses the app access to the camera or the microphone.
    AccessDenied { media: Media },
    /// `camera` could not be opened, or its flow did not come up.
    CameraBusy { camera: String },
    /// `device`, saved in the selection, is not connected now.
    NotConnected { device: String },
    /// Nothing is ticked.
    NoInputsTicked,
    /// Strom sees no camera at all.
    NoVideoDevices,
    /// `mic` is paired with two cameras.
    AudioPairedTwice { mic: String, camera: String },
    /// Strom exited while starting.
    StromExited,
    /// Strom did not answer within its start-up time.
    StromTimeout,
    /// The Strom found running wants a key.
    StromNeedsKey,
    /// No Strom to start.
    StromMissing,
    /// Another program holds the local Strom's port.
    StromPortInUse { port: u16 },
    /// Another gateway is live on this computer.
    AlreadyLive,
    /// An input runs but nothing has reached the cloud for 30 seconds.
    NothingDelivered,
    /// Open Live refused the gateway token on the heartbeat. A banner.
    GatewayTokenRefused,
    /// The settings file has a problem.
    SettingsInvalid { problem: String },
    /// The Windows installer finds no WebView2. Shown by the installer (OL-59h), kept
    /// here so the wording lives with the rest.
    WebView2Missing,
    /// Anything not recognised.
    Unmatched,
}

/// The installer's line when WebView2 is missing (section 10's last row).
pub const WEBVIEW2_MISSING: &str =
    "Microsoft Edge WebView2 is required. Install it from Microsoft, then run this installer again.";

impl Failure {
    /// The line and the action the window shows.
    pub fn message(&self) -> Message {
        use Action::*;
        match self {
            Failure::OpenLiveUnreachable { host } => Message::new(
                format!("Can't reach Open Live at {host}. Check the internet connection, then press Go live again."),
                Some(Retry),
            ),
            Failure::OpenLiveUnreachableWhileLive => Message::new(
                "Open Live can't be reached. Your cameras are still sending; Studio may show this venue as offline.",
                None,
            ),
            Failure::SignInExpired => Message::new(
                "Your Open Source Cloud sign-in has expired. Sign in again to go live.",
                Some(SignIn),
            ),
            Failure::SignInEnds { at } => Message::new(
                format!(
                    "Your sign-in ends at {}, during this session. Sign in again, or paste a personal access token under Advanced.",
                    clock_utc(*at)
                ),
                Some(SignIn),
            ),
            Failure::ApiKeyRefused => Message::new(
                "Open Live refused the API key. Check it under Where to send.",
                Some(Change),
            ),
            Failure::NoFreePort { input } => Message::new(
                format!("Open Live has no free port for {input}. Remove an unused source in Studio, or ask the Open Live admin to add ports."),
                Some(OpenStudio),
            ),
            Failure::PortHeldByOther { port, source } => Message::new(
                format!("Port {port} is used by the source \"{source}\" in Studio. Remove that source in Studio, then press Go live."),
                Some(OpenStudio),
            ),
            Failure::PortHeldByLeftover { source } => Message::new(
                format!("\"{source}\" from an earlier session is still in an active production. Deactivate that production in Studio, then press Go live."),
                Some(OpenStudio),
            ),
            Failure::AccessDenied { media } => Message::new(
                format!(
                    "Open Live Ingest is not allowed to use the {}. Allow it in System Settings → Privacy & Security, then press Go live.",
                    media.noun()
                ),
                Some(match media {
                    Media::Camera => OpenSettings,
                    Media::Microphone => OpenMicrophoneSettings,
                }),
            ),
            Failure::CameraBusy { camera } => Message::new(
                format!("{camera} could not be opened. Close other apps that use it, then press Go live."),
                Some(Retry),
            ),
            Failure::NotConnected { device } => Message::new(
                format!("{device} is not connected. It is skipped; the others go live."),
                None,
            ),
            Failure::NoInputsTicked => {
                Message::new("Tick at least one camera or the test pattern.", None)
            }
            Failure::NoVideoDevices => {
                Message::new("No cameras found. Connect one, or tick Test pattern.", None)
            }
            Failure::AudioPairedTwice { mic, camera } => Message::new(
                format!("{mic} is already used with {camera}. Pick another audio input."),
                None,
            ),
            Failure::StromExited => Message::new(
                "The video engine stopped while starting. Show logs, and send strom.log to support.",
                Some(ShowStromLog),
            ),
            Failure::StromTimeout => Message::new(
                "The video engine did not start within 45 seconds. Show logs for details.",
                Some(ShowStromLog),
            ),
            Failure::StromNeedsKey => Message::new(
                "Another Strom on this computer needs a key. Enter it under Advanced → Local Strom.",
                Some(Advanced),
            ),
            Failure::StromMissing => Message::new(
                "The video engine is missing. Reinstall Open Live Ingest.",
                Some(Help),
            ),
            Failure::StromPortInUse { port } => Message::new(
                format!("Port {port} is used by another program. Change the local Strom port under Advanced."),
                Some(Advanced),
            ),
            Failure::AlreadyLive => Message::new(
                "Open Live Ingest is already live from another window or a terminal.",
                Some(StopIt),
            ),
            Failure::NothingDelivered => Message::new(
                "Nothing has reached the cloud yet. Check that this network allows outgoing UDP.",
                None,
            ),
            Failure::GatewayTokenRefused => Message::new(
                "Studio can't see this venue's status (gateway token refused). Your cameras are still sending.",
                Some(Advanced),
            ),
            Failure::SettingsInvalid { problem } => Message::new(
                format!("The settings file has a problem: {problem}."),
                Some(ShowFile),
            ),
            Failure::WebView2Missing => Message::new(WEBVIEW2_MISSING, None),
            Failure::Unmatched => Message::new(
                "Something went wrong. Show logs for details.",
                Some(ShowLogs),
            ),
        }
    }
}

/// What the messages need from the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// The host of `open_live.url`.
    pub open_live_host: String,
    /// The port of `strom.url`.
    pub strom_port: Option<u16>,
}

impl Context {
    pub fn of(settings: &open_live_ingest::config::Config) -> Self {
        let url = settings.open_live.url.as_deref().unwrap_or_default();
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split(['/', '?'])
            .next()
            .unwrap_or_default()
            .to_string();
        Context {
            open_live_host: host,
            strom_port: open_live_ingest::local_strom::port_of(&settings.strom.url),
        }
    }
}

/// The message for an error that ended Go live: a session's [`Failed`] reason (the
/// error chain as `{err:#}` prints it) or a check before it. Logged at `warn` when no
/// row matches.
///
/// [`Failed`]: open_live_ingest::gateway::Event::Failed
pub fn for_error(reason: &str, ctx: &Context) -> Message {
    let failure = classify(reason, ctx);
    if failure == Failure::Unmatched {
        tracing::warn!("no message for this failure, showing the generic one: {reason}");
    }
    failure.message()
}

/// The message for one input that could not be started or failed while live
/// (`InputFailed`, or an `Input` event whose state is `Failed`). Its flow did not
/// reach running, which is the camera being busy or failing to open.
pub fn for_input_failure(input: &str, reason: &str) -> Message {
    tracing::warn!(input, "an input failed: {reason}");
    Failure::CameraBusy {
        camera: input.to_string(),
    }
    .message()
}

/// The settings file's first problem, as the window shows it.
pub fn for_problem(problem: &Problem) -> Message {
    match audio_paired_twice(&problem.message) {
        Some(failure) => failure.message(),
        None => Failure::SettingsInvalid {
            problem: problem.message.trim_end_matches('.').to_string(),
        }
        .message(),
    }
}

/// What a session's `Warning` means for the window, when it is one the window shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningKind {
    /// Open Live could not be reached for this tick's registration: a banner that
    /// clears itself on the next tick that reaches it.
    OpenLiveUnreachable,
    /// The heartbeat's gateway token was refused: a banner.
    GatewayTokenRefused,
    /// A saved device is not connected: its row.
    NotConnected { device: String },
}

/// Recognises the session warnings the window shows; None for the rest, which the
/// log already has.
pub fn for_warning(message: &str) -> Option<(WarningKind, Message)> {
    if message.starts_with("could not update the Open Live sources") {
        return Some((
            WarningKind::OpenLiveUnreachable,
            Failure::OpenLiveUnreachableWhileLive.message(),
        ));
    }
    if message.starts_with("Open Live refused the token for gateway") {
        return Some((
            WarningKind::GatewayTokenRefused,
            Failure::GatewayTokenRefused.message(),
        ));
    }
    // "[[input]] device "X" is not connected; ..." (a saved selection), or
    // "[[input]] pairs audio with "X", which is not among this run's cameras; ...".
    if message.starts_with("[[input]]")
        && (message.contains("is not connected")
            || message.contains("not among this run's cameras"))
    {
        let device = quoted(message, 0).unwrap_or_default();
        return Some((
            WarningKind::NotConnected {
                device: device.clone(),
            },
            Failure::NotConnected { device }.message(),
        ));
    }
    None
}

/// Which row of section 10 an error chain is. Order matters where one chain could
/// match two rows: the more specific row comes first.
pub fn classify(reason: &str, ctx: &Context) -> Failure {
    let has = |needle: &str| reason.contains(needle);
    let refused_status = has("HTTP 401") || has("HTTP 403");

    // `AlreadyRunning`'s words: "already running (pid N); run 'open-live-ingest down' first".
    if has("already running (pid") && has("open-live-ingest down") {
        return Failure::AlreadyLive;
    }
    if let Some(failure) = audio_paired_twice(reason) {
        return failure;
    }
    // Strom.
    if has("Strom exited during startup") {
        let in_use = [
            "address already in use",
            "address in use",
            "os error 48",
            "os error 98",
            "os error 10048",
        ];
        let lower = reason.to_lowercase();
        if let (true, Some(port)) = (in_use.iter().any(|n| lower.contains(n)), ctx.strom_port) {
            return Failure::StromPortInUse { port };
        }
        return Failure::StromExited;
    }
    if has("Strom did not become ready within") {
        return Failure::StromTimeout;
    }
    if has("wants a credential") {
        return Failure::StromNeedsKey;
    }
    if (has("could not find") && has("on PATH")) || has("no Strom executable at") {
        return Failure::StromMissing;
    }
    if has("Strom reports no capture devices") {
        return Failure::NoVideoDevices;
    }
    // The Open Source Cloud sign-in.
    if has("OSC token exchange failed with HTTP 401")
        || has("OSC token exchange failed with HTTP 403")
        || has(open_live_ingest::credential::SIGN_IN_AGAIN)
        || has("no Open Source Cloud credential")
    {
        return Failure::SignInExpired;
    }
    // Ports.
    if has("no SRT port range for the cloud Strom") {
        return Failure::NoFreePort {
            input: "your inputs".to_string(),
        };
    }
    if has("which is not this gateway's") {
        if let (Some(port), Some(source)) = (port_after(reason, "port "), quoted(reason, 1)) {
            return Failure::PortHeldByOther { port, source };
        }
    }
    if has("left over from an earlier run, and Open Live will not remove it") {
        if let Some(source) = quoted(reason, 1) {
            return Failure::PortHeldByLeftover { source };
        }
    }
    if has("registering \"") && (has("HTTP 409") || has("HTTP 422")) {
        if let Some(input) = quoted(reason, 0) {
            return Failure::NoFreePort { input };
        }
    }
    // Open Live itself: a refused key, or no answer.
    let talks_to_open_live = has("asking Open Live where its Strom is")
        || has("listing Open Live's sources")
        || has("with Open Live")
        || has("asking Open Live for a new port");
    if talks_to_open_live && refused_status {
        return Failure::ApiKeyRefused;
    }
    if talks_to_open_live {
        return Failure::OpenLiveUnreachable {
            host: ctx.open_live_host.clone(),
        };
    }
    if has("no input could be started") {
        return Failure::CameraBusy {
            camera: "No camera".to_string(),
        };
    }
    Failure::Unmatched
}

/// `audio source "M" is paired with both "A" and "B" in [[input]]; ...`, the
/// settings check's words for one audio source with two cameras.
fn audio_paired_twice(text: &str) -> Option<Failure> {
    if !text.contains("is paired with both") {
        return None;
    }
    Some(Failure::AudioPairedTwice {
        mic: quoted(text, 0)?,
        camera: quoted(text, 1)?,
    })
}

/// The `n`th double-quoted string in `text` (as Rust's `{:?}` writes one), unquoted.
fn quoted(text: &str, n: usize) -> Option<String> {
    let mut rest = text;
    let mut found = Vec::new();
    while found.len() <= n {
        let start = rest.find('"')? + 1;
        let mut out = String::new();
        let mut chars = rest[start..].char_indices();
        let end = loop {
            let (i, c) = chars.next()?;
            match c {
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        out.push(escaped);
                    }
                }
                '"' => break start + i,
                c => out.push(c),
            }
        };
        found.push(out);
        rest = &rest[end + 1..];
    }
    found.pop()
}

/// The number right after the first `prefix` in `text`.
fn port_after(text: &str, prefix: &str) -> Option<u16> {
    let at = text.find(prefix)? + prefix.len();
    let digits: String = text[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `HH:MM UTC` for Unix seconds. UTC, because the app has no time zone database; the
/// message says so.
fn clock_utc(at: u64) -> String {
    let in_day = at % 86_400;
    format!("{:02}:{:02} UTC", in_day / 3600, in_day % 3600 / 60)
}

#[cfg(test)]
mod tests {
    //! One test per row of section 10, each feeding what the library reports (its
    //! error text or event, produced by the library itself where it can be) and
    //! asserting the message and the action.

    use super::*;
    use open_live_ingest::config::{Config, InputSettings};
    use open_live_ingest::gateway::Event;
    use open_live_ingest::run::AlreadyRunning;

    fn ctx() -> Context {
        let mut settings = Config::default();
        settings.open_live.url = Some("https://venue.example.osaas.io/".to_string());
        settings.strom.url = "http://127.0.0.1:8080".to_string();
        Context::of(&settings)
    }

    #[track_caller]
    fn assert_message(got: Message, text: &str, action: Option<Action>) {
        assert_eq!(got.message, text);
        assert_eq!(got.action, action);
        assert!(!got.message.contains('\n'), "one line: {}", got.message);
    }

    #[test]
    fn the_context_reads_the_open_live_host_and_the_strom_port() {
        assert_eq!(
            ctx(),
            Context {
                open_live_host: "venue.example.osaas.io".to_string(),
                strom_port: Some(8080),
            }
        );
    }

    #[test]
    fn open_live_unreachable_at_go_live() {
        for reason in [
            "asking Open Live where its Strom is: GET server-info: error sending request for url (https://venue.example.osaas.io/api/v1/server-info)",
            "listing Open Live's sources to assign ports: GET sources: operation timed out",
        ] {
            assert_message(
                for_error(reason, &ctx()),
                "Can't reach Open Live at venue.example.osaas.io. Check the internet connection, then press Go live again.",
                Some(Action::Retry),
            );
        }
    }

    #[test]
    fn open_live_unreachable_while_live_is_a_banner() {
        let (kind, message) = for_warning(
            "could not update the Open Live sources (GET sources: connection refused); the feeds are unaffected, retrying next tick",
        )
        .unwrap();
        assert_eq!(kind, WarningKind::OpenLiveUnreachable);
        assert_message(
            message,
            "Open Live can't be reached. Your cameras are still sending; Studio may show this venue as offline.",
            None,
        );
    }

    #[test]
    fn osc_sign_in_expired_or_refused() {
        for reason in [
            "asking Open Live where its Strom is: OSC token exchange failed with HTTP 401 Unauthorized; sign in again",
            &format!(
                "asking Open Live where its Strom is: the Open Source Cloud token in the keychain has expired; {}",
                open_live_ingest::credential::SIGN_IN_AGAIN
            ),
        ] {
            assert_message(
                for_error(reason, &ctx()),
                "Your Open Source Cloud sign-in has expired. Sign in again to go live.",
                Some(Action::SignIn),
            );
        }
    }

    #[test]
    fn osc_sign_in_ends_during_the_session() {
        // 2026-11-18 14:05:00 UTC.
        let event = Event::OscSignInEnds { at: 1_795_010_700 };
        let Event::OscSignInEnds { at } = event else {
            unreachable!()
        };
        assert_message(
            Failure::SignInEnds { at }.message(),
            "Your sign-in ends at 14:05 UTC, during this session. Sign in again, or paste a personal access token under Advanced.",
            Some(Action::SignIn),
        );
    }

    #[test]
    fn api_key_refused() {
        assert_message(
            for_error(
                "asking Open Live where its Strom is: GET server-info failed with HTTP 401 Unauthorized",
                &ctx(),
            ),
            "Open Live refused the API key. Check it under Where to send.",
            Some(Action::Change),
        );
    }

    #[test]
    fn no_free_srt_port() {
        assert_message(
            for_error(
                "registering \"Venue — Camera\" with Open Live: POST source failed with HTTP 409 Conflict: no free SRT port",
                &ctx(),
            ),
            "Open Live has no free port for Venue — Camera. Remove an unused source in Studio, or ask the Open Live admin to add ports.",
            Some(Action::OpenStudio),
        );
        assert_message(
            for_error(
                "no SRT port range for the cloud Strom: Open Live has SRT port reservation disabled, or set uplink.port_range in the settings",
                &ctx(),
            ),
            "Open Live has no free port for your inputs. Remove an unused source in Studio, or ask the Open Live admin to add ports.",
            Some(Action::OpenStudio),
        );
    }

    #[test]
    fn port_held_by_another_gateways_source() {
        assert_message(
            for_error(
                "port 47110 for \"Venue — Camera\" is already used by the Open Live source \"Stage — Cam 2\", which is not this gateway's. Remove it in Studio, or set uplink.port_range to ports it does not use.",
                &ctx(),
            ),
            "Port 47110 is used by the source \"Stage — Cam 2\" in Studio. Remove that source in Studio, then press Go live.",
            Some(Action::OpenStudio),
        );
    }

    #[test]
    fn port_held_by_our_leftover_in_an_active_production() {
        assert_message(
            for_error(
                "port 47110 for \"Venue — Camera\" is held by the Open Live source \"Venue — Old\", left over from an earlier run, and Open Live will not remove it: in active production \"Morning Show\". Deactivate that production, then start again.",
                &ctx(),
            ),
            "\"Venue — Old\" from an earlier session is still in an active production. Deactivate that production in Studio, then press Go live.",
            Some(Action::OpenStudio),
        );
    }

    #[test]
    fn camera_or_microphone_access_denied() {
        assert_message(
            Failure::AccessDenied { media: Media::Camera }.message(),
            "Open Live Ingest is not allowed to use the camera. Allow it in System Settings → Privacy & Security, then press Go live.",
            Some(Action::OpenSettings),
        );
        assert_message(
            Failure::AccessDenied { media: Media::Microphone }.message(),
            "Open Live Ingest is not allowed to use the microphone. Allow it in System Settings → Privacy & Security, then press Go live.",
            Some(Action::OpenMicrophoneSettings),
        );
    }

    #[test]
    fn camera_busy_or_failed_to_open() {
        let event = Event::InputFailed {
            input: "Venue — FaceTime HD Camera".to_string(),
            reason: "Strom did not report the flow running after start".to_string(),
        };
        let Event::InputFailed { input, reason } = event else {
            unreachable!()
        };
        assert_message(
            for_input_failure(&input, &reason),
            "Venue — FaceTime HD Camera could not be opened. Close other apps that use it, then press Go live.",
            Some(Action::Retry),
        );
    }

    #[test]
    fn saved_device_not_present() {
        for warning in [
            "[[input]] device \"dev-cam\" is not connected; it is skipped, the others go live",
            "[[input]] pairs audio with \"dev-cam\", which is not among this run's cameras; skipped",
        ] {
            let (kind, message) = for_warning(warning).unwrap();
            assert_eq!(
                kind,
                WarningKind::NotConnected {
                    device: "dev-cam".to_string()
                }
            );
            assert_message(
                message,
                "dev-cam is not connected. It is skipped; the others go live.",
                None,
            );
        }
    }

    #[test]
    fn no_inputs_ticked() {
        assert_message(
            Failure::NoInputsTicked.message(),
            "Tick at least one camera or the test pattern.",
            None,
        );
    }

    #[test]
    fn no_video_devices_found() {
        assert_message(
            for_error(
                "Strom reports no capture devices. Virtual ones are skipped unless --all; --test streams a test pattern instead.",
                &ctx(),
            ),
            "No cameras found. Connect one, or tick Test pattern.",
            None,
        );
    }

    /// The settings check itself finds the pairing; its problem becomes the row.
    #[test]
    fn audio_source_paired_twice() {
        let settings = Config {
            inputs: vec![
                InputSettings {
                    device: "Camera A".to_string(),
                    audio: Some("USB Mic".to_string()),
                    label: None,
                },
                InputSettings {
                    device: "Camera B".to_string(),
                    audio: Some("USB Mic".to_string()),
                    label: None,
                },
            ],
            ..Config::default()
        };
        let problems = open_live_ingest::config::problems(&settings);
        let problem = problems
            .iter()
            .find(|p| p.message.contains("paired with both"))
            .expect("the settings check refuses it");
        assert_message(
            for_problem(problem),
            "USB Mic is already used with Camera A. Pick another audio input.",
            None,
        );
    }

    #[test]
    fn strom_exited_during_start_up() {
        assert_message(
            for_error(
                "Strom exited during startup (exit status: 1).\n\nThe end of /tmp/strom.log:\n    panic: no GPU\n",
                &ctx(),
            ),
            "The video engine stopped while starting. Show logs, and send strom.log to support.",
            Some(Action::ShowStromLog),
        );
    }

    #[test]
    fn strom_did_not_answer_in_time() {
        assert_message(
            for_error(
                "Strom did not become ready within 45s (connection refused).",
                &ctx(),
            ),
            "The video engine did not start within 45 seconds. Show logs for details.",
            Some(Action::ShowStromLog),
        );
    }

    #[test]
    fn an_adopted_strom_wants_a_key() {
        assert_message(
            for_error(
                "Strom at http://127.0.0.1:8080 wants a credential.\n\nIt runs with STROM_API_KEY set. Run `open-live-ingest setup` and enter that key when asked.",
                &ctx(),
            ),
            "Another Strom on this computer needs a key. Enter it under Advanced → Local Strom.",
            Some(Action::Advanced),
        );
    }

    /// The library's own error for a Strom that is nowhere.
    #[test]
    fn strom_not_found() {
        let missing = std::env::temp_dir().join("oli-no-such-dir").join("strom");
        let err = open_live_ingest::local_strom::locate_binary(&missing.display().to_string())
            .unwrap_err();
        assert_message(
            for_error(&format!("{err:#}"), &ctx()),
            "The video engine is missing. Reinstall Open Live Ingest.",
            Some(Action::Help),
        );
        assert_message(
            for_error(
                "could not find \"strom\" on PATH.\n\nInstall Strom, or set [strom] binary to its path",
                &ctx(),
            ),
            "The video engine is missing. Reinstall Open Live Ingest.",
            Some(Action::Help),
        );
    }

    #[test]
    fn stroms_port_in_use_by_another_program() {
        assert_message(
            for_error(
                "Strom exited during startup (exit status: 1).\n\nThe end of strom.log:\n    Error: Address already in use (os error 48)\n",
                &ctx(),
            ),
            "Port 8080 is used by another program. Change the local Strom port under Advanced.",
            Some(Action::Advanced),
        );
    }

    /// The library's own refusal, as `up` and a session give it.
    #[test]
    fn another_gateway_already_live() {
        let reason = format!("{:#}", anyhow::Error::from(AlreadyRunning { pid: 4242 }));
        assert_message(
            for_error(&reason, &ctx()),
            "Open Live Ingest is already live from another window or a terminal.",
            Some(Action::StopIt),
        );
    }

    #[test]
    fn nothing_delivered_for_30_seconds() {
        assert_message(
            Failure::NothingDelivered.message(),
            "Nothing has reached the cloud yet. Check that this network allows outgoing UDP.",
            None,
        );
    }

    #[test]
    fn gateway_token_refused_on_the_heartbeat() {
        let (kind, message) = for_warning(
            "Open Live refused the token for gateway gw-1; check open_live.gateway_id and open_live.gateway_token, or rotate the token",
        )
        .unwrap();
        assert_eq!(kind, WarningKind::GatewayTokenRefused);
        assert_message(
            message,
            "Studio can't see this venue's status (gateway token refused). Your cameras are still sending.",
            Some(Action::Advanced),
        );
    }

    /// The settings check's own first problem.
    #[test]
    fn settings_file_invalid() {
        let mut settings = Config::default();
        settings.uplink.passphrase = Some("short".to_string());
        let problems = open_live_ingest::config::problems(&settings);
        let problem = problems
            .iter()
            .find(|p| p.setting == Some("uplink.passphrase"))
            .expect("a short passphrase is refused");
        let message = for_problem(problem);
        assert!(
            message
                .message
                .starts_with("The settings file has a problem: uplink.passphrase"),
            "{}",
            message.message
        );
        assert!(message.message.ends_with('.'));
        assert!(!message.message.ends_with(".."));
        assert_eq!(message.action, Some(Action::ShowFile));
    }

    #[test]
    fn webview2_missing() {
        assert_message(
            Failure::WebView2Missing.message(),
            "Microsoft Edge WebView2 is required. Install it from Microsoft, then run this installer again.",
            None,
        );
    }

    #[test]
    fn an_unmatched_failure_is_the_generic_line_with_show_logs_and_is_logged() {
        let log = crate::testing::Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish();
        let message = tracing::subscriber::with_default(subscriber, || {
            for_error("something nobody planned for", &ctx())
        });
        assert_message(
            message,
            "Something went wrong. Show logs for details.",
            Some(Action::ShowLogs),
        );
        let log = log.text();
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("something nobody planned for"), "{log}");
    }

    #[test]
    fn actions_serialise_as_their_ids() {
        assert_eq!(
            serde_json::to_value(Failure::AlreadyLive.message()).unwrap(),
            serde_json::json!({
                "message": "Open Live Ingest is already live from another window or a terminal.",
                "action": "stop_it",
            })
        );
    }

    #[test]
    fn quoted_strings_are_read_as_debug_writes_them() {
        assert_eq!(
            quoted(r#"a "b \"c\"" d "e""#, 0).as_deref(),
            Some(r#"b "c""#)
        );
        assert_eq!(quoted(r#"a "b" d "e""#, 1).as_deref(), Some("e"));
        assert_eq!(quoted(r#"a "b" d"#, 1), None);
    }
}
