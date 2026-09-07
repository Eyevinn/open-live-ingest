//! Runtime state shared between the flow supervisors, the control API, and the Open
//! Live registration loop.

use crate::strom::client::SrtUplink;
use open_live_gateway_types::config::InputConfig;
use open_live_gateway_types::status::{GatewayStatus, InputState, InputStatus, UplinkStats};
use open_live_gateway_types::GatewayConfig;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// The outcome of one uplink poll.
pub struct UplinkSample {
    pub delivering: bool,
    pub consecutive_stalls: u32,
}

pub struct SharedState {
    gateway_id: String,
    started: Instant,
    strom_reachable: AtomicBool,
    inputs: Mutex<BTreeMap<String, InputStatus>>,
}

impl SharedState {
    pub fn new(gateway_id: String, cfg: &GatewayConfig) -> Self {
        let inputs = cfg
            .inputs
            .iter()
            .map(|input| {
                (
                    input.id.clone(),
                    InputStatus {
                        id: input.id.clone(),
                        state: InputState::Idle,
                        flow_id: crate::strom::flow::flow_id(&gateway_id, &input.id),
                        gst_state: None,
                        source_id: None,
                        listener_address: input.uplink.cloud_uri(),
                        restarts: 0,
                        last_error: None,
                        uplink: None,
                        stalled_polls: 0,
                    },
                )
            })
            .collect();

        Self {
            gateway_id,
            started: Instant::now(),
            strom_reachable: AtomicBool::new(false),
            inputs: Mutex::new(inputs),
        }
    }

    /// Adds an input created at runtime, as the desktop app does when an operator
    /// starts a device. A no-op if it is already present, so restarting one input
    /// does not discard its accumulated status.
    pub fn add_input(&self, input: &InputConfig, gateway_id: &str) {
        let mut inputs = self.inputs.lock().expect("state poisoned");
        inputs
            .entry(input.id.clone())
            .or_insert_with(|| InputStatus {
                id: input.id.clone(),
                state: InputState::Idle,
                flow_id: crate::strom::flow::flow_id(gateway_id, &input.id),
                gst_state: None,
                source_id: None,
                listener_address: input.uplink.cloud_uri(),
                restarts: 0,
                last_error: None,
                uplink: None,
                stalled_polls: 0,
            });
    }

    /// Removes an input that is no longer running, so the UI and the registration
    /// loop stop reporting it.
    pub fn remove_input(&self, input_id: &str) {
        self.inputs.lock().expect("state poisoned").remove(input_id);
    }

    pub fn gateway_id(&self) -> &str {
        &self.gateway_id
    }

    pub fn snapshot(&self) -> GatewayStatus {
        let inputs = self
            .inputs
            .lock()
            .expect("state poisoned")
            .values()
            .cloned()
            .collect();

        GatewayStatus {
            gateway_id: self.gateway_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started.elapsed().as_secs(),
            strom_reachable: self.strom_reachable.load(Ordering::Relaxed),
            inputs,
        }
    }

    pub fn set_strom_reachable(&self, reachable: bool) {
        self.strom_reachable.store(reachable, Ordering::Relaxed);
    }

    pub fn set_state(&self, input_id: &str, state: InputState) {
        self.mutate(input_id, |status| status.state = state);
    }

    pub fn set_gst_state(&self, input_id: &str, gst_state: Option<&str>) {
        self.mutate(input_id, |status| {
            status.gst_state = gst_state.map(str::to_string);
        });
    }

    pub fn set_source_id(&self, input_id: &str, source_id: &str) {
        self.mutate(input_id, |status| {
            status.source_id = Some(source_id.to_string());
        });
    }

    /// Records an uplink sample and reports whether it is delivering, which is true
    /// only when `bytes_sent` advanced since the previous sample.
    /// Records an uplink sample, returning whether it is delivering and how many
    /// consecutive polls have now delivered nothing.
    pub fn record_uplink(&self, input_id: &str, sample: &SrtUplink) -> UplinkSample {
        let mut inputs = self.inputs.lock().expect("state poisoned");
        let Some(status) = inputs.get_mut(input_id) else {
            return UplinkSample {
                delivering: false,
                consecutive_stalls: 0,
            };
        };

        let delivering = match status.uplink.as_ref().map(|u| u.bytes_sent) {
            // Any change means bytes are moving. Not just an increase: a flow restart
            // resets the counter, and requiring growth would then treat a healthy new
            // connection as stalled until it passed the old total.
            Some(before) => sample.bytes_sent != before && sample.bytes_sent > 0,
            // The first sample cannot prove movement; require a second one rather
            // than declaring an unproven uplink healthy.
            None => false,
        };

        status.stalled_polls = if delivering {
            0
        } else {
            status.stalled_polls.saturating_add(1)
        };
        let consecutive_stalls = status.stalled_polls;

        status.uplink = Some(UplinkStats {
            delivering,
            bytes_sent: sample.bytes_sent,
            rtt_ms: sample.rtt_ms,
            send_rate_mbps: sample.send_rate_mbps,
            packets_retransmitted: sample.packets_retransmitted,
            packets_sent_dropped: sample.packets_sent_dropped,
            negotiated_latency_ms: sample.negotiated_latency_ms,
        });

        UplinkSample {
            delivering,
            consecutive_stalls,
        }
    }

    pub fn clear_uplink(&self, input_id: &str) {
        self.mutate(input_id, |status| {
            status.uplink = None;
            status.stalled_polls = 0;
        });
    }

    pub fn record_error(&self, input_id: &str, err: &str) {
        self.mutate(input_id, |status| {
            status.last_error = Some(err.to_string());
            status.restarts = status.restarts.saturating_add(1);
        });
    }

    fn mutate(&self, input_id: &str, f: impl FnOnce(&mut InputStatus)) {
        let mut inputs = self.inputs.lock().expect("state poisoned");
        if let Some(status) = inputs.get_mut(input_id) {
            f(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_live_gateway_types::config::{
        CaptureConfig, GatewayIdentity, InputConfig, StromConfig, UplinkConfig, UplinkMode,
        VideoConfig,
    };

    fn state() -> SharedState {
        let cfg = GatewayConfig {
            gateway: GatewayIdentity {
                id: Some("gw".to_string()),
                name: "GW".to_string(),
            },
            strom: StromConfig {
                url: "http://127.0.0.1:8080".to_string(),
                api_key: None,
            },
            inputs: vec![InputConfig {
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
                    mode: UplinkMode::Caller,
                    host: "cloud.example.com".to_string(),
                    public_host: None,
                    port: 9000,
                    latency_ms: 200,
                    passphrase: None,
                    pbkeylen: None,
                    stream_id: None,
                },
                enabled: true,
            }],
            app: Default::default(),
            open_live: Default::default(),
            control: Default::default(),
            log: Default::default(),
        };
        SharedState::new("gw".to_string(), &cfg)
    }

    fn sample(bytes_sent: u64) -> SrtUplink {
        SrtUplink {
            bytes_sent,
            ..SrtUplink::default()
        }
    }

    /// The first sample has nothing to compare against, so it cannot prove the uplink
    /// is delivering — reporting it healthy would publish an unverified feed.
    #[test]
    fn the_first_sample_is_never_delivering() {
        let s = state();
        let r = s.record_uplink("cam1", &sample(1000));
        assert!(!r.delivering);
        assert_eq!(r.consecutive_stalls, 1);
    }

    #[test]
    fn advancing_bytes_mean_delivering_and_reset_the_stall_count() {
        let s = state();
        s.record_uplink("cam1", &sample(1000));
        let r = s.record_uplink("cam1", &sample(2000));
        assert!(r.delivering);
        assert_eq!(r.consecutive_stalls, 0);
    }

    #[test]
    fn a_frozen_counter_accumulates_stalls() {
        let s = state();
        s.record_uplink("cam1", &sample(1000));
        s.record_uplink("cam1", &sample(2000));
        for expected in 1..=3 {
            let r = s.record_uplink("cam1", &sample(2000));
            assert!(!r.delivering);
            assert_eq!(r.consecutive_stalls, expected);
        }
    }

    /// A flow restart resets the SRT counter, so requiring growth would treat a
    /// healthy new connection as stalled until it passed the previous total.
    #[test]
    fn a_counter_reset_after_a_restart_counts_as_delivering() {
        let s = state();
        s.record_uplink("cam1", &sample(50_000_000));
        s.record_uplink("cam1", &sample(60_000_000));
        let r = s.record_uplink("cam1", &sample(5000));
        assert!(
            r.delivering,
            "a reset counter is a new connection, not a stall"
        );
        assert_eq!(r.consecutive_stalls, 0);
    }

    /// Zero stays a stall: an SRT socket that connected but never sent anything
    /// reports zero on every poll, and that is precisely the dead case to catch.
    #[test]
    fn a_counter_stuck_at_zero_is_a_stall() {
        let s = state();
        s.record_uplink("cam1", &sample(0));
        let r = s.record_uplink("cam1", &sample(0));
        assert!(!r.delivering);
        assert_eq!(r.consecutive_stalls, 2);
    }

    #[test]
    fn clearing_the_uplink_resets_the_stall_count() {
        let s = state();
        s.record_uplink("cam1", &sample(1000));
        s.record_uplink("cam1", &sample(1000));
        s.clear_uplink("cam1");
        let snapshot = s.snapshot();
        assert_eq!(snapshot.inputs[0].stalled_polls, 0);
        assert!(snapshot.inputs[0].uplink.is_none());
    }
}
