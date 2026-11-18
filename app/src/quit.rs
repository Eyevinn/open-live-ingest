//! Quitting while live (docs/specs/desktop-app.md, section 8).
//!
//! The rule is the CLI's: a deliberate stop stops, an accident does not. Every way of
//! quitting (the close button, Cmd-W, Cmd-Q, the app menu's Quit, Dock → Quit, a
//! macOS logout or restart) asks [`QuitGuard::request`]. Not live, the app quits. Live,
//! the window asks "You are live with N inputs. Stop and quit?"; Stop and quit runs the
//! same teardown as Stop and quits when it is done, and after
//! [`QUIT_ANYWAY_AFTER`] the question offers Quit anyway.
//!
//! Two stops do not ask: SIGTERM on macOS (`open-live-ingest down`), which stops as
//! the Stop button does and then quits, and a Windows logout or shutdown, where
//! Windows does not wait for a dialog and the teardown is bounded instead
//! ([`QuitHandle::end_session`], the library's `teardown_within`).
//!
//! [`QuitGuard`] is the decision alone, pure apart from the instants it is handed, so
//! it is tested on every OS. The platform hooks are [`macos`] and [`windows`].

use crate::go_live::GoLive;
use crate::ipc::{Deliver, Effect, Event};
use open_live_ingest::run::ENDING_TEARDOWN;
use serde_json::json;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::instrument::WithSubscriber;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

/// How long Stop and quit waits for the teardown before the question adds Quit anyway.
pub const QUIT_ANYWAY_AFTER: Duration = Duration::from_secs(15);

/// How long a bounded teardown is waited for: the library's deadline for removing
/// flows and sources, plus a second for stopping a Strom the app started.
pub const END_SESSION_WAIT: Duration = ENDING_TEARDOWN.saturating_add(Duration::from_secs(1));

/// What a request to quit comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is live: quit now.
    Quit,
    /// Live: ask the question.
    Ask,
    /// Stop and quit is already under way: nothing new to do.
    Wait,
}

/// What Stop and quit comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmed {
    /// Nothing is live any more: quit now.
    Quit,
    /// Stop, then quit once stopped. `first` is false when a stop to quit was already
    /// under way, so the caller does not start a second.
    Stop { first: bool },
}

/// The quit guard's state: whether a stop to quit is under way, and whether the app
/// may end now.
#[derive(Debug)]
pub struct QuitGuard {
    /// Since when Stop and quit (or SIGTERM) has been waiting for the teardown.
    stopping_since: Option<Instant>,
    /// The app may end: nothing was live, the teardown is done, or the person chose
    /// Quit anyway.
    exiting: bool,
    quit_anyway_after: Duration,
}

impl Default for QuitGuard {
    fn default() -> Self {
        Self::with_quit_anyway_after(QUIT_ANYWAY_AFTER)
    }
}

impl QuitGuard {
    /// A guard that offers Quit anyway after `after` rather than
    /// [`QUIT_ANYWAY_AFTER`]: shorter, in tests.
    pub fn with_quit_anyway_after(after: Duration) -> Self {
        QuitGuard {
            stopping_since: None,
            exiting: false,
            quit_anyway_after: after,
        }
    }

    /// How long a stop to quit runs before Quit anyway is offered.
    pub fn quit_anyway_after(&self) -> Duration {
        self.quit_anyway_after
    }

    /// A request to quit: the close button, Cmd-W, Cmd-Q, the app menu, the Dock, a
    /// macOS logout. `live` is whether a session is going live, live or stopping.
    pub fn request(&mut self, live: bool) -> Decision {
        if self.exiting || !live {
            self.exiting = true;
            Decision::Quit
        } else if self.stopping_since.is_some() {
            Decision::Wait
        } else {
            Decision::Ask
        }
    }

    /// Stop and quit, or a stop that does not ask (SIGTERM).
    pub fn confirm(&mut self, live: bool, now: Instant) -> Confirmed {
        if self.exiting || !live {
            self.exiting = true;
            return Confirmed::Quit;
        }
        let first = self.stopping_since.is_none();
        self.stopping_since.get_or_insert(now);
        Confirmed::Stop { first }
    }

    /// Whether the question offers Quit anyway now.
    pub fn offers_quit_anyway(&self, now: Instant) -> bool {
        !self.exiting
            && self
                .stopping_since
                .is_some_and(|since| now.saturating_duration_since(since) >= self.quit_anyway_after)
    }

    /// Quit anyway: allowed once it is offered. What is left behind is swept by the
    /// next Go live or `down`, which find flows by derived id and sources by name.
    pub fn quit_anyway(&mut self, now: Instant) -> bool {
        if self.offers_quit_anyway(now) {
            self.exiting = true;
        }
        self.exiting
    }

    /// The session has stopped. True when a stop to quit was waiting for that, and
    /// the app quits now.
    pub fn stopped(&mut self) -> bool {
        if self.stopping_since.is_some() {
            self.exiting = true;
        }
        self.exiting
    }

    /// The app may end now.
    pub fn exiting(&self) -> bool {
        self.exiting
    }

    /// The process ends whatever is decided here (a bounded teardown has run).
    pub fn ended(&mut self) {
        self.exiting = true;
    }
}

/// Ends the app: the window's event loop exits when it is called. Called from the
/// core's runtime, so it only asks the loop.
pub type Exit = Arc<dyn Fn() + Send + Sync>;

/// The quit guard over the core's session: what every way of quitting goes through.
/// The window's event loop, the platform hooks (macOS `applicationShouldTerminate:`,
/// the Windows window subclass) and the SIGTERM listener each hold one. The hooks run
/// on the main thread outside the loop's handler, so everything here decides from
/// shared state and never waits on the loop.
#[derive(Clone)]
pub struct QuitHandle {
    pub(crate) go_live: Arc<GoLive>,
    pub(crate) guard: Arc<Mutex<QuitGuard>>,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) deliver: Deliver,
    pub(crate) exit: Exit,
}

impl QuitHandle {
    fn guard(&self) -> MutexGuard<'_, QuitGuard> {
        self.guard.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Going live, live or stopping.
    pub fn is_live(&self) -> bool {
        self.go_live.is_live()
    }

    /// Whether the app may end now without asking: nothing is live, or a quit has
    /// already been decided.
    pub fn may_terminate(&self) -> bool {
        self.guard().exiting() || !self.is_live()
    }

    /// A request to quit. Live, the page is sent `quit_requested` to ask the question
    /// and nothing else happens; otherwise [`Effect::Quit`].
    pub fn request(&self) -> Effect {
        let decision = self.guard().request(self.is_live());
        match decision {
            Decision::Quit => Effect::Quit,
            Decision::Ask => {
                let inputs_live = self.go_live.inputs_live();
                tracing::info!(inputs_live, "quit while live: asking");
                (self.deliver)(
                    Event::new("quit_requested", json!({ "inputs_live": inputs_live })).script(),
                );
                Effect::None
            }
            Decision::Wait => Effect::None,
        }
    }

    /// Stop and quit: the same teardown as Stop, and the app ends when it is done
    /// (through [`Exit`]). [`Effect::Quit`] when nothing is live any more. The page
    /// is sent `quit_anyway` if the teardown is still running after
    /// [`QUIT_ANYWAY_AFTER`].
    pub fn stop_and_quit(&self) -> Effect {
        let confirmed = self.guard().confirm(self.is_live(), Instant::now());
        let Confirmed::Stop { first } = confirmed else {
            return Effect::Quit;
        };
        if !first {
            return Effect::None;
        }
        tracing::info!("stopping, then quitting");
        self.go_live.stop();

        let this = self.clone();
        let when_stopped = async move {
            this.go_live.until_idle().await;
            if this.guard().stopped() {
                tracing::info!("stopped: quitting");
                (this.exit)();
            }
        };
        self.runtime.spawn(when_stopped.with_current_subscriber());

        let this = self.clone();
        let after = self.guard().quit_anyway_after();
        let offer = async move {
            tokio::time::sleep(after).await;
            if this.guard().offers_quit_anyway(Instant::now()) {
                tracing::info!(
                    "still stopping after {}s: offering Quit anyway",
                    after.as_secs()
                );
                (this.deliver)(Event::new("quit_anyway", json!({})).script());
            }
        };
        self.runtime.spawn(offer.with_current_subscriber());
        Effect::None
    }

    /// Quit anyway, once offered: the app ends with the teardown still running.
    pub fn quit_anyway(&self) -> bool {
        let quit = self.guard().quit_anyway(Instant::now());
        if quit {
            tracing::warn!(
                "quitting before the stop finished; the next Go live or `open-live-ingest down` removes what is left"
            );
        }
        quit
    }

    /// SIGTERM (`open-live-ingest down` on macOS) stops as the Stop button does,
    /// without asking, and then the app ends. Registered on the core's runtime now,
    /// so a SIGTERM from here on no longer ends the process at once.
    #[cfg(unix)]
    pub fn stop_on_sigterm(&self) {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = {
            let _runtime = self.runtime.enter();
            match signal(SignalKind::terminate()) {
                Ok(term) => term,
                Err(err) => {
                    tracing::warn!("could not listen for SIGTERM: {err}");
                    return;
                }
            }
        };
        let this = self.clone();
        let task = async move {
            if term.recv().await.is_some() {
                tracing::info!("SIGTERM: stopping without asking, then quitting");
                if this.stop_and_quit() == Effect::Quit {
                    (this.exit)();
                }
            }
        };
        self.runtime.spawn(task.with_current_subscriber());
    }

    /// The process is about to be ended from outside (a Windows logout or shutdown,
    /// or on macOS `applicationWillTerminate:` when `applicationShouldTerminate:`
    /// could not be added): stops the session with the bounded teardown and waits at
    /// most [`END_SESSION_WAIT`] for it. Blocks; call it on the main thread, never on
    /// the core's runtime. Does nothing when nothing is live or a quit was already
    /// decided (Quit anyway means now).
    pub fn end_session(&self) {
        if self.may_terminate() {
            return;
        }
        tracing::info!(
            "the session is ending: stopping the live inputs within {}s",
            ENDING_TEARDOWN.as_secs()
        );
        self.go_live.stop_ending();
        let go_live = self.go_live.clone();
        let stopped = self.runtime.block_on(async move {
            tokio::time::timeout(END_SESSION_WAIT, go_live.until_idle())
                .await
                .is_ok()
        });
        if !stopped {
            tracing::warn!(
                "not stopped before the session ends; the next Go live or `open-live-ingest down` removes what is left"
            );
        }
        self.guard().ended();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_live_every_request_quits() {
        let mut guard = QuitGuard::default();
        assert_eq!(guard.request(false), Decision::Quit);
        assert!(guard.exiting());
    }

    #[test]
    fn live_a_request_asks_and_asking_changes_nothing() {
        let mut guard = QuitGuard::default();
        assert_eq!(guard.request(true), Decision::Ask);
        // Cancel is the page closing the question: asked again, it asks again.
        assert_eq!(guard.request(true), Decision::Ask);
        assert!(!guard.exiting());
    }

    #[test]
    fn stop_and_quit_stops_once_then_quits_when_stopped() {
        let mut guard = QuitGuard::default();
        let t = Instant::now();
        assert_eq!(guard.confirm(true, t), Confirmed::Stop { first: true });
        // Cmd-Q again, or SIGTERM, while the teardown runs: no second stop.
        assert_eq!(guard.request(true), Decision::Wait);
        assert_eq!(guard.confirm(true, t), Confirmed::Stop { first: false });
        assert!(!guard.exiting());
        assert!(guard.stopped());
        assert!(guard.exiting());
        assert_eq!(guard.request(true), Decision::Quit);
    }

    #[test]
    fn stop_and_quit_with_nothing_live_any_more_quits_now() {
        let mut guard = QuitGuard::default();
        assert_eq!(guard.request(true), Decision::Ask);
        // The session ended by itself while the question was shown.
        assert_eq!(guard.confirm(false, Instant::now()), Confirmed::Quit);
        assert!(guard.exiting());
    }

    #[test]
    fn a_session_ending_by_itself_does_not_quit() {
        let mut guard = QuitGuard::default();
        assert!(!guard.stopped());
        assert!(!guard.exiting());
    }

    #[test]
    fn quit_anyway_is_offered_after_15_seconds_of_stopping() {
        let mut guard = QuitGuard::default();
        let t = Instant::now();
        assert!(!guard.offers_quit_anyway(t + Duration::from_secs(60)));
        guard.confirm(true, t);
        assert!(!guard.offers_quit_anyway(t + Duration::from_secs(14)));
        assert!(!guard.quit_anyway(t + Duration::from_secs(14)));
        assert!(!guard.exiting());
        assert!(guard.offers_quit_anyway(t + QUIT_ANYWAY_AFTER));
        assert!(guard.quit_anyway(t + QUIT_ANYWAY_AFTER));
        assert!(guard.exiting());
        // Decided: nothing is offered any more.
        assert!(!guard.offers_quit_anyway(t + Duration::from_secs(60)));
    }

    #[test]
    fn the_15_seconds_count_from_the_first_stop_and_quit() {
        let mut guard = QuitGuard::default();
        let t = Instant::now();
        guard.confirm(true, t);
        guard.confirm(true, t + Duration::from_secs(10));
        assert!(guard.offers_quit_anyway(t + QUIT_ANYWAY_AFTER));
    }

    #[test]
    fn the_bounded_wait_is_the_libraries_deadline_plus_a_second_for_strom() {
        assert_eq!(END_SESSION_WAIT, Duration::from_secs(5));
    }
}
