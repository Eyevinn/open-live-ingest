//! Configuration: one TOML file per user, overlaid by a few environment variables.
//!
//! Section and property names mirror the Strom blocks they end up on, so a value in
//! the file can be traced to the block it configures.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

const ENV_OPEN_LIVE_URL: &str = "OLG_OPEN_LIVE_URL";
const ENV_OPEN_LIVE_API_KEY: &str = "OLG_OPEN_LIVE_API_KEY";
const ENV_OPEN_LIVE_AUTH_MODE: &str = "OLG_OPEN_LIVE_AUTH_MODE";
const ENV_STROM_URL: &str = "OLG_STROM_URL";
const ENV_STROM_API_KEY: &str = "OLG_STROM_API_KEY";

// Unknown keys are an error rather than silently ignored: a file in an older layout
// would otherwise load as all defaults and stream to the wrong ports.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub gateway: Gateway,
    pub strom: Strom,
    pub open_live: OpenLive,
    pub uplink: Uplink,
    pub video: Video,
    pub capture: Capture,
    pub log: Log,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Gateway {
    /// Stable identity for this machine. It seeds the flow ids, so changing it hides
    /// existing flows from `down` and `status`. Defaults to the hostname.
    pub id: Option<String>,
    /// Prefix of every source name in Open Live, so two venues' cameras can be told
    /// apart in Studio.
    pub name: String,
}

impl Gateway {
    pub fn resolved_id(&self) -> String {
        match self
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => id.to_string(),
            None => hostname().unwrap_or_else(|| "open-live-gateway".to_string()),
        }
    }
}

pub fn hostname() -> Option<String> {
    hostname::get().ok().and_then(|h| h.into_string().ok())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Strom {
    /// The Strom on this machine, which does the capturing and encoding. Not the
    /// cloud Strom, and not something Open Live knows about.
    pub url: String,
    /// Set when that Strom runs with `STROM_API_KEY`.
    pub api_key: Option<String>,
    /// Start a headless Strom when nothing is listening at `url`, and stop it again
    /// on the way out. A Strom already running is always adopted, never replaced or
    /// stopped: it may be a service this box depends on, or someone else's.
    pub manage: bool,
    /// The Strom executable: a name to find on `PATH`, or a path.
    pub binary: String,
    /// Where a managed Strom keeps its flows. Defaults beside our own settings, so it
    /// never disturbs an existing install.
    pub data_dir: Option<String>,
}

impl Default for Strom {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8080".to_string(),
            api_key: None,
            manage: true,
            binary: "strom".to_string(),
            data_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenLive {
    pub url: Option<String>,
    pub auth_mode: AuthMode,
    /// Bearer token or OSC personal access token, depending on `auth_mode`. In osc
    /// mode it can stay unset, and the login saved by `npx @osaas/cli login` is used.
    pub api_key: Option<String>,
    /// Register a source per input. Off for a box that only streams to a Strom.
    pub register: bool,
}

impl Default for OpenLive {
    fn default() -> Self {
        Self {
            url: None,
            auth_mode: AuthMode::Direct,
            api_key: None,
            register: true,
        }
    }
}

/// How the Open Live credential is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Sent as the bearer token as-is: a self-hosted Open Live with a static `API_KEY`,
    /// or none at all.
    #[default]
    Direct,
    /// An OSC personal access token, exchanged for a short-lived service token. The
    /// proxy in front of an OSC-hosted instance rejects a PAT presented directly.
    Osc,
}

impl std::str::FromStr for AuthMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "direct" => Ok(AuthMode::Direct),
            "osc" => Ok(AuthMode::Osc),
            other => bail!("auth_mode must be \"direct\" or \"osc\", got {other:?}"),
        }
    }
}

/// The SRT link template. A port is allocated per input from `port_range`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Uplink {
    pub mode: UplinkMode,
    /// The cloud Strom to send to. Discovered from Open Live when unset.
    pub host: Option<String>,
    /// This machine's address as the cloud sees it. Needed in listener mode, where
    /// the cloud dials the venue.
    pub public_host: Option<String>,
    /// Inclusive `first-last` range, e.g. "47110-47129".
    pub port_range: String,
    /// Rule of thumb: 3-4x the measured RTT.
    pub latency_ms: u32,
    pub passphrase: Option<String>,
    /// AES key length in bytes: 16, 24, or 32.
    pub pbkeylen: Option<u32>,
}

impl Default for Uplink {
    fn default() -> Self {
        Self {
            mode: UplinkMode::Caller,
            host: None,
            public_host: None,
            port_range: "47110-47129".to_string(),
            latency_ms: 200,
            passphrase: None,
            pbkeylen: None,
        }
    }
}

/// Which end of the SRT link dials. A deployment constraint, not a preference:
/// whichever end listens needs an inbound UDP port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UplinkMode {
    /// The venue dials the cloud. Nothing inbound at the venue.
    #[default]
    Caller,
    /// The cloud dials the venue. Matches the convention Open Live's own seeded
    /// sources use, at the cost of a public address or port forward at the venue.
    Listener,
}

impl Uplink {
    pub fn ports(&self) -> Result<RangeInclusive<u16>> {
        let (first, last) = self.port_range.split_once('-').with_context(|| {
            format!(
                "port_range must be \"first-last\", got {:?}",
                self.port_range
            )
        })?;
        let first: u16 = first
            .trim()
            .parse()
            .with_context(|| format!("invalid first port in {:?}", self.port_range))?;
        let last: u16 = last
            .trim()
            .parse()
            .with_context(|| format!("invalid last port in {:?}", self.port_range))?;
        if first == 0 || last < first {
            bail!("empty port range {:?}", self.port_range);
        }
        Ok(first..=last)
    }

    /// One concrete link, once a cloud host and a port are known.
    pub fn endpoint(&self, cloud_host: &str, port: u16) -> Endpoint {
        Endpoint {
            mode: self.mode,
            cloud_host: cloud_host.to_string(),
            public_host: self.public_host.clone().unwrap_or_default(),
            port,
            latency_ms: self.latency_ms,
            passphrase: self.passphrase.clone().filter(|p| !p.is_empty()),
            pbkeylen: self.pbkeylen,
        }
    }
}

/// One SRT link, described by the two URIs that mirror each other: the one on the
/// venue Strom's output block, and the one registered on the Open Live source for
/// the cloud Strom's input block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub mode: UplinkMode,
    pub cloud_host: String,
    pub public_host: String,
    pub port: u16,
    pub latency_ms: u32,
    passphrase: Option<String>,
    pbkeylen: Option<u32>,
}

impl Endpoint {
    /// For `builtin.mpegtssrt_output` on the venue Strom.
    pub fn venue_uri(&self) -> String {
        let mut uri = match self.mode {
            UplinkMode::Caller => format!(
                "srt://{}:{}?mode=caller&latency={}",
                self.cloud_host, self.port, self.latency_ms
            ),
            UplinkMode::Listener => {
                format!(
                    "srt://:{}?mode=listener&latency={}",
                    self.port, self.latency_ms
                )
            }
        };
        uri.push_str(&self.crypto());
        uri
    }

    /// Registered on the Open Live source, which becomes the cloud Strom's `srt_uri`.
    /// `srtsrc` defaults to caller mode, so the mode is always explicit here.
    pub fn cloud_uri(&self) -> String {
        let mut uri = match self.mode {
            UplinkMode::Caller => format!("srt://:{}?mode=listener", self.port),
            UplinkMode::Listener => format!("srt://{}:{}?mode=caller", self.public_host, self.port),
        };
        uri.push_str(&self.crypto());
        uri
    }

    fn crypto(&self) -> String {
        let Some(passphrase) = &self.passphrase else {
            return String::new();
        };
        match self.pbkeylen {
            Some(len) => format!("&passphrase={passphrase}&pbkeylen={len}"),
            None => format!("&passphrase={passphrase}"),
        }
    }
}

/// Maps to `builtin.videoenc`. Strom picks the encoder element itself, hardware
/// first, so this expresses intent rather than an element name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Video {
    /// `h264`, `h265`, `av1`, `vp9`.
    pub codec: String,
    pub bitrate_kbps: u32,
    /// `auto`, `hardware`, or `software`.
    pub encoder_preference: String,
    /// `ultrafast`, `fast`, `medium`, `slow`, `veryslow`.
    pub quality_preset: String,
    pub tune: String,
    /// `cbr`, `vbr`, or `cqp`.
    pub rate_control: String,
    /// In frames. One or two seconds' worth lets a reconnecting receiver lock on.
    pub keyframe_interval: u32,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            codec: "h264".to_string(),
            bitrate_kbps: 6000,
            encoder_preference: "auto".to_string(),
            quality_preset: "ultrafast".to_string(),
            tune: "zerolatency".to_string(),
            rate_control: "cbr".to_string(),
            keyframe_interval: 25,
        }
    }
}

/// Capture format to request. Blank takes whatever each device offers, which is the
/// right answer for most devices: `builtin.local_input` cannot scale or re-time, so
/// asking for a format a device does not advertise fails negotiation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Capture {
    /// `WxH`, e.g. `1280x720`.
    pub video_resolution: Option<String>,
    /// `N/D`, e.g. `25/1`.
    pub video_framerate: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub level: String,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// Where per-user files live. Per-user because this is run by an operator, not root.
pub fn user_dir() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("open-live-gateway")
}

pub fn default_path() -> PathBuf {
    user_dir().join("gateway.toml")
}

/// The pid of a running `up`, so `down` can ask it to stop.
pub fn pidfile() -> PathBuf {
    user_dir().join("gateway.pid")
}

/// The pid of a Strom the gateway started, so `down` can stop it after a hard kill.
/// These two pidfiles are the only local state.
pub fn strom_pidfile() -> PathBuf {
    user_dir().join("strom.pid")
}

/// Loads the file, or defaults when it does not exist yet: first run is not an error,
/// it is what setup is for.
pub fn load_or_default(path: &Path) -> Result<Config> {
    let cfg = match std::fs::read_to_string(path) {
        Ok(raw) => toml::from_str(&raw).context("parsing the settings file")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(err) => return Err(err).context("reading the settings file"),
    };
    with_env_overrides(cfg)
}

/// The handful of values a deployment tool needs to inject without templating a file.
fn with_env_overrides(mut cfg: Config) -> Result<Config> {
    if let Ok(url) = std::env::var(ENV_OPEN_LIVE_URL) {
        cfg.open_live.url = Some(url);
    }
    if let Ok(key) = std::env::var(ENV_OPEN_LIVE_API_KEY) {
        cfg.open_live.api_key = Some(key);
    }
    if let Ok(mode) = std::env::var(ENV_OPEN_LIVE_AUTH_MODE) {
        cfg.open_live.auth_mode = mode.parse().context(ENV_OPEN_LIVE_AUTH_MODE)?;
    }
    if let Ok(url) = std::env::var(ENV_STROM_URL) {
        cfg.strom.url = url;
    }
    if let Ok(key) = std::env::var(ENV_STROM_API_KEY) {
        cfg.strom.api_key = Some(key);
    }
    Ok(cfg)
}

/// Writes the file: validated first, via a temporary file and a rename so an
/// interrupted save cannot leave half a file, and mode 0600 because it holds the
/// Open Live credential and any SRT passphrase.
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    validate(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the settings directory")?;
    }
    let toml = toml::to_string_pretty(cfg).context("serialising settings")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml).context("writing settings")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .context("restricting settings permissions")?;
    }
    std::fs::rename(&tmp, path).context("replacing the settings file")?;
    Ok(())
}

/// Rejects settings that would otherwise fail confusingly at runtime.
pub fn validate(cfg: &Config) -> Result<()> {
    validate_with(cfg, crate::osc::saved_token().is_some())
}

/// `osc_login` says whether the OSC CLI has a saved login, which stands in for
/// `open_live.api_key` in osc mode. Passed in so the check itself stays pure.
fn validate_with(cfg: &Config, osc_login: bool) -> Result<()> {
    if !cfg.strom.url.starts_with("http://") && !cfg.strom.url.starts_with("https://") {
        bail!(
            "strom.url must be an http or https URL, got {:?}",
            cfg.strom.url
        );
    }

    cfg.uplink.ports()?;
    if cfg.uplink.mode == UplinkMode::Listener
        && cfg
            .uplink
            .public_host
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        bail!(
            "uplink.mode is \"listener\", where the cloud dials this machine, so \
             uplink.public_host must be its address as the cloud sees it"
        );
    }
    if let Some(len) = cfg.uplink.pbkeylen {
        if !matches!(len, 16 | 24 | 32) {
            bail!("uplink.pbkeylen must be 16, 24, or 32, got {len}");
        }
    }
    if let Some(passphrase) = cfg.uplink.passphrase.as_deref().filter(|p| !p.is_empty()) {
        // libsrt's own limits, and the value travels unescaped inside two URIs.
        let n = passphrase.chars().count();
        if !(10..=79).contains(&n) {
            bail!("uplink.passphrase must be 10 to 79 characters, got {n}");
        }
        if !passphrase
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.~!*".contains(c))
        {
            bail!("uplink.passphrase may only contain letters, digits, and - _ . ~ ! *");
        }
    }

    if let Some(res) = cfg
        .capture
        .video_resolution
        .as_deref()
        .filter(|r| !r.is_empty())
    {
        parse_resolution(res)?;
    }
    if let Some(rate) = cfg
        .capture
        .video_framerate
        .as_deref()
        .filter(|r| !r.is_empty())
    {
        let ok = rate.split_once('/').is_some_and(|(n, d)| {
            n.parse::<u32>().is_ok() && d.parse::<u32>().is_ok_and(|d| d > 0)
        });
        if !ok {
            bail!("capture.video_framerate must be N/D, e.g. 25/1, got {rate:?}");
        }
    }

    if cfg.open_live.register {
        if cfg
            .open_live
            .url
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            bail!("open_live.register is on but open_live.url is unset");
        }
        // Only osc mode needs a credential: a self-hosted Open Live with API_KEY
        // unset leaves /api/v1 open, which is the normal local development case.
        // The OSC CLI's saved login serves in place of a token in the file.
        if cfg.open_live.auth_mode == AuthMode::Osc
            && !osc_login
            && cfg
                .open_live
                .api_key
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
        {
            bail!(
                "open_live.auth_mode is \"osc\" but there is no OSC token: log in with `{}`, or set open_live.api_key",
                crate::osc::LOGIN_COMMAND
            );
        }
    }
    Ok(())
}

/// Splits a `WxH` string.
pub fn parse_resolution(resolution: &str) -> Result<(u32, u32)> {
    let (w, h) = resolution
        .split_once(['x', 'X'])
        .with_context(|| format!("resolution must be WxH, got {resolution:?}"))?;
    let w = w
        .trim()
        .parse()
        .with_context(|| format!("bad width in {resolution:?}"))?;
    let h = h
        .trim()
        .parse()
        .with_context(|| format!("bad height in {resolution:?}"))?;
    Ok((w, h))
}

/// Replaces a passphrase value in a URI with the same mask Open Live applies on
/// read. Used both to compare addresses and to keep the passphrase off the terminal.
pub fn mask_passphrase(uri: &str) -> String {
    let lower = uri.to_ascii_lowercase();
    let Some(at) = lower.find("passphrase=") else {
        return uri.to_string();
    };
    let value_start = at + "passphrase=".len();
    let value_end = uri[value_start..]
        .find('&')
        .map(|i| value_start + i)
        .unwrap_or(uri.len());
    format!("{}***{}", &uri[..value_start], &uri[value_end..])
}

pub fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uplink(mode: UplinkMode) -> Uplink {
        Uplink {
            mode,
            public_host: Some("venue.example.com".to_string()),
            ..Uplink::default()
        }
    }

    /// Venue dials out: nothing inbound at the venue, the cloud publishes the port.
    #[test]
    fn caller_mode_has_the_venue_dial_and_the_cloud_bind() {
        let e = uplink(UplinkMode::Caller).endpoint("strom.example.com", 9000);
        assert_eq!(
            e.venue_uri(),
            "srt://strom.example.com:9000?mode=caller&latency=200"
        );
        assert_eq!(e.cloud_uri(), "srt://:9000?mode=listener");
    }

    /// Cloud dials in: the direction Open Live's own seeded demo sources use.
    #[test]
    fn listener_mode_has_the_cloud_dial_and_the_venue_bind() {
        let e = uplink(UplinkMode::Listener).endpoint("strom.example.com", 9000);
        assert_eq!(e.venue_uri(), "srt://:9000?mode=listener&latency=200");
        assert_eq!(e.cloud_uri(), "srt://venue.example.com:9000?mode=caller");
    }

    #[test]
    fn both_uris_carry_the_passphrase_so_the_ends_agree() {
        let mut u = uplink(UplinkMode::Caller);
        u.passphrase = Some("s3cret-s3cret".to_string());
        u.pbkeylen = Some(16);
        let e = u.endpoint("strom.example.com", 9000);
        assert!(e
            .venue_uri()
            .ends_with("&passphrase=s3cret-s3cret&pbkeylen=16"));
        assert!(e
            .cloud_uri()
            .ends_with("&passphrase=s3cret-s3cret&pbkeylen=16"));
    }

    #[test]
    fn an_empty_passphrase_is_not_sent() {
        let mut u = uplink(UplinkMode::Caller);
        u.passphrase = Some(String::new());
        let e = u.endpoint("h", 9000);
        assert!(!e.venue_uri().contains("passphrase"));
        assert!(!e.cloud_uri().contains("passphrase"));
    }

    #[test]
    fn masking_covers_the_value_and_nothing_else() {
        assert_eq!(
            mask_passphrase("srt://:9000?mode=listener&passphrase=abc&pbkeylen=16"),
            "srt://:9000?mode=listener&passphrase=***&pbkeylen=16"
        );
        assert_eq!(
            mask_passphrase("srt://:9000?passphrase=abc"),
            "srt://:9000?passphrase=***"
        );
        assert_eq!(
            mask_passphrase("srt://:9000?mode=listener"),
            "srt://:9000?mode=listener"
        );
    }

    fn valid() -> Config {
        let mut cfg = Config::default();
        cfg.gateway.name = "Venue A".to_string();
        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        cfg
    }

    #[test]
    fn the_defaults_plus_an_open_live_url_validate() {
        validate(&valid()).expect("should validate");
    }

    /// The shipped example is the first thing an operator copies, so it must load.
    #[test]
    fn the_shipped_example_parses_and_validates() {
        let raw =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/gateway.toml.example"))
                .expect("example config is missing");
        let cfg: Config = toml::from_str(&raw).expect("example config does not parse");
        assert!(
            cfg.open_live.api_key.is_none(),
            "the example shows the OSC CLI login path, with no token in the file"
        );
        validate_with(&cfg, true).expect("example config does not validate");
    }

    #[test]
    fn registration_requires_a_url_but_direct_mode_needs_no_key() {
        let mut cfg = valid();
        cfg.open_live.url = None;
        assert!(validate(&cfg).is_err());
        cfg.open_live.register = false;
        validate(&cfg).expect("registration off needs no url");
    }

    /// OSC's proxy always wants a credential, and failing at startup beats every
    /// request answering 401. The OSC CLI's login counts as one.
    #[test]
    fn osc_mode_requires_a_token_or_an_osc_cli_login() {
        let mut cfg = valid();
        cfg.open_live.auth_mode = AuthMode::Osc;
        assert!(validate_with(&cfg, false).is_err());
        validate_with(&cfg, true).expect("osc with a CLI login should validate");
        cfg.open_live.api_key = Some("pat".to_string());
        validate_with(&cfg, false).expect("osc with a token should validate");
    }

    /// The cloud cannot dial a venue whose address it does not know.
    #[test]
    fn listener_mode_requires_a_public_host() {
        let mut cfg = valid();
        cfg.uplink.mode = UplinkMode::Listener;
        assert!(validate(&cfg).is_err());
        cfg.uplink.public_host = Some("venue.example.com".to_string());
        validate(&cfg).expect("with a public host it should validate");
    }

    /// libsrt rejects short passphrases at runtime with an opaque error, and a `&`
    /// inside one would split the URI.
    #[test]
    fn passphrases_are_checked_for_length_and_characters() {
        let mut cfg = valid();
        cfg.uplink.passphrase = Some("short".to_string());
        assert!(validate(&cfg).is_err());
        cfg.uplink.passphrase = Some("long-enough&bad".to_string());
        assert!(validate(&cfg).is_err());
        cfg.uplink.passphrase = Some("long-enough-fine".to_string());
        validate(&cfg).expect("a plain passphrase should validate");
        cfg.uplink.pbkeylen = Some(20);
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn capture_format_strings_are_checked() {
        let mut cfg = valid();
        cfg.capture.video_resolution = Some("1080p".to_string());
        assert!(validate(&cfg).is_err());
        cfg.capture.video_resolution = Some("1920x1080".to_string());
        cfg.capture.video_framerate = Some("25".to_string());
        assert!(validate(&cfg).is_err());
        cfg.capture.video_framerate = Some("25/1".to_string());
        validate(&cfg).expect("a well-formed format should validate");
    }

    #[test]
    fn malformed_port_ranges_are_rejected() {
        for bad in ["9000", "9100-9000", "", "abc-def", "0-10"] {
            let u = Uplink {
                port_range: bad.to_string(),
                ..Uplink::default()
            };
            assert!(u.ports().is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(Uplink::default().ports().unwrap(), 47110..=47129);
    }

    #[test]
    fn a_scheme_less_strom_url_is_rejected() {
        let mut cfg = valid();
        cfg.strom.url = "127.0.0.1:8080".to_string();
        assert!(validate(&cfg).is_err());
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("olg-test-{}-{name}", std::process::id()))
            .join("gateway.toml")
    }

    /// Setup is the only editor, so what it writes has to load back identically.
    #[test]
    fn a_saved_config_loads_back_unchanged() {
        let path = temp_path("roundtrip");
        let mut cfg = valid();
        cfg.open_live.auth_mode = AuthMode::Osc;
        cfg.open_live.api_key = Some("a-secret-token".to_string());
        cfg.uplink.port_range = "9400-9500".to_string();
        cfg.video.bitrate_kbps = 7000;

        save(&path, &cfg).expect("saves");
        let loaded = load_or_default(&path).expect("loads");
        assert_eq!(loaded.gateway.name, "Venue A");
        assert_eq!(loaded.open_live.api_key, cfg.open_live.api_key);
        assert_eq!(loaded.open_live.auth_mode, AuthMode::Osc);
        assert_eq!(loaded.uplink.port_range, "9400-9500");
        assert_eq!(loaded.video.bitrate_kbps, 7000);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group and other must have no access");
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_missing_file_yields_defaults_and_is_not_created() {
        let path = temp_path("missing");
        let cfg = load_or_default(&path).expect("a missing file is not an error");
        assert!(cfg.open_live.url.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn an_invalid_config_is_refused_before_it_reaches_disk() {
        let path = temp_path("invalid");
        let mut cfg = valid();
        cfg.strom.url = "not-a-url".to_string();
        assert!(save(&path, &cfg).is_err());
        assert!(!path.exists());
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// An older layout must fail loudly, naming the key, rather than load as
    /// defaults and quietly stream to the wrong ports.
    #[test]
    fn an_unknown_section_or_key_is_an_error_that_names_it() {
        let err = toml::from_str::<Config>("[app.uplink]\nport_range = \"47110-47129\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("app"), "{err}");

        let err = toml::from_str::<Config>("[uplink]\nport_rnage = \"47110-47129\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("port_rnage"), "{err}");
    }
}
