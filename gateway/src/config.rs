//! Configuration loading: TOML file, then environment overrides, then CLI flags
//! (precedence CLI > env > file), followed by validation.

use anyhow::{bail, Context, Result};
use open_live_gateway_types::GatewayConfig;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;

/// Environment overrides applied on top of the file. Deliberately a short list —
/// the values a deployment tool needs to inject without templating a whole file.
const ENV_OPEN_LIVE_URL: &str = "OLG_OPEN_LIVE_URL";
const ENV_OPEN_LIVE_API_KEY: &str = "OLG_OPEN_LIVE_API_KEY";
const ENV_CONTROL_BIND: &str = "OLG_CONTROL_BIND";
const ENV_CONTROL_TOKEN: &str = "OLG_CONTROL_TOKEN";
const ENV_STROM_URL: &str = "OLG_STROM_URL";
const ENV_STROM_API_KEY: &str = "OLG_STROM_API_KEY";

pub fn load(path: &Path, log_level_override: Option<&str>) -> Result<GatewayConfig> {
    let raw = std::fs::read_to_string(path).context("reading config file")?;
    let mut cfg: GatewayConfig = toml::from_str(&raw).context("parsing config TOML")?;

    if let Ok(url) = std::env::var(ENV_OPEN_LIVE_URL) {
        cfg.open_live.url = Some(url);
    }
    if let Ok(key) = std::env::var(ENV_OPEN_LIVE_API_KEY) {
        cfg.open_live.api_key = Some(key);
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
    if let Some(level) = log_level_override {
        cfg.log.level = level.to_string();
    }

    validate(&cfg)?;
    Ok(cfg)
}

/// Rejects configurations that would fail confusingly at runtime.
fn validate(cfg: &GatewayConfig) -> Result<()> {
    if cfg.inputs.is_empty() {
        bail!("no inputs configured");
    }

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
        if cfg.open_live.api_key.is_none() {
            bail!("open_live.register is on but open_live.api_key is unset");
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
    fn registration_requires_url_and_api_key() {
        let mut cfg = config_from(BASE);
        cfg.open_live.register = true;
        assert!(
            validate(&cfg).is_err(),
            "registration without a URL must be rejected"
        );

        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        assert!(
            validate(&cfg).is_err(),
            "registration without an API key must be rejected"
        );

        cfg.open_live.api_key = Some("key".to_string());
        validate(&cfg).expect("registration with URL and key should validate");
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

    #[test]
    fn empty_input_list_is_rejected() {
        let mut cfg = config_from(BASE);
        cfg.inputs.clear();
        assert!(
            validate(&cfg).is_err(),
            "a gateway with no inputs must be rejected"
        );
    }
}
