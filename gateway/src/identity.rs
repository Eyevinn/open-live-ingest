//! Gateway identity and the persisted mapping from local input id to Open Live
//! source id. Persisting the mapping is what keeps a restart from creating a
//! duplicate source on every boot.

use anyhow::{Context, Result};
use open_live_gateway_types::GatewayConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    /// Local input id -> Open Live source id (`src-<uuid>`).
    #[serde(default)]
    pub source_ids: HashMap<String, String>,
    /// What the last `up` started, keyed by input id.
    ///
    /// Recorded independently of registration on purpose: `status` and `down` have to
    /// work when registration is off, and after a hard kill left flows behind. Tying
    /// the record to a registered source made both blind in exactly those cases.
    #[serde(default)]
    pub started: BTreeMap<String, StartedInput>,
    /// The pid of a Strom this gateway started, if it started one. Recorded so a
    /// `down` after a hard kill can stop it; an adopted Strom is never recorded,
    /// and so never stopped.
    #[serde(default)]
    pub strom_pid: Option<u32>,
}

/// Enough about a started input to report on it and to tear it down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartedInput {
    pub device_name: String,
    pub port: u16,
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

/// Records that an input was started, so `status` and `down` can find it later.
pub fn record_started(path: &Path, input_id: &str, device_name: &str, port: u16) -> Result<()> {
    let mut state = load(path)?;
    state.started.insert(
        input_id.to_string(),
        StartedInput {
            device_name: device_name.to_string(),
            port,
        },
    );
    store(path, &state)
}

/// Forgets a torn-down input.
pub fn forget_started(path: &Path, input_id: &str) -> Result<()> {
    let mut state = load(path)?;
    state.started.remove(input_id);
    state.source_ids.remove(input_id);
    store(path, &state)
}

/// Records the pid of a Strom we started.
pub fn record_strom_pid(path: &Path, pid: u32) -> Result<()> {
    let mut state = load(path)?;
    state.strom_pid = Some(pid);
    store(path, &state)
}

/// Forgets a Strom pid, once it has been stopped.
pub fn forget_strom_pid(path: &Path) -> Result<()> {
    let mut state = load(path)?;
    state.strom_pid = None;
    store(path, &state)
}

pub fn store(path: &Path, state: &PersistedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating the state directory {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(state)?;
    // Write-then-rename so a power cut cannot leave a truncated state file behind,
    // which would orphan the registered sources.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("writing state file")?;
    std::fs::rename(&tmp, path).context("renaming state file")?;
    Ok(())
}
