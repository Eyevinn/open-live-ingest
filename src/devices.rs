//! From the devices Strom can see to the inputs the gateway streams: filtering,
//! selection, naming, and port allocation.

use crate::config::{format_ports, Capture, InputSettings, Uplink};
use crate::flow::{self, Input, Source};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use strom_types::discovery::DeviceResponse;

/// Devices that exist but produce nothing unless another application is running.
/// Streaming one registers a source that sits in Studio waiting for a picture, which
/// reads as a fault, so they are skipped unless asked for. A name heuristic, because
/// the platform providers do not distinguish them.
const VIRTUAL_HINTS: &[&str] = &[
    "virtual",
    "obs",
    "loopback",
    "dummy",
    "null",
    "screen capture",
    "desktop",
];

pub fn is_probably_virtual(device: &DeviceResponse) -> bool {
    let name = device.name.to_lowercase();
    VIRTUAL_HINTS.iter().any(|hint| name.contains(hint))
}

/// Filters and selects, sorted by name so output is stable between runs.
///
/// Selection is by id or by name fragment, never by position: a device list changes
/// between runs, so a number that meant one camera yesterday can mean another today.
pub fn choose(
    all: Vec<DeviceResponse>,
    include_virtual: bool,
    selection: Option<&str>,
) -> Result<Vec<DeviceResponse>> {
    let mut devices: Vec<DeviceResponse> = all
        .into_iter()
        .filter(|d| include_virtual || !is_probably_virtual(d))
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));

    let Some(selection) = selection else {
        return Ok(devices);
    };

    let mut chosen = Vec::new();
    for wanted in selection
        .split(',')
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        chosen.push(find_one(&devices, wanted, ("device", "devices"))?.clone());
    }
    if chosen.is_empty() {
        bail!("--devices selected nothing");
    }
    Ok(chosen)
}

/// The one device `wanted` names, by id or name fragment. `what` names the list in
/// the error, singular and plural.
fn find_one<'a>(
    devices: &'a [DeviceResponse],
    wanted: &str,
    (what, whats): (&str, &str),
) -> Result<&'a DeviceResponse> {
    let matches = matching(devices, wanted);
    match matches.as_slice() {
        [device] => Ok(device),
        [] => bail!("no {what} matches {wanted:?}. Run `open-live-ingest devices` to see them."),
        // Starting the wrong camera is worse than asking again with a longer name.
        several => bail!(
            "{wanted:?} matches {} {whats} ({}). Use a longer name or an id.",
            several.len(),
            several
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn matching<'a>(devices: &'a [DeviceResponse], wanted: &str) -> Vec<&'a DeviceResponse> {
    let needle = wanted.to_lowercase();
    devices
        .iter()
        .filter(|d| d.id.eq_ignore_ascii_case(wanted) || d.name.to_lowercase().contains(&needle))
        .collect()
}

/// What a saved `[[input]]` selection comes to in this run.
#[derive(Debug, Clone)]
pub struct SavedSelection {
    /// The cameras to send, sorted by name as [`choose`] sorts them, so ports are
    /// allocated as they would be for the same devices picked by flag.
    pub cameras: Vec<DeviceResponse>,
    /// Whether the test pattern is one of the inputs.
    pub test_pattern: bool,
    /// The entries whose device Strom does not report now (unplugged, or another
    /// machine's), as written. Skipped, not an error: the others still go live.
    pub absent: Vec<String>,
}

/// The inputs the settings' `[[input]]` entries name, among every device Strom
/// reports. A virtual device is sent when an entry names it: someone chose it.
/// Refused when an entry's name fragment matches several devices (starting the wrong
/// camera is worse than asking again), or when two entries come to one device.
pub fn choose_saved(all: &[DeviceResponse], entries: &[InputSettings]) -> Result<SavedSelection> {
    let mut cameras: Vec<DeviceResponse> = Vec::new();
    let mut test_pattern = false;
    let mut absent = Vec::new();
    for entry in entries {
        if entry.is_test_pattern() {
            test_pattern = true;
            continue;
        }
        let wanted = entry.device.trim();
        let device = match exact_or_matching(all, wanted).as_slice() {
            [] => {
                absent.push(entry.device.clone());
                continue;
            }
            [device] => *device,
            several => bail!(
                "[[input]] device {wanted:?} matches {} devices ({}). Use the device id `open-live-ingest devices` lists.",
                several.len(),
                several
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        if cameras.iter().any(|c| c.id == device.id) {
            bail!(
                "two [[input]] entries name {:?}; one entry per input",
                device.name
            );
        }
        cameras.push(device.clone());
    }
    cameras.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(SavedSelection {
        cameras,
        test_pattern,
        absent,
    })
}

/// The device an entry names: the one whose id is exactly `wanted`, else every
/// device matching it by id or name fragment, as `--devices` matches.
fn exact_or_matching<'a>(devices: &'a [DeviceResponse], wanted: &str) -> Vec<&'a DeviceResponse> {
    match devices.iter().find(|d| d.id == wanted) {
        Some(exact) => vec![exact],
        None => matching(devices, wanted),
    }
}

/// Names each input by its `[[input]]` entry's label, `<gateway.name> — <label>`,
/// where one is set; the others keep the device's name. Entries are matched to this
/// run's cameras as [`choose_saved`] matches them, so a label follows its device when
/// flags pick the devices too.
pub fn apply_labels(
    inputs: &mut [Input],
    entries: &[InputSettings],
    cameras: &[DeviceResponse],
    gateway_name: &str,
) {
    let prefix = name_prefix(gateway_name);
    for input in inputs {
        let entry = entries.iter().find(|entry| match &input.source {
            Source::Test { .. } => entry.is_test_pattern(),
            Source::Device { device_id, .. } => {
                !entry.is_test_pattern()
                    && matches!(
                        exact_or_matching(cameras, entry.device.trim()).as_slice(),
                        [device] if device.id == *device_id
                    )
            }
        });
        if let Some(label) = entry.and_then(InputSettings::label) {
            input.name = format!("{prefix}{label}");
        }
    }
}

/// An audio source to send with a camera, each named by id or name fragment as
/// `--devices` names cameras. Written `<camera>=<audio>` on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPairing {
    pub camera: String,
    pub audio: String,
}

impl FromStr for AudioPairing {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (camera, audio) = s
            .split_once('=')
            .map(|(c, a)| (c.trim(), a.trim()))
            .filter(|(c, a)| !c.is_empty() && !a.is_empty())
            .ok_or_else(|| {
                format!(
                    "expected <camera>=<audio source>, e.g. \"FaceTime=USB Microphone\", got {s:?}"
                )
            })?;
        Ok(Self {
            camera: camera.to_string(),
            audio: audio.to_string(),
        })
    }
}

/// The settings' `[[input]]` entries that pair audio with one of this run's cameras.
/// Entries for a camera not in this run (unplugged, or left out by `--devices`) are
/// returned separately so the caller can say so; they are not an error, because the
/// settings outlive any one run.
pub fn settings_pairings(
    entries: &[InputSettings],
    cameras: &[DeviceResponse],
) -> (Vec<AudioPairing>, Vec<String>) {
    let mut pairings = Vec::new();
    let mut absent = Vec::new();
    for entry in entries {
        let Some(audio) = entry
            .audio
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        else {
            continue;
        };
        if exact_or_matching(cameras, entry.device.trim()).is_empty() {
            absent.push(entry.device.clone());
            continue;
        }
        pairings.push(AudioPairing {
            camera: entry.device.trim().to_string(),
            audio: audio.to_string(),
        });
    }
    (pairings, absent)
}

/// Resolves pairings against this run's cameras and the audio sources Strom reports,
/// to camera id -> audio source id. Refused before any flow starts: one audio source
/// on two cameras (the same sound on two sources is a mistake, and two flows would
/// open one device), and two audio sources on one camera (a feed carries one track).
pub fn pair_audio(
    cameras: &[DeviceResponse],
    audio_sources: &[DeviceResponse],
    pairings: &[AudioPairing],
) -> Result<BTreeMap<String, String>> {
    let mut by_camera: BTreeMap<String, String> = BTreeMap::new();
    // audio id -> (audio name, camera name), for the error message.
    let mut by_audio: BTreeMap<String, (String, String)> = BTreeMap::new();
    for pairing in pairings {
        let camera = find_one(
            cameras,
            &pairing.camera,
            ("camera in this run", "cameras in this run"),
        )?;
        let audio = find_one(
            audio_sources,
            &pairing.audio,
            ("audio source", "audio sources"),
        )?;
        if let Some(existing) = by_camera.get(&camera.id) {
            if *existing == audio.id {
                continue;
            }
            let other = by_audio.get(existing).map_or(existing.as_str(), |(n, _)| n);
            bail!(
                "{:?} is paired with two audio sources ({other:?} and {:?}). A camera carries one.",
                camera.name,
                audio.name
            );
        }
        if let Some((audio_name, other_camera)) = by_audio.get(&audio.id) {
            bail!(
                "{audio_name:?} is already paired with {other_camera:?}, so it cannot also go with {:?}. Pick another audio source.",
                camera.name
            );
        }
        by_camera.insert(camera.id.clone(), audio.id.clone());
        by_audio.insert(audio.id.clone(), (audio.name.clone(), camera.name.clone()));
    }
    Ok(by_camera)
}

/// The prefix every source and flow of this gateway carries, so an operator can tell
/// two venues' cameras apart in Studio and cleanup can recognise its own sources.
pub fn name_prefix(gateway_name: &str) -> String {
    format!("{} — ", gateway_name.trim())
}

/// Builds one input per device, allocating the lowest free port in `ports` to each in
/// name order. Deterministic, so the same devices land on the same ports across runs
/// and the addresses registered in Open Live stay stable. Whose ports they are is the
/// caller's business: the cloud's range in caller mode, this machine's in listener.
pub fn inputs_for(
    devices: &[DeviceResponse],
    test_pattern: bool,
    gateway_name: &str,
    uplink: &Uplink,
    capture: &Capture,
    cloud_host: &str,
    ports: &BTreeSet<u16>,
) -> Result<Vec<Input>> {
    let mut taken = BTreeSet::new();
    let mut next_port = || {
        let port = ports
            .iter()
            .copied()
            .find(|p| !taken.contains(p))
            .ok_or_else(|| anyhow::anyhow!("no free SRT port left in {}", format_ports(ports)))?;
        taken.insert(port);
        Ok::<u16, anyhow::Error>(port)
    };

    let prefix = name_prefix(gateway_name);
    let mut inputs = Vec::new();
    if test_pattern {
        inputs.push(Input {
            id: flow::TEST_INPUT_ID.to_string(),
            name: format!("{prefix}Test pattern"),
            source: Source::Test {
                resolution: capture
                    .video_resolution
                    .clone()
                    .unwrap_or_else(|| "1920x1080".to_string()),
                framerate: capture
                    .video_framerate
                    .clone()
                    .unwrap_or_else(|| "25/1".to_string()),
            },
            endpoint: uplink.endpoint(cloud_host, next_port()?),
        });
    }
    for device in devices {
        inputs.push(Input {
            id: flow::device_input_id(&device.id),
            name: format!("{prefix}{}", device.name),
            source: Source::Device {
                device_id: device.id.clone(),
                audio_device: None,
                resolution: capture.video_resolution.clone(),
                framerate: capture.video_framerate.clone(),
            },
            endpoint: uplink.endpoint(cloud_host, next_port()?),
        });
    }
    Ok(inputs)
}

/// Gives each camera input the audio source paired with it. `audio` maps a camera's
/// device id to an audio source id, as [`pair_audio`] returns it.
pub fn attach_audio(inputs: &mut [Input], audio: &BTreeMap<String, String>) {
    for input in inputs {
        if let Source::Device {
            device_id,
            audio_device,
            ..
        } = &mut input.source
        {
            *audio_device = audio.get(device_id.as_str()).cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> DeviceResponse {
        DeviceResponse {
            id: id.to_string(),
            name: name.to_string(),
            device_class: "Video/Source".to_string(),
            category: strom_types::discovery::DeviceCategory::VideoSource,
            provider: "test".to_string(),
            properties: Default::default(),
            first_seen_secs_ago: 0,
            last_seen_secs_ago: 0,
        }
    }

    fn devices() -> Vec<DeviceResponse> {
        vec![
            device("d1", "OBS Virtual Camera"),
            device("d2", "FaceTime HD Camera"),
            device("d3", "DeckLink Mini Recorder"),
        ]
    }

    #[test]
    fn virtual_devices_are_skipped_unless_asked_for() {
        let names: Vec<String> = choose(devices(), false, None)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["DeckLink Mini Recorder", "FaceTime HD Camera"]);
        assert_eq!(choose(devices(), true, None).unwrap().len(), 3);
    }

    /// Real hardware must never be filtered out: a camera vanishing from the list
    /// because of a name heuristic is worse than a stray virtual device appearing.
    #[test]
    fn real_capture_hardware_is_not_treated_as_virtual() {
        for name in [
            "FaceTime HD Camera",
            "DeckLink Mini Recorder 4K",
            "Magewell USB Capture SDI",
            "Logitech BRIO",
        ] {
            assert!(!is_probably_virtual(&device("d", name)), "{name}");
        }
        assert!(is_probably_virtual(&device("d", "v4l2loopback")));
    }

    #[test]
    fn devices_are_selected_by_name_fragment_or_id_and_never_by_position() {
        assert_eq!(
            choose(devices(), false, Some("facetime")).unwrap()[0].id,
            "d2"
        );
        assert_eq!(choose(devices(), true, Some("d1")).unwrap()[0].id, "d1");
        assert_eq!(
            choose(devices(), true, Some("facetime, obs"))
                .unwrap()
                .len(),
            2
        );
        assert!(choose(devices(), false, Some("1")).is_err());
        assert!(choose(devices(), false, Some("nonexistent")).is_err());
    }

    /// Starting the wrong camera is worse than asking again.
    #[test]
    fn an_ambiguous_name_is_refused() {
        let mut list = devices();
        list.push(device("d4", "Camera Link Pro"));
        let err = choose(list, true, Some("camera")).unwrap_err().to_string();
        assert!(err.contains("matches"), "{err}");
    }

    #[test]
    fn inputs_get_stable_names_and_the_lowest_free_ports_in_order() {
        let inputs = inputs_for(
            &choose(devices(), false, None).unwrap(),
            true,
            " Venue A ",
            &Uplink::default(),
            &Capture::default(),
            "cloud",
            &(9000..=9002).collect(),
        )
        .unwrap();
        let summary: Vec<(String, u16)> = inputs
            .iter()
            .map(|i| (i.name.clone(), i.endpoint.port))
            .collect();
        assert_eq!(
            summary,
            [
                ("Venue A — Test pattern".to_string(), 9000),
                ("Venue A — DeckLink Mini Recorder".to_string(), 9001),
                ("Venue A — FaceTime HD Camera".to_string(), 9002),
            ]
        );
        assert!(inputs
            .iter()
            .all(|i| i.name.starts_with(&name_prefix("Venue A"))));
        assert_eq!(inputs[0].id, flow::TEST_INPUT_ID);
        assert_eq!(inputs[1].id, "dev-d3");
    }

    fn audio(id: &str, name: &str) -> DeviceResponse {
        DeviceResponse {
            device_class: "Audio/Source".to_string(),
            category: strom_types::discovery::DeviceCategory::AudioSource,
            ..device(id, name)
        }
    }

    fn mics() -> Vec<DeviceResponse> {
        vec![
            audio("m1", "USB Microphone"),
            audio("m2", "MacBook Pro Microphone"),
            audio("m3", "Scarlett 2i2 USB"),
        ]
    }

    fn pairing(s: &str) -> AudioPairing {
        s.parse().unwrap()
    }

    #[test]
    fn a_pairing_is_camera_equals_audio() {
        assert_eq!(
            pairing(" FaceTime = USB Microphone "),
            AudioPairing {
                camera: "FaceTime".to_string(),
                audio: "USB Microphone".to_string()
            }
        );
        for bad in ["FaceTime", "=USB", "FaceTime=", " = ", ""] {
            let err = bad.parse::<AudioPairing>().unwrap_err();
            assert!(err.contains("<camera>=<audio source>"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn pairings_resolve_to_ids_by_name_fragment_or_id() {
        let cameras = choose(devices(), false, None).unwrap();
        let paired = pair_audio(
            &cameras,
            &mics(),
            &[pairing("facetime=usb microphone"), pairing("d3=m3")],
        )
        .unwrap();
        assert_eq!(paired.get("d2").map(String::as_str), Some("m1"));
        assert_eq!(paired.get("d3").map(String::as_str), Some("m3"));
        assert_eq!(paired.len(), 2);

        let mut inputs = inputs_for(
            &cameras,
            false,
            "V",
            &Uplink::default(),
            &Capture::default(),
            "cloud",
            &(9000..=9001).collect(),
        )
        .unwrap();
        attach_audio(&mut inputs, &paired);
        let audio_of: Vec<Option<&str>> = inputs
            .iter()
            .map(|i| match &i.source {
                Source::Device { audio_device, .. } => audio_device.as_deref(),
                Source::Test { .. } => None,
            })
            .collect();
        assert_eq!(audio_of, [Some("m3"), Some("m1")]);
    }

    /// #64 AC5: refused with a clear error, before any flow starts.
    #[test]
    fn one_audio_source_on_two_cameras_is_refused() {
        let cameras = choose(devices(), false, None).unwrap();
        let err = pair_audio(
            &cameras,
            &mics(),
            &[pairing("FaceTime=USB Microphone"), pairing("DeckLink=m1")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("\"USB Microphone\" is already paired with \"FaceTime HD Camera\""),
            "{err}"
        );
        assert!(err.contains("\"DeckLink Mini Recorder\""), "{err}");
    }

    #[test]
    fn two_audio_sources_on_one_camera_are_refused_but_a_repeat_is_not() {
        let cameras = choose(devices(), false, None).unwrap();
        let err = pair_audio(
            &cameras,
            &mics(),
            &[
                pairing("FaceTime=USB Microphone"),
                pairing("FaceTime=Scarlett"),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("paired with two audio sources"), "{err}");

        let same_twice = pair_audio(
            &cameras,
            &mics(),
            &[pairing("FaceTime=USB Microphone"), pairing("d2=m1")],
        )
        .unwrap();
        assert_eq!(same_twice.len(), 1);
    }

    #[test]
    fn a_pairing_must_name_exactly_one_camera_of_this_run_and_one_audio_source() {
        let cameras = choose(devices(), false, Some("FaceTime")).unwrap();
        let err = pair_audio(&cameras, &mics(), &[pairing("DeckLink=m1")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no camera in this run matches"), "{err}");
        let err = pair_audio(&cameras, &mics(), &[pairing("FaceTime=Microphone")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 audio sources"), "{err}");
        let err = pair_audio(&cameras, &mics(), &[pairing("FaceTime=Rode")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no audio source matches"), "{err}");
    }

    /// The settings outlive a run: an entry for a camera not in it is set aside, not
    /// an error, and an entry without audio pairs nothing.
    #[test]
    fn settings_entries_pair_only_this_runs_cameras() {
        let cameras = choose(devices(), false, Some("FaceTime")).unwrap();
        let entries = [
            InputSettings {
                device: "FaceTime".to_string(),
                audio: Some("USB Microphone".to_string()),
                label: None,
            },
            InputSettings {
                device: "DeckLink".to_string(),
                audio: Some("Scarlett".to_string()),
                label: None,
            },
            InputSettings {
                device: "d2".to_string(),
                audio: None,
                label: None,
            },
        ];
        let (pairings, absent) = settings_pairings(&entries, &cameras);
        assert_eq!(pairings, [pairing("FaceTime=USB Microphone")]);
        assert_eq!(absent, ["DeckLink"]);
    }

    fn entry(device: &str, label: Option<&str>) -> InputSettings {
        InputSettings {
            device: device.to_string(),
            audio: None,
            label: label.map(str::to_string),
        }
    }

    /// A saved selection sends exactly what it names: by exact id first, a virtual
    /// device too when named, the test pattern by name, and an unplugged device set
    /// aside rather than refused.
    #[test]
    fn a_saved_selection_picks_exactly_its_entries() {
        let picked = choose_saved(
            &devices(),
            &[
                entry("d2", None),
                entry("test", None),
                entry("OBS", None),
                entry("dev-gone", None),
            ],
        )
        .unwrap();
        let ids: Vec<&str> = picked.cameras.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["d2", "d1"], "sorted by name");
        assert!(picked.test_pattern);
        assert_eq!(picked.absent, ["dev-gone"]);

        let picked = choose_saved(&devices(), &[entry("d3", None)]).unwrap();
        assert!(!picked.test_pattern);
        assert_eq!(picked.cameras.len(), 1);

        // An id that is also a fragment of another device's name means that id.
        let mut list = devices();
        list.push(device("Face", "Studio Face Cam"));
        let picked = choose_saved(&list, &[entry("Face", None)]).unwrap();
        assert_eq!(picked.cameras[0].id, "Face");

        let err = choose_saved(&devices(), &[entry("Camera", None)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 devices"), "{err}");
        let err = choose_saved(&devices(), &[entry("d2", None), entry("FaceTime", None)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("two [[input]] entries"), "{err}");
    }

    /// Only the part after the prefix is the label: the prefix stays, so cleanup still
    /// finds the source.
    #[test]
    fn labels_rename_after_the_prefix_and_unlabelled_inputs_keep_the_device_name() {
        let cameras = choose(devices(), false, None).unwrap();
        let mut inputs = inputs_for(
            &cameras,
            true,
            "Venue A",
            &Uplink::default(),
            &Capture::default(),
            "cloud",
            &(9000..=9002).collect(),
        )
        .unwrap();
        apply_labels(
            &mut inputs,
            &[
                entry("test", Some("Bars")),
                entry("FaceTime", Some(" Stage left ")),
                entry("d3", None),
                entry("dev-gone", Some("Gone")),
            ],
            &cameras,
            "Venue A",
        );
        let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Venue A — Bars",
                "Venue A — DeckLink Mini Recorder",
                "Venue A — Stage left",
            ]
        );
        // The flow id follows the device, never the label.
        assert_eq!(inputs[2].id, "dev-d2");
    }

    /// Better a clear error than two inputs fighting over one port.
    #[test]
    fn an_exhausted_range_is_an_error() {
        let err = inputs_for(
            &choose(devices(), false, None).unwrap(),
            false,
            "V",
            &Uplink::default(),
            &Capture::default(),
            "cloud",
            &(9000..=9000).collect(),
        )
        .unwrap_err()
        .to_string();
        // A one-port set reads as the port, not as "9000-9000".
        assert!(err.contains("no free SRT port left in 9000"), "{err}");
    }
}
