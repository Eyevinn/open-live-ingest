//! Runtime state shared between the flow supervisors, the control API, and the Open
//! Live registration loop.

use open_live_gateway_types::status::{GatewayStatus, InputState, InputStatus};
use open_live_gateway_types::GatewayConfig;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

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
                        listener_address: input.uplink.listener_uri(),
                        restarts: 0,
                        last_error: None,
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
