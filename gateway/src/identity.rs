//! Gateway identity and the persisted mapping from local input id to Open Live
//! source id. Persisting the mapping is what keeps a restart from creating a
//! duplicate source on every boot.

use anyhow::{Context, Result};
use open_live_gateway_types::GatewayConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    /// Local input id -> Open Live source id (`src-<uuid>`).
    #[serde(default)]
    pub source_ids: HashMap<String, String>,
}

pub fn resolve_gateway_id(cfg: &GatewayConfig) -> String {
    if let Some(id) = cfg.gateway.id.as_deref().filter(|id| !id.is_empty()) {
        return id.to_string();
    }
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "open-live-gateway".to_string())
}

pub fn load(path: &Path) -> Result<PersistedState> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).context("parsing gateway state file"),
        // A missing state file is the normal first-boot case.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(PersistedState::default()),
        Err(err) => Err(err).context("reading gateway state file"),
    }
}

pub fn store(path: &Path, state: &PersistedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating state directory")?;
    }
    let json = serde_json::to_string_pretty(state)?;
    // Write-then-rename so a power cut cannot leave a truncated state file behind,
    // which would orphan the registered sources.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("writing state file")?;
    std::fs::rename(&tmp, path).context("renaming state file")?;
    Ok(())
}
