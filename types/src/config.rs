//! Gateway configuration model.
//!
//! Loaded from TOML, then overlaid by environment variables and CLI flags
//! (precedence: CLI > env > file). See `gateway.toml.example`.
//!
//! Property names in the capture, video, and uplink sections deliberately mirror the
//! property names of the Strom blocks they configure (`builtin.decklink_input`,
//! `builtin.local_input`, `builtin.videoenc`, `builtin.mpegtssrt_output`), so a value
//! in this file can be traced straight to the block it lands on.

use serde::{Deserialize, Serialize};

/// Default SRT latency in ms. Matches the default of Strom's SRT blocks so both ends
/// agree unless told otherwise.
pub const DEFAULT_SRT_LATENCY_MS: u32 = 200;

/// Default video bitrate in kbps for a 1080p25 contribution feed.
pub const DEFAULT_VIDEO_BITRATE_KBPS: u32 = 6000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayConfig {
    #[serde(default)]
    pub gateway: GatewayIdentity,
    /// The local Strom instance this gateway drives. Defaults to loopback, since on a
    /// venue box Strom runs beside the gateway.
    #[serde(default)]
    pub strom: StromConfig,
    /// One entry per capture input. Each becomes its own Strom flow with its own SRT
    /// port, so one camera failing cannot disturb another.
    #[serde(default)]
    pub inputs: Vec<InputConfig>,
    /// Settings the desktop app uses for inputs an operator starts at runtime.
    /// Not needed by the headless daemon, which declares its inputs above.
    #[serde(default)]
    pub app: AppConfig,
    #[serde(default)]
    pub open_live: OpenLiveConfig,
    #[serde(default)]
    pub control: ControlConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GatewayIdentity {
    /// Stable identity for this box. Defaults to the hostname when unset. Also seeds
    /// the deterministic flow ids, so changing it orphans the flows already created.
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable name, used as a prefix for registered Open Live source names.
    pub name: String,
}

/// Defaults applied to an input the operator starts from the desktop app, where
/// there is no config entry to read them from.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default)]
    pub video: VideoConfig,
    /// Capture format to request from the device, or nothing to take what it offers.
    /// See the note on `CaptureConfig::Local` — asking for a format a device does not
    /// advertise fails negotiation rather than being converted.
    #[serde(default)]
    pub video_resolution: Option<String>,
    #[serde(default)]
    pub video_framerate: Option<String>,
    #[serde(default)]
    pub uplink: UplinkTemplate,
}

/// An uplink with no port yet: the app allocates one per input from `port_range`,
/// because an operator picking a camera cannot be asked to choose a UDP port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UplinkTemplate {
    #[serde(default = "default_uplink_mode")]
    pub mode: UplinkMode,
    /// Hostname of the cloud Strom to dial. Leave unset and it is discovered from
    /// Open Live's `server-info`, so a venue box needs only the Open Live address.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub public_host: Option<String>,
    /// Inclusive `first-last` range to allocate SRT ports from, e.g. "9000-9100".
    #[serde(default = "default_port_range")]
    pub port_range: String,
    #[serde(default = "default_srt_latency")]
    pub latency_ms: u32,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub pbkeylen: Option<u32>,
}

impl Default for UplinkTemplate {
    fn default() -> Self {
        Self {
            mode: default_uplink_mode(),
            host: None,
            public_host: None,
            port_range: default_port_range(),
            latency_ms: DEFAULT_SRT_LATENCY_MS,
            passphrase: None,
            pbkeylen: None,
        }
    }
}

impl UplinkTemplate {
    /// The inclusive port range to allocate from.
    pub fn ports(&self) -> Result<std::ops::RangeInclusive<u16>, String> {
        let (first, last) = self
            .port_range
            .split_once('-')
            .ok_or_else(|| format!("port_range must be \"first-last\", got {}", self.port_range))?;
        let first: u16 = first
            .trim()
            .parse()
            .map_err(|_| format!("invalid first port in {}", self.port_range))?;
        let last: u16 = last
            .trim()
            .parse()
            .map_err(|_| format!("invalid last port in {}", self.port_range))?;
        if first == 0 || last < first {
            return Err(format!("empty port range {}", self.port_range));
        }
        Ok(first..=last)
    }

    /// Fills in a concrete uplink for one input.
    ///
    /// `host` is passed in rather than read from the template because it may have
    /// been discovered from Open Live at startup.
    pub fn materialize(&self, port: u16, host: &str) -> UplinkConfig {
        UplinkConfig {
            mode: self.mode,
            host: host.to_string(),
            public_host: self.public_host.clone(),
            port,
            latency_ms: self.latency_ms,
            passphrase: self.passphrase.clone(),
            pbkeylen: self.pbkeylen,
            stream_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StromConfig {
    /// Base URL of the *local* Strom instance — the one on this machine that captures
    /// and encodes. Not the cloud Strom, and not something Open Live knows about.
    #[serde(default = "default_strom_url")]
    pub url: String,
    /// Start a headless Strom when nothing is listening at `url`, and stop it again
    /// on the way out.
    ///
    /// An already-running Strom is always adopted rather than replaced, and never
    /// stopped — it may be a service this box depends on, or someone else's.
    #[serde(default = "default_true")]
    pub manage: bool,
    /// The Strom executable: a name to find on `PATH`, or an absolute path.
    #[serde(default = "default_strom_binary")]
    pub binary: String,
    /// Where a managed Strom keeps its flows and blocks. Defaults beside our own
    /// settings, so a gateway-managed Strom does not disturb an existing install.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Strom API key, sent as a bearer token. Unset when Strom runs without auth.
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Local identifier, unique within this gateway (e.g. "cam1").
    pub id: String,
    /// Name registered with Open Live. Defaults to "<gateway name> — <input id>".
    #[serde(default)]
    pub name: Option<String>,
    pub capture: CaptureConfig,
    #[serde(default)]
    pub video: VideoConfig,
    pub uplink: UplinkConfig,
    /// Whether the gateway should create and start this input's flow automatically.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Capture backend, mapped to the Strom block that implements it.
///
/// DeckLink is preferred where available because `decklinkvideosrc` and
/// `decklinkaudiosrc` share the card's clock. `builtin.local_input` covers USB/UVC
/// capture, where video and audio arrive on independent clocks and drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CaptureConfig {
    /// `builtin.decklink_input`
    Decklink {
        #[serde(default)]
        device_number: i32,
        /// `auto`, `sdi`, `hdmi`, `optical`, `component`, `composite`, `svideo`.
        #[serde(default = "default_auto")]
        connection: String,
        /// `auto`, or an explicit mode such as `1080p25`.
        #[serde(default = "default_auto")]
        mode: String,
        #[serde(default = "default_auto")]
        video_format: String,
        /// Keeps frames from a card with no signal off the wire.
        #[serde(default = "default_true")]
        drop_no_signal_frames: bool,
        #[serde(default = "default_true")]
        audio: bool,
    },
    /// `builtin.local_input` — any device the OS exposes as a video/audio source:
    /// USB webcams, USB capture dongles, HDMI/SDI grabbers, virtual sources.
    Local {
        /// Strom device id, as listed by
        /// `GET /api/discovery/devices?category=video_source` on the target Strom.
        /// **Not** a `/dev/video*` path. Leave unset to let Strom pick the OS
        /// default via `autovideosrc`, which is usually what you want with a
        /// single camera attached.
        #[serde(default)]
        video_device: Option<String>,
        /// `WxH`, e.g. `1920x1080`. Leave unset to take whatever the device offers.
        ///
        /// Not a request the pipeline can satisfy by conversion: `local_input`
        /// normalises through `videoconvert`, which changes pixel format but cannot
        /// scale or re-time. Asking for a format the device does not offer fails
        /// negotiation outright, so the safe default is to ask for nothing.
        #[serde(default)]
        video_resolution: Option<String>,
        #[serde(default)]
        video_framerate: Option<String>,
        /// Strom device id from
        /// `GET /api/discovery/devices?category=audio_source`. Leave unset for
        /// video-only capture — a USB camera's microphone is a separate device, and
        /// its independent clock will drift against the video anyway.
        #[serde(default)]
        audio_device: Option<String>,
        #[serde(default = "default_channels")]
        audio_channels: u32,
        #[serde(default = "default_sample_rate")]
        audio_rate: u32,
    },
    /// Test pattern and tone, built from raw `videotestsrc`/`audiotestsrc` elements.
    /// Useful for commissioning an SRT link before the cameras arrive — so it
    /// defaults to broadcast format rather than the elements' own 320x240 defaults,
    /// which would exercise nothing like a real feed.
    Test {
        #[serde(default = "default_resolution")]
        video_resolution: String,
        #[serde(default = "default_framerate")]
        video_framerate: String,
        #[serde(default = "default_sample_rate")]
        audio_rate: u32,
        #[serde(default = "default_channels")]
        audio_channels: u32,
    },
}

/// Maps to `builtin.videoenc`. Strom already selects the encoder element itself,
/// hardware first across NVENC, QSV, VA-API, VideoToolbox, AMF, and V4L2, so this
/// config expresses intent (`encoder_preference`) rather than an element name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// `h264`, `h265`, `av1`, `vp9`. H.264 is the interoperable default.
    #[serde(default = "default_codec")]
    pub codec: String,
    /// Bitrate in kbps.
    #[serde(default = "default_bitrate")]
    pub bitrate_kbps: u32,
    /// `auto`, `hardware`, or `software`.
    #[serde(default = "default_auto")]
    pub encoder_preference: String,
    /// `ultrafast`, `fast`, `medium`, `slow`, `veryslow`.
    #[serde(default = "default_quality_preset")]
    pub quality_preset: String,
    /// `zerolatency` for contribution.
    #[serde(default = "default_tune")]
    pub tune: String,
    /// `cbr`, `vbr`, or `cqp`. CBR for contribution over a committed link.
    #[serde(default = "default_rate_control")]
    pub rate_control: String,
    /// Keyframe interval in frames. 1–2 seconds' worth so a reconnecting receiver
    /// locks on quickly.
    #[serde(default = "default_keyframe_interval")]
    pub keyframe_interval: u32,
}

/// Which end of the SRT link dials the other.
///
/// This is a deployment question, not a preference: whichever end is the caller needs
/// no inbound UDP, and whichever end listens does. Both directions are legitimate and
/// the right one depends on which side is reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UplinkMode {
    /// The venue dials the cloud. Needs a published UDP port on the *cloud* host, and
    /// nothing inbound at the venue.
    Caller,
    /// The cloud dials the venue. Needs a published UDP port at the *venue* — a public
    /// address or a port forward — and nothing inbound in the cloud. This matches the
    /// convention Open Live's own seeded sources use, where the address on the source
    /// is what the cloud Strom dials.
    Listener,
    /// Both ends dial each other, punching through NAT. Needs each side to know the
    /// other's address but no port forward, so it is worth trying when neither end
    /// can publish a port.
    Rendezvous,
}

/// SRT uplink to the cloud Strom instance, mapped to `builtin.mpegtssrt_output`.
///
/// One link is described by two URIs that mirror each other: the one this gateway puts
/// on the venue Strom's output block, and the one it registers on the Open Live source
/// for the cloud Strom's input block. `mode` decides which side dials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UplinkConfig {
    /// Which end dials. Default `caller` (the venue dials out).
    #[serde(default = "default_uplink_mode")]
    pub mode: UplinkMode,
    /// Public hostname of the cloud Strom instance. Used when the venue dials.
    pub host: String,
    /// This gateway's address as the cloud sees it — a public hostname or IP. Required
    /// for `listener` and `rendezvous`, where the cloud has to dial the venue.
    #[serde(default)]
    pub public_host: Option<String>,
    /// UDP port the cloud Strom binds for this input. Must be unique across every
    /// gateway pointing at the same Strom, and open on the cloud firewall.
    pub port: u16,
    /// SRT latency in ms. Rule of thumb: 3–4x the measured RTT.
    #[serde(default = "default_srt_latency")]
    pub latency_ms: u32,
    /// Optional AES passphrase. Owned by the gateway: Open Live masks it on read,
    /// so it is never read back from that API.
    #[serde(default)]
    pub passphrase: Option<String>,
    /// AES key length in bytes (16, 24, or 32).
    #[serde(default)]
    pub pbkeylen: Option<u32>,
    #[serde(default)]
    pub stream_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenLiveConfig {
    /// Base URL of the Open Live API, e.g. `https://open-live.example.com`.
    #[serde(default)]
    pub url: Option<String>,
    /// How `api_key` is used:
    ///
    /// - `direct` (default) — sent as the bearer token as-is, for a self-hosted
    ///   Open Live protected by a static `API_KEY`.
    /// - `osc` — treated as an OSC Personal Access Token and exchanged for a
    ///   short-lived Service Access Token. Required for an OSC-hosted instance,
    ///   whose reverse proxy rejects a PAT presented directly.
    #[serde(default = "default_auth_mode")]
    pub auth_mode: String,
    /// Bearer token or OSC PAT, depending on `auth_mode`. Redacted from all logs.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Create and update source documents automatically. Turn off when an operator
    /// manages sources by hand in Studio.
    #[serde(default = "default_true")]
    pub register: bool,
    /// Where resolved Open Live source ids are persisted so restarts do not create
    /// duplicate sources. Strom flow ids are derived, not stored.
    #[serde(default = "default_state_path")]
    pub state_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    /// Local control API bind address. Loopback by default; a non-loopback bind
    /// requires `token`.
    pub bind: String,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    pub level: String,
}

impl Default for OpenLiveConfig {
    fn default() -> Self {
        Self {
            url: None,
            auth_mode: default_auth_mode(),
            api_key: None,
            register: true,
            state_path: default_state_path(),
        }
    }
}

impl Default for StromConfig {
    fn default() -> Self {
        Self {
            url: default_strom_url(),
            manage: true,
            binary: default_strom_binary(),
            data_dir: None,
            api_key: None,
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        // Built from the same functions serde uses, so a partial [video] table and an
        // omitted one cannot drift apart.
        Self {
            codec: default_codec(),
            bitrate_kbps: default_bitrate(),
            encoder_preference: default_auto(),
            quality_preset: default_quality_preset(),
            tune: default_tune(),
            rate_control: default_rate_control(),
            keyframe_interval: default_keyframe_interval(),
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9001".to_string(),
            token: None,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

impl UplinkConfig {
    /// The URI for the venue Strom's `builtin.mpegtssrt_output`.
    pub fn venue_uri(&self) -> String {
        let mut uri = match self.mode {
            // Dial the cloud.
            UplinkMode::Caller => format!(
                "srt://{}:{}?mode=caller&latency={}",
                self.host, self.port, self.latency_ms
            ),
            // Bind and wait for the cloud to dial in.
            UplinkMode::Listener => {
                format!(
                    "srt://:{}?mode=listener&latency={}",
                    self.port, self.latency_ms
                )
            }
            // Both dial. Neither of these params is optional. `localport` pins the
            // port we send from, because rendezvous only works if each peer sends
            // from the port the other dials. `localaddress` is needed because
            // GStreamer otherwise tries to *bind* the URI host — the far end's
            // address — and fails with "Cannot bind to <host>: unable to
            // create/configure SRT socket".
            UplinkMode::Rendezvous => format!(
                "srt://{}:{}?mode=rendezvous&latency={}&localport={}&localaddress=0.0.0.0",
                self.host, self.port, self.latency_ms, self.port
            ),
        };
        self.append_crypto(&mut uri);
        if let Some(stream_id) = self.stream_id.as_deref().filter(|s| !s.is_empty()) {
            uri.push_str(&format!("&streamid={stream_id}"));
        }
        uri
    }

    /// The address registered on the Open Live source, which becomes the `srt_uri` of
    /// the cloud Strom's `builtin.mpegtssrt_input`. The mirror image of `venue_uri`.
    ///
    /// Note that `srtsrc` defaults to caller mode, so an address with no explicit
    /// `mode=` makes the cloud dial out — which is what Open Live's own seeded demo
    /// sources rely on.
    pub fn cloud_uri(&self) -> String {
        let mut uri = match self.mode {
            // The venue dials, so the cloud binds.
            UplinkMode::Caller => format!("srt://:{}?mode=listener", self.port),
            // The cloud dials the venue.
            UplinkMode::Listener => format!(
                "srt://{}:{}?mode=caller",
                self.public_host.as_deref().unwrap_or_default(),
                self.port
            ),
            // The cloud needs the same treatment: without localaddress it would try
            // to bind the venue's public address, which it cannot, and its input
            // block would fail to start — taking the production down with it.
            UplinkMode::Rendezvous => format!(
                "srt://{}:{}?mode=rendezvous&localport={}&localaddress=0.0.0.0",
                self.public_host.as_deref().unwrap_or_default(),
                self.port,
                self.port
            ),
        };
        self.append_crypto(&mut uri);
        uri
    }

    fn append_crypto(&self, uri: &mut String) {
        if let Some(passphrase) = self.passphrase.as_deref().filter(|p| !p.is_empty()) {
            uri.push_str(&format!("&passphrase={passphrase}"));
            if let Some(pbkeylen) = self.pbkeylen {
                uri.push_str(&format!("&pbkeylen={pbkeylen}"));
            }
        }
    }
}

impl InputConfig {
    /// Name registered with Open Live.
    pub fn source_name(&self, gateway_name: &str) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{gateway_name} — {}", self.id))
    }
}

fn default_true() -> bool {
    true
}

fn default_auto() -> String {
    "auto".to_string()
}

fn default_resolution() -> String {
    "1920x1080".to_string()
}

fn default_framerate() -> String {
    "25/1".to_string()
}

fn default_channels() -> u32 {
    2
}

fn default_sample_rate() -> u32 {
    48000
}

fn default_uplink_mode() -> UplinkMode {
    UplinkMode::Caller
}

fn default_auth_mode() -> String {
    "direct".to_string()
}

fn default_codec() -> String {
    "h264".to_string()
}

fn default_bitrate() -> u32 {
    DEFAULT_VIDEO_BITRATE_KBPS
}

fn default_quality_preset() -> String {
    "ultrafast".to_string()
}

fn default_tune() -> String {
    "zerolatency".to_string()
}

fn default_rate_control() -> String {
    "cbr".to_string()
}

fn default_keyframe_interval() -> u32 {
    25
}

fn default_strom_binary() -> String {
    "strom".to_string()
}

fn default_strom_url() -> String {
    "http://127.0.0.1:8080".to_string()
}

fn default_port_range() -> String {
    "9000-9100".to_string()
}

fn default_srt_latency() -> u32 {
    DEFAULT_SRT_LATENCY_MS
}

/// Where per-user files live: settings, recorded state, a managed Strom's data.
///
/// Per-user because this is run by an operator, not by root. The previous default
/// put state under `/var/lib`, which a normal user cannot create — and the failure
/// aborted a run that was already streaming. A packaged service can still point at
/// `/var/lib` explicitly.
pub fn user_dir() -> std::path::PathBuf {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| {
            std::path::PathBuf::from(h)
                .join("Library")
                .join("Application Support")
        })
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })
    };

    base.unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("open-live-gateway")
}

fn default_state_path() -> String {
    user_dir().join("state.json").to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uplink(mode: UplinkMode) -> UplinkConfig {
        UplinkConfig {
            mode,
            host: "strom.example.com".to_string(),
            public_host: Some("venue.example.com".to_string()),
            port: 9000,
            latency_ms: 200,
            passphrase: None,
            pbkeylen: None,
            stream_id: None,
        }
    }

    /// Venue dials out: nothing inbound at the venue, cloud must publish the port.
    #[test]
    fn caller_mode_has_the_venue_dial_and_the_cloud_bind() {
        let cfg = uplink(UplinkMode::Caller);
        assert_eq!(
            cfg.venue_uri(),
            "srt://strom.example.com:9000?mode=caller&latency=200"
        );
        assert_eq!(cfg.cloud_uri(), "srt://:9000?mode=listener");
    }

    /// Cloud dials in: nothing inbound in the cloud, venue must publish the port.
    /// This is the direction Open Live's own seeded demo sources use.
    #[test]
    fn listener_mode_has_the_cloud_dial_and_the_venue_bind() {
        let cfg = uplink(UplinkMode::Listener);
        assert_eq!(cfg.venue_uri(), "srt://:9000?mode=listener&latency=200");
        assert_eq!(cfg.cloud_uri(), "srt://venue.example.com:9000?mode=caller");
    }

    /// Both ends dial, and both must pin their local port and bind address: rendezvous only works if
    /// each peer sends from the port the other dials, and libsrt otherwise binds an
    /// ephemeral port the far end can never reach.
    #[test]
    fn rendezvous_mode_has_both_ends_dial_each_other_from_a_pinned_port() {
        let cfg = uplink(UplinkMode::Rendezvous);
        assert_eq!(
            cfg.venue_uri(),
            "srt://strom.example.com:9000?mode=rendezvous&latency=200&localport=9000&localaddress=0.0.0.0"
        );
        assert_eq!(
            cfg.cloud_uri(),
            "srt://venue.example.com:9000?mode=rendezvous&localport=9000&localaddress=0.0.0.0"
        );
    }

    /// The venue's own address must never appear in the caller-mode cloud address:
    /// there the cloud only binds a port.
    #[test]
    fn caller_mode_cloud_uri_stays_hostless() {
        let uri = uplink(UplinkMode::Caller).cloud_uri();
        assert!(!uri.contains("venue.example.com"));
        assert!(!uri.contains("strom.example.com"));
    }

    #[test]
    fn both_uris_carry_the_passphrase_so_the_ends_agree() {
        let mut cfg = uplink(UplinkMode::Caller);
        cfg.passphrase = Some("s3cret".to_string());
        cfg.pbkeylen = Some(16);

        assert!(cfg.venue_uri().contains("passphrase=s3cret&pbkeylen=16"));
        assert!(cfg.cloud_uri().contains("passphrase=s3cret&pbkeylen=16"));
    }

    #[test]
    fn an_empty_passphrase_is_not_sent() {
        let mut cfg = uplink(UplinkMode::Caller);
        cfg.passphrase = Some(String::new());

        assert!(!cfg.venue_uri().contains("passphrase"));
        assert!(!cfg.cloud_uri().contains("passphrase"));
    }

    /// `streamid` is a caller-side selector, so it belongs on whichever URI dials.
    #[test]
    fn stream_id_applies_to_the_venue_uri_only() {
        let mut cfg = uplink(UplinkMode::Caller);
        cfg.stream_id = Some("cam1".to_string());

        assert!(cfg.venue_uri().contains("streamid=cam1"));
        assert!(!cfg.cloud_uri().contains("streamid"));
    }

    #[test]
    fn uplink_mode_defaults_to_caller() {
        let cfg: UplinkConfig = toml::from_str(
            r#"
host = "strom.example.com"
port = 9000
"#,
        )
        .expect("parses");
        assert_eq!(cfg.mode, UplinkMode::Caller);
    }
}

#[cfg(test)]
mod partial_table_tests {
    use super::*;

    /// Naming one field must not force an operator to restate the rest — and the
    /// values filled in must match `Default`, or an omitted table and a partial one
    /// would behave differently.
    #[test]
    fn a_partial_video_table_keeps_the_defaults_for_everything_else() {
        let cfg: VideoConfig = toml::from_str("bitrate_kbps = 9000").expect("parses");
        let defaults = VideoConfig::default();

        assert_eq!(cfg.bitrate_kbps, 9000);
        assert_eq!(cfg.codec, defaults.codec);
        assert_eq!(cfg.encoder_preference, defaults.encoder_preference);
        assert_eq!(cfg.quality_preset, defaults.quality_preset);
        assert_eq!(cfg.tune, defaults.tune);
        assert_eq!(cfg.rate_control, defaults.rate_control);
        assert_eq!(cfg.keyframe_interval, defaults.keyframe_interval);
    }

    #[test]
    fn an_empty_video_table_equals_the_defaults() {
        let cfg: VideoConfig = toml::from_str("").expect("parses");
        assert_eq!(cfg.bitrate_kbps, VideoConfig::default().bitrate_kbps);
        assert_eq!(cfg.codec, VideoConfig::default().codec);
    }

    /// The whole point of discovery: a config naming only Open Live must load.
    #[test]
    fn a_config_with_only_open_live_loads() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
[gateway]
name = "Venue A"

[open_live]
url = "https://open-live.example.com"
"#,
        )
        .expect("parses");

        assert_eq!(cfg.strom.url, "http://127.0.0.1:8080");
        assert!(
            cfg.app.uplink.host.is_none(),
            "the cloud host is discovered"
        );
        assert!(cfg.inputs.is_empty());
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// State must default somewhere an operator can actually write. It used to
    /// default under /var/lib, where creating it fails for a normal user — and that
    /// failure aborted a run whose feeds were already live.
    #[test]
    fn the_default_state_path_is_under_the_users_own_directory() {
        let path = default_state_path();
        assert!(
            !path.starts_with("/var/"),
            "state must not default to a root-owned location, got {path}"
        );
        assert!(path.ends_with("state.json"), "got {path}");
        assert!(path.contains("open-live-gateway"), "got {path}");
    }

    #[test]
    fn settings_and_state_share_one_directory() {
        let dir = user_dir();
        assert!(default_state_path().starts_with(&*dir.to_string_lossy()));
    }
}
