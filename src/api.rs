//! What `setup` and `devices` find out, as calls that ask nothing.
//!
//! The CLI's `setup` asks on the terminal and checks each answer as it is given; an
//! app (#62) shows a window instead, so it needs the same lookups and checks with the
//! answers passed in and the results handed back as values:
//!
//! - [`devices`]: the video and audio sources Strom can see, as `devices --json`.
//! - [`instances`]: the Open Live instances in an Open Source Cloud workspace, the
//!   list `setup` offers to pick from. [`check_self_hosted`] checks an address and key
//!   for one that is not on Open Source Cloud.
//! - [`validate`]: every check `setup --non-interactive` makes, each failure reported
//!   as a [`Problem`] instead of stopping at the first.
//! - [`load`] and [`save`]: the settings file the CLI reads, in the same format, through
//!   the same code.
//! - [`keep_osc_sign_in`], [`keep_osc_token`] and [`forget_osc_credential`]: the Open
//!   Source Cloud credential in the OS keychain, with only a marker in the settings.
//!
//! The CLI's `setup` calls these too, and keeps only its prompts.

use crate::config::{self, AuthMode, Config, CredentialStore, UplinkMode, SUGGESTED_PORT_RANGE};
use crate::credential::{self, StoredCredential};
use crate::gateway::Events;
use crate::heartbeat;
use crate::keychain::SecretStore;
use crate::local_strom;
use crate::openlive::{OpenLiveClient, ServerInfo};
use crate::osc;
use crate::run::{self, DeviceKind, DeviceReport};
use crate::strom::{Reachability, StromClient};
use anyhow::{bail, Result};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};

pub use crate::config::Problem;
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
    let (video, audio) = run::devices(settings, events)
        .await?
        .into_iter()
        .partition(|d| d.kind == DeviceKind::Video);
    Ok(Devices { video, audio })
}

/// The Open Live instances in the workspace `credential` belongs to: an Open Source
/// Cloud personal access token, `OSC_ACCESS_TOKEN`, or the OSC CLI's login. An empty
/// list is not an error; the instance may live in another workspace, so a caller
/// should still let an address be typed in.
pub async fn instances(credential: &str) -> Result<Vec<Instance>> {
    let credential = credential.trim();
    if credential.is_empty() {
        bail!("no Open Source Cloud credential");
    }
    if osc::inspect(credential).is_some_and(|i| i.is_expired()) {
        bail!("that Open Source Cloud token has expired; sign in again");
    }
    osc::open_live_instances(credential).await
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
    let cfg = settings;
    let mut found = config::problems(cfg);
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
        let expired = cfg.open_live.auth_mode == AuthMode::Osc
            && !cfg.open_live.uses_keychain()
            && osc_credential(cfg).is_some_and(|(token, source, _)| {
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
            match check_open_live(cfg).await {
                Ok(info) => published = info.and_then(|i| i.srt_ports),
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

    if !reported(&found, "strom.url") {
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

    // In listener mode the range is this machine's own, and config::problems already
    // insists on it.
    if cfg.uplink.mode == UplinkMode::Caller
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

/// Confirms the address and credential work, returning what Open Live says about
/// its cloud Strom: its host and the SRT ports it has leased.
pub async fn check_open_live(settings: &Config) -> Result<Option<ServerInfo>> {
    let client = OpenLiveClient::from_settings(&settings.open_live)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
