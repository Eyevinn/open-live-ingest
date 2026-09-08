//! Runtime input sessions, for inputs an operator starts rather than declares.
//!
//! The headless daemon reconciles a fixed set of inputs forever. The desktop app is
//! the opposite: an input exists only while the operator wants it, and closing the
//! window ends it. That makes the lifecycle simpler than the daemon's, not harder —
//! the app owns each flow outright and tears it down on the way out, so nothing has
//! to be reconciled against leftovers from a previous run.
//!
//! Two things still have to be handled, because a window can be closed the hard way:
//!
//! - **Leftovers from a crash.** Flow ids are derived (UUIDv5 over gateway id + input
//!   id), so a fresh start can find and delete its own orphans without any local
//!   state to consult.
//! - **Ports.** An operator picking a camera cannot be asked to choose a UDP port, so
//!   one is allocated per input from a configured range.

use crate::state::SharedState;
use crate::strom::client::{CaptureDevice, StromClient};
use crate::strom::flow;
use anyhow::{anyhow, bail, Result};
use open_live_gateway_types::config::{
    AppConfig, CaptureConfig, GatewayConfig, InputConfig, UplinkTemplate,
};
use std::collections::BTreeSet;

/// Devices that exist but produce nothing unless some other application is running.
///
/// Streaming one of these registers a source in Studio that sits permanently waiting
/// for a picture, which looks like a fault. Included only when explicitly asked for.
const VIRTUAL_HINTS: &[&str] = &[
    "virtual",
    "obs",
    "loopback",
    "dummy",
    "null",
    "screen capture",
    "desktop",
];

/// Whether a device looks like a virtual one rather than real capture hardware.
///
/// A name heuristic, because the platform providers do not distinguish them: on macOS
/// a virtual camera and a built-in one both arrive from `avfprovider`.
pub fn is_probably_virtual(device: &CaptureDevice) -> bool {
    let name = device.display_name.to_lowercase();
    VIRTUAL_HINTS.iter().any(|hint| name.contains(hint))
}

/// Turns a device id into an input id that is stable for that device.
///
/// Stability matters: restarting the app and picking the same camera addresses the
/// same flow and the same Open Live source, rather than accumulating a new one each
/// time. Strom's ids look like `dev-d24eb9fb6733c220`, so the prefix is dropped for
/// legibility in Studio's source list.
pub fn input_id_for_device(device_id: &str) -> String {
    let trimmed = device_id.strip_prefix("dev-").unwrap_or(device_id);
    let cleaned: String = trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("dev-{cleaned}")
}

/// Picks the lowest free port in the template's range.
///
/// Deterministic rather than random so the same set of inputs lands on the same ports
/// across restarts, which keeps the address registered in Open Live stable.
pub fn allocate_port(template: &UplinkTemplate, taken: &BTreeSet<u16>) -> Result<u16> {
    let range = template.ports().map_err(|e| anyhow!(e))?;
    range
        .into_iter()
        .find(|p| !taken.contains(p))
        .ok_or_else(|| {
            anyhow!(
                "no free SRT port left in range {} — every port is in use",
                template.port_range
            )
        })
}

/// The name a device's source carries in Open Live.
///
/// Prefixed with the gateway's name so an operator can tell two venues' cameras
/// apart in Studio — "FaceTime HD Camera" on its own says nothing about which
/// machine it came from — and so cleanup can recognise its own sources.
pub fn source_name_for_device(gateway_name: &str, device: &CaptureDevice) -> String {
    format!("{} — {}", gateway_name.trim(), device.display_name)
}

/// The prefix every source from this gateway carries.
pub fn source_name_prefix(gateway_name: &str) -> String {
    format!("{} — ", gateway_name.trim())
}

/// Builds the input for a chosen capture device.
pub fn input_for_device(
    app: &AppConfig,
    device: &CaptureDevice,
    port: u16,
    cloud_host: &str,
    gateway_name: &str,
) -> InputConfig {
    InputConfig {
        id: input_id_for_device(&device.id),
        name: Some(source_name_for_device(gateway_name, device)),
        capture: CaptureConfig::Local {
            video_device: Some(device.id.clone()),
            video_resolution: app.video_resolution.clone(),
            video_framerate: app.video_framerate.clone(),
            // Left out on purpose: a webcam's microphone is a separate device on its
            // own clock and drifts against video over a long show. An operator who
            // wants embedded audio should be using SDI.
            audio_device: None,
            audio_channels: 2,
            audio_rate: 48000,
        },
        video: app.video.clone(),
        uplink: app.uplink.materialize(port, cloud_host),
        enabled: true,
    }
}

/// Everything needed to run one input, held for as long as it is streaming.
pub struct ActiveInput {
    pub input: InputConfig,
    pub flow_id: String,
    pub device_id: String,
}

/// Starts an input: creates the flow in Strom and starts it.
///
/// Registration with Open Live is deliberately not done here — it is the registration
/// loop's job, and keeping it separate preserves the invariant that Open Live being
/// unreachable never stops a feed from going up.
pub async fn start_input(
    client: &StromClient,
    gateway_id: &str,
    gateway_name: &str,
    input: &InputConfig,
) -> Result<ActiveInput> {
    let flow_id = flow::flow_id(gateway_id, &input.id);
    let desired = flow::build(gateway_id, gateway_name, input)?;

    // A flow may survive from a previous run that was killed. Replace it rather than
    // failing, and rather than trusting its contents.
    if client.get_flow(&flow_id).await?.is_some() {
        client.stop_flow(&flow_id).await.ok();
        client.delete_flow(&flow_id).await?;
    }
    client.create_flow(&desired).await?;
    let started = client.start_flow(&flow_id).await?;
    if !started.running {
        bail!("Strom started the flow but does not report it running");
    }

    let device_id = match &input.capture {
        CaptureConfig::Local { video_device, .. } => video_device.clone().unwrap_or_default(),
        _ => String::new(),
    };

    Ok(ActiveInput {
        input: input.clone(),
        flow_id,
        device_id,
    })
}

/// Stops an input and removes its flow, so nothing is left behind in Strom.
pub async fn stop_input(client: &StromClient, active: &ActiveInput) -> Result<()> {
    client.stop_flow(&active.flow_id).await.ok();
    client.delete_flow(&active.flow_id).await
}

/// The capture device a flow opens, if it has a local-input block.
pub fn flow_capture_device(flow: &serde_json::Value) -> Option<String> {
    flow.get("blocks")?
        .as_array()?
        .iter()
        .find(|b| {
            b.get("block_definition_id").and_then(|v| v.as_str()) == Some("builtin.local_input")
        })?
        .get("properties")?
        .get("video_device")?
        .as_str()
        .map(str::to_string)
}

/// Removes any flow already holding one of the devices we are about to open.
///
/// Derived flow ids only find our own leftovers *under the current gateway id*, so a
/// renamed gateway — or a flow from an earlier experiment — stays invisible to
/// `reap_orphans` and keeps the camera open. Two pipelines on one device contend for
/// frames, which presents as stutter rather than as an error, so this matches on the
/// device itself.
pub async fn clear_conflicting_flows(
    client: &StromClient,
    device_ids: &[String],
    keep: &[String],
) -> Vec<String> {
    let Ok(flows) = client.list_flows().await else {
        return Vec::new();
    };

    let mut cleared = Vec::new();
    for flow in flows {
        let Some(id) = flow.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if keep.iter().any(|k| k == id) {
            continue;
        }
        let Some(device) = flow_capture_device(&flow) else {
            continue;
        };
        if !device_ids.iter().any(|d| d == &device) {
            continue;
        }

        let name = flow
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        client.stop_flow(id).await.ok();
        if client.delete_flow(id).await.is_ok() {
            cleared.push(name);
        }
    }
    cleared
}

/// Deletes any flow this gateway would have created for the given input ids.
///
/// Called at startup to clear orphans from a run that ended without cleanup. Derived
/// flow ids are what make this possible with no stored state.
pub async fn reap_orphans(client: &StromClient, gateway_id: &str, input_ids: &[String]) -> usize {
    let mut reaped = 0;
    for id in input_ids {
        let flow_id = flow::flow_id(gateway_id, id);
        if matches!(client.get_flow(&flow_id).await, Ok(Some(_))) {
            client.stop_flow(&flow_id).await.ok();
            if client.delete_flow(&flow_id).await.is_ok() {
                reaped += 1;
            }
        }
    }
    reaped
}

/// The ports already claimed by config-declared inputs, so runtime allocation avoids
/// colliding with an input the operator wrote down.
pub fn ports_in_config(cfg: &GatewayConfig) -> BTreeSet<u16> {
    cfg.inputs.iter().map(|i| i.uplink.port).collect()
}

/// A snapshot of one input for the UI, pairing configuration with live status.
pub struct InputView {
    pub input_id: String,
    pub device_id: String,
    pub label: String,
    pub listener_address: String,
}

impl ActiveInput {
    pub fn view(&self) -> InputView {
        InputView {
            input_id: self.input.id.clone(),
            device_id: self.device_id.clone(),
            label: self
                .input
                .name
                .clone()
                .unwrap_or_else(|| self.input.id.clone()),
            listener_address: self.input.uplink.cloud_uri(),
        }
    }
}

/// Adds a runtime input to the shared status map so the UI and the registration loop
/// can see it. Mirrors what `SharedState::new` does for config inputs.
pub fn register_in_state(state: &SharedState, input: &InputConfig, gateway_id: &str) {
    state.add_input(input, gateway_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn device(id: &str, name: &str) -> CaptureDevice {
        CaptureDevice {
            id: id.to_string(),
            display_name: name.to_string(),
            provider: Some("avfprovider".to_string()),
        }
    }

    /// Picking the same camera after a restart must address the same flow and source,
    /// or Studio's source list fills up with duplicates of one device.
    #[test]
    fn a_flows_capture_device_is_read_from_its_local_input_block() {
        let flow = json!({
            "id": "f1",
            "blocks": [
                {"id": "capture", "block_definition_id": "builtin.local_input",
                 "properties": {"video_device": "dev-abc"}},
                {"id": "encoder", "block_definition_id": "builtin.videoenc", "properties": {}}
            ]
        });
        assert_eq!(flow_capture_device(&flow).as_deref(), Some("dev-abc"));
    }

    /// A flow with no local input — a test pattern, an SRT relay, someone else's
    /// work — must not be mistaken for one holding a camera.
    #[test]
    fn a_flow_without_a_local_input_has_no_capture_device() {
        let flow = json!({
            "id": "f1",
            "blocks": [{"id": "e", "block_definition_id": "builtin.videoenc", "properties": {}}]
        });
        assert!(flow_capture_device(&flow).is_none());

        let flow = json!({
            "id": "f2",
            "blocks": [{"id": "c", "block_definition_id": "builtin.local_input",
                        "properties": {}}]
        });
        assert!(
            flow_capture_device(&flow).is_none(),
            "no device set is not a device"
        );
    }

    /// Two venues streaming the same model of camera must be distinguishable in
    /// Studio, and cleanup has to be able to recognise its own sources.
    #[test]
    fn source_names_carry_the_gateway_name() {
        let name = source_name_for_device("Venue A", &device("d", "FaceTime HD Camera"));
        assert_eq!(name, "Venue A — FaceTime HD Camera");
        assert!(name.starts_with(&source_name_prefix("Venue A")));
        assert!(!name.starts_with(&source_name_prefix("Venue B")));
    }

    #[test]
    fn a_padded_gateway_name_still_produces_a_matching_prefix() {
        let name = source_name_for_device("  Venue A  ", &device("d", "Cam"));
        assert!(name.starts_with(&source_name_prefix("Venue A")));
    }

    #[test]
    fn virtual_devices_are_recognised_by_name() {
        assert!(is_probably_virtual(&device("d1", "OBS Virtual Camera")));
        assert!(is_probably_virtual(&device("d2", "Screen Capture")));
        assert!(is_probably_virtual(&device("d3", "v4l2loopback")));
    }

    /// Real hardware must never be filtered out — a venue whose camera vanished from
    /// the list because of a name heuristic is a worse outcome than a stray virtual
    /// device appearing.
    #[test]
    fn real_capture_hardware_is_not_filtered() {
        for name in [
            "FaceTime HD Camera",
            "DeckLink Mini Recorder 4K",
            "Magewell USB Capture SDI",
            "Luddes Camera",
            "Logitech BRIO",
        ] {
            assert!(
                !is_probably_virtual(&device("d", name)),
                "{name} must not be treated as virtual"
            );
        }
    }

    #[test]
    fn input_ids_are_stable_per_device() {
        let a = input_id_for_device("dev-d24eb9fb6733c220");
        assert_eq!(a, input_id_for_device("dev-d24eb9fb6733c220"));
        assert_ne!(a, input_id_for_device("dev-f8bbeeb309724160"));
    }

    #[test]
    fn input_ids_survive_awkward_device_ids() {
        assert_eq!(
            input_id_for_device("v4l2:/dev/video0"),
            "dev-v4l2--dev-video0"
        );
        assert_eq!(input_id_for_device("plain"), "dev-plain");
    }

    #[test]
    fn ports_are_allocated_lowest_first_and_skip_taken_ones() {
        let t = UplinkTemplate {
            port_range: "9000-9002".to_string(),
            ..UplinkTemplate::default()
        };
        let mut taken = BTreeSet::new();
        assert_eq!(allocate_port(&t, &taken).unwrap(), 9000);
        taken.insert(9000);
        assert_eq!(allocate_port(&t, &taken).unwrap(), 9001);
        taken.insert(9001);
        assert_eq!(allocate_port(&t, &taken).unwrap(), 9002);
    }

    /// Better a clear error than silently reusing a port and having two inputs fight
    /// over one SRT listener.
    #[test]
    fn an_exhausted_range_is_an_error() {
        let t = UplinkTemplate {
            port_range: "9000-9001".to_string(),
            ..UplinkTemplate::default()
        };
        let taken = BTreeSet::from([9000, 9001]);
        let err = allocate_port(&t, &taken).expect_err("exhausted range must fail");
        assert!(err.to_string().contains("9000-9001"));
    }

    #[test]
    fn a_malformed_range_is_an_error() {
        for bad in ["9000", "9100-9000", "", "abc-def"] {
            let t = UplinkTemplate {
                port_range: bad.to_string(),
                ..UplinkTemplate::default()
            };
            assert!(
                allocate_port(&t, &BTreeSet::new()).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_device_becomes_a_local_capture_input_named_after_it() {
        let app = AppConfig::default();
        let input = input_for_device(
            &app,
            &device("dev-abc", "FaceTime HD Camera"),
            9000,
            "cloud.example.com",
            "Venue A",
        );

        assert_eq!(input.name.as_deref(), Some("Venue A — FaceTime HD Camera"));
        assert_eq!(input.uplink.port, 9000);
        assert_eq!(input.uplink.host, "cloud.example.com");
        match input.capture {
            CaptureConfig::Local {
                video_device,
                audio_device,
                ..
            } => {
                assert_eq!(video_device.as_deref(), Some("dev-abc"));
                // Video-only on purpose: a webcam mic drifts against its video.
                assert!(audio_device.is_none());
            }
            other => panic!("expected a local capture input, got {other:?}"),
        }
    }

    #[test]
    fn config_ports_are_reserved_against_runtime_allocation() {
        let cfg: GatewayConfig = toml::from_str(
            r#"
[gateway]
name = "GW"

[strom]
url = "http://127.0.0.1:8080"

[[inputs]]
id = "cam1"

  [inputs.capture]
  kind = "test"

  [inputs.uplink]
  host = "h"
  port = 9000
"#,
        )
        .expect("parses");

        let taken = ports_in_config(&cfg);
        assert!(taken.contains(&9000));

        let t = UplinkTemplate {
            port_range: "9000-9100".to_string(),
            ..UplinkTemplate::default()
        };
        assert_eq!(
            allocate_port(&t, &taken).unwrap(),
            9001,
            "a runtime input must not take the port a config input declared"
        );
    }
}
