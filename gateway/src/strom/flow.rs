//! Flow template: turns one `InputConfig` into the Strom flow JSON that captures,
//! encodes, and pushes it to the cloud.
//!
//! ```text
//!   builtin.decklink_input        builtin.videoenc         builtin.mpegtssrt_output
//!   (or builtin.local_input)
//!        video_out  ──────────>  video_in    encoded_out ──────────>  video_in
//!        audio_out  ────────────────────────────────────────────────>  audio_in_0
//! ```
//!
//! Audio goes straight from capture to the TS output: that block auto-encodes raw
//! audio to AAC, so no separate audio encoder block is needed.
//!
//! Block ids and property names are Strom's, not ours. They are duplicated here as
//! string literals because the gateway talks to Strom over HTTP rather than linking
//! its crates — so a rename upstream surfaces as a runtime error from Strom, not a
//! compile error here. `GET /api/blocks` on the target Strom is the authority.

use anyhow::{bail, Result};
use open_live_gateway_types::config::{CaptureConfig, InputConfig};
use serde_json::{json, Value};
use uuid::Uuid;

/// Namespace for deriving flow ids. Constant so the same gateway id and input id
/// always address the same flow — a reboot needs no local state to find its flow.
const FLOW_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x6f, 0x70, 0x65, 0x6e, 0x2d, 0x6c, 0x69, 0x76, 0x65, 0x2d, 0x67, 0x77, 0x00, 0x00, 0x00, 0x01,
]);

const BLOCK_CAPTURE: &str = "capture";
const BLOCK_ENCODER: &str = "encoder";
const BLOCK_UPLINK: &str = "uplink";
const ELEMENT_TEST_VIDEO: &str = "testvideo";
const ELEMENT_TEST_AUDIO: &str = "testaudio";
const ELEMENT_TEST_VIDEO_CAPS: &str = "testvideocaps";
const ELEMENT_TEST_AUDIO_CAPS: &str = "testaudiocaps";

/// Deterministic flow id for an input on this gateway.
pub fn flow_id(gateway_id: &str, input_id: &str) -> String {
    Uuid::new_v5(
        &FLOW_ID_NAMESPACE,
        format!("{gateway_id}/{input_id}").as_bytes(),
    )
    .to_string()
}

/// Whether the flow Strom holds differs from the one the gateway wants.
///
/// Compares only what the gateway owns — block ids, definitions and properties,
/// element ids, types and properties, and the link set — and deliberately ignores
/// everything Strom owns or derives: `running`, `gst_state`, `properties`, editor
/// positions (Strom runs auto-layout and rewrites them), and any extra block fields
/// such as `computed_external_pads`. Without this the agent would rewrite the flow on
/// every tick, and each rewrite drops the fields Strom set.
pub fn differs(existing: &Value, desired: &Value) -> bool {
    if existing.get("name") != desired.get("name") {
        return true;
    }
    shape_of(existing) != shape_of(desired)
}

/// The gateway-owned shape of a flow, in a form that compares cleanly.
fn shape_of(flow: &Value) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let mut blocks: Vec<Value> = flow
        .get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| {
                    json!({
                        "id": b.get("id"),
                        "block_definition_id": b.get("block_definition_id"),
                        "properties": b.get("properties"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    blocks.sort_by_key(|b| b["id"].as_str().unwrap_or_default().to_string());

    let mut elements: Vec<Value> = flow
        .get("elements")
        .and_then(Value::as_array)
        .map(|elements| {
            elements
                .iter()
                .map(|e| {
                    json!({
                        "id": e.get("id"),
                        "element_type": e.get("element_type"),
                        "properties": e.get("properties"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    elements.sort_by_key(|e| e["id"].as_str().unwrap_or_default().to_string());

    let mut links: Vec<String> = flow
        .get("links")
        .and_then(Value::as_array)
        .map(|links| {
            links
                .iter()
                .map(|l| {
                    format!(
                        "{}->{}",
                        l.get("from").and_then(Value::as_str).unwrap_or_default(),
                        l.get("to").and_then(Value::as_str).unwrap_or_default()
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    links.sort();

    (blocks, elements, links)
}

/// Copies Strom-owned lifecycle state from the stored flow onto an update body, so
/// updating a flow does not stop it auto-restarting after a reboot or make Strom
/// report a live flow as stopped.
pub fn preserving_strom_state(desired: &Value, existing: &Value) -> Value {
    let mut body = desired.clone();
    for field in ["properties", "running", "gst_state"] {
        if let Some(value) = existing.get(field) {
            body[field] = value.clone();
        }
    }
    body
}

/// Builds the complete flow JSON for one input.
pub fn build(gateway_id: &str, gateway_name: &str, input: &InputConfig) -> Result<Value> {
    let id = flow_id(gateway_id, input.id.as_str());
    let (mut blocks, elements, mut links, has_audio) = capture_stage(&input.capture)?;

    blocks.push(json!({
        "id": BLOCK_ENCODER,
        "block_definition_id": "builtin.videoenc",
        "name": "Encoder",
        "properties": {
            "codec": input.video.codec,
            "encoder_preference": input.video.encoder_preference,
            "bitrate": input.video.bitrate_kbps,
            "quality_preset": input.video.quality_preset,
            "tune": input.video.tune,
            "rate_control": input.video.rate_control,
            "keyframe_interval": input.video.keyframe_interval,
        },
        "position": { "x": 250.0, "y": 0.0 },
    }));

    blocks.push(json!({
        "id": BLOCK_UPLINK,
        "block_definition_id": "builtin.mpegtssrt_output",
        "name": "SRT Uplink",
        "properties": {
            "srt_uri": input.uplink.venue_uri(),
            "latency": input.uplink.latency_ms,
            "num_video_tracks": 1u32,
            "num_audio_tracks": if has_audio { 1u32 } else { 0u32 },
            // wait_for_connection and auto_reconnect are left at Strom's defaults
            // (false / true): the flow must come up before the cloud side is
            // listening, and the block reconnects on its own afterwards.
        },
        "position": { "x": 500.0, "y": 0.0 },
    }));

    links.push(link(BLOCK_ENCODER, "encoded_out", BLOCK_UPLINK, "video_in"));

    Ok(json!({
        "id": id,
        "name": format!("{} — {} uplink", gateway_name, input.id),
        "elements": elements,
        "blocks": blocks,
        "links": links,
    }))
}

/// Builds the capture stage and its links into the encoder and uplink.
///
/// Returns the blocks, raw elements, links, and whether an audio path exists.
type CaptureStage = (Vec<Value>, Vec<Value>, Vec<Value>, bool);

fn capture_stage(capture: &CaptureConfig) -> Result<CaptureStage> {
    let mut blocks = Vec::new();
    let mut elements = Vec::new();
    let mut links = Vec::new();

    let has_audio = match capture {
        CaptureConfig::Decklink {
            device_number,
            connection,
            mode,
            video_format,
            drop_no_signal_frames,
            audio,
        } => {
            blocks.push(json!({
                "id": BLOCK_CAPTURE,
                "block_definition_id": "builtin.decklink_input",
                "name": "SDI Capture",
                "properties": {
                    "device_number": device_number,
                    "connection": connection,
                    "mode": mode,
                    "video_format": video_format,
                    "drop_no_signal_frames": drop_no_signal_frames,
                    "stream_mode": if *audio { "audio_video" } else { "video" },
                },
                "position": { "x": 0.0, "y": 0.0 },
            }));
            links.push(link(BLOCK_CAPTURE, "video_out", BLOCK_ENCODER, "video_in"));
            if *audio {
                links.push(link(BLOCK_CAPTURE, "audio_out", BLOCK_UPLINK, "audio_in_0"));
            }
            *audio
        }

        CaptureConfig::Local {
            video_device,
            video_resolution,
            video_framerate,
            audio_device,
            audio_channels,
            audio_rate,
        } => {
            let audio = audio_device.as_deref().is_some_and(|d| !d.is_empty());
            let mut properties = json!({
                "video_resolution": video_resolution,
                "video_framerate": video_framerate,
                "stream_mode": if audio { "audio_video" } else { "video" },
            });
            // Omitted rather than sent empty: Strom falls back to autovideosrc and
            // picks the OS default device, which is what a single-camera box wants.
            if let Some(device) = video_device.as_deref().filter(|d| !d.is_empty()) {
                properties["video_device"] = json!(device);
            }
            if let Some(device) = audio_device.as_deref().filter(|d| !d.is_empty()) {
                properties["audio_device"] = json!(device);
                properties["audio_channels"] = json!(audio_channels);
                properties["audio_rate"] = json!(audio_rate);
            }

            blocks.push(json!({
                "id": BLOCK_CAPTURE,
                "block_definition_id": "builtin.local_input",
                "name": "USB Capture",
                "properties": properties,
                "position": { "x": 0.0, "y": 0.0 },
            }));
            links.push(link(BLOCK_CAPTURE, "video_out", BLOCK_ENCODER, "video_in"));
            if audio {
                links.push(link(BLOCK_CAPTURE, "audio_out", BLOCK_UPLINK, "audio_in_0"));
            }
            audio
        }

        // Raw elements rather than a block: Strom has no test-source block, and a
        // commissioning feed does not need one. The capsfilters are not optional —
        // without them videotestsrc negotiates 320x240 and audiotestsrc 44.1 kHz
        // mono, which tells you nothing about a real 1080p25 uplink.
        CaptureConfig::Test {
            video_resolution,
            video_framerate,
            audio_rate,
            audio_channels,
        } => {
            let (width, height) = parse_resolution(video_resolution)?;

            elements.push(json!({
                "id": ELEMENT_TEST_VIDEO,
                "element_type": "videotestsrc",
                "properties": { "is-live": true, "pattern": "smpte" },
                "position": [0.0, 0.0],
            }));
            elements.push(json!({
                "id": ELEMENT_TEST_VIDEO_CAPS,
                "element_type": "capsfilter",
                "properties": {
                    "caps": format!(
                        "video/x-raw,width={width},height={height},framerate={video_framerate}"
                    ),
                },
                "position": [120.0, 0.0],
            }));
            elements.push(json!({
                "id": ELEMENT_TEST_AUDIO,
                "element_type": "audiotestsrc",
                "properties": { "is-live": true, "wave": "sine", "freq": 1000.0 },
                "position": [0.0, 150.0],
            }));
            elements.push(json!({
                "id": ELEMENT_TEST_AUDIO_CAPS,
                "element_type": "capsfilter",
                "properties": {
                    "caps": format!("audio/x-raw,rate={audio_rate},channels={audio_channels}"),
                },
                "position": [120.0, 150.0],
            }));

            links.push(link(
                ELEMENT_TEST_VIDEO,
                "src",
                ELEMENT_TEST_VIDEO_CAPS,
                "sink",
            ));
            links.push(link(
                ELEMENT_TEST_VIDEO_CAPS,
                "src",
                BLOCK_ENCODER,
                "video_in",
            ));
            links.push(link(
                ELEMENT_TEST_AUDIO,
                "src",
                ELEMENT_TEST_AUDIO_CAPS,
                "sink",
            ));
            links.push(link(
                ELEMENT_TEST_AUDIO_CAPS,
                "src",
                BLOCK_UPLINK,
                "audio_in_0",
            ));
            true
        }
    };

    if blocks.is_empty() && elements.is_empty() {
        bail!("capture stage produced no blocks or elements");
    }

    Ok((blocks, elements, links, has_audio))
}

/// Splits a `WxH` resolution string.
fn parse_resolution(resolution: &str) -> Result<(u32, u32)> {
    let (w, h) = resolution
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("resolution must be WxH, got {resolution}"))?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

fn link(from_id: &str, from_pad: &str, to_id: &str, to_pad: &str) -> Value {
    json!({
        "from": format!("{from_id}:{from_pad}"),
        "to": format!("{to_id}:{to_pad}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_live_gateway_types::config::{UplinkConfig, VideoConfig};

    fn input(capture: CaptureConfig) -> InputConfig {
        InputConfig {
            id: "cam1".to_string(),
            name: None,
            capture,
            video: VideoConfig::default(),
            uplink: UplinkConfig {
                mode: open_live_gateway_types::config::UplinkMode::Caller,
                host: "strom.example.com".to_string(),
                public_host: None,
                port: 9000,
                latency_ms: 200,
                passphrase: None,
                pbkeylen: None,
                stream_id: None,
            },
            enabled: true,
        }
    }

    fn test_capture() -> CaptureConfig {
        CaptureConfig::Test {
            video_resolution: "1920x1080".to_string(),
            video_framerate: "25/1".to_string(),
            audio_rate: 48000,
            audio_channels: 2,
        }
    }

    fn decklink(audio: bool) -> CaptureConfig {
        CaptureConfig::Decklink {
            device_number: 0,
            connection: "sdi".to_string(),
            mode: "1080p25".to_string(),
            video_format: "auto".to_string(),
            drop_no_signal_frames: true,
            audio,
        }
    }

    fn links_of(flow: &Value) -> Vec<(String, String)> {
        flow["links"]
            .as_array()
            .expect("links array")
            .iter()
            .map(|l| {
                (
                    l["from"].as_str().unwrap().to_string(),
                    l["to"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// The flow id must be stable: it is how the agent finds its own flow after a
    /// reboot without consulting local state.
    #[test]
    fn flow_id_is_deterministic_and_input_scoped() {
        assert_eq!(flow_id("venue-a", "cam1"), flow_id("venue-a", "cam1"));
        assert_ne!(flow_id("venue-a", "cam1"), flow_id("venue-a", "cam2"));
        assert_ne!(flow_id("venue-a", "cam1"), flow_id("venue-b", "cam1"));
        assert!(Uuid::parse_str(&flow_id("venue-a", "cam1")).is_ok());
    }

    #[test]
    fn decklink_flow_wires_video_through_the_encoder_and_audio_around_it() {
        let flow = build("venue-a", "Venue A", &input(decklink(true))).unwrap();
        let links = links_of(&flow);

        assert!(links.contains(&("capture:video_out".into(), "encoder:video_in".into())));
        assert!(links.contains(&("encoder:encoded_out".into(), "uplink:video_in".into())));
        // Audio bypasses videoenc, which is video-only, and the TS block encodes it.
        assert!(links.contains(&("capture:audio_out".into(), "uplink:audio_in_0".into())));
        assert_eq!(links.len(), 3);
    }

    /// Declaring an audio track the flow does not deliver leaves the TS output waiting
    /// on a pad that never produces, which stalls the whole pipeline.
    #[test]
    fn video_only_capture_declares_no_audio_track() {
        let flow = build("venue-a", "Venue A", &input(decklink(false))).unwrap();
        let uplink = &flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "uplink")
            .unwrap()["properties"];

        assert_eq!(uplink["num_audio_tracks"], 0);
        assert!(!links_of(&flow)
            .iter()
            .any(|(_, to)| to.contains("audio_in_0")));
    }

    #[test]
    fn local_input_without_an_audio_device_is_video_only() {
        let capture = CaptureConfig::Local {
            video_device: Some("usb-camera-0".to_string()),
            video_resolution: "1920x1080".to_string(),
            video_framerate: "25/1".to_string(),
            audio_device: None,
            audio_channels: 2,
            audio_rate: 48000,
        };
        let flow = build("venue-a", "Venue A", &input(capture)).unwrap();
        let capture_block = flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "capture")
            .unwrap()
            .clone();

        assert_eq!(capture_block["block_definition_id"], "builtin.local_input");
        assert_eq!(capture_block["properties"]["stream_mode"], "video");
        assert_eq!(capture_block["properties"]["video_device"], "usb-camera-0");
        assert!(capture_block["properties"].get("audio_device").is_none());
    }

    /// With no device id configured, the property must be absent rather than empty
    /// so Strom's autovideosrc fallback picks the OS default camera.
    #[test]
    fn local_input_without_a_device_id_omits_the_property() {
        let capture = CaptureConfig::Local {
            video_device: None,
            video_resolution: "1280x720".to_string(),
            video_framerate: "25/1".to_string(),
            audio_device: None,
            audio_channels: 2,
            audio_rate: 48000,
        };
        let flow = build("venue-a", "Venue A", &input(capture)).unwrap();
        let properties = flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "capture")
            .unwrap()["properties"]
            .clone();

        assert!(properties.get("video_device").is_none());
        assert_eq!(properties["video_resolution"], "1280x720");
    }

    /// The uplink block must receive the caller URI. Sending it the listener form
    /// would have the venue box bind a port and wait for the cloud to dial in — which
    /// venue NAT makes impossible.
    #[test]
    fn uplink_block_gets_the_caller_uri() {
        let flow = build("venue-a", "Venue A", &input(decklink(true))).unwrap();
        let uri = flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "uplink")
            .unwrap()["properties"]["srt_uri"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(uri.starts_with("srt://strom.example.com:9000?mode=caller"));
        assert!(!uri.contains("listener"));
    }

    #[test]
    fn bitrate_is_passed_in_kbps_as_strom_expects() {
        let flow = build("venue-a", "Venue A", &input(decklink(true))).unwrap();
        let encoder = &flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "encoder")
            .unwrap()["properties"];

        assert_eq!(encoder["bitrate"], 6000);
        assert_eq!(encoder["codec"], "h264");
    }

    #[test]
    fn test_capture_builds_raw_elements_instead_of_a_capture_block() {
        let flow = build("venue-a", "Venue A", &input(test_capture())).unwrap();

        assert_eq!(flow["elements"].as_array().unwrap().len(), 4);
        assert!(!flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == "capture"));
        assert!(links_of(&flow).contains(&("testvideocaps:src".into(), "encoder:video_in".into())));
    }

    /// Without capsfilters the test sources negotiate 320x240 and 44.1 kHz mono,
    /// which exercises nothing like the real uplink it is meant to commission.
    #[test]
    fn test_capture_pins_broadcast_caps() {
        let flow = build("venue-a", "Venue A", &input(test_capture())).unwrap();
        let caps: Vec<String> = flow["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["element_type"] == "capsfilter")
            .map(|e| e["properties"]["caps"].as_str().unwrap().to_string())
            .collect();

        assert!(caps
            .iter()
            .any(|c| c == "video/x-raw,width=1920,height=1080,framerate=25/1"));
        assert!(caps
            .iter()
            .any(|c| c == "audio/x-raw,rate=48000,channels=2"));
    }

    #[test]
    fn a_malformed_test_resolution_is_rejected() {
        let capture = CaptureConfig::Test {
            video_resolution: "1080p".to_string(),
            video_framerate: "25/1".to_string(),
            audio_rate: 48000,
            audio_channels: 2,
        };
        assert!(build("venue-a", "Venue A", &input(capture)).is_err());
    }
}

#[cfg(test)]
mod drift_tests {
    use super::*;
    use open_live_gateway_types::config::{CaptureConfig, UplinkConfig, VideoConfig};

    fn input() -> InputConfig {
        InputConfig {
            id: "cam1".to_string(),
            name: None,
            capture: CaptureConfig::Test {
                video_resolution: "1920x1080".to_string(),
                video_framerate: "25/1".to_string(),
                audio_rate: 48000,
                audio_channels: 2,
            },
            video: VideoConfig::default(),
            uplink: UplinkConfig {
                mode: open_live_gateway_types::config::UplinkMode::Caller,
                host: "strom.example.com".to_string(),
                public_host: None,
                port: 9000,
                latency_ms: 200,
                passphrase: None,
                pbkeylen: None,
                stream_id: None,
            },
            enabled: true,
        }
    }

    #[test]
    fn an_unchanged_flow_does_not_drift() {
        let flow = build("venue-a", "Venue A", &input()).unwrap();
        assert!(!differs(&flow, &flow));
    }

    /// Strom echoes back its own lifecycle state, runs auto-layout over editor
    /// positions, and adds computed pads. None of that is drift — treating it as
    /// drift would make the agent rewrite the flow on every single tick.
    #[test]
    fn strom_owned_fields_are_not_drift() {
        let desired = build("venue-a", "Venue A", &input()).unwrap();
        let mut stored = desired.clone();

        stored["running"] = json!(true);
        stored["gst_state"] = json!("Playing");
        stored["properties"] =
            json!({ "auto_restart": true, "started_at": "2026-09-07T12:00:00Z" });
        for block in stored["blocks"].as_array_mut().unwrap() {
            block["position"] = json!({ "x": 1234.0, "y": 567.0 });
            block["computed_external_pads"] = json!({ "inputs": [], "outputs": [] });
        }
        stored["blocks"].as_array_mut().unwrap().reverse();

        assert!(!differs(&stored, &desired));
    }

    #[test]
    fn a_changed_bitrate_is_drift() {
        let desired = build("venue-a", "Venue A", &input()).unwrap();
        let mut stored = desired.clone();
        stored["blocks"][0]["properties"]["bitrate"] = json!(2000);

        assert!(differs(&stored, &desired));
    }

    #[test]
    fn a_changed_uplink_uri_is_drift() {
        let desired = build("venue-a", "Venue A", &input()).unwrap();
        let mut stored = desired.clone();
        for block in stored["blocks"].as_array_mut().unwrap() {
            if block["id"] == "uplink" {
                block["properties"]["srt_uri"] = json!("srt://other.example.com:9000?mode=caller");
            }
        }

        assert!(differs(&stored, &desired));
    }

    #[test]
    fn a_missing_link_is_drift() {
        let desired = build("venue-a", "Venue A", &input()).unwrap();
        let mut stored = desired.clone();
        stored["links"].as_array_mut().unwrap().pop();

        assert!(differs(&stored, &desired));
    }

    /// The whole point of the fix: an update must carry Strom's lifecycle fields
    /// back, or the flow stops auto-restarting and Strom reports it as stopped.
    #[test]
    fn an_update_body_preserves_strom_state() {
        let desired = build("venue-a", "Venue A", &input()).unwrap();
        let mut stored = desired.clone();
        stored["properties"] =
            json!({ "auto_restart": true, "started_at": "2026-09-07T12:00:00Z" });
        stored["running"] = json!(true);
        stored["gst_state"] = json!("Playing");

        let body = preserving_strom_state(&desired, &stored);

        assert_eq!(body["properties"]["auto_restart"], true);
        assert_eq!(body["running"], true);
        assert_eq!(body["gst_state"], "Playing");
        // And still carries the gateway's own desired shape.
        assert_eq!(body["links"], desired["links"]);
    }

    #[test]
    fn preserving_state_from_a_flow_without_properties_is_harmless() {
        let desired = build("venue-a", "Venue A", &input()).unwrap();
        let body = preserving_strom_state(&desired, &json!({}));
        assert_eq!(body, desired);
    }
}
