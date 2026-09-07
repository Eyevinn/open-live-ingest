//! Configuration loading: TOML file, then environment overrides, then CLI flags
//! (precedence CLI > env > file), followed by validation.

use anyhow::{bail, Context, Result};
use open_live_gateway_types::config::UplinkMode;
use open_live_gateway_types::GatewayConfig;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Environment overrides applied on top of the file. Deliberately a short list —
/// the values a deployment tool needs to inject without templating a whole file.
const ENV_OPEN_LIVE_URL: &str = "OLG_OPEN_LIVE_URL";
const ENV_OPEN_LIVE_API_KEY: &str = "OLG_OPEN_LIVE_API_KEY";
const ENV_OPEN_LIVE_AUTH_MODE: &str = "OLG_OPEN_LIVE_AUTH_MODE";
const ENV_CONTROL_BIND: &str = "OLG_CONTROL_BIND";
const ENV_CONTROL_TOKEN: &str = "OLG_CONTROL_TOKEN";
const ENV_STROM_URL: &str = "OLG_STROM_URL";
const ENV_STROM_API_KEY: &str = "OLG_STROM_API_KEY";

/// Where the desktop app keeps its config when none is given on the command line.
///
/// A per-user location rather than the working directory: someone launching the app
/// from Finder has no working directory to speak of.
pub fn default_path() -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };

    dir.unwrap_or_else(|| PathBuf::from("."))
        .join("open-live-gateway")
        .join("gateway.toml")
}

/// Loads the config, or returns defaults if the file does not exist yet.
///
/// First run must not be an error: the app opens with empty settings for the operator
/// to fill in, rather than refusing to start until someone writes a TOML file.
pub fn load_or_default(path: &Path) -> Result<GatewayConfig> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let cfg: GatewayConfig = toml::from_str(&raw).context("parsing config TOML")?;
            Ok(with_env_overrides(cfg))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(with_env_overrides(GatewayConfig::default()))
        }
        Err(err) => Err(err).context("reading config file"),
    }
}

/// Writes the config back, so the app can own its own settings.
///
/// Written to a temporary file and renamed, so an interrupted save cannot leave a
/// half-written config behind; and mode 0600, because it holds the Open Live
/// credential and any SRT passphrase.
pub fn save(path: &Path, cfg: &GatewayConfig) -> Result<()> {
    validate(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let toml = toml::to_string_pretty(cfg).context("serialising config")?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml).context("writing config")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .context("restricting config permissions")?;
    }

    std::fs::rename(&tmp, path).context("replacing the config file")?;
    Ok(())
}

pub fn load(path: &Path, log_level_override: Option<&str>) -> Result<GatewayConfig> {
    let raw = std::fs::read_to_string(path).context("reading config file")?;
    let cfg: GatewayConfig = toml::from_str(&raw).context("parsing config TOML")?;
    let mut cfg = with_env_overrides(cfg);
    if let Some(level) = log_level_override {
        cfg.log.level = level.to_string();
    }
    validate(&cfg)?;
    Ok(cfg)
}

/// Applies the environment overrides a deployment tool may set.
fn with_env_overrides(mut cfg: GatewayConfig) -> GatewayConfig {
    if let Ok(url) = std::env::var(ENV_OPEN_LIVE_URL) {
        cfg.open_live.url = Some(url);
    }
    if let Ok(key) = std::env::var(ENV_OPEN_LIVE_API_KEY) {
        cfg.open_live.api_key = Some(key);
    }
    if let Ok(mode) = std::env::var(ENV_OPEN_LIVE_AUTH_MODE) {
        cfg.open_live.auth_mode = mode;
    }
    if let Ok(bind) = std::env::var(ENV_CONTROL_BIND) {
        cfg.control.bind = bind;
    }
    if let Ok(token) = std::env::var(ENV_CONTROL_TOKEN) {
        cfg.control.token = Some(token);
    }
    if let Ok(url) = std::env::var(ENV_STROM_URL) {
        cfg.strom.url = url;
    }
    if let Ok(key) = std::env::var(ENV_STROM_API_KEY) {
        cfg.strom.api_key = Some(key);
    }
    cfg
}

/// Rejects configurations that would fail confusingly at runtime.
fn validate(cfg: &GatewayConfig) -> Result<()> {
    if cfg.strom.url.trim().is_empty() {
        bail!("strom.url is unset — the gateway needs a local Strom instance to drive");
    }
    if !cfg.strom.url.starts_with("http://") && !cfg.strom.url.starts_with("https://") {
        bail!(
            "strom.url must be an http or https URL, got {}",
            cfg.strom.url
        );
    }

    let mut ids = HashSet::new();
    let mut ports = HashSet::new();
    for input in &cfg.inputs {
        if !ids.insert(&input.id) {
            bail!("duplicate input id: {}", input.id);
        }
        // Local uniqueness only. Collisions across a fleet pointing at one Strom are
        // still the operator's problem — see docs/DESIGN.md §9.
        if !ports.insert(input.uplink.port) {
            bail!(
                "input {} reuses SRT port {} already claimed by another input",
                input.id,
                input.uplink.port
            );
        }
        if input.uplink.host.trim().is_empty() {
            bail!("input {} has no uplink host", input.id);
        }
        if matches!(
            input.uplink.mode,
            UplinkMode::Listener | UplinkMode::Rendezvous
        ) && input
            .uplink
            .public_host
            .as_deref()
            .is_none_or(|h| h.trim().is_empty())
        {
            bail!(
                "input {} uses uplink mode {:?}, where the cloud dials the venue, so \
                 uplink.public_host must be set to this gateway's address as the cloud sees it",
                input.id,
                input.uplink.mode
            );
        }
        if let Some(len) = input.uplink.pbkeylen {
            if !matches!(len, 16 | 24 | 32) {
                bail!(
                    "input {} has invalid pbkeylen {} (expected 16, 24, or 32)",
                    input.id,
                    len
                );
            }
        }
    }

    let bind: SocketAddr = cfg
        .control
        .bind
        .parse()
        .with_context(|| format!("invalid control bind address: {}", cfg.control.bind))?;
    if !bind.ip().is_loopback() && cfg.control.token.is_none() {
        bail!(
            "control API bound to {} without a token — set control.token or bind to loopback",
            bind
        );
    }

    if cfg.open_live.register {
        if cfg.open_live.url.is_none() {
            bail!("open_live.register is on but open_live.url is unset");
        }
        // Only osc mode needs a credential: a self-hosted Open Live with API_KEY
        // unset leaves /api/v1 open, which is the normal local development case.
        if cfg.open_live.auth_mode == "osc"
            && cfg
                .open_live
                .api_key
                .as_deref()
                .is_none_or(|k| k.trim().is_empty())
        {
            bail!("open_live.auth_mode is \"osc\" but open_live.api_key (the OSC PAT) is unset");
        }
        if !matches!(cfg.open_live.auth_mode.as_str(), "direct" | "osc") {
            bail!(
                "open_live.auth_mode must be \"direct\" or \"osc\", got {:?}",
                cfg.open_live.auth_mode
            );
        }
    }

    Ok(())
}

pub fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example must stay loadable — it is the first thing an operator
    /// copies, and a stale key or a renamed section makes the daemon fail at startup.
    #[test]
    fn shipped_example_config_parses_and_validates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gateway.toml.example");
        let raw = std::fs::read_to_string(&path).expect("example config is missing");
        let cfg: GatewayConfig = toml::from_str(&raw).expect("example config does not parse");

        validate(&cfg).expect("example config does not validate");
        assert_eq!(cfg.inputs.len(), 1);
        assert_eq!(cfg.inputs[0].uplink.port, 9000);
    }

    fn config_from(toml_str: &str) -> GatewayConfig {
        toml::from_str(toml_str).expect("test config does not parse")
    }

    const BASE: &str = r#"
[gateway]
name = "Venue A"

[strom]
url = "http://127.0.0.1:8080"

[open_live]
register = false

[[inputs]]
id = "cam1"

  [inputs.capture]
  kind = "test"

  [inputs.uplink]
  host = "strom.example.com"
  port = 9000
"#;

    #[test]
    fn base_config_is_valid() {
        validate(&config_from(BASE)).expect("base config should validate");
    }

    /// Two inputs on one box sharing a port would have Strom bind once and silently
    /// drop the second feed, so this has to fail loudly at startup.
    #[test]
    fn duplicate_srt_ports_are_rejected() {
        let cfg = config_from(&format!(
            "{BASE}
[[inputs]]
id = \"cam2\"

  [inputs.capture]
  kind = \"test\"

  [inputs.uplink]
  host = \"strom.example.com\"
  port = 9000
"
        ));

        let err = validate(&cfg).expect_err("duplicate ports must be rejected");
        assert!(
            err.to_string().contains("9000"),
            "error should name the port: {err}"
        );
    }

    #[test]
    fn duplicate_input_ids_are_rejected() {
        let cfg = config_from(&format!(
            "{BASE}
[[inputs]]
id = \"cam1\"

  [inputs.capture]
  kind = \"test\"

  [inputs.uplink]
  host = \"strom.example.com\"
  port = 9001
"
        ));

        assert!(
            validate(&cfg).is_err(),
            "duplicate input ids must be rejected"
        );
    }

    /// An unauthenticated control API on a routable interface would let anyone on the
    /// venue network stop the feed, so refuse to start rather than warn.
    #[test]
    fn non_loopback_control_bind_requires_a_token() {
        let mut cfg = config_from(BASE);
        cfg.control.bind = "0.0.0.0:9001".to_string();
        assert!(
            validate(&cfg).is_err(),
            "non-loopback bind without a token must be rejected"
        );

        cfg.control.token = Some("secret".to_string());
        validate(&cfg).expect("non-loopback bind with a token should validate");
    }

    #[test]
    fn registration_requires_a_url() {
        let mut cfg = config_from(BASE);
        cfg.open_live.register = true;
        assert!(
            validate(&cfg).is_err(),
            "registration without a URL must be rejected"
        );

        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        validate(&cfg).expect("direct mode needs no key");
    }

    /// A local Open Live with API_KEY unset needs no credential, but OSC's proxy
    /// always does, and failing at startup beats every request 401ing.
    #[test]
    fn osc_mode_requires_a_pat() {
        let mut cfg = config_from(BASE);
        cfg.open_live.register = true;
        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        cfg.open_live.auth_mode = "osc".to_string();
        assert!(
            validate(&cfg).is_err(),
            "osc mode without a PAT must be rejected"
        );

        cfg.open_live.api_key = Some("pat".to_string());
        validate(&cfg).expect("osc mode with a PAT should validate");
    }

    /// The cloud cannot dial a venue whose address it does not know, and failing at
    /// startup beats a production that silently never receives anything.
    #[test]
    fn cloud_dialling_modes_require_a_public_host() {
        for mode in [UplinkMode::Listener, UplinkMode::Rendezvous] {
            let mut cfg = config_from(BASE);
            cfg.inputs[0].uplink.mode = mode;
            cfg.inputs[0].uplink.public_host = None;
            assert!(
                validate(&cfg).is_err(),
                "{mode:?} without a public_host must be rejected"
            );

            cfg.inputs[0].uplink.public_host = Some("venue.example.com".to_string());
            validate(&cfg).expect("with a public_host it should validate");
        }
    }

    #[test]
    fn caller_mode_needs_no_public_host() {
        let mut cfg = config_from(BASE);
        cfg.inputs[0].uplink.mode = UplinkMode::Caller;
        cfg.inputs[0].uplink.public_host = None;
        validate(&cfg).expect("caller mode should validate without a public_host");
    }

    #[test]
    fn invalid_pbkeylen_is_rejected() {
        let mut cfg = config_from(BASE);
        cfg.inputs[0].uplink.pbkeylen = Some(20);
        assert!(validate(&cfg).is_err(), "pbkeylen must be 16, 24, or 32");

        cfg.inputs[0].uplink.pbkeylen = Some(32);
        validate(&cfg).expect("pbkeylen 32 should validate");
    }

    #[test]
    fn strom_url_must_be_an_http_url() {
        let mut cfg = config_from(BASE);
        cfg.strom.url = "127.0.0.1:8080".to_string();
        assert!(
            validate(&cfg).is_err(),
            "a scheme-less Strom URL must be rejected"
        );

        cfg.strom.url = String::new();
        assert!(
            validate(&cfg).is_err(),
            "an empty Strom URL must be rejected"
        );
    }

    /// The desktop app creates its inputs at runtime, so an empty list is valid for
    /// the shared validation. The headless binary applies its own stricter check.
    #[test]
    fn an_empty_input_list_is_allowed_by_shared_validation() {
        let mut cfg = config_from(BASE);
        cfg.inputs.clear();
        validate(&cfg).expect("an empty input list is for the app to fill at runtime");
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use open_live_gateway_types::config::UplinkMode;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("olg-test-{}-{name}", std::process::id()))
            .join("gateway.toml")
    }

    fn sample() -> GatewayConfig {
        let mut cfg = GatewayConfig::default();
        cfg.gateway.name = "Venue A".to_string();
        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        cfg.open_live.api_key = Some("a-secret-token".to_string());
        cfg.open_live.auth_mode = "osc".to_string();
        cfg.app.uplink.mode = UplinkMode::Caller;
        cfg.app.uplink.port_range = "9400-9500".to_string();
        cfg.app.video.bitrate_kbps = 7000;
        cfg
    }

    /// The app is the only editor, so what it writes has to load back identically —
    /// otherwise settings quietly change every time the window is opened.
    #[test]
    fn a_saved_config_loads_back_unchanged() {
        let path = temp_path("roundtrip");
        let cfg = sample();

        save(&path, &cfg).expect("saves");
        let loaded = load_or_default(&path).expect("loads");

        assert_eq!(loaded.gateway.name, "Venue A");
        assert_eq!(loaded.open_live.url, cfg.open_live.url);
        assert_eq!(loaded.open_live.api_key, cfg.open_live.api_key);
        assert_eq!(loaded.open_live.auth_mode, "osc");
        assert_eq!(loaded.app.uplink.port_range, "9400-9500");
        assert_eq!(loaded.app.video.bitrate_kbps, 7000);
        assert_eq!(loaded.strom.url, cfg.strom.url);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// First run must not be an error: the window opens with empty settings instead
    /// of the app refusing to start.
    #[test]
    fn a_missing_file_yields_defaults_rather_than_an_error() {
        let path = temp_path("missing");
        let cfg = load_or_default(&path).expect("a missing file is not an error");
        assert!(cfg.open_live.url.is_none());
        assert_eq!(cfg.strom.url, "http://127.0.0.1:8080");
        assert!(!path.exists(), "loading must not create the file");
    }

    /// The file holds the Open Live credential, so it must not be world-readable.
    #[cfg(unix)]
    #[test]
    fn a_saved_config_is_not_readable_by_others() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("perms");
        save(&path, &sample()).expect("saves");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o077, 0, "group and other must have no access");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Saving an invalid config would leave the app unable to start next time.
    #[test]
    fn an_invalid_config_is_refused_before_it_reaches_disk() {
        let path = temp_path("invalid");
        let mut cfg = sample();
        cfg.strom.url = "not-a-url".to_string();

        assert!(save(&path, &cfg).is_err());
        assert!(!path.exists(), "nothing should have been written");
    }
}
