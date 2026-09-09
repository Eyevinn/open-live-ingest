//! From the devices Strom can see to the inputs the gateway streams: filtering,
//! selection, naming, and port allocation.

use crate::config::{Capture, Uplink};
use crate::flow::{self, Input, Source};
use anyhow::{bail, Result};
use std::collections::BTreeSet;
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
        let needle = wanted.to_lowercase();
        let matches: Vec<&DeviceResponse> = devices
            .iter()
            .filter(|d| {
                d.id.eq_ignore_ascii_case(wanted) || d.name.to_lowercase().contains(&needle)
            })
            .collect();
        match matches.as_slice() {
            [device] => chosen.push((*device).clone()),
            [] => {
                bail!("no device matches {wanted:?}. Run `open-live-gateway devices` to see them.")
            }
            // Starting the wrong camera is worse than asking again with a longer name.
            several => bail!(
                "{wanted:?} matches {} devices ({}). Use a longer name or an id.",
                several.len(),
                several
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    if chosen.is_empty() {
        bail!("--devices selected nothing");
    }
    Ok(chosen)
}

/// The prefix every source and flow of this gateway carries, so an operator can tell
/// two venues' cameras apart in Studio and cleanup can recognise its own sources.
pub fn name_prefix(gateway_name: &str) -> String {
    format!("{} — ", gateway_name.trim())
}

/// Builds one input per device, allocating the lowest free port to each in name
/// order. Deterministic, so the same devices land on the same ports across runs and
/// the addresses registered in Open Live stay stable.
pub fn inputs_for(
    devices: &[DeviceResponse],
    test_pattern: bool,
    gateway_name: &str,
    uplink: &Uplink,
    capture: &Capture,
    cloud_host: &str,
) -> Result<Vec<Input>> {
    let ports = uplink.ports()?;
    let mut taken = BTreeSet::new();
    let mut next_port = || {
        let port = ports.clone().find(|p| !taken.contains(p)).ok_or_else(|| {
            anyhow::anyhow!("no free SRT port left in range {}", uplink.port_range)
        })?;
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
                resolution: capture.video_resolution.clone(),
                framerate: capture.video_framerate.clone(),
            },
            endpoint: uplink.endpoint(cloud_host, next_port()?),
        });
    }
    Ok(inputs)
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
        let uplink = Uplink {
            port_range: "9000-9002".to_string(),
            ..Uplink::default()
        };
        let inputs = inputs_for(
            &choose(devices(), false, None).unwrap(),
            true,
            " Venue A ",
            &uplink,
            &Capture::default(),
            "cloud",
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

    /// Better a clear error than two inputs fighting over one port.
    #[test]
    fn an_exhausted_range_is_an_error() {
        let uplink = Uplink {
            port_range: "9000-9000".to_string(),
            ..Uplink::default()
        };
        let err = inputs_for(
            &choose(devices(), false, None).unwrap(),
            false,
            "V",
            &uplink,
            &Capture::default(),
            "cloud",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("9000-9000"), "{err}");
    }
}
