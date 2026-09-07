//! Idempotent source registration and status reconciliation.
//!
//! Registration resolves each input's Open Live source id from the persisted state
//! file, patches the source if a field drifted, and creates it only when absent. That
//! persistence is what stops every reboot from leaving another orphaned source behind
//! in Studio's source list.

use crate::identity;
use crate::openlive::client::{OpenLiveClient, SourcePayload};
use crate::state::SharedState;
use anyhow::{Context, Result};
use open_live_gateway_types::config::{GatewayConfig, InputConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How often the loop reconciles source status against live pipeline state.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

pub fn spawn_registration(state: Arc<SharedState>, cfg: &GatewayConfig) -> Result<()> {
    let url = cfg
        .open_live
        .url
        .clone()
        .context("open_live.url is unset")?;
    let api_key = cfg
        .open_live
        .api_key
        .clone()
        .context("open_live.api_key is unset")?;

    let client = OpenLiveClient::new(&url, &cfg.open_live.auth_mode, &api_key)?;
    let inputs = cfg.inputs.clone();
    let gateway_name = cfg.gateway.name.clone();
    let state_path = PathBuf::from(&cfg.open_live.state_path);

    tokio::spawn(async move {
        run(state, client, inputs, gateway_name, state_path).await;
    });

    Ok(())
}

async fn run(
    state: Arc<SharedState>,
    client: OpenLiveClient,
    inputs: Vec<InputConfig>,
    gateway_name: String,
    state_path: PathBuf,
) {
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);

    loop {
        ticker.tick().await;

        for input in &inputs {
            if let Err(err) = reconcile(&state, &client, input, &gateway_name, &state_path).await {
                // Retried on the next tick. Never fatal: the feed is already flowing.
                warn!(input = %input.id, %err, "source reconciliation failed");
            }
        }
    }
}

async fn reconcile(
    state: &SharedState,
    client: &OpenLiveClient,
    input: &InputConfig,
    gateway_name: &str,
    state_path: &Path,
) -> Result<()> {
    let snapshot = state.snapshot();
    let input_state = snapshot
        .inputs
        .iter()
        .find(|status| status.id == input.id)
        .map(|status| status.state);

    let payload = SourcePayload {
        name: input.source_name(gateway_name),
        address: input.uplink.listener_uri(),
        stream_type: "srt".to_string(),
        status: match input_state {
            Some(s) if s.is_active() => "active".to_string(),
            _ => "inactive".to_string(),
        },
        latency: input.uplink.latency_ms,
        live_camera: Some(true),
    };

    let mut persisted = identity::load(state_path)?;
    let known_id = persisted.source_ids.get(&input.id).cloned();

    let source = match known_id {
        // A source id we remember may have been deleted in Studio since; fall through
        // to creating a fresh one rather than failing forever on a dead id.
        Some(id) => match client.get_source(&id).await? {
            Some(_) => client.patch_source(&id, &payload).await?,
            None => {
                info!(input = %input.id, %id, "remembered source is gone, recreating");
                client.create_source(&payload).await?
            }
        },
        None => {
            let created = client.create_source(&payload).await?;
            info!(input = %input.id, source_id = %created.id, "registered source with Open Live");
            created
        }
    };

    if persisted.source_ids.get(&input.id) != Some(&source.id) {
        persisted
            .source_ids
            .insert(input.id.clone(), source.id.clone());
        identity::store(state_path, &persisted)?;
    }
    state.set_source_id(&input.id, &source.id);

    Ok(())
}
