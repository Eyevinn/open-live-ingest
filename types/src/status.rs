//! Runtime status model, served by the local control API and used to reconcile the
//! Open Live source `status` field.

use serde::{Deserialize, Serialize};

/// Per-input supervision state.
///
/// The gateway does not own a media pipeline, so these states describe the local
/// Strom flow: whether it exists, whether it is running, and whether Strom reports it
/// healthy. Transient SRT drops do not appear here — `builtin.mpegtssrt_output`
/// reconnects on its own (`auto_reconnect` defaults to true), so a dropped uplink is
/// not a reason to touch the flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputState {
    /// Configured but not started.
    Idle,
    /// Flow being created or updated in Strom.
    Provisioning,
    /// Flow created, start requested, not yet reported running.
    Starting,
    /// Strom reports the flow running.
    Running,
    /// Strom is unreachable, so the flow's true state is unknown. The feed may well
    /// still be on air — Strom keeps running when the agent dies.
    Unknown,
    /// Flow failed and the supervisor is waiting on backoff before retrying.
    Failed,
}

impl InputState {
    /// Whether this state should surface as an `active` source in Open Live.
    ///
    /// `Unknown` counts as active on purpose: the agent losing contact with Strom is
    /// not evidence that the feed stopped, and marking the source inactive would
    /// invite an operator to drop a production that is still on air.
    pub fn is_active(self) -> bool {
        matches!(self, InputState::Running | InputState::Unknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub gateway_id: String,
    pub version: String,
    pub uptime_seconds: u64,
    /// Whether the last Strom API call succeeded.
    pub strom_reachable: bool,
    pub inputs: Vec<InputStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputStatus {
    pub id: String,
    pub state: InputState,
    /// Deterministic Strom flow id for this input.
    pub flow_id: String,
    /// Raw GStreamer pipeline state as Strom reports it, for diagnostics.
    pub gst_state: Option<String>,
    /// Open Live source id once registered.
    pub source_id: Option<String>,
    /// SRT listener address registered with Open Live, for cross-checking the cloud side.
    pub listener_address: String,
    pub restarts: u32,
    pub last_error: Option<String>,
}
