//! The flow the gateway asks Strom to run for one input.
//!
//! ```text
//!   builtin.local_input ── video_out ──> builtin.videoenc ── encoded_out ──> builtin.mpegtssrt_output
//!   (or videotestsrc/audiotestsrc)  ── audio_out ─────────────────────────────> audio_in_0
//! ```
//!
//! Audio reaches the uplink only from a paired audio device or the test tone.
//!
//! The desktop app's sessions add two taps ([`Taps::On`]): `builtin.thumbnail`
//! between capture and encoder, `builtin.meter` between the audio and the uplink.
//! In step 2 it runs a [`preview`] flow per ticked row instead: the same capture
//! into those two blocks and a `fakesink` each, nothing encoded or sent.
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

const THUMBNAIL: &str = "builtin.thumbnail";
const METER: &str = "builtin.meter";

/// The picture tap's block id: what `StromClient::thumbnail` is asked for.
pub const BLOCK_PICTURE: &str = "picture";
/// The sound tap's block id: the `element_id` its meter readings carry.
pub const BLOCK_SOUND: &str = "sound";
const PICTURE_SINK: &str = "picture_sink";
const SOUND_SINK: &str = "sound_sink";

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
    /// A device Strom can see, by its discovery id, and optionally an audio source
    /// paired with it (also a discovery id). The audio device runs on its own clock;
    /// see docs/DESIGN.md on drift.
    Device {
        device_id: String,
        audio_device: Option<String>,
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

/// Whether a flow carries the picture and sound taps the desktop app reads its
/// preview from (docs/specs/desktop-app.md §12). `Off` builds exactly the flow the
/// gateway has always built; every caller but the app's sessions uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Taps {
    #[default]
    Off,
    /// `builtin.thumbnail` between the capture and the encoder, and `builtin.meter`
    /// between the capture's audio and the uplink (when there is audio). Both pass
    /// media through unchanged.
    On,
}

pub fn build(gateway_id: &str, input: &Input, video: &Video, taps: Taps) -> Flow {
    let mut blocks = Vec::new();
    let mut elements = Vec::new();
    let mut links = Vec::new();

    let taps = taps == Taps::On;
    let video_to = if taps {
        (BLOCK_PICTURE, "video_in")
    } else {
        (BLOCK_ENCODER, "video_in")
    };
    let audio_to = if taps {
        (BLOCK_SOUND, "audio_in")
    } else {
        (BLOCK_UPLINK, "audio_in_0")
    };
    let has_audio = capture(
        &input.source,
        video_to,
        audio_to,
        &mut blocks,
        &mut elements,
        &mut links,
    );

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

    // The taps sit in line: the picture tee hands the encoder every buffer (its
    // thumbnail branch is attached only when asked, behind a leaky queue), and the
    // level element passes the audio through.
    if taps {
        blocks.push(picture_block((125.0, 150.0)));
        links.push(link(BLOCK_PICTURE, "video_out", BLOCK_ENCODER, "video_in"));
        if has_audio {
            blocks.push(sound_block((375.0, 150.0)));
            links.push(link(BLOCK_SOUND, "audio_out", BLOCK_UPLINK, "audio_in_0"));
        }
    }

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

/// The capture half of a flow: the local-input block (or the test sources), its
/// video linked to `video_to` and its audio, when there is any, to `audio_to`.
/// Whether there is audio.
fn capture(
    source: &Source,
    video_to: (&str, &str),
    audio_to: (&str, &str),
    blocks: &mut Vec<BlockInstance>,
    elements: &mut Vec<Element>,
    links: &mut Vec<Link>,
) -> bool {
    match source {
        Source::Device {
            device_id,
            audio_device,
            resolution,
            framerate,
        } => {
            let audio_device = audio_device.as_deref().filter(|a| !a.is_empty());
            let mut properties = props([
                (
                    "stream_mode",
                    if audio_device.is_some() {
                        "audio_video"
                    } else {
                        "video"
                    }
                    .into(),
                ),
                ("video_device", device_id.as_str().into()),
            ]);
            if let Some(audio) = audio_device {
                properties.insert("audio_device".into(), audio.into());
            }
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
            links.push(link(BLOCK_CAPTURE, "video_out", video_to.0, video_to.1));
            if audio_device.is_some() {
                links.push(link(BLOCK_CAPTURE, "audio_out", audio_to.0, audio_to.1));
            }
            audio_device.is_some()
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
            links.push(link("testvideocaps", "src", video_to.0, video_to.1));
            links.push(link("testaudio", "src", "testaudiocaps", "sink"));
            links.push(link("testaudiocaps", "src", audio_to.0, audio_to.1));
            true
        }
    }
}

/// The input id a source is sent under: the device's stable id, or the test
/// pattern's. What `devices::inputs_for` gives an input, and what a preview flow's
/// id is derived from.
pub fn input_id_for(source: &Source) -> String {
    match source {
        Source::Device { device_id, .. } => device_input_id(device_id),
        Source::Test { .. } => TEST_INPUT_ID.to_string(),
    }
}

/// The id of the desktop app's preview flow for an input: the v5 UUID of
/// `"{gateway_id}/preview/{input_id}"`, in the namespace of [`flow_id`], so it never
/// equals an uplink flow's id and nothing has to be stored to find it again.
pub fn preview_flow_id(gateway_id: &str, input_id: &str) -> FlowId {
    Uuid::new_v5(
        &NAMESPACE,
        format!("{gateway_id}/preview/{input_id}").as_bytes(),
    )
}

/// The desktop app's preview flow for one input in step 2 (docs/specs/desktop-app.md
/// §5.2, §12): the uplink's capture, with the same properties, into a
/// `builtin.thumbnail` and, when there is audio, a `builtin.meter`, each ended by a
/// `fakesink`. No encoder, nothing sent. `name` is the input's name
/// (`<gateway.name> — <label>`); the flow is named after it with ` (preview)`.
pub fn preview(gateway_id: &str, name: &str, source: &Source) -> Flow {
    let mut blocks = Vec::new();
    let mut elements = Vec::new();
    let mut links = Vec::new();

    let has_audio = capture(
        source,
        (BLOCK_PICTURE, "video_in"),
        (BLOCK_SOUND, "audio_in"),
        &mut blocks,
        &mut elements,
        &mut links,
    );

    blocks.push(picture_block((250.0, 0.0)));
    elements.push(fakesink(PICTURE_SINK, (500.0, 0.0)));
    links.push(link(BLOCK_PICTURE, "video_out", PICTURE_SINK, "sink"));
    if has_audio {
        blocks.push(sound_block((250.0, 150.0)));
        elements.push(fakesink(SOUND_SINK, (500.0, 150.0)));
        links.push(link(BLOCK_SOUND, "audio_out", SOUND_SINK, "sink"));
    }

    Flow {
        id: preview_flow_id(gateway_id, &input_id_for(source)),
        name: format!("{name} (preview)"),
        elements,
        blocks,
        links,
        running: false,
        gst_state: None,
        properties: FlowProperties::default(),
    }
}

/// Whether a Strom lists both blocks the preview and the taps need. An older Strom
/// without them gets no preview flows and uplinks built with [`Taps::Off`].
pub fn has_preview_blocks<S: AsRef<str>>(block_ids: &[S]) -> bool {
    [THUMBNAIL, METER]
        .iter()
        .all(|needed| block_ids.iter().any(|id| id.as_ref() == *needed))
}

fn picture_block(position: (f32, f32)) -> BlockInstance {
    block_at(
        BLOCK_PICTURE,
        THUMBNAIL,
        "Picture",
        HashMap::new(),
        position,
    )
}

fn sound_block(position: (f32, f32)) -> BlockInstance {
    // 100 ms is the meter's default and one of the values it lists.
    block_at(
        BLOCK_SOUND,
        METER,
        "Sound",
        props([("interval", "100".into())]),
        position,
    )
}

/// Ends a preview branch, so the pipeline has a sink to reach PLAYING with; without
/// `sync` it never holds a buffer back to the clock.
fn fakesink(id: &str, position: (f32, f32)) -> Element {
    element(
        id,
        "fakesink",
        props([
            ("sync", PropertyValue::Bool(false)),
            ("async", PropertyValue::Bool(false)),
        ]),
        position,
    )
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
    block_at(id, definition, name, properties, (x, 0.0))
}

fn block_at(
    id: &str,
    definition: &str,
    name: &str,
    properties: HashMap<String, PropertyValue>,
    (x, y): (f32, f32),
) -> BlockInstance {
    BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: Some(name.to_string()),
        properties,
        position: Position { x, y },
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

/// Whether a flow is the desktop app's preview flow for one of this gateway's
/// inputs ([`preview`]), by its derived id, as [`is_ours`] recognises an uplink.
/// `prepare` and `down` remove these too: a test pattern's preview holds no camera,
/// so the camera rule would not catch it.
pub fn is_preview_of_ours(flow: &Flow, gateway_id: &str) -> bool {
    input_id_of(flow).is_some_and(|input_id| preview_flow_id(gateway_id, &input_id) == flow.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Uplink;
    use serde_json::json;

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
            audio_device: None,
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
        let flow = build(
            "gw",
            &input(device(None, None)),
            &Video::default(),
            Taps::Off,
        );
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

    #[test]
    fn a_device_flow_without_audio_asks_for_video_only() {
        let flow = build(
            "gw",
            &input(device(None, None)),
            &Video::default(),
            Taps::Off,
        );
        let capture = block_of(&flow, "capture");
        assert_eq!(text(capture, "stream_mode").as_deref(), Some("video"));
        assert!(text(capture, "audio_device").is_none());
    }

    fn with_audio(audio: &str) -> Source {
        Source::Device {
            device_id: "dev-abc".to_string(),
            audio_device: Some(audio.to_string()),
            resolution: None,
            framerate: None,
        }
    }

    /// A paired audio device rides in the same local-input block and goes straight
    /// to the uplink, which encodes it; the track is declared because it is delivered.
    #[test]
    fn a_paired_audio_device_is_captured_and_linked_to_the_uplink() {
        let flow = build(
            "gw",
            &input(with_audio("dev-mic")),
            &Video::default(),
            Taps::Off,
        );
        let capture = block_of(&flow, "capture");
        assert_eq!(text(capture, "stream_mode").as_deref(), Some("audio_video"));
        assert_eq!(text(capture, "audio_device").as_deref(), Some("dev-mic"));
        assert_eq!(text(capture, "video_device").as_deref(), Some("dev-abc"));
        let links = links_of(&flow);
        assert!(links.contains(&("capture:video_out".into(), "encoder:video_in".into())));
        assert!(links.contains(&("capture:audio_out".into(), "uplink:audio_in_0".into())));
        assert!(links.contains(&("encoder:encoded_out".into(), "uplink:video_in".into())));
        assert_eq!(links.len(), 3);
        assert_eq!(number(block_of(&flow, "uplink"), "num_audio_tracks"), 1);
        assert_eq!(number(block_of(&flow, "uplink"), "num_video_tracks"), 1);

        // The flow id follows the camera alone, so pairing a microphone does not
        // orphan the flow an earlier run left.
        assert_eq!(flow.id, flow_id("gw", "dev-abc"));
        assert!(is_ours(&flow, "gw"));
        assert_eq!(capture_device_of(&flow), Some("dev-abc"));

        // What Strom receives: plain strings, under the block's own property names.
        let json = serde_json::to_value(&flow).unwrap();
        let capture_json = json["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "capture")
            .unwrap();
        assert_eq!(capture_json["block_definition_id"], LOCAL_INPUT);
        assert_eq!(capture_json["properties"]["stream_mode"], "audio_video");
        assert_eq!(capture_json["properties"]["audio_device"], "dev-mic");
    }

    /// An empty audio id is no audio, not a request for the OS default microphone.
    #[test]
    fn an_empty_audio_device_leaves_the_flow_video_only() {
        let flow = build("gw", &input(with_audio("")), &Video::default(), Taps::Off);
        assert_eq!(
            text(block_of(&flow, "capture"), "stream_mode").as_deref(),
            Some("video")
        );
        assert_eq!(number(block_of(&flow, "uplink"), "num_audio_tracks"), 0);
        assert_eq!(links_of(&flow).len(), 2);
    }

    /// The default is "whatever the device offers": a requested format the device
    /// does not advertise fails negotiation instead of being converted.
    #[test]
    fn the_capture_format_is_only_constrained_when_asked() {
        let flow = build(
            "gw",
            &input(device(None, Some(""))),
            &Video::default(),
            Taps::Off,
        );
        let capture = block_of(&flow, "capture");
        assert!(text(capture, "video_resolution").is_none());
        assert!(text(capture, "video_framerate").is_none());

        let flow = build(
            "gw",
            &input(device(Some("1280x720"), Some("25/1"))),
            &Video::default(),
            Taps::Off,
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
        let flow = build("gw", &input(device(None, None)), &video, Taps::Off);
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
        let flow = build("gw", &input(test_pattern()), &Video::default(), Taps::Off);
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
        let ours = build("gw", &input(device(None, None)), &video, Taps::Off);
        assert!(is_ours(&ours, "gw"));
        assert!(!is_ours(&ours, "other-gateway"));
        assert!(is_ours(
            &build("gw", &input(test_pattern()), &video, Taps::Off),
            "gw"
        ));

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
        let flow = build(
            "gw",
            &input(device(None, None)),
            &Video::default(),
            Taps::Off,
        );
        assert_eq!(capture_device_of(&flow), Some("dev-abc"));
        assert_eq!(
            capture_device_of(&build(
                "gw",
                &input(test_pattern()),
                &Video::default(),
                Taps::Off
            )),
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
            Taps::Off,
        );
        let json = serde_json::to_string(&flow).unwrap();
        let back: Flow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, flow.id);
        assert_eq!(back.blocks.len(), 3);
        assert_eq!(links_of(&back), links_of(&flow));
        assert!(is_ours(&back, "gw"));
    }

    // -----------------------------------------------------------------------------
    // Taps and the preview flow (docs/specs/desktop-app.md §12, #172)
    // -----------------------------------------------------------------------------

    fn camera_and_mic() -> Source {
        Source::Device {
            device_id: "dev-abc".to_string(),
            audio_device: Some("dev-mic".to_string()),
            resolution: Some("1280x720".to_string()),
            framerate: Some("25/1".to_string()),
        }
    }

    fn uplink_json(source: Source, taps: Taps) -> serde_json::Value {
        serde_json::to_value(build("gw", &input(source), &Video::default(), taps)).unwrap()
    }

    fn fixture(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    /// `Taps::Off` is the flow every caller but the app's sessions builds: the CLI's
    /// `up` must send Strom exactly what it sent before the taps existed. The
    /// fixtures are that flow as built before #172, verbatim.
    #[test]
    fn without_taps_the_uplink_is_the_flow_built_before_taps_existed() {
        assert_eq!(
            uplink_json(device(None, None), Taps::Off),
            fixture(include_str!("../tests/fixtures/uplink_camera.json"))
        );
        assert_eq!(
            uplink_json(camera_and_mic(), Taps::Off),
            fixture(include_str!("../tests/fixtures/uplink_camera_mic.json"))
        );
        assert_eq!(
            uplink_json(test_pattern(), Taps::Off),
            fixture(include_str!("../tests/fixtures/uplink_test_pattern.json"))
        );
        assert_eq!(Taps::default(), Taps::Off);
    }

    /// The same flow with the two blocks inserted in line and nothing else changed:
    /// the blocks, elements and links not touched by the taps are identical.
    fn assert_only_taps_added(source: Source, has_audio: bool) {
        let off = build("gw", &input(source.clone()), &Video::default(), Taps::Off);
        let on = build("gw", &input(source), &Video::default(), Taps::On);
        assert_eq!(on.id, off.id);
        assert_eq!(on.name, off.name);
        assert_eq!(
            serde_json::to_value(&on.elements).unwrap(),
            serde_json::to_value(&off.elements).unwrap()
        );
        let untouched = |f: &Flow| -> Vec<serde_json::Value> {
            f.blocks
                .iter()
                .filter(|b| b.id != BLOCK_PICTURE && b.id != BLOCK_SOUND)
                .map(|b| serde_json::to_value(b).unwrap())
                .collect()
        };
        assert_eq!(untouched(&on), untouched(&off));

        let picture = block_of(&on, BLOCK_PICTURE);
        assert_eq!(picture.block_definition_id, "builtin.thumbnail");
        assert!(picture.properties.is_empty());
        if has_audio {
            let sound = block_of(&on, BLOCK_SOUND);
            assert_eq!(sound.block_definition_id, "builtin.meter");
            assert_eq!(text(sound, "interval").as_deref(), Some("100"));
            assert_eq!(on.blocks.len(), off.blocks.len() + 2);
        } else {
            assert!(on.blocks.iter().all(|b| b.id != BLOCK_SOUND));
            assert_eq!(on.blocks.len(), off.blocks.len() + 1);
        }

        // Each link into the encoder's video or the uplink's audio now goes through
        // the tap, and the tap goes on to where it went.
        let mut expected: Vec<(String, String)> = Vec::new();
        for (from, to) in links_of(&off) {
            if to == "encoder:video_in" {
                expected.push((from, "picture:video_in".into()));
                expected.push(("picture:video_out".into(), to));
            } else if to == "uplink:audio_in_0" {
                expected.push((from, "sound:audio_in".into()));
                expected.push(("sound:audio_out".into(), to));
            } else {
                expected.push((from, to));
            }
        }
        let mut got = links_of(&on);
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn taps_go_in_line_for_a_camera() {
        assert_only_taps_added(device(None, None), false);
        let on = build(
            "gw",
            &input(device(None, None)),
            &Video::default(),
            Taps::On,
        );
        assert!(links_of(&on).contains(&("capture:video_out".into(), "picture:video_in".into())));
        assert!(links_of(&on).contains(&("picture:video_out".into(), "encoder:video_in".into())));
        assert_eq!(number(block_of(&on, "uplink"), "num_audio_tracks"), 0);
    }

    #[test]
    fn taps_go_in_line_for_a_camera_with_a_microphone() {
        assert_only_taps_added(camera_and_mic(), true);
        let on = build("gw", &input(camera_and_mic()), &Video::default(), Taps::On);
        assert!(links_of(&on).contains(&("capture:audio_out".into(), "sound:audio_in".into())));
        assert!(links_of(&on).contains(&("sound:audio_out".into(), "uplink:audio_in_0".into())));
        assert_eq!(number(block_of(&on, "uplink"), "num_audio_tracks"), 1);
    }

    #[test]
    fn taps_go_in_line_for_the_test_pattern() {
        assert_only_taps_added(test_pattern(), true);
        let on = build("gw", &input(test_pattern()), &Video::default(), Taps::On);
        assert!(links_of(&on).contains(&("testvideocaps:src".into(), "picture:video_in".into())));
        assert!(links_of(&on).contains(&("testaudiocaps:src".into(), "sound:audio_in".into())));
    }

    /// The taps change nothing the gateway finds its flows by.
    #[test]
    fn a_tapped_uplink_is_still_ours_and_holds_the_same_camera() {
        let on = build("gw", &input(camera_and_mic()), &Video::default(), Taps::On);
        assert!(is_ours(&on, "gw"));
        assert!(!is_preview_of_ours(&on, "gw"));
        assert_eq!(capture_device_of(&on), Some("dev-abc"));
        assert_eq!(on.id, flow_id("gw", "dev-abc"));
        let tp = build("gw", &input(test_pattern()), &Video::default(), Taps::On);
        assert!(is_ours(&tp, "gw"));
    }

    fn preview_json(source: &Source) -> serde_json::Value {
        let mut v = serde_json::to_value(preview("gw", "Venue A — Cam", source)).unwrap();
        // Positions are layout for Strom's editor, not shape.
        for b in v["blocks"].as_array_mut().unwrap() {
            b.as_object_mut().unwrap().remove("position");
            b.as_object_mut().unwrap().remove("name");
        }
        for e in v["elements"].as_array_mut().unwrap() {
            e.as_object_mut().unwrap().remove("position");
        }
        v
    }

    /// §12's preview flow for a camera with a microphone, field for field.
    #[test]
    fn the_preview_of_a_camera_with_a_microphone_is_the_specified_flow() {
        let v = preview_json(&camera_and_mic());
        assert_eq!(v["id"], preview_flow_id("gw", "dev-abc").to_string());
        assert_eq!(v["name"], "Venue A — Cam (preview)");
        assert_eq!(v["running"], false);
        assert_eq!(
            v["blocks"],
            json!([
                { "id": "capture", "block_definition_id": "builtin.local_input",
                  "properties": { "stream_mode": "audio_video", "video_device": "dev-abc",
                                  "audio_device": "dev-mic", "video_resolution": "1280x720",
                                  "video_framerate": "25/1" } },
                { "id": "picture", "block_definition_id": "builtin.thumbnail", "properties": {} },
                { "id": "sound", "block_definition_id": "builtin.meter",
                  "properties": { "interval": "100" } }
            ])
        );
        assert_eq!(
            v["elements"],
            json!([
                { "id": "picture_sink", "element_type": "fakesink",
                  "properties": { "sync": false, "async": false } },
                { "id": "sound_sink", "element_type": "fakesink",
                  "properties": { "sync": false, "async": false } }
            ])
        );
        assert_eq!(
            v["links"],
            json!([
                { "from": "capture:video_out", "to": "picture:video_in" },
                { "from": "capture:audio_out", "to": "sound:audio_in" },
                { "from": "picture:video_out", "to": "picture_sink:sink" },
                { "from": "sound:audio_out", "to": "sound_sink:sink" }
            ])
        );
    }

    /// The capture block is the uplink's own, so the camera opens in the mode it
    /// will be sent in.
    #[test]
    fn the_preview_captures_exactly_as_the_uplink_does() {
        for source in [camera_and_mic(), device(Some("1920x1080"), None)] {
            let up = build("gw", &input(source.clone()), &Video::default(), Taps::Off);
            let pre = preview("gw", "Venue A — Cam", &source);
            assert_eq!(
                serde_json::to_value(block_of(&pre, "capture")).unwrap(),
                serde_json::to_value(block_of(&up, "capture")).unwrap()
            );
        }
    }

    /// Without a microphone: video only, and no sound block, sink or links.
    #[test]
    fn the_preview_of_a_camera_alone_has_no_sound_branch() {
        let v = preview_json(&device(None, None));
        assert_eq!(
            v["blocks"],
            json!([
                { "id": "capture", "block_definition_id": "builtin.local_input",
                  "properties": { "stream_mode": "video", "video_device": "dev-abc" } },
                { "id": "picture", "block_definition_id": "builtin.thumbnail", "properties": {} }
            ])
        );
        assert_eq!(v["elements"].as_array().unwrap().len(), 1);
        assert_eq!(v["elements"][0]["id"], "picture_sink");
        assert_eq!(
            v["links"],
            json!([
                { "from": "capture:video_out", "to": "picture:video_in" },
                { "from": "picture:video_out", "to": "picture_sink:sink" }
            ])
        );
    }

    /// The test pattern: today's test sources into the two blocks.
    #[test]
    fn the_preview_of_the_test_pattern_uses_the_test_sources() {
        let flow = preview("gw", "Venue A — Test pattern", &test_pattern());
        assert_eq!(flow.id, preview_flow_id("gw", TEST_INPUT_ID));
        assert_eq!(flow.name, "Venue A — Test pattern (preview)");
        let types: Vec<&str> = flow
            .elements
            .iter()
            .map(|e| e.element_type.as_str())
            .collect();
        assert_eq!(
            types,
            [
                "videotestsrc",
                "capsfilter",
                "audiotestsrc",
                "capsfilter",
                "fakesink",
                "fakesink"
            ]
        );
        let uplink = build("gw", &input(test_pattern()), &Video::default(), Taps::Off);
        assert_eq!(
            serde_json::to_value(&flow.elements[..4]).unwrap(),
            serde_json::to_value(&uplink.elements).unwrap()
        );
        assert_eq!(
            links_of(&flow),
            [
                ("testvideo:src".into(), "testvideocaps:sink".into()),
                ("testvideocaps:src".into(), "picture:video_in".into()),
                ("testaudio:src".into(), "testaudiocaps:sink".into()),
                ("testaudiocaps:src".into(), "sound:audio_in".into()),
                ("picture:video_out".into(), "picture_sink:sink".into()),
                ("sound:audio_out".into(), "sound_sink:sink".into()),
            ]
        );
        let ids: Vec<&str> = flow.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["picture", "sound"]);
    }

    /// Strom takes the preview flow in its own types.
    #[test]
    fn a_preview_flow_round_trips_through_json() {
        let flow = preview("gw", "Venue A — Cam", &camera_and_mic());
        let back: Flow = serde_json::from_str(&serde_json::to_string(&flow).unwrap()).unwrap();
        assert_eq!(back.id, flow.id);
        assert_eq!(links_of(&back), links_of(&flow));
        assert!(is_preview_of_ours(&back, "gw"));
    }

    /// A preview id is never an uplink id, and both are found again by derivation.
    #[test]
    fn preview_flows_are_recognised_by_their_derived_id() {
        assert_ne!(preview_flow_id("gw", "dev-abc"), flow_id("gw", "dev-abc"));
        assert_eq!(
            preview_flow_id("gw", "dev-abc"),
            preview_flow_id("gw", "dev-abc")
        );
        assert_ne!(
            preview_flow_id("gw", "dev-abc"),
            preview_flow_id("gw2", "dev-abc")
        );

        for source in [camera_and_mic(), device(None, None), test_pattern()] {
            let pre = preview("gw", "Venue A — Cam", &source);
            assert!(is_preview_of_ours(&pre, "gw"));
            assert!(!is_preview_of_ours(&pre, "other-gateway"));
            assert!(!is_ours(&pre, "gw"), "a preview is not an uplink");
            let up = build("gw", &input(source), &Video::default(), Taps::Off);
            assert!(!is_preview_of_ours(&up, "gw"));
        }

        let mut hand_built = preview("gw", "Venue A — Cam", &camera_and_mic());
        hand_built.id = Uuid::from_u128(2);
        assert!(!is_preview_of_ours(&hand_built, "gw"));
        // A camera preview still holds its camera, so `prepare` would catch it anyway.
        assert_eq!(capture_device_of(&hand_built), Some("dev-abc"));
    }

    #[test]
    fn input_ids_follow_the_source() {
        assert_eq!(input_id_for(&device(None, None)), "dev-abc");
        assert_eq!(input_id_for(&test_pattern()), TEST_INPUT_ID);
    }

    /// An older Strom without either block gets no preview and no taps.
    #[test]
    fn both_preview_blocks_must_be_listed() {
        let all = ["builtin.local_input", "builtin.thumbnail", "builtin.meter"];
        assert!(has_preview_blocks(&all));
        assert!(!has_preview_blocks(&[
            "builtin.local_input",
            "builtin.meter"
        ]));
        assert!(!has_preview_blocks(&["builtin.thumbnail"]));
        assert!(!has_preview_blocks::<String>(&[]));
    }
}
