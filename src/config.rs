//! Configuration: one TOML file per user, overlaid by a few environment variables.
//!
//! Section and property names mirror the Strom blocks they end up on, so a value in
//! the file can be traced to the block it configures.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

const ENV_OPEN_LIVE_URL: &str = "OLI_OPEN_LIVE_URL";
const ENV_OPEN_LIVE_API_KEY: &str = "OLI_OPEN_LIVE_API_KEY";
const ENV_OPEN_LIVE_AUTH_MODE: &str = "OLI_OPEN_LIVE_AUTH_MODE";
const ENV_OPEN_LIVE_GATEWAY_ID: &str = "OLI_OPEN_LIVE_GATEWAY_ID";
const ENV_OPEN_LIVE_GATEWAY_TOKEN: &str = "OLI_OPEN_LIVE_GATEWAY_TOKEN";
const ENV_STROM_URL: &str = "OLI_STROM_URL";
const ENV_STROM_API_KEY: &str = "OLI_STROM_API_KEY";

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
    /// `[[input]]`: settings for one input, by device.
    #[serde(rename = "input", skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputSettings>,
}

/// One `[[input]]` entry. Today it pairs an audio source with a camera; `up --audio`
/// replaces these for that run.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputSettings {
    /// The camera, by Strom device id or name fragment, as `--devices` takes it.
    pub device: String,
    /// An audio source to send with it, by Strom device id or name fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
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
            None => hostname().unwrap_or_else(|| "open-live-ingest".to_string()),
        }
    }
}

pub fn hostname() -> Option<String> {
    hostname::get().ok().and_then(|h| h.into_string().ok())
}

/// `strom.binary` when it is not set. Only this value looks next to the gateway first.
pub const DEFAULT_STROM_BINARY: &str = "strom";

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
    /// The Strom executable: a name to find on `PATH`, or a path. Left at the default,
    /// a `strom` next to the running gateway is used first. It gets the bundle's
    /// GStreamer only inside the macOS app bundle (see `local_strom::BUNDLE_PLUGIN_DIR`).
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
            binary: DEFAULT_STROM_BINARY.to_string(),
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
    /// This gateway's id in Open Live, from `POST /api/v1/gateways`. With its token
    /// it turns on the status heartbeat: an outbound WebSocket carrying what `status`
    /// reports, so a Studio operator sees the venue without shelling into it.
    pub gateway_id: Option<String>,
    /// The token Open Live returned once when the gateway was created. It opens the
    /// heartbeat socket and nothing else; the API key is not accepted there.
    pub gateway_token: Option<String>,
}

impl Default for OpenLive {
    fn default() -> Self {
        Self {
            url: None,
            auth_mode: AuthMode::Direct,
            api_key: None,
            register: true,
            gateway_id: None,
            gateway_token: None,
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

/// The SRT link template. A port is allocated per input from a range: the one Open
/// Live publishes for its Strom in caller mode, or `port_range` otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Uplink {
    pub mode: UplinkMode,
    /// The cloud Strom to send to. Discovered from Open Live when unset.
    pub host: Option<String>,
    /// This machine's address as the cloud sees it. Needed in listener mode, where
    /// the cloud dials the venue.
    pub public_host: Option<String>,
    /// Inclusive `first-last` range, e.g. "47110-47129". In listener mode these are
    /// this machine's own ports and the range is required. In caller mode the ports
    /// belong to the cloud Strom, so the range Open Live publishes is used and this
    /// is only a fallback for an Open Live that publishes none.
    pub port_range: Option<String>,
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
            port_range: None,
            latency_ms: 200,
            passphrase: None,
            pbkeylen: None,
        }
    }
}

/// Which end of the SRT link dials. A deployment constraint, not a preference:
/// whichever end listens needs an inbound UDP port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum UplinkMode {
    /// The venue dials the cloud. Nothing inbound at the venue.
    #[default]
    Caller,
    /// The cloud dials the venue. Matches the convention Open Live's own seeded
    /// sources use, at the cost of a public address or port forward at the venue.
    Listener,
}

/// The range suggested when setup has to ask for one.
pub const SUGGESTED_PORT_RANGE: &str = "47110-47129";

/// An inclusive port range from its two ends, as read from the settings or from Open
/// Live's server info. One check for both, so a range is valid by the same rules
/// whichever side it came from.
pub fn port_range(first: u64, last: u64) -> Result<RangeInclusive<u16>> {
    let port = |n: u64, which: &str| -> Result<u16> {
        u16::try_from(n)
            .ok()
            .filter(|p| *p != 0)
            .with_context(|| format!("{which} port {n} is not in 1-65535"))
    };
    let first = port(first, "first")?;
    let last = port(last, "last")?;
    if last < first {
        bail!("empty port range {first}-{last}");
    }
    Ok(first..=last)
}

/// Parses `first-last`.
pub fn parse_port_range(raw: &str) -> Result<RangeInclusive<u16>> {
    let (first, last) = raw
        .split_once('-')
        .with_context(|| format!("port_range must be \"first-last\", got {raw:?}"))?;
    let first: u64 = first
        .trim()
        .parse()
        .with_context(|| format!("invalid first port in {raw:?}"))?;
    let last: u64 = last
        .trim()
        .parse()
        .with_context(|| format!("invalid last port in {raw:?}"))?;
    port_range(first, last).with_context(|| format!("port_range {raw:?}"))
}

/// The settings-file form of a range.
pub fn format_port_range(range: &RangeInclusive<u16>) -> String {
    format!("{}-{}", range.start(), range.end())
}

/// How a set of ports reads to an operator: runs where they are consecutive,
/// individual numbers where they are not.
///
/// The ports Open Live publishes are a set, not a range — Strom's pool can have
/// holes and a port it found already bound leaves a gap — but the usual case is
/// still one unbroken run, and that is what should appear in a log line.
pub fn format_ports(ports: &std::collections::BTreeSet<u16>) -> String {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for port in ports {
        match runs.last_mut() {
            Some(run) if run.1 + 1 == *port => run.1 = *port,
            _ => runs.push((*port, *port)),
        }
    }
    runs.iter()
        .map(|(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl Uplink {
    /// The range from the settings, if one is set. Which range is actually used is
    /// decided at startup, once Open Live has been asked.
    pub fn local_ports(&self) -> Result<Option<RangeInclusive<u16>>> {
        self.port_range
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(parse_port_range)
            .transpose()
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
    let base = if cfg!(windows) {
        // An empty APPDATA counts as unset, rather than giving a relative directory.
        std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("open-live-ingest")
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
    if let Ok(id) = std::env::var(ENV_OPEN_LIVE_GATEWAY_ID) {
        cfg.open_live.gateway_id = Some(id);
    }
    if let Ok(token) = std::env::var(ENV_OPEN_LIVE_GATEWAY_TOKEN) {
        cfg.open_live.gateway_token = Some(token);
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
/// interrupted save cannot leave half a file, and readable by the user only (mode
/// 0600, or on Windows an ACL of the user, SYSTEM and Administrators) because it
/// holds the Open Live credential and any SRT passphrase.
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
    #[cfg(windows)]
    if let Err(err) = restrict_to_user(&tmp) {
        std::fs::remove_file(&tmp).ok();
        return Err(err.context("restricting settings permissions"));
    }
    std::fs::rename(&tmp, path).context("replacing the settings file")?;
    Ok(())
}

/// Replaces the file's ACL with full control for the current user, SYSTEM and
/// Administrators, inheritance removed. By SID, so it works in any locale.
#[cfg(windows)]
fn restrict_to_user(path: &Path) -> Result<()> {
    use std::process::Command;
    let whoami = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .context("running whoami to find the current user's SID")?;
    if !whoami.status.success() {
        bail!("whoami /user failed ({})", whoami.status);
    }
    let sid = sid_from_whoami(&String::from_utf8_lossy(&whoami.stdout))
        .context("reading the current user's SID from whoami")?;
    let icacls = Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("*{sid}:F"))
        .args(["*S-1-5-18:F", "*S-1-5-32-544:F"])
        .output()
        .context("running icacls")?;
    if !icacls.status.success() {
        bail!(
            "icacls failed ({}): {}",
            icacls.status,
            String::from_utf8_lossy(&icacls.stderr).trim()
        );
    }
    Ok(())
}

/// The SID in `whoami /user /fo csv /nh` output, a line such as
/// `"host\user","S-1-5-21-..."`. The SID is the last field; the name may hold commas.
#[cfg(any(windows, test))]
fn sid_from_whoami(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    let valid = sid.starts_with("S-1-")
        && sid[2..]
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    valid.then(|| sid.to_string())
}

/// Rejects settings that would otherwise fail confusingly at runtime. The error is
/// the first of [`problems`], unchanged.
pub fn validate(cfg: &Config) -> Result<()> {
    validate_with(cfg, crate::osc::saved_token().is_some())
}

/// One thing wrong with the settings, in words an operator can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// The settings key or section it concerns, as written in the file, e.g.
    /// `uplink.public_host` or `open_live`. None when it concerns no one setting.
    pub setting: Option<&'static str>,
    /// One line, shown as is.
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Problem {
    pub fn new(setting: impl Into<Option<&'static str>>, message: impl Into<String>) -> Self {
        Problem {
            setting: setting.into(),
            message: message.into(),
        }
    }
}

/// Every check [`validate`] makes, each failure reported instead of only the first.
/// Checks the settings alone; nothing is contacted.
pub fn problems(cfg: &Config) -> Vec<Problem> {
    problems_with(cfg, crate::osc::saved_token().is_some())
}

fn problems_with(cfg: &Config, osc_login: bool) -> Vec<Problem> {
    check(cfg, osc_login)
        .into_iter()
        .map(|(setting, err)| Problem::new(setting, format!("{err:#}")))
        .collect()
}

/// `osc_login` says whether the OSC CLI has a saved login, which stands in for
/// `open_live.api_key` in osc mode. Passed in so the check itself stays pure.
fn validate_with(cfg: &Config, osc_login: bool) -> Result<()> {
    match check(cfg, osc_login).into_iter().next() {
        Some((_, err)) => Err(err),
        None => Ok(()),
    }
}

/// The checks, in the order `validate` has always made them, each failure with the
/// setting it concerns.
fn check(cfg: &Config, osc_login: bool) -> Vec<(&'static str, anyhow::Error)> {
    let mut found: Vec<(&'static str, anyhow::Error)> = Vec::new();
    if !cfg.strom.url.starts_with("http://") && !cfg.strom.url.starts_with("https://") {
        found.push((
            "strom.url",
            anyhow!(
                "strom.url must be an http or https URL, got {:?}",
                cfg.strom.url
            ),
        ));
    }

    // None when the range does not parse, which is reported here and nowhere else.
    let local_ports = match cfg.uplink.local_ports() {
        Ok(ports) => Some(ports),
        Err(err) => {
            found.push(("uplink.port_range", err));
            None
        }
    };
    if cfg.uplink.mode == UplinkMode::Listener {
        if cfg
            .uplink
            .public_host
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            found.push((
                "uplink.public_host",
                anyhow!(
                    "uplink.mode is \"listener\", where the cloud dials this machine, so \
                     uplink.public_host must be its address as the cloud sees it"
                ),
            ));
        }
        // In caller mode the ports are the cloud Strom's and Open Live publishes them;
        // here they are this machine's own, and nobody else can know them.
        if matches!(local_ports, Some(None)) {
            found.push((
                "uplink.port_range",
                anyhow!(
                    "uplink.mode is \"listener\", where this machine listens, so \
                     uplink.port_range must name its own SRT ports"
                ),
            ));
        }
    }
    if let Some(len) = cfg.uplink.pbkeylen {
        if !matches!(len, 16 | 24 | 32) {
            found.push((
                "uplink.pbkeylen",
                anyhow!("uplink.pbkeylen must be 16, 24, or 32, got {len}"),
            ));
        }
    }
    if let Some(passphrase) = cfg.uplink.passphrase.as_deref().filter(|p| !p.is_empty()) {
        // libsrt's own limits, and the value travels unescaped inside two URIs.
        let n = passphrase.chars().count();
        if !(10..=79).contains(&n) {
            found.push((
                "uplink.passphrase",
                anyhow!("uplink.passphrase must be 10 to 79 characters, got {n}"),
            ));
        }
        if !passphrase
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.~!*".contains(c))
        {
            found.push((
                "uplink.passphrase",
                anyhow!("uplink.passphrase may only contain letters, digits, and - _ . ~ ! *"),
            ));
        }
    }

    if let Some(res) = cfg
        .capture
        .video_resolution
        .as_deref()
        .filter(|r| !r.is_empty())
    {
        if let Err(err) = parse_resolution(res) {
            found.push(("capture.video_resolution", err));
        }
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
            found.push((
                "capture.video_framerate",
                anyhow!("capture.video_framerate must be N/D, e.g. 25/1, got {rate:?}"),
            ));
        }
    }

    // The same check `up` makes against the resolved devices, made early on the text
    // so the settings file is refused before anything starts.
    let mut paired: Vec<(&str, &str)> = Vec::new();
    for entry in &cfg.inputs {
        let device = entry.device.trim();
        if device.is_empty() {
            found.push(("input", anyhow!("an [[input]] entry has no device")));
            continue;
        }
        let Some(audio) = entry.audio.as_deref().map(str::trim) else {
            continue;
        };
        if audio.is_empty() {
            found.push((
                "input",
                anyhow!("the [[input]] entry for {device:?} has an empty audio; remove the line for no audio"),
            ));
            continue;
        }
        if let Some((other, _)) = paired.iter().find(|(_, a)| a.eq_ignore_ascii_case(audio)) {
            found.push((
                "input",
                anyhow!("audio source {audio:?} is paired with both {other:?} and {device:?} in [[input]]; one audio source goes with one camera"),
            ));
            continue;
        }
        paired.push((device, audio));
    }

    if cfg.open_live.register {
        if cfg
            .open_live
            .url
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            found.push((
                "open_live.url",
                anyhow!("open_live.register is on but open_live.url is unset"),
            ));
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
            found.push((
                "open_live.api_key",
                anyhow!(
                    "open_live.auth_mode is \"osc\" but there is no OSC token: log in with `{}`, or set open_live.api_key",
                    crate::osc::LOGIN_COMMAND
                ),
            ));
        }
    }
    // An id without its token, or the reverse, is a half-finished provisioning step;
    // it is refused here rather than warned about on every retry during the show.
    if let Err(err) = crate::heartbeat::Target::from_config(&cfg.open_live) {
        found.push(("open_live.gateway_id", err));
    }
    found
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

    #[test]
    fn input_entries_pair_audio_and_one_audio_source_is_not_paired_twice() {
        let mut cfg: Config = toml::from_str(
            "[[input]]\ndevice = \"FaceTime\"\naudio = \"USB Microphone\"\n\n[[input]]\ndevice = \"DeckLink\"\n",
        )
        .expect("parses");
        assert_eq!(
            cfg.inputs,
            [
                InputSettings {
                    device: "FaceTime".to_string(),
                    audio: Some("USB Microphone".to_string()),
                },
                InputSettings {
                    device: "DeckLink".to_string(),
                    audio: None,
                },
            ]
        );
        cfg.gateway = valid().gateway;
        cfg.open_live = valid().open_live;
        validate(&cfg).expect("should validate");

        cfg.inputs[1].audio = Some("usb microphone".to_string());
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("paired with both"), "{err}");

        cfg.inputs[1].audio = Some(" ".to_string());
        assert!(validate(&cfg).is_err());

        assert!(toml::from_str::<Config>("[[input]]\ndevice = \"x\"\nmic = \"y\"\n").is_err());
    }

    /// A file without `[[input]]` is written back without one.
    #[test]
    fn no_input_entries_write_no_input_section() {
        let text = toml::to_string_pretty(&valid()).unwrap();
        assert!(!text.contains("input"), "{text}");
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

    /// The heartbeat is optional, but half of its credential is a mistake.
    #[test]
    fn a_gateway_id_and_its_token_come_together_or_not_at_all() {
        let mut cfg = valid();
        validate(&cfg).expect("no gateway is fine");
        cfg.open_live.gateway_id = Some("gw-1".to_string());
        assert!(validate(&cfg).is_err(), "an id alone");
        cfg.open_live.gateway_token = Some("olgw_v1_secret".to_string());
        validate(&cfg).expect("both should validate");
        cfg.open_live.gateway_id = None;
        assert!(validate(&cfg).is_err(), "a token alone");
        cfg.open_live.gateway_id = Some("gw 1".to_string());
        assert!(
            validate(&cfg).is_err(),
            "an id that cannot go in a URL path"
        );
    }

    /// The cloud cannot dial a venue whose address it does not know.
    #[test]
    fn listener_mode_requires_a_public_host() {
        let mut cfg = valid();
        cfg.uplink.mode = UplinkMode::Listener;
        cfg.uplink.port_range = Some("47110-47129".to_string());
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

    /// An app shows everything wrong at once; the CLI's `check` still reports the
    /// first, worded as before.
    #[test]
    fn problems_lists_every_failure_and_validate_reports_the_first() {
        assert_eq!(problems_with(&valid(), false), []);

        let mut cfg = valid();
        cfg.strom.url = "127.0.0.1:8080".to_string();
        cfg.uplink.mode = UplinkMode::Listener;
        cfg.uplink.port_range = Some("9100-9000".to_string());
        cfg.uplink.passphrase = Some("short&".to_string());
        cfg.capture.video_framerate = Some("25".to_string());
        cfg.open_live.url = None;
        cfg.open_live.gateway_id = Some("gw-1".to_string());
        cfg.inputs = vec![
            InputSettings {
                device: "FaceTime".to_string(),
                audio: Some("USB Microphone".to_string()),
            },
            InputSettings {
                device: "DeckLink".to_string(),
                audio: Some("usb microphone".to_string()),
            },
        ];
        let found = problems_with(&cfg, false);
        let settings: Vec<Option<&str>> = found.iter().map(|p| p.setting).collect();
        assert_eq!(
            settings,
            [
                Some("strom.url"),
                Some("uplink.port_range"),
                Some("uplink.public_host"),
                Some("uplink.passphrase"),
                Some("uplink.passphrase"),
                Some("capture.video_framerate"),
                Some("input"),
                Some("open_live.url"),
                Some("open_live.gateway_id"),
            ],
            "{found:#?}"
        );
        // A range that does not parse is not also called missing.
        assert!(found[1].message.contains("9100-9000"), "{}", found[1]);
        assert_eq!(
            validate_with(&cfg, false).unwrap_err().to_string(),
            found[0].message
        );
    }

    #[test]
    fn malformed_port_ranges_are_rejected() {
        for bad in ["9000", "9100-9000", "abc-def", "0-10", "9000-70000"] {
            assert!(parse_port_range(bad).is_err(), "{bad:?} should be rejected");
            let u = Uplink {
                port_range: Some(bad.to_string()),
                ..Uplink::default()
            };
            assert!(u.local_ports().is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(parse_port_range("47110-47129").unwrap(), 47110..=47129);
        assert_eq!(Uplink::default().local_ports().unwrap(), None);
        let blank = Uplink {
            port_range: Some("  ".to_string()),
            ..Uplink::default()
        };
        assert_eq!(blank.local_ports().unwrap(), None);
    }

    /// The same checks apply to the two numbers Open Live publishes.
    #[test]
    fn a_range_from_two_numbers_is_checked_the_same_way() {
        assert_eq!(port_range(9000, 9002).unwrap(), 9000..=9002);
        assert_eq!(port_range(9000, 9000).unwrap(), 9000..=9000);
        assert!(port_range(0, 10).is_err());
        assert!(port_range(9002, 9000).is_err());
        assert!(port_range(9000, 65536).is_err());
        assert_eq!(format_port_range(&(9000..=9002)), "9000-9002");
    }

    /// In caller mode the cloud owns the ports and Open Live publishes them, so a
    /// range in the settings is optional. In listener mode they are this machine's
    /// own ports and nobody else can supply them.
    #[test]
    fn only_listener_mode_requires_a_local_port_range() {
        let mut cfg = valid();
        assert_eq!(cfg.uplink.port_range, None);
        validate(&cfg).expect("caller mode validates without a range");
        cfg.uplink.mode = UplinkMode::Listener;
        cfg.uplink.public_host = Some("venue.example.com".to_string());
        assert!(validate(&cfg).is_err(), "listener mode needs a range");
        cfg.uplink.port_range = Some("47110-47129".to_string());
        validate(&cfg).expect("listener mode with a range validates");
        cfg.uplink.mode = UplinkMode::Caller;
        cfg.uplink.port_range = Some("9100-9000".to_string());
        assert!(validate(&cfg).is_err(), "a set range is still checked");
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
        cfg.uplink.port_range = Some("9400-9500".to_string());
        cfg.video.bitrate_kbps = 7000;

        save(&path, &cfg).expect("saves");
        let loaded = load_or_default(&path).expect("loads");
        assert_eq!(loaded.gateway.name, "Venue A");
        assert_eq!(loaded.open_live.api_key, cfg.open_live.api_key);
        assert_eq!(loaded.open_live.auth_mode, AuthMode::Osc);
        assert_eq!(loaded.uplink.port_range.as_deref(), Some("9400-9500"));
        assert_eq!(loaded.video.bitrate_kbps, 7000);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group and other must have no access");
        }
        #[cfg(windows)]
        {
            let out = std::process::Command::new("icacls")
                .arg(&path)
                .output()
                .expect("icacls runs");
            assert!(out.status.success());
            let listing = String::from_utf8_lossy(&out.stdout);
            let aces: Vec<&str> = listing.lines().filter(|l| l.contains(":(")).collect();
            assert_eq!(
                aces.len(),
                3,
                "user, SYSTEM and Administrators only:\n{listing}"
            );
            assert!(
                aces.iter().all(|ace| !ace.contains("(I)")),
                "no inherited entries:\n{listing}"
            );
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_sid_is_the_last_field_of_whoami_output() {
        assert_eq!(
            sid_from_whoami(
                "\"host\\op, venue\",\"S-1-5-21-1004336348-1177238915-682003330-1001\"\r\n"
            )
            .as_deref(),
            Some("S-1-5-21-1004336348-1177238915-682003330-1001")
        );
        assert_eq!(sid_from_whoami(""), None);
        assert_eq!(sid_from_whoami("\"host\\op\",\"not-a-sid\""), None);
        assert_eq!(sid_from_whoami("\"host\\op\",\"S-1-\""), None);
    }

    #[cfg(windows)]
    #[test]
    fn user_files_live_under_appdata() {
        let appdata = std::env::var_os("APPDATA").expect("APPDATA is set on Windows");
        assert_eq!(user_dir(), PathBuf::from(appdata).join("open-live-ingest"));
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
