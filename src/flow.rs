//! The flow the gateway asks Strom to run for one input.
//!
//! ```text
//!   builtin.local_input ── video_out ──> builtin.videoenc ── encoded_out ──> builtin.mpegtssrt_output
//!   (or videotestsrc/audiotestsrc)                                           audio_in_0 <── test tone
//! ```
//!
//! Block ids and property names are Strom's, sent as strings over HTTP, so a rename
//! upstream fails at runtime rather than at compile time. `GET /api/blocks` on a live
//! Strom is the authority.

use crate::config::{parse_resolution, Endpoint, Video};
use serde_json::{json, Value};
use uuid::Uuid;

/// Namespace for flow ids: the same gateway id and input id always name the same
/// flow, so nothing has to be stored to find it again.
const NAMESPACE: Uuid = Uuid::from_bytes([
    0x6f, 0x70, 0x65, 0x6e, 0x2d, 0x6c, 0x69, 0x76, 0x65, 0x2d, 0x67, 0x77, 0x00, 0x00, 0x00, 0x01,
]);

/// The input id of the test pattern, which has no device.
pub const TEST_INPUT_ID: &str = "test";

const BLOCK_CAPTURE: &str = "capture";
const BLOCK_ENCODER: &str = "encoder";
const BLOCK_UPLINK: &str = "uplink";

pub fn flow_id(gateway_id: &str, input_id: &str) -> String {
    Uuid::new_v5(&NAMESPACE, format!("{gateway_id}/{input_id}").as_bytes()).to_string()
}

/// A stable input id for a device, so the same camera addresses the same flow and
/// the same source across runs. Strom ids look like `dev-d24eb9fb6733c220`.
pub fn device_input_id(device_id: &str) -> String {
    let trimmed = device_id.strip_prefix("dev-").unwrap_or(device_id);
    let cleaned: String = trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("dev-{cleaned}")
}

/// What an input captures.
#[derive(Debug, Clone)]
pub enum Source {
    /// A device Strom can see, by its discovery id. Video only: a webcam's
    /// microphone is a separate device on its own clock and drifts against the
    /// video over a long show. Embedded audio means SDI.
    Device {
        device_id: String,
        resolution: Option<String>,
        framerate: Option<String>,
    },
    /// SMPTE bars and a 1 kHz tone, for commissioning a link before cameras arrive.
    Test {
        resolution: String,
        framerate: String,
    },
}

#[derive(Debug, Clone)]
pub struct Input {
    pub id: String,
    /// Both the flow name in Strom and the source name in Open Live.
    pub name: String,
    pub source: Source,
    pub endpoint: Endpoint,
}

pub fn build(gateway_id: &str, input: &Input, video: &Video) -> Value {
    let mut blocks = Vec::new();
    let mut elements = Vec::new();
    let mut links = Vec::new();

    let has_audio = match &input.source {
        Source::Device {
            device_id,
            resolution,
            framerate,
        } => {
            let mut properties = json!({ "stream_mode": "video", "video_device": device_id });
            if let Some(res) = resolution.as_deref().filter(|r| !r.is_empty()) {
                properties["video_resolution"] = json!(res);
            }
            if let Some(rate) = framerate.as_deref().filter(|r| !r.is_empty()) {
                properties["video_framerate"] = json!(rate);
            }
            blocks.push(json!({
                "id": BLOCK_CAPTURE,
                "block_definition_id": "builtin.local_input",
                "name": "Capture",
                "properties": properties,
                "position": { "x": 0.0, "y": 0.0 },
            }));
            links.push(link(BLOCK_CAPTURE, "video_out", BLOCK_ENCODER, "video_in"));
            false
        }
        // Raw elements: Strom has no test-source block. The capsfilters are not
        // optional; without them the sources negotiate 320x240 and 44.1 kHz mono.
        Source::Test {
            resolution,
            framerate,
        } => {
            let (width, height) = parse_resolution(resolution).unwrap_or((1920, 1080));
            elements.push(json!({
                "id": "testvideo", "element_type": "videotestsrc",
                "properties": { "is-live": true, "pattern": "smpte" }, "position": [0.0, 0.0],
            }));
            elements.push(json!({
                "id": "testvideocaps", "element_type": "capsfilter",
                "properties": { "caps": format!("video/x-raw,width={width},height={height},framerate={framerate}") },
                "position": [120.0, 0.0],
            }));
            elements.push(json!({
                "id": "testaudio", "element_type": "audiotestsrc",
                "properties": { "is-live": true, "wave": "sine", "freq": 1000.0 }, "position": [0.0, 150.0],
            }));
            elements.push(json!({
                "id": "testaudiocaps", "element_type": "capsfilter",
                "properties": { "caps": "audio/x-raw,rate=48000,channels=2" }, "position": [120.0, 150.0],
            }));
            links.push(link("testvideo", "src", "testvideocaps", "sink"));
            links.push(link("testvideocaps", "src", BLOCK_ENCODER, "video_in"));
            links.push(link("testaudio", "src", "testaudiocaps", "sink"));
            links.push(link("testaudiocaps", "src", BLOCK_UPLINK, "audio_in_0"));
            true
        }
    };

    blocks.push(json!({
        "id": BLOCK_ENCODER,
        "block_definition_id": "builtin.videoenc",
        "name": "Encoder",
        "properties": {
            "codec": video.codec,
            "encoder_preference": video.encoder_preference,
            "bitrate": video.bitrate_kbps,
            "quality_preset": video.quality_preset,
            "tune": video.tune,
            "rate_control": video.rate_control,
            "keyframe_interval": video.keyframe_interval,
        },
        "position": { "x": 250.0, "y": 0.0 },
    }));

    // Audio goes straight to the TS output, which encodes raw audio to AAC itself.
    // Declaring a track the flow never delivers would stall the pipeline on a pad
    // that never produces, so the count follows the source.
    blocks.push(json!({
        "id": BLOCK_UPLINK,
        "block_definition_id": "builtin.mpegtssrt_output",
        "name": "SRT Uplink",
        "properties": {
            "srt_uri": input.endpoint.venue_uri(),
            "latency": input.endpoint.latency_ms,
            "num_video_tracks": 1u32,
            "num_audio_tracks": if has_audio { 1u32 } else { 0u32 },
        },
        "position": { "x": 500.0, "y": 0.0 },
    }));
    links.push(link(BLOCK_ENCODER, "encoded_out", BLOCK_UPLINK, "video_in"));

    json!({
        "id": flow_id(gateway_id, &input.id),
        "name": input.name,
        "elements": elements,
        "blocks": blocks,
        "links": links,
    })
}

fn link(from_id: &str, from_pad: &str, to_id: &str, to_pad: &str) -> Value {
    json!({ "from": format!("{from_id}:{from_pad}"), "to": format!("{to_id}:{to_pad}") })
}

/// The device a flow captures from, if it has a local-input block with one set.
pub fn capture_device_of(flow: &Value) -> Option<&str> {
    flow.get("blocks")?
        .as_array()?
        .iter()
        .find(|b| {
            b.get("block_definition_id").and_then(Value::as_str) == Some("builtin.local_input")
        })?
        .pointer("/properties/video_device")?
        .as_str()
}

/// The input id a flow would have been built for, if it has the shape of one of ours.
fn input_id_of(flow: &Value) -> Option<String> {
    if let Some(device) = capture_device_of(flow) {
        return Some(device_input_id(device));
    }
    let is_test = flow
        .get("elements")
        .and_then(Value::as_array)
        .is_some_and(|els| {
            els.iter()
                .any(|e| e.get("element_type").and_then(Value::as_str) == Some("videotestsrc"))
        });
    is_test.then(|| TEST_INPUT_ID.to_string())
}

/// Whether a flow is one this gateway built: its id is the one this gateway would
/// derive for its input. No name convention and no stored state needed, and a flow
/// someone built by hand on the same camera is never mistaken for ours.
pub fn is_ours(flow: &Value, gateway_id: &str) -> bool {
    let Some(id) = flow.get("id").and_then(Value::as_str) else {
        return false;
    };
    input_id_of(flow).is_some_and(|input_id| flow_id(gateway_id, &input_id) == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Uplink;

    fn input(source: Source) -> Input {
        Input {
            id: match &source {
                Source::Device { device_id, .. } => device_input_id(device_id),
                Source::Test { .. } => TEST_INPUT_ID.to_string(),
            },
            name: "Venue A — Cam".to_string(),
            source,
            endpoint: Uplink::default().endpoint("strom.example.com", 9000),
        }
    }

    fn device(resolution: Option<&str>, framerate: Option<&str>) -> Source {
        Source::Device {
            device_id: "dev-abc".to_string(),
            resolution: resolution.map(str::to_string),
            framerate: framerate.map(str::to_string),
        }
    }

    fn test_pattern() -> Source {
        Source::Test {
            resolution: "1920x1080".to_string(),
            framerate: "25/1".to_string(),
        }
    }

    fn block<'a>(flow: &'a Value, id: &str) -> &'a Value {
        flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == id)
            .unwrap()
    }

    fn links_of(flow: &Value) -> Vec<(String, String)> {
        flow["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["from"].as_str().unwrap().to_string(),
                    l["to"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// The id is how the gateway finds its own flows again with nothing stored.
    #[test]
    fn flow_ids_are_deterministic_and_scoped() {
        assert_eq!(flow_id("venue-a", "cam1"), flow_id("venue-a", "cam1"));
        assert_ne!(flow_id("venue-a", "cam1"), flow_id("venue-a", "cam2"));
        assert_ne!(flow_id("venue-a", "cam1"), flow_id("venue-b", "cam1"));
    }

    #[test]
    fn device_input_ids_are_stable_and_survive_awkward_device_ids() {
        assert_eq!(
            device_input_id("dev-d24eb9fb6733c220"),
            "dev-d24eb9fb6733c220"
        );
        assert_eq!(device_input_id("v4l2:/dev/video0"), "dev-v4l2--dev-video0");
    }

    #[test]
    fn a_device_flow_wires_video_through_the_encoder_and_declares_no_audio() {
        let flow = build("gw", &input(device(None, None)), &Video::default());
        let links = links_of(&flow);
        assert!(links.contains(&("capture:video_out".into(), "encoder:video_in".into())));
        assert!(links.contains(&("encoder:encoded_out".into(), "uplink:video_in".into())));
        assert_eq!(links.len(), 2);
        assert_eq!(block(&flow, "uplink")["properties"]["num_audio_tracks"], 0);
        assert_eq!(
            block(&flow, "capture")["properties"]["video_device"],
            "dev-abc"
        );
        assert_eq!(flow["id"], flow_id("gw", "dev-abc"));
        assert_eq!(flow["name"], "Venue A — Cam");
    }

    /// The default is "whatever the device offers": a requested format the device
    /// does not advertise fails negotiation instead of being converted.
    #[test]
    fn the_capture_format_is_only_constrained_when_asked() {
        let flow = build("gw", &input(device(None, Some(""))), &Video::default());
        let props = &block(&flow, "capture")["properties"];
        assert!(props.get("video_resolution").is_none());
        assert!(props.get("video_framerate").is_none());

        let flow = build(
            "gw",
            &input(device(Some("1280x720"), Some("25/1"))),
            &Video::default(),
        );
        let props = &block(&flow, "capture")["properties"];
        assert_eq!(props["video_resolution"], "1280x720");
        assert_eq!(props["video_framerate"], "25/1");
    }

    #[test]
    fn the_uplink_block_gets_the_venue_uri_and_the_video_settings_land_on_the_encoder() {
        let video = Video {
            bitrate_kbps: 4000,
            ..Video::default()
        };
        let flow = build("gw", &input(device(None, None)), &video);
        assert_eq!(
            block(&flow, "uplink")["properties"]["srt_uri"],
            "srt://strom.example.com:9000?mode=caller&latency=200"
        );
        assert_eq!(block(&flow, "encoder")["properties"]["bitrate"], 4000);
    }

    /// Without capsfilters the test sources negotiate 320x240 and 44.1 kHz mono,
    /// which commissions nothing like a real feed.
    #[test]
    fn the_test_pattern_uses_raw_elements_with_pinned_caps_and_a_tone() {
        let flow = build("gw", &input(test_pattern()), &Video::default());
        assert_eq!(flow["elements"].as_array().unwrap().len(), 4);
        assert!(flow["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["id"] != "capture"));
        let caps: Vec<&str> = flow["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["properties"]["caps"].as_str())
            .collect();
        assert!(caps.contains(&"video/x-raw,width=1920,height=1080,framerate=25/1"));
        assert!(caps.contains(&"audio/x-raw,rate=48000,channels=2"));
        assert_eq!(block(&flow, "uplink")["properties"]["num_audio_tracks"], 1);
        assert!(links_of(&flow).contains(&("testaudiocaps:src".into(), "uplink:audio_in_0".into())));
    }

    /// Ownership is by derived id, so `down` and `status` need no stored state, and a
    /// hand-built flow on the same camera is left alone.
    #[test]
    fn a_flow_is_ours_only_when_its_id_is_the_one_we_would_derive() {
        let video = Video::default();
        let ours = build("gw", &input(device(None, None)), &video);
        assert!(is_ours(&ours, "gw"));
        assert!(!is_ours(&ours, "other-gateway"));

        let test = build("gw", &input(test_pattern()), &video);
        assert!(is_ours(&test, "gw"));

        let mut hand_built = ours.clone();
        hand_built["id"] = json!("11111111-2222-3333-4444-555555555555");
        assert!(!is_ours(&hand_built, "gw"));

        let unrelated = json!({ "id": flow_id("gw", "dev-abc"), "blocks": [] });
        assert!(
            !is_ours(&unrelated, "gw"),
            "a flow with no capture is not one of ours"
        );
    }

    #[test]
    fn the_capture_device_is_read_from_the_local_input_block() {
        let flow = build("gw", &input(device(None, None)), &Video::default());
        assert_eq!(capture_device_of(&flow), Some("dev-abc"));
        assert_eq!(
            capture_device_of(&build("gw", &input(test_pattern()), &Video::default())),
            None
        );
    }
}
