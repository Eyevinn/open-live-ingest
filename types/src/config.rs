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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub gateway: GatewayIdentity,
    /// The local Strom instance this gateway drives.
    pub strom: StromConfig,
    /// One entry per capture input. Each becomes its own Strom flow with its own SRT
    /// port, so one camera failing cannot disturb another.
    #[serde(default)]
    pub inputs: Vec<InputConfig>,
    #[serde(default)]
    pub open_live: OpenLiveConfig,
    #[serde(default)]
    pub control: ControlConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayIdentity {
    /// Stable identity for this box. Defaults to the hostname when unset. Also seeds
    /// the deterministic flow ids, so changing it orphans the flows already created.
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable name, used as a prefix for registered Open Live source names.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StromConfig {
    /// Base URL of the local Strom instance, e.g. `http://127.0.0.1:8080`.
    pub url: String,
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
    /// `builtin.local_input` — USB/UVC capture via V4L2.
    Local {
        #[serde(default = "default_video_device")]
        video_device: String,
        /// `WxH`, e.g. `1920x1080`.
        #[serde(default = "default_resolution")]
        video_resolution: String,
        #[serde(default = "default_framerate")]
        video_framerate: String,
        /// ALSA device carrying this capture card's audio, e.g. `hw:1,0`.
        #[serde(default)]
        audio_device: Option<String>,
        #[serde(default = "default_channels")]
        audio_channels: u32,
        #[serde(default = "default_sample_rate")]
        audio_rate: u32,
    },
    /// Test pattern and tone, built from raw `videotestsrc`/`audiotestsrc` elements.
    /// Useful for commissioning an SRT link before the cameras arrive.
    Test,
}

/// Maps to `builtin.videoenc`. Strom already selects the encoder element itself,
/// hardware first across NVENC, QSV, VA-API, VideoToolbox, AMF, and V4L2, so this
/// config expresses intent (`encoder_preference`) rather than an element name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// `h264`, `h265`, `av1`, `vp9`. H.264 is the interoperable default.
    pub codec: String,
    /// Bitrate in kbps.
    pub bitrate_kbps: u32,
    /// `auto`, `hardware`, or `software`.
    pub encoder_preference: String,
    /// `ultrafast`, `fast`, `medium`, `slow`, `veryslow`.
    pub quality_preset: String,
    /// `zerolatency` for contribution.
    pub tune: String,
    /// `cbr`, `vbr`, or `cqp`. CBR for contribution over a committed link.
    pub rate_control: String,
    /// Keyframe interval in frames. 1–2 seconds' worth so a reconnecting receiver
    /// locks on quickly.
    pub keyframe_interval: u32,
}

/// SRT uplink to the cloud Strom instance, mapped to `builtin.mpegtssrt_output`.
///
/// Note the two-URI split: `host`/`port` form the *caller* URI the local Strom dials,
/// while the address registered with Open Live is the matching *listener* form
/// (`srt://:PORT?mode=listener`) that the cloud Strom binds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UplinkConfig {
    /// Public hostname of the cloud Strom instance.
    pub host: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenLiveConfig {
    /// Base URL of the Open Live API, e.g. `https://open-live.example.com`.
    #[serde(default)]
    pub url: Option<String>,
    /// Bearer token for `/api/v1`. Redacted from all log output.
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

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec: "h264".to_string(),
            bitrate_kbps: DEFAULT_VIDEO_BITRATE_KBPS,
            encoder_preference: default_auto(),
            quality_preset: "ultrafast".to_string(),
            tune: "zerolatency".to_string(),
            rate_control: "cbr".to_string(),
            keyframe_interval: 25,
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
    /// The caller URI the local Strom's `builtin.mpegtssrt_output` dials out to.
    pub fn caller_uri(&self) -> String {
        let mut uri = format!(
            "srt://{}:{}?mode=caller&latency={}",
            self.host, self.port, self.latency_ms
        );
        self.append_crypto(&mut uri);
        if let Some(stream_id) = self.stream_id.as_deref().filter(|s| !s.is_empty()) {
            uri.push_str(&format!("&streamid={stream_id}"));
        }
        uri
    }

    /// The listener form registered with Open Live, which the cloud Strom binds.
    /// Open Live's source validation explicitly permits this hostless form.
    pub fn listener_uri(&self) -> String {
        let mut uri = format!("srt://:{}?mode=listener", self.port);
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

fn default_video_device() -> String {
    "/dev/video0".to_string()
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

fn default_srt_latency() -> u32 {
    DEFAULT_SRT_LATENCY_MS
}

fn default_state_path() -> String {
    "/var/lib/open-live-gateway/state.json".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uplink() -> UplinkConfig {
        UplinkConfig {
            host: "strom.example.com".to_string(),
            port: 9000,
            latency_ms: 200,
            passphrase: None,
            pbkeylen: None,
            stream_id: None,
        }
    }

    #[test]
    fn caller_uri_dials_the_cloud_host() {
        assert_eq!(
            uplink().caller_uri(),
            "srt://strom.example.com:9000?mode=caller&latency=200"
        );
    }

    /// The listener form is what the cloud Strom binds, so it must stay hostless —
    /// Open Live's source validation permits `srt://:PORT` specifically.
    #[test]
    fn listener_uri_is_hostless_and_never_leaks_the_cloud_host() {
        let uri = uplink().listener_uri();
        assert_eq!(uri, "srt://:9000?mode=listener");
        assert!(!uri.contains("strom.example.com"));
    }

    #[test]
    fn both_uris_carry_the_passphrase_so_the_ends_agree() {
        let mut cfg = uplink();
        cfg.passphrase = Some("s3cret".to_string());
        cfg.pbkeylen = Some(16);

        assert!(cfg.caller_uri().contains("passphrase=s3cret&pbkeylen=16"));
        assert!(cfg.listener_uri().contains("passphrase=s3cret&pbkeylen=16"));
    }

    #[test]
    fn an_empty_passphrase_is_not_sent() {
        let mut cfg = uplink();
        cfg.passphrase = Some(String::new());

        assert!(!cfg.caller_uri().contains("passphrase"));
        assert!(!cfg.listener_uri().contains("passphrase"));
    }

    /// `streamid` is a caller-side selector; on the listener it would be meaningless
    /// and Open Live would store a misleading address.
    #[test]
    fn stream_id_applies_to_the_caller_only() {
        let mut cfg = uplink();
        cfg.stream_id = Some("cam1".to_string());

        assert!(cfg.caller_uri().contains("streamid=cam1"));
        assert!(!cfg.listener_uri().contains("streamid"));
    }
}
