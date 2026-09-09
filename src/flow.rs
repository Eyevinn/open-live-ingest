//! The flow the gateway asks Strom to run for one input.
//!
//! ```text
//!   builtin.local_input ── video_out ──> builtin.videoenc ── encoded_out ──> builtin.mpegtssrt_output
//!   (or videotestsrc/audiotestsrc)                                           audio_in_0 <── test tone
//! ```
//!
//! The flow's structure is typed through `strom-types`. Block definition ids and
//! property names are still Strom's strings: they live in each block's builder in
//! the backend, so a rename there fails at runtime rather than at compile time.
//! `GET /api/blocks` on a live Strom is the authority.

use crate::config::{parse_resolution, Endpoint, Video};
use std::collections::HashMap;
use strom_types::block::Position;
use strom_types::flow::FlowProperties;
use strom_types::{BlockInstance, Element, Flow, FlowId, Link, PropertyValue};
use uuid::Uuid;

/// Namespace for flow ids: the same gateway id and input id always name the same
/// flow, so nothing has to be stored to find it again.
const NAMESPACE: Uuid = Uuid::from_bytes([
    0x6f, 0x70, 0x65, 0x6e, 0x2d, 0x6c, 0x69, 0x76, 0x65, 0x2d, 0x67, 0x77, 0x00, 0x00, 0x00, 0x01,
]);

/// The input id of the test pattern, which has no device.
pub const TEST_INPUT_ID: &str = "test";

const LOCAL_INPUT: &str = "builtin.local_input";
const VIDEOENC: &str = "builtin.videoenc";
const MPEGTSSRT_OUTPUT: &str = "builtin.mpegtssrt_output";

const BLOCK_CAPTURE: &str = "capture";
const BLOCK_ENCODER: &str = "encoder";
const BLOCK_UPLINK: &str = "uplink";

pub fn flow_id(gateway_id: &str, input_id: &str) -> FlowId {
    Uuid::new_v5(&NAMESPACE, format!("{gateway_id}/{input_id}").as_bytes())
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

pub fn build(gateway_id: &str, input: &Input, video: &Video) -> Flow {
    let mut blocks = Vec::new();
    let mut elements = Vec::new();
    let mut links = Vec::new();

    let has_audio = match &input.source {
        Source::Device {
            device_id,
            resolution,
            framerate,
        } => {
            let mut properties = props([
                ("stream_mode", "video".into()),
                ("video_device", device_id.as_str().into()),
            ]);
            if let Some(res) = resolution.as_deref().filter(|r| !r.is_empty()) {
                properties.insert("video_resolution".into(), res.into());
            }
            if let Some(rate) = framerate.as_deref().filter(|r| !r.is_empty()) {
                properties.insert("video_framerate".into(), rate.into());
            }
            blocks.push(block(
                BLOCK_CAPTURE,
                LOCAL_INPUT,
                "Capture",
                properties,
                0.0,
            ));
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
            elements.push(element(
                "testvideo",
                "videotestsrc",
                props([
                    ("is-live", PropertyValue::Bool(true)),
                    ("pattern", "smpte".into()),
                ]),
                (0.0, 0.0),
            ));
            elements.push(element(
                "testvideocaps",
                "capsfilter",
                props([(
                    "caps",
                    format!("video/x-raw,width={width},height={height},framerate={framerate}")
                        .into(),
                )]),
                (120.0, 0.0),
            ));
            elements.push(element(
                "testaudio",
                "audiotestsrc",
                props([
                    ("is-live", PropertyValue::Bool(true)),
                    ("wave", "sine".into()),
                    ("freq", PropertyValue::Float(1000.0)),
                ]),
                (0.0, 150.0),
            ));
            elements.push(element(
                "testaudiocaps",
                "capsfilter",
                props([("caps", "audio/x-raw,rate=48000,channels=2".into())]),
                (120.0, 150.0),
            ));
            links.push(link("testvideo", "src", "testvideocaps", "sink"));
            links.push(link("testvideocaps", "src", BLOCK_ENCODER, "video_in"));
            links.push(link("testaudio", "src", "testaudiocaps", "sink"));
            links.push(link("testaudiocaps", "src", BLOCK_UPLINK, "audio_in_0"));
            true
        }
    };

    blocks.push(block(
        BLOCK_ENCODER,
        VIDEOENC,
        "Encoder",
        props([
            ("codec", video.codec.as_str().into()),
            (
                "encoder_preference",
                video.encoder_preference.as_str().into(),
            ),
            ("bitrate", uint(video.bitrate_kbps)),
            ("quality_preset", video.quality_preset.as_str().into()),
            ("tune", video.tune.as_str().into()),
            ("rate_control", video.rate_control.as_str().into()),
            ("keyframe_interval", uint(video.keyframe_interval)),
        ]),
        250.0,
    ));

    // Audio goes straight to the TS output, which encodes raw audio to AAC itself.
    // Declaring a track the flow never delivers would stall the pipeline on a pad
    // that never produces, so the count follows the source.
    blocks.push(block(
        BLOCK_UPLINK,
        MPEGTSSRT_OUTPUT,
        "SRT Uplink",
        props([
            ("srt_uri", input.endpoint.venue_uri().into()),
            ("latency", uint(input.endpoint.latency_ms)),
            ("num_video_tracks", uint(1)),
            ("num_audio_tracks", uint(if has_audio { 1 } else { 0 })),
        ]),
        500.0,
    ));
    links.push(link(BLOCK_ENCODER, "encoded_out", BLOCK_UPLINK, "video_in"));

    Flow {
        id: flow_id(gateway_id, &input.id),
        name: input.name.clone(),
        elements,
        blocks,
        links,
        running: false,
        gst_state: None,
        properties: FlowProperties::default(),
    }
}

fn props<const N: usize>(pairs: [(&str, PropertyValue); N]) -> HashMap<String, PropertyValue> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn uint(n: u32) -> PropertyValue {
    PropertyValue::UInt(u64::from(n))
}

fn block(
    id: &str,
    definition: &str,
    name: &str,
    properties: HashMap<String, PropertyValue>,
    x: f32,
) -> BlockInstance {
    BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: Some(name.to_string()),
        properties,
        position: Position { x, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

fn element(
    id: &str,
    element_type: &str,
    properties: HashMap<String, PropertyValue>,
    position: (f32, f32),
) -> Element {
    Element {
        id: id.to_string(),
        element_type: element_type.to_string(),
        properties,
        pad_properties: HashMap::new(),
        position,
    }
}

fn link(from_id: &str, from_pad: &str, to_id: &str, to_pad: &str) -> Link {
    Link {
        from: format!("{from_id}:{from_pad}"),
        to: format!("{to_id}:{to_pad}"),
    }
}

/// The device a flow captures from, if it has a local-input block with one set.
pub fn capture_device_of(flow: &Flow) -> Option<&str> {
    match flow
        .blocks
        .iter()
        .find(|b| b.block_definition_id == LOCAL_INPUT)?
        .properties
        .get("video_device")?
    {
        PropertyValue::String(device) if !device.is_empty() => Some(device),
        _ => None,
    }
}

/// The input id a flow would have been built for, if it has the shape of one of ours.
fn input_id_of(flow: &Flow) -> Option<String> {
    if let Some(device) = capture_device_of(flow) {
        return Some(device_input_id(device));
    }
    let is_test = flow
        .elements
        .iter()
        .any(|e| e.element_type == "videotestsrc");
    is_test.then(|| TEST_INPUT_ID.to_string())
}

/// Whether a flow is one this gateway built: its id is the one this gateway would
/// derive for its input. No name convention and no stored state needed, and a flow
/// someone built by hand on the same camera is never mistaken for ours.
pub fn is_ours(flow: &Flow, gateway_id: &str) -> bool {
    input_id_of(flow).is_some_and(|input_id| flow_id(gateway_id, &input_id) == flow.id)
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

    fn block_of<'a>(flow: &'a Flow, id: &str) -> &'a BlockInstance {
        flow.blocks.iter().find(|b| b.id == id).unwrap()
    }

    fn text(block: &BlockInstance, key: &str) -> Option<String> {
        match block.properties.get(key)? {
            PropertyValue::String(s) => Some(s.clone()),
            other => panic!("{key} is not a string: {other:?}"),
        }
    }

    fn number(block: &BlockInstance, key: &str) -> u64 {
        match block.properties.get(key) {
            Some(PropertyValue::UInt(n)) => *n,
            other => panic!("{key} is not a number: {other:?}"),
        }
    }

    fn links_of(flow: &Flow) -> Vec<(String, String)> {
        flow.links
            .iter()
            .map(|l| (l.from.clone(), l.to.clone()))
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
        assert_eq!(number(block_of(&flow, "uplink"), "num_audio_tracks"), 0);
        assert_eq!(
            text(block_of(&flow, "capture"), "video_device").as_deref(),
            Some("dev-abc")
        );
        assert_eq!(block_of(&flow, "capture").block_definition_id, LOCAL_INPUT);
        assert_eq!(flow.id, flow_id("gw", "dev-abc"));
        assert_eq!(flow.name, "Venue A — Cam");
    }

    /// The default is "whatever the device offers": a requested format the device
    /// does not advertise fails negotiation instead of being converted.
    #[test]
    fn the_capture_format_is_only_constrained_when_asked() {
        let flow = build("gw", &input(device(None, Some(""))), &Video::default());
        let capture = block_of(&flow, "capture");
        assert!(text(capture, "video_resolution").is_none());
        assert!(text(capture, "video_framerate").is_none());

        let flow = build(
            "gw",
            &input(device(Some("1280x720"), Some("25/1"))),
            &Video::default(),
        );
        let capture = block_of(&flow, "capture");
        assert_eq!(
            text(capture, "video_resolution").as_deref(),
            Some("1280x720")
        );
        assert_eq!(text(capture, "video_framerate").as_deref(), Some("25/1"));
    }

    #[test]
    fn the_uplink_block_gets_the_venue_uri_and_the_video_settings_land_on_the_encoder() {
        let video = Video {
            bitrate_kbps: 4000,
            ..Video::default()
        };
        let flow = build("gw", &input(device(None, None)), &video);
        assert_eq!(
            text(block_of(&flow, "uplink"), "srt_uri").as_deref(),
            Some("srt://strom.example.com:9000?mode=caller&latency=200")
        );
        assert_eq!(number(block_of(&flow, "encoder"), "bitrate"), 4000);
    }

    /// Without capsfilters the test sources negotiate 320x240 and 44.1 kHz mono,
    /// which commissions nothing like a real feed.
    #[test]
    fn the_test_pattern_uses_raw_elements_with_pinned_caps_and_a_tone() {
        let flow = build("gw", &input(test_pattern()), &Video::default());
        assert_eq!(flow.elements.len(), 4);
        assert!(flow.blocks.iter().all(|b| b.id != "capture"));
        let caps: Vec<&str> = flow
            .elements
            .iter()
            .filter_map(|e| match e.properties.get("caps") {
                Some(PropertyValue::String(c)) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert!(caps.contains(&"video/x-raw,width=1920,height=1080,framerate=25/1"));
        assert!(caps.contains(&"audio/x-raw,rate=48000,channels=2"));
        assert_eq!(number(block_of(&flow, "uplink"), "num_audio_tracks"), 1);
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
        assert!(is_ours(&build("gw", &input(test_pattern()), &video), "gw"));

        let mut hand_built = ours.clone();
        hand_built.id = Uuid::from_u128(1);
        assert!(!is_ours(&hand_built, "gw"));

        let mut unrelated = ours.clone();
        unrelated.blocks.clear();
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

    /// What goes over the wire is what Strom's own types serialise, so a flow must
    /// survive a round trip through JSON unchanged in the fields the gateway owns.
    #[test]
    fn a_built_flow_round_trips_through_json() {
        let flow = build(
            "gw",
            &input(device(Some("1280x720"), None)),
            &Video::default(),
        );
        let json = serde_json::to_string(&flow).unwrap();
        let back: Flow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, flow.id);
        assert_eq!(back.blocks.len(), 3);
        assert_eq!(links_of(&back), links_of(&flow));
        assert!(is_ours(&back, "gw"));
    }
}
