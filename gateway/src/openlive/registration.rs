//! Idempotent source registration and status reconciliation.
//!
//! Registration resolves each input's Open Live source id from the persisted state
//! file, patches the source if a field drifted, and creates it only when absent. That
//! persistence is what stops every reboot from leaving another orphaned source behind
//! in Studio's source list.

use crate::identity;
use crate::openlive::client::{OpenLiveClient, SourcePayload, SourceResponse};
use crate::state::SharedState;
use anyhow::{Context, Result};
use open_live_gateway_types::config::{GatewayConfig, InputConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How often the loop reconciles source status against live pipeline state.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Builds the Open Live client from config, if registration is configured at all.
pub fn client_from(cfg: &GatewayConfig) -> Result<OpenLiveClient> {
    let url = cfg
        .open_live
        .url
        .clone()
        .context("open_live.url is unset")?;
    OpenLiveClient::new(
        &url,
        &cfg.open_live.auth_mode,
        cfg.open_live.api_key.as_deref().filter(|k| !k.is_empty()),
    )
}

/// Reconciles a single input forever. Spawned per input, so the desktop app can start
/// and stop one without disturbing the others.
pub async fn reconcile_forever(
    state: Arc<SharedState>,
    client: OpenLiveClient,
    input: InputConfig,
    gateway_name: String,
    state_path: PathBuf,
) {
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        ticker.tick().await;
        if let Err(err) = reconcile(&state, &client, &input, &gateway_name, &state_path).await {
            warn!(input = %input.id, %err, "source reconciliation failed");
        }
    }
}

/// Marks an input's source inactive, so a stopped feed does not sit in Studio looking
/// available. Best-effort: a failure here must not block shutdown.
pub async fn mark_inactive(client: &OpenLiveClient, state_path: &Path, input_id: &str) {
    let Ok(persisted) = identity::load(state_path) else {
        return;
    };
    if let Some(source_id) = persisted.source_ids.get(input_id) {
        if let Err(err) = client.set_status(source_id, "inactive").await {
            warn!(input = %input_id, %err, "could not mark the source inactive on the way out");
        }
    }
}

pub fn spawn_registration(state: Arc<SharedState>, cfg: &GatewayConfig) -> Result<()> {
    let url = cfg
        .open_live
        .url
        .clone()
        .context("open_live.url is unset")?;
    let client = OpenLiveClient::new(
        &url,
        &cfg.open_live.auth_mode,
        cfg.open_live.api_key.as_deref().filter(|k| !k.is_empty()),
    )?;
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

    let desired = SourcePayload {
        name: input.source_name(gateway_name),
        address: input.uplink.cloud_uri(),
        stream_type: "srt".to_string(),
        // `active` means "safe to assign to a production", which requires the
        // uplink to be delivering — not merely that the local flow is running. A
        // source whose feed never arrives stops the cloud flow from reaching
        // playing, so an over-optimistic status here takes down a whole show.
        status: match input_state {
            Some(s) if s.is_deliverable() => "active".to_string(),
            _ => "inactive".to_string(),
        },
        latency: input.uplink.latency_ms,
        live_camera: Some(true),
    };

    // One list call, for two reasons: a 200 with an array proves the API is serving
    // (so an absent id is real evidence of deletion, not a restarting instance), and
    // it gives the stored source to compare against.
    let sources = client.list_sources().await?;

    let mut persisted = identity::load(state_path)?;
    let known_id = persisted.source_ids.get(&input.id).cloned();
    let existing = known_id
        .as_deref()
        .and_then(|id| sources.iter().find(|s| s.id == id));

    let source_id = match existing {
        Some(stored) => {
            // Only write when something actually differs. Open Live stores sources in
            // CouchDB, which keeps a revision per write, so a needless PATCH every
            // reconcile would add thousands of revisions a day per source.
            if drifted(stored, &desired) {
                info!(input = %input.id, source_id = %stored.id, "source differs, updating");
                client.patch_source(&stored.id, &desired).await?.id
            } else {
                stored.id.clone()
            }
        }
        None => {
            if let Some(id) = known_id.as_deref() {
                info!(
                    input = %input.id, %id,
                    "remembered source is absent from a healthy source list, recreating"
                );
            }
            let created = client.create_source(&desired).await?;
            info!(input = %input.id, source_id = %created.id, "registered source with Open Live");
            created.id
        }
    };

    if persisted.source_ids.get(&input.id) != Some(&source_id) {
        persisted
            .source_ids
            .insert(input.id.clone(), source_id.clone());
        identity::store(state_path, &persisted)?;
    }
    state.set_source_id(&input.id, &source_id);

    Ok(())
}

/// Whether the stored source needs updating to match what the gateway wants.
///
/// Addresses are compared with the SRT passphrase masked, because Open Live masks it
/// on read: comparing raw would report drift on every tick for any source configured
/// with a passphrase, and PATCH it forever.
fn drifted(stored: &SourceResponse, desired: &SourcePayload) -> bool {
    stored.name != desired.name
        || stored.stream_type != desired.stream_type
        || stored.status != desired.status
        || stored.latency != Some(desired.latency)
        || mask_passphrase(&stored.address) != mask_passphrase(&desired.address)
}

/// Replaces an SRT passphrase value with the same mask Open Live applies on read.
fn mask_passphrase(address: &str) -> String {
    let Some(at) = address.to_lowercase().find("passphrase=") else {
        return address.to_string();
    };
    let value_start = at + "passphrase=".len();
    let value_end = address[value_start..]
        .find('&')
        .map(|i| value_start + i)
        .unwrap_or(address.len());
    format!("{}***{}", &address[..value_start], &address[value_end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(address: &str, status: &str) -> SourceResponse {
        SourceResponse {
            id: "src-1".to_string(),
            name: "Dev — cam1".to_string(),
            address: address.to_string(),
            stream_type: "srt".to_string(),
            status: status.to_string(),
            latency: Some(200),
        }
    }

    fn desired(address: &str, status: &str) -> SourcePayload {
        SourcePayload {
            name: "Dev — cam1".to_string(),
            address: address.to_string(),
            stream_type: "srt".to_string(),
            status: status.to_string(),
            latency: 200,
            live_camera: Some(true),
        }
    }

    #[test]
    fn an_identical_source_does_not_drift() {
        let address = "srt://:9000?mode=listener";
        assert!(!drifted(
            &stored(address, "active"),
            &desired(address, "active")
        ));
    }

    #[test]
    fn a_status_change_drifts() {
        let address = "srt://:9000?mode=listener";
        assert!(drifted(
            &stored(address, "inactive"),
            &desired(address, "active")
        ));
    }

    #[test]
    fn a_port_change_drifts() {
        assert!(drifted(
            &stored("srt://:9000?mode=listener", "active"),
            &desired("srt://:9001?mode=listener", "active")
        ));
    }

    #[test]
    fn a_latency_change_drifts() {
        let address = "srt://:9000?mode=listener";
        let mut want = desired(address, "active");
        want.latency = 400;
        assert!(drifted(&stored(address, "active"), &want));
    }

    /// Open Live masks the passphrase on read. Comparing raw values would report
    /// drift forever and PATCH the source on every single tick.
    #[test]
    fn a_masked_passphrase_does_not_drift() {
        assert!(!drifted(
            &stored("srt://:9000?mode=listener&passphrase=***", "active"),
            &desired("srt://:9000?mode=listener&passphrase=s3cret", "active")
        ));
    }

    #[test]
    fn a_passphrase_change_is_invisible_but_the_rest_is_not() {
        // Masking makes a changed passphrase undetectable — an accepted limitation,
        // since the alternative is rewriting the source forever. A port change
        // alongside it must still be caught.
        assert!(drifted(
            &stored("srt://:9000?mode=listener&passphrase=***", "active"),
            &desired("srt://:9002?mode=listener&passphrase=other", "active")
        ));
    }

    #[test]
    fn masking_handles_a_passphrase_followed_by_other_params() {
        assert_eq!(
            mask_passphrase("srt://:9000?passphrase=abc&pbkeylen=16"),
            "srt://:9000?passphrase=***&pbkeylen=16"
        );
        assert_eq!(
            mask_passphrase("srt://:9000?mode=listener"),
            "srt://:9000?mode=listener"
        );
    }
}
