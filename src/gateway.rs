//! What the gateway says while it works, as values instead of text.
//!
//! Everything `up`, `down` and a managed Strom used to print is an [`Event`] handed to
//! an [`Events`] sink. The CLI renders each event to the line it has always printed;
//! an app shows the same events in a window, with no terminal to scrape.
//!
//! Warnings are also logged through `tracing` where they happen, as before. The
//! events that carry them are for a caller that shows its own UI; the CLI renders
//! them as nothing, because the log already said it.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// One thing worth telling the operator.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Nothing was listening at the Strom URL, so a headless Strom is being started.
    StromStarting { port: u16, log_path: PathBuf },
    /// The Strom just started answers.
    StromReady { video_sources: usize },
    /// The Strom this process started has been stopped, on the way out.
    StromStopped,
    /// `down` is stopping a Strom an earlier run started and left behind.
    StromLeftOverStopping { pid: u32 },

    /// `up` knows where to send and is about to bring its inputs up.
    BringingUp {
        inputs: usize,
        host: String,
        /// The SRT ports in use, formatted as `47110-47129`.
        ports: String,
        /// Who chose them: `Open Live` or `the settings`.
        port_source: String,
        /// The whole uplink, from the configured video bitrate.
        total_mbps: f64,
    },
    /// An Open Live source of this gateway, left by an earlier run, held a port one of
    /// the inputs needs and was removed.
    LeftoverSourceRemoved { name: String, port: u16 },
    /// A Strom flow was removed before starting: one of ours left by an earlier run,
    /// or someone else's holding one of the cameras.
    ConflictingFlowRemoved { name: String, ours: bool },
    /// An input's source is being created in Open Live, or given a new port there.
    Registering { input: String },
    /// An input's source was created in Open Live.
    Registered { input: String, source_id: String },
    /// Open Live registered an input's source but warns that its address may not
    /// work against the cloud Strom (`connect.warning`, such as
    /// `srt-listener-unsupported`).
    ConnectWarning { input: String, warning: String },
    /// An input could not be started. The others carry on.
    InputFailed { input: String, reason: String },
    /// The inputs that started, and where each is registered.
    Summary { inputs: Vec<SummaryRow> },
    /// Status is pushed to Open Live over the heartbeat, as this gateway id.
    HeartbeatStarted { gateway_id: String },
    /// An input's state changed, as the last poll found it.
    Input { input: String, state: InputState },
    /// The Open Source Cloud credential cannot be renewed and stops working at `at`
    /// (Unix seconds), within the session about to start or already past. Sent before
    /// going live. After that time Open Live refuses the gateway's requests while the
    /// feeds keep running: sign in again, or use a personal access token instead.
    OscSignInEnds { at: u64 },
    /// Something the operator may need to act on, that stops nothing.
    Warning { message: String },
    /// Every tick while running: each input's state and uplink, from the same data
    /// the heartbeat sends to Open Live.
    Stats { inputs: Vec<InputStats> },

    /// `up` is stopping and removing what it started.
    Stopping,
    /// An input's flow was stopped and removed from Strom.
    FlowRemoved { input: String },
    /// A source was removed from Open Live.
    SourceRemoved { input: String },
    /// Open Live refused to remove a source, for `reason` (usually an active
    /// production holding it).
    SourceKept { input: String, reason: String },

    /// `down` found a running `up` and asked it to stop.
    AskingRunningToStop { pid: u32 },
    /// The running `up` did not stop in time; `down` removes what is left itself.
    RunningDidNotStop,
    /// `down` could not reach Strom, so its flows are left alone.
    StromUnreachable { url: String, error: String },
    /// `down` found nothing of this gateway's to remove.
    NothingToRemove,

    /// A session ([`crate::session`]) stopped and removed what it started. Its last
    /// event.
    Stopped,
    /// A session ended on an error, before it ran or as it ran, for `reason`: the
    /// same text the CLI prints after `Error:`. Anything it had started has been
    /// removed again, flows and the Open Live sources it registered, except after a
    /// panic (see `session`'s caveats). Its last event.
    Failed { reason: String },
}

/// One started input, as the start summary lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    pub name: String,
    pub port: u16,
    /// The address registered with Open Live, passphrase masked.
    pub address: String,
}

/// One input in a [`Event::Stats`].
#[derive(Debug, Clone, PartialEq)]
pub struct InputStats {
    pub input: String,
    /// The input's Strom flow, whose `srt-stats` the uplink numbers come from.
    pub flow_id: strom_types::FlowId,
    /// The input's Open Live source, once registered. What a production holds.
    pub source_id: Option<String>,
    pub state: InputState,
    /// None while nothing is receiving the uplink.
    pub uplink: Option<UplinkStats>,
}

/// The uplink of one input, as the heartbeat reports it (`heartbeat::Uplink`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UplinkStats {
    pub bitrate_kbps: u64,
    pub rtt_ms: f64,
    /// Packets dropped before transmission.
    pub dropped: u64,
}

/// What the last poll found out about an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputState {
    /// Start requested, first poll pending.
    Starting,
    /// The flow runs but nothing is receiving the uplink. Normal until the source is
    /// assigned to a production.
    Waiting,
    /// Bytes are leaving. Reported to Open Live as `active`.
    OnAir,
    /// Strom is not answering. Not evidence that the feed stopped.
    Unreachable,
    Failed {
        reason: String,
    },
}

impl InputState {
    /// The state in a few words, as the CLI prints it.
    pub fn describe(&self) -> String {
        match self {
            InputState::Starting => "starting".to_string(),
            InputState::Waiting => "waiting for a receiver".to_string(),
            InputState::OnAir => "on air, the far end is receiving".to_string(),
            InputState::Unreachable => "lost contact with Strom".to_string(),
            InputState::Failed { reason } => format!("failed: {reason}"),
        }
    }
}

/// Where events go. Cheap to clone; every clone feeds the same sink. The sink is
/// called on whatever task or thread emits, including from `Drop` during teardown,
/// so it must not block for long and must not panic.
#[derive(Clone)]
pub struct Events(Arc<dyn Fn(Event) + Send + Sync>);

impl Events {
    pub fn new(sink: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Events(Arc::new(sink))
    }

    /// A sink that drops every event.
    pub fn discard() -> Self {
        Events::new(|_| {})
    }

    pub fn emit(&self, event: Event) {
        (self.0)(event)
    }
}

impl fmt::Debug for Events {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Events(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn every_clone_feeds_the_same_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let events = {
            let seen = seen.clone();
            Events::new(move |e| seen.lock().unwrap().push(e))
        };
        events.emit(Event::Stopping);
        events.clone().emit(Event::NothingToRemove);
        Events::discard().emit(Event::StromStopped);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Event::Stopping, Event::NothingToRemove]
        );
    }

    #[test]
    fn input_states_read_as_the_cli_prints_them() {
        assert_eq!(InputState::Waiting.describe(), "waiting for a receiver");
        assert_eq!(
            InputState::Failed {
                reason: "boom".to_string()
            }
            .describe(),
            "failed: boom"
        );
    }
}
