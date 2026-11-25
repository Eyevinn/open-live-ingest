//! What `setup` and `devices` find out, as calls that ask nothing.
//!
//! The CLI's `setup` asks on the terminal and checks each answer as it is given; an
//! app (#62) shows a window instead, so it needs the same lookups and checks with the
//! answers passed in and the results handed back as values:
//!
//! - [`devices`]: the video and audio sources Strom can see, as `devices --json`.
//! - [`instances`]: the Open Live instances in an Open Source Cloud workspace, the
//!   list `setup` offers to pick from, and [`workspace`] with the workspace's name as
//!   well. [`check_self_hosted`] checks an address and key for one that is not on
//!   Open Source Cloud.
//! - [`osc_sign_in`]: a browser sign-in, kept in the keychain, and the workspace the
//!   person picked. Signing in again is also how to change workspace.
//! - [`validate`]: every check `setup --non-interactive` makes, each failure reported
//!   as a [`Problem`] instead of stopping at the first.
//! - [`load`] and [`save`]: the settings file the CLI reads, in the same format, through
//!   the same code.
//! - [`keep_osc_sign_in`], [`keep_osc_token`] and [`forget_osc_credential`]: the Open
//!   Source Cloud credential in the OS keychain, with only a marker in the settings.
//!
//! Every call that needs Open Source Cloud takes an [`OscCredential`] from the app: a
//! token in hand, or the keychain the sign-in is kept in. None of them reads
//! `OSC_ACCESS_TOKEN` or the OSC CLI's `~/.osc/token`. [`validate_with`] and
//! [`crate::session::Session::start_with`] take one too, for going live with it.
//!
//! The CLI's `setup` calls these too, and keeps only its prompts.

use crate::config::{self, AuthMode, Config, CredentialStore, UplinkMode, SUGGESTED_PORT_RANGE};
use crate::credential::{self, StoredCredential};
use crate::gateway::Events;
use crate::heartbeat;
use crate::keychain::SecretStore;
use crate::local_strom::{self, LocalStrom};
use crate::openlive::{OpenLiveClient, ServerInfo};
use crate::osc;
use crate::run::{self, DeviceKind, DeviceReport};
use crate::strom::{Reachability, StromClient};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};

pub use crate::config::Problem;
pub use crate::credential::OscCredential;
pub use crate::osc::Instance;

/// What `--config` defaults to in the CLI: `OLI_CONFIG` when set, then the per-user file.
const ENV_CONFIG: &str = "OLI_CONFIG";

/// The capture devices Strom can see, each list sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Devices {
    pub video: Vec<DeviceReport>,
    pub audio: Vec<DeviceReport>,
}

/// The video and audio sources Strom reports, with ids, names and the virtual flag,
/// as `devices --json` lists them. A managed Strom is started to ask, if nothing is
/// listening, and stopped again before this returns; `events` hears about both.
pub async fn devices(settings: &Config, events: &Events) -> Result<Devices> {
    Ok(split(run::devices(settings, events).await?))
}

/// [`devices`], asking the Strom `hold` keeps running: nothing is started or stopped.
pub async fn devices_with(hold: &StromHold) -> Result<Devices> {
    Ok(split(run::devices_on(&hold.client).await?))
}

fn split(devices: Vec<DeviceReport>) -> Devices {
    let (video, audio) = devices
        .into_iter()
        .partition(|d| d.kind == DeviceKind::Video);
    Devices { video, audio }
}

/// The local Strom, held by an app between sessions: the desktop app holds it from
/// its What to send step until Go live, so the step can list devices and preview
/// cameras on the same Strom the session then sends with (docs/specs/desktop-app.md,
/// section 4.1). Taken with [`hold_strom`], used with [`devices_with`], and either
/// handed to a session ([`crate::session::Launch`]), which owns it from then on and
/// stops a Strom it holds that was started here when it ends, as it does a Strom it
/// started itself; or given up with [`StromHold::release`].
///
/// Dropping a hold stops a Strom it started, as `devices` does on its way out, with
/// no check for anyone else using it: release it instead, unless the process is
/// going away anyway.
pub struct StromHold {
    local: LocalStrom,
    client: StromClient,
    version: Option<String>,
    /// The `[strom]` settings it was taken with, for [`StromHold::serves`].
    url: String,
    api_key: Option<String>,
    manage: bool,
}

impl fmt::Debug for StromHold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No key: it is a credential.
        f.debug_struct("StromHold")
            .field("url", &self.url)
            .field("managed", &self.local.is_managed())
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// What [`StromHold::release`] did with the Strom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Released {
    /// It was running before the hold was taken, and is left as it was.
    Adopted,
    /// It was started for the hold, and is stopped now.
    Stopped,
    /// It was started for the hold but is left running, still recorded in
    /// `strom.pid`, so `down` (or the next reclaim) stops it.
    LeftRunning(LeftRunning),
}

/// Why a Strom started for a hold is left running at its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftRunning {
    /// `gateway.pid` names another gateway, which may have adopted it.
    OtherGateway { pid: u32 },
    /// It runs flows: someone else's (a CLI `up` that adopted it, Strom's own UI).
    Flows { count: usize },
    /// Its flows could not be listed, so whether anyone uses it is not known.
    FlowsUnknown,
}

impl StromHold {
    /// A client for it.
    pub fn client(&self) -> &StromClient {
        &self.client
    }

    /// Its version, as it reported it when the hold was taken.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Whether it was started for the hold (Managed) rather than adopted.
    pub fn is_managed(&self) -> bool {
        self.local.is_managed()
    }

    /// Whether it is the Strom `settings` name: the same `strom.url`, `strom.api_key`
    /// and `strom.manage` as when it was taken. A change of any of them calls for a
    /// new hold.
    pub fn serves(&self, settings: &Config) -> bool {
        self.url == settings.strom.url
            && self.api_key == settings.strom.api_key
            && self.manage == settings.strom.manage
    }

    /// Gives the hold up. A Strom that was adopted is left as it is. One started for
    /// the hold is stopped, and `strom.pid` removed, only when it runs no flow and
    /// `gateway.pid` names no other gateway; otherwise another gateway (a CLI `up`
    /// that adopted it) or someone else uses it, and it is left running, still
    /// recorded in `strom.pid`, which is logged. Remove your own flows first: every
    /// flow counts. Blocks while a Strom stops (at most 10 s).
    pub async fn release(self) -> Released {
        let other = run::running_gateway();
        self.release_unless(other).await
    }

    async fn release_unless(mut self, other_gateway: Option<u32>) -> Released {
        if !self.local.is_managed() {
            return Released::Adopted;
        }
        let left = match other_gateway {
            // Gone already: nobody uses it, and its record goes with it.
            _ if self.local.has_exited() => None,
            Some(pid) => Some(LeftRunning::OtherGateway { pid }),
            None => match self.client.list_flows().await {
                Ok(flows) if flows.is_empty() => None,
                Ok(flows) => Some(LeftRunning::Flows { count: flows.len() }),
                Err(err) => {
                    tracing::warn!(err = %format_args!("{err:#}"), "could not list the held Strom's flows");
                    Some(LeftRunning::FlowsUnknown)
                }
            },
        };
        match left {
            None => {
                drop(self.local);
                Released::Stopped
            }
            Some(why) => {
                tracing::info!(
                    ?why,
                    "releasing the held Strom: left running, still recorded in strom.pid"
                );
                self.local.leave_running();
                Released::LeftRunning(why)
            }
        }
    }

    /// The parts a session runs on.
    pub(crate) fn into_parts(self) -> (LocalStrom, StromClient, Option<String>) {
        (self.local, self.client, self.version)
    }

    /// A hold over `local`, reached at `url`, for tests.
    #[cfg(test)]
    pub(crate) fn for_test(local: LocalStrom, url: &str) -> Self {
        StromHold {
            local,
            client: StromClient::new(url, None).unwrap(),
            version: None,
            url: url.to_string(),
            api_key: None,
            manage: true,
        }
    }
}

/// Holds the local Strom for an app: adopts the one answering at `strom.url`, or
/// starts one headless when `strom.manage` allows (recorded in `strom.pid`), as `up`
/// does, and reads its version. `events` hears Strom starting and ready, and, unless
/// it is handed to a session, its stop. A Strom left behind by a crash should be
/// reclaimed first ([`local_strom::reclaim_recorded`]): adopted, it would never be
/// stopped. No `gateway.pid` is written: holding a Strom is not going live.
pub async fn hold_strom(settings: &Config, events: &Events) -> Result<StromHold> {
    let (local, client) = run::connect_strom(settings, events).await?;
    let version = client.version().await.ok();
    Ok(StromHold {
        local,
        client,
        version,
        url: settings.strom.url.clone(),
        api_key: settings.strom.api_key.clone(),
        manage: settings.strom.manage,
    })
}

/// An Open Source Cloud workspace as a credential sees it: every credential is bound
/// to exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Workspace {
    /// The workspace, from the token's `tenantId` claim. None for a token that does
    /// not say.
    pub id: Option<String>,
    /// Its Open Live instances.
    pub instances: Vec<Instance>,
}

/// The Open Live instances in the workspace `credential` belongs to: a token in hand
/// (a personal access token, or the access token of a sign-in just made), or the
/// sign-in kept in a keychain, renewed if need be. An empty list is not an error; the
/// instance may live in another workspace, so a caller should still let an address
/// be typed in.
pub async fn instances(credential: impl Into<OscCredential>) -> Result<Vec<Instance>> {
    Ok(workspace(credential).await?.instances)
}

/// The workspace `credential` belongs to and its Open Live instances, for "Sending to
/// <instance> (<workspace>)" and the instance picker.
///
/// Open Source Cloud has no API listing the workspaces a person belongs to
/// (docs/research/OSC_BROWSER_SIGN_IN.md): the workspace is picked on its consent page
/// at sign-in, so this is the only one there is to show. [`osc_sign_in`] changes it.
pub async fn workspace(credential: impl Into<OscCredential>) -> Result<Workspace> {
    workspace_at(&osc::Endpoints::production(), &credential.into()).await
}

/// [`workspace`], asking the Open Source Cloud at `endpoints`: production, or a fake
/// in a test.
pub async fn workspace_at(
    endpoints: &osc::Endpoints,
    credential: &OscCredential,
) -> Result<Workspace> {
    let token = credential.access_token_at(&endpoints.connect).await?;
    let id = osc::inspect(&token).and_then(|i| i.workspace);
    let instances = osc::open_live_instances_at(endpoints, &token)
        .await
        .with_context(|| match &id {
            Some(id) => format!("listing the Open Live instances in workspace {id}"),
            None => "listing the Open Live instances in the workspace".to_string(),
        })?;
    Ok(Workspace { id, instances })
}

/// Only the workspace `credential` belongs to, without listing anything: the token's
/// `tenantId`. Asks Open Source Cloud nothing unless a kept sign-in has to be renewed.
pub async fn workspace_id(credential: impl Into<OscCredential>) -> Result<Option<String>> {
    let token = credential.into().access_token().await?;
    Ok(osc::inspect(&token).and_then(|i| i.workspace))
}

/// Signs in to Open Source Cloud in the browser, keeps the sign-in in `keychain` with
/// the settings pointing at it (as [`keep_osc_sign_in`] does), and returns the
/// workspace the person picked, with its Open Live instances. `open` shows the
/// sign-in page: [`osc::open_browser`], or that and the address in the window, for
/// when no browser opens.
///
/// **Changing workspace is signing in again.** Open Source Cloud has no API listing a
/// person's workspaces and binds every token to one (#74,
/// docs/research/OSC_BROWSER_SIGN_IN.md), so the person picks the workspace on its
/// consent page, and the new sign-in replaces the kept one. `open_live.url` is left
/// alone: when it is not among the returned instances, it is in the other workspace,
/// and the person picks an instance again.
///
/// A sign-in that fails, is declined or times out keeps nothing: the keychain and the
/// settings stay as they were. Save the settings afterwards.
pub async fn osc_sign_in(
    settings: &mut Config,
    keychain: &dyn SecretStore,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<Workspace> {
    osc_sign_in_at(
        &osc::Endpoints::production(),
        osc::SIGN_IN_TIMEOUT,
        settings,
        keychain,
        open,
    )
    .await
}

/// [`osc_sign_in`] against the Open Source Cloud at `endpoints`, waiting at most
/// `wait` for the browser to come back ([`osc::SIGN_IN_TIMEOUT`] in production).
pub async fn osc_sign_in_at(
    endpoints: &osc::Endpoints,
    wait: std::time::Duration,
    settings: &mut Config,
    keychain: &dyn SecretStore,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<Workspace> {
    let signed_in = osc::sign_in(&endpoints.connect, wait, open).await?;
    keep_osc_sign_in(settings, keychain, &signed_in)?;
    // The access token in hand serves for the list; nothing needs renewing yet.
    workspace_at(endpoints, &OscCredential::from(&signed_in)).await
}

/// Checks an Open Live that is not on Open Source Cloud: the address, and that it
/// accepts `api_key` (None or blank for one that needs none). The address must not
/// be an Open Source Cloud one, which takes a sign-in rather than a key.
pub async fn check_self_hosted(url: &str, api_key: Option<&str>) -> Result<(), Problem> {
    let url = url.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Problem::new(
            "open_live.url",
            format!("the Open Live address must start with http:// or https://, got {url:?}"),
        ));
    }
    if infer_auth_mode(url) == AuthMode::Osc {
        return Err(Problem::new(
            "open_live.url",
            "that is an Open Source Cloud address, so it needs an Open Source Cloud sign-in rather than an API key",
        ));
    }
    let mut cfg = Config::default();
    cfg.open_live.url = Some(url.to_string());
    cfg.open_live.auth_mode = AuthMode::Direct;
    cfg.open_live.api_key = api_key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    check_open_live(&cfg).await.map(|_| ()).map_err(|err| {
        Problem::new(
            "open_live",
            format!("could not reach Open Live at {url}: {err:#}"),
        )
    })
}

/// Every check `setup --non-interactive` makes, with nothing asked and nothing
/// stopped at the first failure: the settings themselves ([`config::problems`]), then
/// Open Live and its credential, the heartbeat, the local Strom, and whether there
/// are SRT ports to use. Empty when the settings are ready for `up`.
///
/// Open Live, the heartbeat and Strom are contacted, so this takes as long as the
/// slowest of them answers.
pub async fn validate(settings: &Config) -> Vec<Problem> {
    validate_as(settings, None).await
}

/// [`validate`], for going live with `credential`, the Open Source Cloud credential
/// the app holds, as [`crate::session::Session::start_with`] does: Open Live is
/// checked with it, and no token is looked for in the settings, `OSC_ACCESS_TOKEN`
/// or `~/.osc/token`.
pub async fn validate_with(settings: &Config, credential: &OscCredential) -> Vec<Problem> {
    validate_as(settings, Some(credential)).await
}

async fn validate_as(settings: &Config, credential: Option<&OscCredential>) -> Vec<Problem> {
    let cfg = settings;
    let mut found = match credential {
        Some(_) => config::problems_with(cfg, true),
        None => config::problems(cfg),
    };
    let reported =
        |found: &[Problem], setting: &str| found.iter().any(|p| p.setting == Some(setting));

    if cfg.gateway.name.trim().is_empty() {
        found.push(Problem::new(
            "gateway.name",
            "gateway.name is empty; it prefixes every source name in Open Live",
        ));
    }

    // What Open Live publishes, and whether it was asked: a port range can only be
    // called missing once Open Live has answered without one.
    let mut published = None;
    let mut ports_known = true;
    // The SRT relays Open Live reports, once it has answered.
    let mut relays: Option<Vec<String>> = None;
    if !cfg.open_live.register {
        if cfg
            .uplink
            .host
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            found.push(Problem::new(
                "uplink.host",
                "registration is off, so there is nobody to ask where the cloud Strom is: set uplink.host",
            ));
        }
    } else if reported(&found, "open_live.url") || reported(&found, "open_live.api_key") {
        ports_known = false;
    } else {
        let url = cfg.open_live.url.as_deref().unwrap_or_default().trim();
        // A keychain sign-in is renewed as it is used, so it is checked by using it,
        // in check_open_live below.
        let in_hand = match credential {
            Some(OscCredential::Token(token)) => Some((token.trim().to_string(), "the app")),
            Some(OscCredential::Keychain(_) | OscCredential::KeychainAt(..)) => None,
            None if cfg.open_live.uses_keychain() => None,
            None => osc_credential(cfg).map(|(token, source, _)| (token, source)),
        };
        let expired = cfg.open_live.auth_mode == AuthMode::Osc
            && in_hand.is_some_and(|(token, source)| {
                let expired = osc::inspect(&token).is_some_and(|i| i.is_expired());
                if expired {
                    found.push(Problem::new(
                        "open_live.api_key",
                        format!("the Open Source Cloud token from {source} has expired"),
                    ));
                }
                expired
            });
        if expired {
            ports_known = false;
        } else {
            match check_open_live_as(cfg, credential).await {
                Ok(info) => {
                    relays = Some(
                        info.as_ref()
                            .map(|i| i.relays().iter().map(|r| r.name.clone()).collect())
                            .unwrap_or_default(),
                    );
                    published = info.and_then(|i| i.srt_ports);
                }
                Err(err) => {
                    ports_known = false;
                    found.push(Problem::new(
                        "open_live",
                        format!("could not reach Open Live at {url}: {err:#}"),
                    ));
                }
            }
        }
    }

    if let Ok(Some(target)) = heartbeat::Target::from_config(&cfg.open_live) {
        if let Err(err) = heartbeat::probe(&target).await {
            found.push(Problem::new(
                "open_live.gateway_token",
                format!("checking the heartbeat to Open Live: {err:#}"),
            ));
        }
    }

    // The phone has no Strom: its own code does the media (docs/specs/ios-app.md).
    if !cfg!(target_os = "ios") && !reported(&found, "strom.url") {
        match probe_strom(cfg).await {
            Reachability::Ok { .. } => {}
            Reachability::NeedsCredential => found.push(Problem::new(
                "strom.api_key",
                format!(
                    "Strom at {} wants a credential; set strom.api_key, or OLI_STROM_API_KEY, to its STROM_API_KEY",
                    cfg.strom.url
                ),
            )),
            // `up` starts one, so all that has to be there is the program.
            Reachability::Unreachable(_) if cfg.strom.manage => {
                if let Err(err) = local_strom::locate_binary(&cfg.strom.binary) {
                    found.push(Problem::new("strom.binary", format!("{err:#}")));
                }
            }
            Reachability::Unreachable(err) => found.push(Problem::new(
                "strom.url",
                format!(
                    "could not reach Strom at {}: {err}. Start it, or set strom.manage = true to let the gateway start one",
                    cfg.strom.url
                ),
            )),
        }
    }

    // Through a relay no port is needed; in caller mode the gateway goes through
    // one whenever Open Live reports any (run::choose_relay, on a computer; the phone
    // cannot dial a relay yet), and in relay mode it must.
    let relay = cfg.uplink.relay_name();
    let through_relay = !cfg!(target_os = "ios")
        && relays
            .as_ref()
            .is_some_and(|r| r.iter().any(|name| name == relay));
    if let Some(reported) = relays.as_ref().filter(|_| !through_relay) {
        let wants_relay = match cfg.uplink.mode {
            UplinkMode::Relay => true,
            UplinkMode::Caller => !reported.is_empty() && !cfg!(target_os = "ios"),
            UplinkMode::Listener => false,
        };
        if wants_relay {
            found.push(Problem::new(
                if reported.is_empty() { "uplink.mode" } else { "uplink.relay" },
                if reported.is_empty() {
                    "uplink.mode is \"relay\", but Open Live reports no SRT relay; set uplink.mode = \"caller\"".to_string()
                } else {
                    format!(
                        "Open Live has no SRT relay named {relay:?}; it has {}: set uplink.relay to one of them",
                        reported.join(", ")
                    )
                },
            ));
        }
    }

    // In listener mode the range is this machine's own, and config::problems already
    // insists on it.
    if cfg.uplink.mode == UplinkMode::Caller
        && !through_relay
        && ports_known
        && published.is_none()
        && matches!(cfg.uplink.local_ports(), Ok(None))
    {
        let why = if cfg.open_live.register {
            "Open Live publishes no SRT port range"
        } else {
            "registration is off, so nobody publishes the cloud Strom's SRT ports"
        };
        found.push(Problem::new(
            "uplink.port_range",
            format!("{why}, so one is needed in uplink.port_range, e.g. {SUGGESTED_PORT_RANGE}"),
        ));
    }
    found
}

/// The settings file the CLI uses: `OLI_CONFIG` when set, as `--config` defaults to,
/// and otherwise [`config::default_path`].
pub fn settings_path() -> PathBuf {
    std::env::var_os(ENV_CONFIG)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(config::default_path)
}

/// The settings from [`settings_path`], as the CLI loads them: defaults when there
/// is no file yet, then the `OLI_*` environment overrides.
pub fn load() -> Result<Config> {
    load_from(&settings_path())
}

/// The settings from `path`, as the CLI's `--config path` loads them.
pub fn load_from(path: &Path) -> Result<Config> {
    config::load_or_default(path)
}

/// Writes the settings to [`settings_path`], where the CLI reads them, and returns
/// that path.
pub fn save(settings: &Config) -> Result<PathBuf, SaveError> {
    let path = settings_path();
    save_to(&path, settings)?;
    Ok(path)
}

/// Writes the settings to `path` through [`config::save`], the CLI's own writer: the
/// same TOML, replaced atomically, readable by the user only. Settings with any
/// [`config::problems`] are refused with all of them, and the file is left alone.
pub fn save_to(path: &Path, settings: &Config) -> Result<(), SaveError> {
    let problems = config::problems(settings);
    if !problems.is_empty() {
        return Err(SaveError::Invalid(problems));
    }
    config::save(path, settings).map_err(SaveError::Write)
}

/// Why [`save`] wrote nothing.
#[derive(Debug)]
pub enum SaveError {
    /// The settings have problems, every one listed.
    Invalid(Vec<Problem>),
    /// The settings are fine but the file could not be written.
    Write(anyhow::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Invalid(problems) => {
                let all: Vec<&str> = problems.iter().map(|p| p.message.as_str()).collect();
                write!(f, "the settings were not saved: {}", all.join("; "))
            }
            SaveError::Write(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// Keeps a browser sign-in ([`osc::browser_login`]) in `keychain`, and points the
/// settings at it: `open_live.credential = "keychain"`, osc mode, and no token in
/// `open_live.api_key` (#65 AC3). What is kept is the refresh token, which `up`
/// renews the access token from for as long as a show runs. Save the settings
/// afterwards; nothing is written to the file here.
pub fn keep_osc_sign_in(
    settings: &mut Config,
    keychain: &dyn SecretStore,
    sign_in: &osc::Credential,
) -> Result<()> {
    keep_in_keychain(settings, keychain, &StoredCredential::from_sign_in(sign_in))
}

/// Keeps a pasted personal access token in `keychain`, the settings pointing at it
/// as after a sign-in: the token-paste fallback, with nothing secret in the file.
pub fn keep_osc_token(
    settings: &mut Config,
    keychain: &dyn SecretStore,
    token: &str,
) -> Result<()> {
    keep_in_keychain(settings, keychain, &StoredCredential::from_token(token)?)
}

fn keep_in_keychain(
    settings: &mut Config,
    keychain: &dyn SecretStore,
    kept: &StoredCredential,
) -> Result<()> {
    credential::keep(keychain, kept)?;
    settings.open_live.auth_mode = AuthMode::Osc;
    settings.open_live.credential = Some(CredentialStore::Keychain);
    settings.open_live.api_key = None;
    Ok(())
}

/// Removes the Open Source Cloud credential from `keychain` and the marker from the
/// settings. Save the settings afterwards.
pub fn forget_osc_credential(settings: &mut Config, keychain: &dyn SecretStore) -> Result<()> {
    credential::forget(keychain)?;
    settings.open_live.credential = None;
    Ok(())
}

/// Which kind of credential an Open Live address implies. An Open Source Cloud
/// instance sits behind a proxy that rejects a personal access token presented
/// directly, so it needs the exchange; anything else takes a static key. Inferring it
/// spares an operator a question nobody can answer from the words "osc" and "direct".
pub fn infer_auth_mode(url: &str) -> AuthMode {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if host == "osaas.io" || host.ends_with(".osaas.io") {
        AuthMode::Osc
    } else {
        AuthMode::Direct
    }
}

/// The Open Source Cloud credential found outside the OS keychain, in the order the
/// running gateway tries those sources: the token in the settings,
/// `OSC_ACCESS_TOKEN`, then the OSC CLI's login. Returns the token, where it came
/// from, and whether it is the login.
///
/// This knows nothing of the keychain. When the settings carry the keychain marker
/// ([`crate::config::OpenLive::uses_keychain`]) and no token, the running gateway
/// uses the keychain credential, renewed as it goes, instead of anything returned
/// here; check the marker first, as [`validate`] does.
pub fn osc_credential(settings: &Config) -> Option<(String, &'static str, bool)> {
    if let Some(token) = settings
        .open_live
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some((
            token.to_string(),
            "the settings (a personal access token)",
            false,
        ));
    }
    if let Some(token) = osc::env_token() {
        return Some((token, osc::ENV_TOKEN, false));
    }
    osc::login_token().map(|token| (token, "the OSC CLI's saved login", true))
}

/// The sources in Open Live that carry the settings' source name prefix
/// (`<gateway.name> — `, [`crate::devices::name_prefix`]) and are not tagged as this
/// gateway's (`gatewayId`, set when the heartbeat is configured), by name. A stop
/// and `down` sweep the sources of their prefix, so a gateway renamed onto a prefix
/// that is in use would remove sources it did not register: a front end that lets
/// the name be changed warns with these (docs/specs/ios-app.md, section 7.1).
/// `credential` is the app's Open Source Cloud credential, as for [`validate_with`].
pub async fn sources_named_like(
    settings: &Config,
    credential: Option<&OscCredential>,
) -> Result<Vec<String>> {
    let client = match credential {
        Some(credential) => OpenLiveClient::with_osc_credential(&settings.open_live, credential)?,
        None => OpenLiveClient::from_settings(&settings.open_live)?,
    };
    let prefix = crate::devices::name_prefix(&settings.gateway.name);
    let ours = settings.open_live.gateway_id.as_deref();
    Ok(client
        .list_sources()
        .await?
        .into_iter()
        .filter(|s| s.name.starts_with(&prefix))
        .filter(|s| ours.is_none() || s.gateway_id.as_deref() != ours)
        .map(|s| s.name)
        .collect())
}

/// Confirms the address and credential work, returning what Open Live says about
/// its cloud Strom: its host and the SRT ports it has leased.
pub async fn check_open_live(settings: &Config) -> Result<Option<ServerInfo>> {
    check_open_live_as(settings, None).await
}

async fn check_open_live_as(
    settings: &Config,
    credential: Option<&OscCredential>,
) -> Result<Option<ServerInfo>> {
    let client = match credential {
        Some(credential) => OpenLiveClient::with_osc_credential(&settings.open_live, credential)?,
        None => OpenLiveClient::from_settings(&settings.open_live)?,
    };
    // Listing sources exercises the credential; server-info alone might not.
    client.list_sources().await?;
    client.server_info().await
}

/// Whether the local Strom answers, and with what.
pub async fn probe_strom(settings: &Config) -> Reachability {
    match StromClient::new(&settings.strom.url, settings.strom.api_key.as_deref()) {
        Ok(client) => client.probe().await,
        Err(err) => Reachability::Unreachable(err.to_string()),
    }
}

/// A fake local Strom for the library's tests: plain HTTP/1.1 on std threads, one
/// request per connection, each answered by `handler` from its method and path.
#[cfg(test)]
pub(crate) mod fake_strom {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    pub(crate) type Handler = Arc<dyn Fn(&str, &str) -> (u16, serde_json::Value) + Send + Sync>;

    /// Serves `handler` on a free port of 127.0.0.1; returns `http://127.0.0.1:<port>`.
    pub(crate) fn serve(handler: Handler) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    if reader.read_line(&mut first).is_err() {
                        return;
                    }
                    // The headers, up to the blank line; the routes used have no body.
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                        line.clear();
                    }
                    let mut words = first.split_whitespace();
                    let method = words.next().unwrap_or_default().to_string();
                    let path = words.next().unwrap_or_default().to_string();
                    let (status, body) = handler(&method, &path);
                    let body = body.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        format!("http://{addr}")
    }

    /// A Strom that sees no devices and runs `flows`.
    pub(crate) fn running(flows: Vec<strom_types::Flow>) -> String {
        serve(Arc::new(move |_, path| {
            if path.starts_with("/api/discovery/devices") {
                (200, serde_json::json!([]))
            } else if path == "/api/flows" {
                (200, serde_json::json!({ "flows": flows }))
            } else if path == "/api/version" {
                (
                    200,
                    serde_json::json!({
                        "version": "0.8.4", "git_hash": "0", "git_tag": "v0.8.4",
                        "git_branch": "main", "git_dirty": false, "build_timestamp": ""
                    }),
                )
            } else {
                (404, serde_json::Value::Null)
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::StoredCredential;
    use crate::keychain::MemoryKeychain;
    use crate::osc::fake;
    use std::sync::Arc;
    use std::time::Duration;

    /// `{"tenantId":"eyevinnlab","patId":"x"}`: a console token, which never expires.
    const PAT: &str = "h.eyJ0ZW5hbnRJZCI6ImV5ZXZpbm5sYWIiLCJwYXRJZCI6IngifQ.s";

    fn kept(keychain: &MemoryKeychain) -> Option<StoredCredential> {
        crate::credential::load(keychain).unwrap()
    }

    /// The workspace shown is the token's `tenantId`, whether the token is in hand or
    /// kept in the keychain (#65 AC4). Nothing is asked of the platform for either.
    #[tokio::test]
    async fn the_workspace_is_read_from_the_credentials_tenant_id() {
        assert_eq!(
            workspace_id(PAT).await.unwrap().as_deref(),
            Some("eyevinnlab")
        );

        let keychain = Arc::new(MemoryKeychain::new());
        let mut settings = Config::default();
        keep_osc_token(&mut settings, keychain.as_ref(), PAT).unwrap();
        let from_keychain = OscCredential::Keychain(keychain.clone());
        assert_eq!(
            workspace_id(&from_keychain).await.unwrap().as_deref(),
            Some("eyevinnlab")
        );

        forget_osc_credential(&mut settings, keychain.as_ref()).unwrap();
        let err = workspace_id(&from_keychain).await.unwrap_err();
        assert!(err.to_string().contains("sign in"), "{err:#}");
        assert!(workspace_id("  ").await.is_err());
        assert!(!format!("{:?}", OscCredential::from(PAT)).contains(PAT));
        // A JWT whose `exp` is 1970.
        let err = workspace_id("e30.eyJleHAiOjF9.sig").await.unwrap_err();
        assert!(err.to_string().contains("expired"), "{err:#}");
    }

    /// A kept sign-in is renewed first, the renewed refresh token is kept, and the
    /// list is asked for with the new access token.
    #[tokio::test]
    async fn the_workspace_of_a_kept_sign_in_is_listed_with_a_renewed_token() {
        let osc = fake::osc(true, None).await;
        let keychain = Arc::new(MemoryKeychain::new());
        crate::credential::keep(
            keychain.as_ref(),
            &StoredCredential::SignIn {
                refresh_token: fake::REFRESH.into(),
                client_id: "registered-id".into(),
            },
        )
        .unwrap();

        let found = workspace_at(&osc.endpoints(), &OscCredential::Keychain(keychain.clone()))
            .await
            .unwrap();
        assert_eq!(
            found,
            Workspace {
                id: Some(fake::WORKSPACE.into()),
                instances: vec![Instance {
                    name: "venue".into(),
                    url: "https://venue.example.osaas.io".into(),
                }],
            }
        );
        assert!(matches!(
            kept(&keychain),
            Some(StoredCredential::SignIn { refresh_token, .. }) if refresh_token == fake::RENEWED
        ));
        assert_eq!(
            osc.seen.lock().unwrap().catalog_tokens,
            vec![format!("Bearer {}", fake::ACCESS)]
        );
    }

    /// Signing in (or in again, to change workspace) replaces the kept credential,
    /// points the settings at the keychain with no token in them, and returns the
    /// workspace picked on the consent page with its instances.
    #[tokio::test]
    async fn a_sign_in_is_kept_and_returns_the_workspace_picked_in_the_browser() {
        let osc = fake::osc(true, None).await;
        let keychain = MemoryKeychain::new();
        let mut settings = Config::default();
        settings.open_live.url = Some("https://old.example.osaas.io".into());
        keep_osc_token(&mut settings, &keychain, PAT).unwrap();

        let (open, _) = fake::browser(osc.seen.clone(), "code=the-code");
        let found = osc_sign_in_at(
            &osc.endpoints(),
            Duration::from_secs(10),
            &mut settings,
            &keychain,
            open,
        )
        .await
        .unwrap();

        assert_eq!(found.id.as_deref(), Some(fake::WORKSPACE));
        assert_eq!(found.instances.len(), 1);
        assert_eq!(
            kept(&keychain),
            Some(StoredCredential::SignIn {
                refresh_token: fake::REFRESH.into(),
                client_id: "registered-id".into(),
            }),
            "the new sign-in replaces the old workspace's"
        );
        assert!(settings.open_live.uses_keychain());
        assert_eq!(settings.open_live.api_key, None);
        assert_eq!(
            settings.open_live.url.as_deref(),
            Some("https://old.example.osaas.io"),
            "the instance is the person's to pick again"
        );
    }

    /// Declining on the consent page leaves the workspace that was signed in to.
    #[tokio::test]
    async fn a_declined_sign_in_keeps_the_credential_there_was() {
        let osc = fake::osc(true, None).await;
        let keychain = MemoryKeychain::new();
        let mut settings = Config::default();
        keep_osc_token(&mut settings, &keychain, PAT).unwrap();
        let before = settings.clone();

        let (open, _) = fake::browser(osc.seen.clone(), "error=access_denied");
        let err = osc_sign_in_at(
            &osc.endpoints(),
            Duration::from_secs(10),
            &mut settings,
            &keychain,
            open,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("access_denied"), "{err:#}");
        assert_eq!(kept(&keychain), StoredCredential::from_token(PAT).ok());
        assert_eq!(
            toml::to_string(&settings).unwrap(),
            toml::to_string(&before).unwrap()
        );
    }

    #[test]
    fn an_osaas_address_implies_the_token_exchange_and_lookalikes_do_not() {
        assert_eq!(
            infer_auth_mode("https://venue.eyevinn-open-live.auto.prod-se.osaas.io"),
            AuthMode::Osc
        );
        assert_eq!(infer_auth_mode("https://osaas.io/"), AuthMode::Osc);
        assert_eq!(infer_auth_mode("http://127.0.0.1:3000"), AuthMode::Direct);
        assert_eq!(
            infer_auth_mode("https://open-live.example.com"),
            AuthMode::Direct
        );
        assert_eq!(
            infer_auth_mode("https://osaas.io.example.com"),
            AuthMode::Direct
        );
        assert_eq!(
            infer_auth_mode("https://myosaas.iohost.net"),
            AuthMode::Direct
        );
    }

    #[test]
    fn a_token_in_the_settings_comes_first() {
        let mut cfg = Config::default();
        cfg.open_live.api_key = Some("  pat  ".to_string());
        assert_eq!(
            osc_credential(&cfg),
            Some((
                "pat".to_string(),
                "the settings (a personal access token)",
                false
            ))
        );
    }

    #[test]
    fn a_save_error_lists_every_problem() {
        let err = SaveError::Invalid(vec![
            Problem::new("strom.url", "one"),
            Problem::new(None, "two"),
        ]);
        assert_eq!(err.to_string(), "the settings were not saved: one; two");
    }

    // -----------------------------------------------------------------------
    // The Strom an app holds (docs/specs/desktop-app.md, section 4.1)
    // -----------------------------------------------------------------------

    /// A Strom answering at the settings' address is adopted for the hold, listed
    /// through it, and given up untouched.
    #[tokio::test]
    async fn a_running_strom_is_held_adopted_and_released_untouched() {
        let url = fake_strom::running(Vec::new());
        let mut settings = Config::default();
        settings.strom.url = url.clone();
        settings.strom.manage = false;
        let hold = hold_strom(&settings, &Events::discard()).await.unwrap();
        assert!(!hold.is_managed());
        assert_eq!(hold.version(), Some("0.8.4"));
        assert!(hold.serves(&settings));
        let devices = devices_with(&hold).await.unwrap();
        assert!(devices.video.is_empty() && devices.audio.is_empty());

        let mut moved = settings.clone();
        moved.strom.url = "http://127.0.0.1:9".to_string();
        assert!(!hold.serves(&moved), "another address");
        let mut keyed = settings.clone();
        keyed.strom.api_key = Some("k".to_string());
        assert!(!hold.serves(&keyed), "another key");
        let mut managed = settings.clone();
        managed.strom.manage = true;
        assert!(!hold.serves(&managed), "manage changed");

        assert_eq!(hold.release().await, Released::Adopted);
    }

    /// A process standing in for a Strom started for the hold, recorded in a scratch
    /// `strom.pid`, with its events kept.
    #[cfg(unix)]
    struct Started {
        dir: std::path::PathBuf,
        pid: u32,
        pidfile: std::path::PathBuf,
        seen: Arc<std::sync::Mutex<Vec<crate::gateway::Event>>>,
    }

    #[cfg(unix)]
    impl Started {
        fn hold(name: &str, url: &str) -> (Self, StromHold) {
            let dir = std::env::temp_dir().join(format!("oli-hold-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let child = std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap();
            let pid = child.id();
            let pidfile = dir.join("strom.pid");
            std::fs::write(&pidfile, pid.to_string()).unwrap();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let events = {
                let seen = seen.clone();
                Events::new(move |e| seen.lock().unwrap().push(e))
            };
            let local = LocalStrom::managed_for_test(child, pidfile.clone(), events);
            let hold = StromHold::for_test(local, url);
            (
                Started {
                    dir,
                    pid,
                    pidfile,
                    seen,
                },
                hold,
            )
        }

        fn alive(&self) -> bool {
            // Signal 0 checks the pid exists; a stopped child is reaped, so it is gone.
            unsafe { libc::kill(self.pid as libc::pid_t, 0) == 0 }
        }

        fn stopped_events(&self) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, crate::gateway::Event::StromStopped))
                .count()
        }
    }

    #[cfg(unix)]
    impl Drop for Started {
        fn drop(&mut self) {
            if self.alive() {
                unsafe {
                    libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A Strom started for the hold that runs a flow that is not ours (a CLI `up`
    /// adopted it meanwhile) is left running at the release, still in `strom.pid`.
    #[cfg(unix)]
    #[tokio::test]
    async fn release_leaves_a_strom_with_a_foreign_flow_running_and_keeps_strom_pid() {
        let url = fake_strom::running(vec![strom_types::Flow::new("Someone else's")]);
        let (started, hold) = Started::hold("foreign", &url);
        assert!(hold.is_managed());

        let released = hold.release_unless(None).await;

        assert_eq!(
            released,
            Released::LeftRunning(LeftRunning::Flows { count: 1 })
        );
        assert!(started.alive(), "the Strom was stopped");
        assert_eq!(
            std::fs::read_to_string(&started.pidfile).unwrap(),
            started.pid.to_string(),
            "strom.pid still names it"
        );
        assert_eq!(started.stopped_events(), 0);
    }

    /// Another gateway live on this machine may use it: left running, unasked.
    #[cfg(unix)]
    #[tokio::test]
    async fn release_leaves_the_strom_running_while_another_gateway_is_live() {
        let url = fake_strom::running(Vec::new());
        let (started, hold) = Started::hold("other", &url);

        let released = hold.release_unless(Some(4242)).await;

        assert_eq!(
            released,
            Released::LeftRunning(LeftRunning::OtherGateway { pid: 4242 })
        );
        assert!(started.alive());
        assert!(started.pidfile.exists());
    }

    /// Flows that cannot be listed may be anyone's: left running.
    #[cfg(unix)]
    #[tokio::test]
    async fn release_leaves_the_strom_running_when_its_flows_are_not_known() {
        let url = fake_strom::serve(Arc::new(|_, _| (500, serde_json::Value::Null)));
        let (started, hold) = Started::hold("unknown", &url);

        let released = hold.release_unless(None).await;

        assert_eq!(released, Released::LeftRunning(LeftRunning::FlowsUnknown));
        assert!(started.alive());
        assert!(started.pidfile.exists());
    }

    /// With no flow and no other gateway, the Strom started for the hold is stopped
    /// and its record removed.
    #[cfg(unix)]
    #[tokio::test]
    async fn release_stops_an_idle_strom_started_for_the_hold() {
        let url = fake_strom::running(Vec::new());
        let (started, hold) = Started::hold("idle", &url);

        let released = hold.release_unless(None).await;

        assert_eq!(released, Released::Stopped);
        assert!(!started.alive(), "the Strom still runs");
        assert!(!started.pidfile.exists(), "strom.pid kept");
        assert_eq!(started.stopped_events(), 1);
    }
}
