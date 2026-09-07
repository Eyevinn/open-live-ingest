//! Flow supervision.
//!
//! One task per input, reconciling desired state against what the local Strom reports.
//! The reconcile is idempotent and safe to run forever: create the flow if absent,
//! update it if the config changed, start it if it is not running.
//!
//! Boot recovery is not this loop's job: Strom sets `auto_restart` on a flow when it
//! is started and restarts every flagged flow at startup, so a box whose flow was
//! already running comes back on its own. The loop exists to provision the flow in
//! the first place, to push config changes, and to recover a flow that fails while
//! running — which boot-time auto-restart does not cover.
//!
//! Transient SRT drops are deliberately not handled here: `builtin.mpegtssrt_output`
//! has `auto_reconnect` on by default, so the block re-dials the cloud on its own.
//! Restarting the flow for a network blip would turn a recoverable gap into an
//! encoder restart.

use crate::state::SharedState;
use crate::strom::client::{FlowCreateOutcome, StromClient};
use crate::strom::flow;
use anyhow::Result;
use open_live_gateway_types::config::{GatewayConfig, InputConfig};
use open_live_gateway_types::status::InputState;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How often each input's flow is reconciled against Strom.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Backoff bounds after a failed reconcile.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn spawn_supervisors(state: Arc<SharedState>, cfg: &GatewayConfig) -> Result<()> {
    let gateway_id = state.gateway_id().to_string();
    let gateway_name = cfg.gateway.name.clone();

    for input in cfg.inputs.iter().filter(|i| i.enabled) {
        let client = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
        let state = Arc::clone(&state);
        let input = input.clone();
        let gateway_id = gateway_id.clone();
        let gateway_name = gateway_name.clone();

        tokio::spawn(async move {
            supervise(state, client, input, gateway_id, gateway_name).await;
        });
    }

    for input in cfg.inputs.iter().filter(|i| !i.enabled) {
        info!(input = %input.id, "input disabled by config, not supervised");
    }

    Ok(())
}

async fn supervise(
    state: Arc<SharedState>,
    client: StromClient,
    input: InputConfig,
    gateway_id: String,
    gateway_name: String,
) {
    let flow_id = flow::flow_id(&gateway_id, &input.id);
    let mut backoff = BACKOFF_MIN;

    let desired = match flow::build(&gateway_id, &gateway_name, &input) {
        Ok(flow) => flow,
        Err(err) => {
            // A flow that cannot be built will never build; retrying is pointless.
            warn!(input = %input.id, %err, "cannot build flow, input will not start");
            state.set_state(&input.id, InputState::Failed);
            state.record_error(&input.id, &err.to_string());
            return;
        }
    };

    loop {
        match reconcile(&state, &client, &input, &flow_id, &desired).await {
            Ok(()) => {
                backoff = BACKOFF_MIN;
                tokio::time::sleep(RECONCILE_INTERVAL).await;
            }
            Err(err) => {
                warn!(input = %input.id, %err, "flow reconciliation failed, retrying");
                // Unknown, not Failed: Strom being unreachable is not evidence that
                // the flow stopped. It may well still be pushing to the cloud.
                state.set_state(&input.id, InputState::Unknown);
                state.set_strom_reachable(false);
                state.record_error(&input.id, &err.to_string());

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

async fn reconcile(
    state: &SharedState,
    client: &StromClient,
    input: &InputConfig,
    flow_id: &str,
    desired: &Value,
) -> Result<()> {
    let existing = client.get_flow(flow_id).await?;
    state.set_strom_reachable(true);

    let running = match existing {
        Some(fetched) => {
            // Only write when the shape actually differs. An unconditional update
            // would rewrite the flow every tick, and each rewrite drops the fields
            // Strom owns — auto_restart among them, which is what recovers the feed
            // after a reboot.
            if flow::differs(&fetched.raw, desired) {
                info!(input = %input.id, flow_id, "flow differs from config, updating");
                state.set_state(&input.id, InputState::Provisioning);
                let body = flow::preserving_strom_state(desired, &fetched.raw);
                client.update_flow(flow_id, &body).await?;

                // Updating a flow only rewrites Strom's stored copy — a running
                // pipeline keeps the properties it was built with, so a changed
                // bitrate or uplink URI would never take effect. Restarting is the
                // only way to apply it, and it briefly interrupts the feed, so it
                // happens only for a real shape change and is logged as such.
                if fetched.state.running {
                    warn!(
                        input = %input.id, flow_id,
                        "restarting flow to apply the new configuration, interrupting the feed briefly"
                    );
                    client.stop_flow(flow_id).await?;
                }
                state.set_gst_state(&input.id, None);
                // Fall through to the start path below.
                false
            } else {
                state.set_gst_state(&input.id, fetched.state.gst_state.as_deref());
                fetched.state.running
            }
        }
        None => {
            state.set_state(&input.id, InputState::Provisioning);
            match client.create_flow(desired).await? {
                FlowCreateOutcome::Created(created) => {
                    info!(input = %input.id, flow_id, "created flow in Strom");
                    created.running
                }
                // Raced with another writer between the GET and the POST. Re-read
                // rather than blind-updating, for the same reason as above.
                FlowCreateOutcome::AlreadyExists => client
                    .get_flow(flow_id)
                    .await?
                    .map(|f| f.state.running)
                    .unwrap_or(false),
            }
        }
    };

    if running {
        // The flow being up says nothing about whether the far end is taking the
        // stream. Only a delivering uplink makes this input safe to assign to a
        // production, so the state distinguishes the two.
        classify_uplink(state, client, input, flow_id).await?;
        return Ok(());
    }

    state.set_state(&input.id, InputState::Starting);
    let started = client.start_flow(flow_id).await?;
    state.set_gst_state(&input.id, started.gst_state.as_deref());

    if started.running {
        info!(input = %input.id, flow_id, "flow running");
        state.clear_uplink(&input.id);
        state.set_state(&input.id, InputState::Stalled);
    } else {
        // Start returned without the pipeline reaching a running state — surface it
        // and let the next tick try again.
        state.set_state(&input.id, InputState::Failed);
        state.record_error(&input.id, "Strom reported the flow not running after start");
    }

    Ok(())
}

/// Sets `Running` or `Stalled` from the uplink's SRT statistics.
async fn classify_uplink(
    state: &SharedState,
    client: &StromClient,
    input: &InputConfig,
    flow_id: &str,
) -> Result<()> {
    let Some(uplink) = client.srt_uplink(flow_id).await? else {
        // No SRT connection reported at all: the pipeline is up but the uplink has
        // not established, so nothing is reaching the far end.
        state.clear_uplink(&input.id);
        state.set_state(&input.id, InputState::Stalled);
        return Ok(());
    };

    let delivering = state.record_uplink(&input.id, &uplink);

    if delivering {
        state.set_state(&input.id, InputState::Running);
    } else {
        state.set_state(&input.id, InputState::Stalled);
    }
    Ok(())
}
