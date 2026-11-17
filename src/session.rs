//! A running gateway as a value: start it, read its events, stop it.
//!
//! [`Session::start`] runs what `up` runs on a task of its own and hands back a
//! [`SessionHandle`] and the receiving end of its [`Event`]s. The handle stops it:
//! [`SessionHandle::stop`] runs the teardown `up` runs on Ctrl-C, reporting each flow
//! and source it removes, and waits for it to finish. The CLI's `up` is one consumer
//! of a session, an app is another.
//!
//! What arrives, in order: Strom starting and ready (when it had to be started), per
//! input `Registering` / `Registered` (when registration is on), the start
//! `Summary`, then every tick an `Input` event for each input whose state changed
//! (starting → waiting for a receiver → on air, or failed with a reason) and one
//! `Stats` event with every input's uplink. Stopping brings `Stopping`, then
//! `FlowRemoved` / `SourceRemoved` / `SourceKept` per input and `StromStopped` for a
//! Strom the session started. The last event is always [`Event::Stopped`] or
//! [`Event::Failed`]; after it the receiver yields `None`. A session that fails to
//! start removes what it had registered before [`Event::Failed`]: the sources it
//! created in Open Live are reported with `SourceRemoved` (or `SourceKept`).
//!
//! # Caveats
//!
//! - **A panic.** Should the session's task panic, it ends without its teardown: the
//!   flows and sources it had started stay until the next `up` or `down`, which find
//!   them by derived id and name prefix. The receiver still ends with
//!   [`Event::Failed`] (reason "the session ended abnormally: …"), and
//!   [`SessionHandle::wait`] returns the same error.
//! - **The runtime shutting down.** The session, and the teardown a dropped handle
//!   starts, run on the Tokio runtime [`Session::start`] was called on. If that
//!   runtime shuts down first (the app returns from `main`, or drops its runtime),
//!   the task is cancelled wherever it is, and flows and sources are left as after a
//!   panic, with no terminal event: the receiver just yields `None`. An app that
//!   wants a clean stop awaits [`SessionHandle::stop`] (or [`SessionHandle::wait`]
//!   after [`SessionHandle::request_stop`]) before its runtime goes away.
//! - **An unbounded channel.** Events are queued for the receiver without limit, so
//!   emitting never blocks the session on a slow consumer. A consumer that stops
//!   reading but keeps the receiver lets the queue grow by roughly one `Stats` event
//!   per tick (every 10 s) for as long as the session runs. Drop the receiver
//!   instead: events for nobody are discarded.

use crate::config::{self, Config};
use crate::credential::OscCredential;
use crate::devices::AudioPairing;
use crate::gateway::{Event, Events};
use crate::run::{self, Stop};
use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Where a session's events arrive. Unbounded, so emitting never blocks the
/// session; a session emits a handful of events per input per tick. Keep reading it,
/// or drop it: see the module documentation's caveats.
pub type Receiver = mpsc::UnboundedReceiver<Event>;

/// Which inputs a session streams: the choices `up` takes as flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    /// Include virtual devices, which produce nothing on their own.
    pub include_virtual: bool,
    /// Only these devices, by id or name fragment, comma separated.
    pub selection: Option<String>,
    /// Audio sources sent with cameras. Empty takes the settings' `[[input]]`
    /// pairings.
    pub audio: Vec<AudioPairing>,
    /// Stream a test pattern and tone instead of any device. Cannot be combined
    /// with `audio`.
    pub test_pattern: bool,
}

impl Inputs {
    /// The test pattern and tone, and nothing else.
    pub fn test_pattern() -> Self {
        Inputs {
            test_pattern: true,
            ..Inputs::default()
        }
    }
}

/// Starts sessions. See the module documentation.
#[derive(Debug)]
pub struct Session {
    _private: (),
}

impl Session {
    /// Starts streaming `inputs` with `settings`, on a task spawned on the current
    /// Tokio runtime (so it must be called from within one).
    ///
    /// The settings are validated first, as the CLI does before `up`. Nothing here
    /// reads stdin, prints, or handles a signal. Startup is not interrupted: a stop
    /// asked for while it runs is acted on as soon as the inputs are up, so whatever
    /// was started is always removed again. Dropping the handle stops the session in
    /// the same way, in the background, as long as the runtime keeps running (see
    /// the module documentation's caveats).
    pub fn start(settings: Config, inputs: Inputs) -> (SessionHandle, Receiver) {
        Self::start_inner(settings, None, inputs)
    }

    /// [`start`](Self::start), registering with `credential`, the Open Source Cloud
    /// credential the app holds, rather than the one the settings point at: a token
    /// in hand, or the sign-in in a keychain, renewed for as long as the session
    /// runs. `open_live.api_key`, `OSC_ACCESS_TOKEN` and `~/.osc/token` are not read.
    /// A self-hosted Open Live keeps using the key in the settings.
    pub fn start_with(
        settings: Config,
        inputs: Inputs,
        credential: OscCredential,
    ) -> (SessionHandle, Receiver) {
        Self::start_inner(settings, Some(credential), inputs)
    }

    fn start_inner(
        settings: Config,
        credential: Option<OscCredential>,
        inputs: Inputs,
    ) -> (SessionHandle, Receiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel::<Stop>();
        let last = tx.clone();
        let session = tokio::spawn(async move {
            let events = Events::new(move |event| {
                // Nobody listening any more is not the session's problem.
                let _ = tx.send(event);
            });
            // A dropped handle is a stop too: the session must not outlive its owner
            // with flows and sources nobody can remove.
            let stop = async move { stop_rx.await.unwrap_or(Stop::Interrupt) };
            let outcome = run(settings, credential, inputs, &events, stop).await;
            events.emit(match &outcome {
                Ok(()) => Event::Stopped,
                Err(err) => Event::Failed {
                    reason: format!("{err:#}"),
                },
            });
            outcome
        });
        // Watches the session so that a panic in it still ends the events with
        // `Failed`: the panicking task has dropped its sender without a last event.
        let task = tokio::spawn(async move {
            match session.await {
                Ok(outcome) => outcome,
                Err(err) => {
                    let err = anyhow!("the session ended abnormally: {err}");
                    let _ = last.send(Event::Failed {
                        reason: format!("{err:#}"),
                    });
                    Err(err)
                }
            }
        });
        (
            SessionHandle {
                stop: Some(stop_tx),
                task,
            },
            rx,
        )
    }
}

async fn run(
    settings: Config,
    credential: Option<OscCredential>,
    inputs: Inputs,
    events: &Events,
    stop: impl std::future::Future<Output = Stop> + Send,
) -> Result<()> {
    match credential {
        // The app's credential stands in for one in the settings or a CLI login.
        Some(_) => config::validate_with(&settings, true)?,
        None => config::validate(&settings)?,
    }
    run::up(settings, credential, inputs, events, stop).await
}

/// A running session. See [`Session::start`].
#[derive(Debug)]
pub struct SessionHandle {
    stop: Option<oneshot::Sender<Stop>>,
    task: JoinHandle<Result<()>>,
}

impl SessionHandle {
    /// Asks the session to stop, for `why`, without waiting for it. Asking again, or
    /// after it has ended, does nothing. Progress arrives as events; the receiver
    /// yields `None` once the teardown is done.
    pub fn request_stop(&mut self, why: Stop) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(why);
        }
    }

    /// Stops the session and removes what it started, as `up` does on Ctrl-C, taking
    /// as long as that takes. Returns how the session ended: an error when it could
    /// not start, as [`Event::Failed`] said.
    pub async fn stop(self) -> Result<()> {
        self.stop_with(Stop::Interrupt).await
    }

    /// [`SessionHandle::stop`], for `why`: [`Stop::Ending`] bounds the teardown to
    /// [`run::ENDING_TEARDOWN`] (`teardown_within`), for when the process is about to
    /// be ended from outside.
    pub async fn stop_with(mut self, why: Stop) -> Result<()> {
        self.request_stop(why);
        self.wait().await
    }

    /// Waits for the session to end, by a stop already asked for or by an error, and
    /// returns how it ended.
    pub async fn wait(self) -> Result<()> {
        match self.task.await {
            Ok(outcome) => outcome,
            // Only when the runtime is shutting down: the watcher itself never panics.
            Err(err) => Err(anyhow!("the session ended abnormally: {err}")),
        }
    }

    /// Whether the session has ended.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings that fail validation end the session at once, with the reason the
    /// CLI would print, as the last event.
    #[tokio::test]
    async fn invalid_settings_end_the_session_with_a_failed_event() {
        let mut settings = Config::default();
        settings.uplink.passphrase = Some("short".to_string());
        let (handle, mut events) = Session::start(settings, Inputs::test_pattern());
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            seen.push(event);
        }
        let err = handle.wait().await.unwrap_err();
        let Some(Event::Failed { reason }) = seen.last() else {
            panic!("no Failed event last: {seen:?}");
        };
        assert_eq!(reason, &format!("{err:#}"));
        assert!(reason.contains("uplink.passphrase"), "{reason}");
    }

    /// With the app's credential, settings for an Open Source Cloud instance need no
    /// token of their own and no keychain marker: the session gets past the
    /// settings to the next step, here a Strom that is not there.
    #[tokio::test]
    async fn the_apps_credential_stands_in_for_a_token_in_the_settings() {
        let mut settings = Config::default();
        settings.open_live.url = Some("https://venue.example.osaas.io".to_string());
        settings.open_live.auth_mode = config::AuthMode::Osc;
        settings.strom.url = "http://127.0.0.1:9".to_string();
        settings.strom.manage = false;
        let (handle, mut events) = Session::start_with(
            settings,
            Inputs::test_pattern(),
            OscCredential::from("app-token"),
        );
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            seen.push(event);
        }
        assert!(handle.wait().await.is_err());
        let Some(Event::Failed { reason }) = seen.last() else {
            panic!("no Failed event last: {seen:?}");
        };
        assert!(!reason.contains("no OSC token"), "{reason}");
        assert!(reason.contains("Strom"), "{reason}");
    }

    /// Test pattern and audio cannot go together; the session says so rather than
    /// starting anything.
    #[tokio::test]
    async fn a_test_pattern_with_audio_fails_with_the_cli_reason() {
        let mut settings = Config::default();
        // Nothing listens here and nothing may be started: the check comes first.
        settings.strom.url = "http://127.0.0.1:9".to_string();
        settings.strom.manage = false;
        settings.open_live.register = false;
        let inputs = Inputs {
            audio: vec!["FaceTime=USB Microphone".parse().unwrap()],
            ..Inputs::test_pattern()
        };
        let (handle, mut events) = Session::start(settings, inputs);
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            seen.push(event);
        }
        assert!(handle.wait().await.is_err());
        assert_eq!(seen.len(), 1, "{seen:?}");
        let Event::Failed { reason } = &seen[0] else {
            panic!("{seen:?}");
        };
        assert!(
            reason.starts_with("--audio pairs an audio source"),
            "{reason}"
        );
    }
}
