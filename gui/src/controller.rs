//! The async half of the app: everything that talks to Strom and Open Live.
//!
//! Kept apart from the UI so the UI thread never blocks on a network call. They share
//! two things: `SharedState`, which the supervisors already write per-input status
//! into, and a small `UiState` for the device list and the last error.

use anyhow::Result;
use open_live_gateway::openlive::client::OpenLiveClient;
use open_live_gateway::openlive::registration;
use open_live_gateway::session::{self, ActiveInput};
use open_live_gateway::state::SharedState;
use open_live_gateway::strom::client::{CaptureDevice, StromClient};
use open_live_gateway::strom::supervisor;
use open_live_gateway_types::GatewayConfig;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// What the UI asks the controller to do.
pub enum Cmd {
    Rescan,
    Start(CaptureDevice),
    Stop(String),
    /// Stop everything and acknowledge, so the window can close cleanly.
    Shutdown(oneshot::Sender<()>),
}

/// What the UI reads, beyond the per-input status in `SharedState`.
#[derive(Default)]
pub struct UiState {
    pub devices: Vec<CaptureDevice>,
    /// input id -> the device it came from and its registered address.
    pub active: BTreeMap<String, ActiveView>,
    pub strom_reachable: bool,
    pub last_error: Option<String>,
    pub busy: bool,
}

#[derive(Clone)]
pub struct ActiveView {
    pub label: String,
    pub listener_address: String,
}

pub type Ui = Arc<Mutex<UiState>>;

/// Per-input tasks, aborted when the input is stopped.
struct Tasks {
    supervisor: JoinHandle<()>,
    registration: Option<JoinHandle<()>>,
}

pub struct Controller {
    cfg: GatewayConfig,
    gateway_id: String,
    state: Arc<SharedState>,
    ui: Ui,
    strom: StromClient,
    open_live: Option<OpenLiveClient>,
    state_path: PathBuf,
    active: HashMap<String, ActiveInput>,
    tasks: HashMap<String, Tasks>,
}

impl Controller {
    pub fn new(
        cfg: GatewayConfig,
        gateway_id: String,
        state: Arc<SharedState>,
        ui: Ui,
    ) -> Result<Self> {
        let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
        // Registration is optional: without it the feed still runs, it just does not
        // appear in Studio. Losing it must never stop the app from working.
        let open_live = if cfg.open_live.register {
            match registration::client_from(&cfg) {
                Ok(c) => Some(c),
                Err(err) => {
                    warn!(%err, "Open Live registration unavailable, continuing without it");
                    None
                }
            }
        } else {
            None
        };
        let state_path = PathBuf::from(&cfg.open_live.state_path);

        Ok(Self {
            cfg,
            gateway_id,
            state,
            ui,
            strom,
            open_live,
            state_path,
            active: HashMap::new(),
            tasks: HashMap::new(),
        })
    }

    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        self.rescan().await;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Rescan => self.rescan().await,
                Cmd::Start(device) => {
                    if let Err(err) = self.start(device).await {
                        self.fail(err);
                    }
                }
                Cmd::Stop(input_id) => self.stop(&input_id).await,
                Cmd::Shutdown(ack) => {
                    // Closing the window ends the streams: that is the whole point of
                    // this front end, and it is why nothing has to be reconciled
                    // against leftovers on the next run.
                    let ids: Vec<String> = self.active.keys().cloned().collect();
                    for id in ids {
                        self.stop(&id).await;
                    }
                    let _ = ack.send(());
                    return;
                }
            }
        }
    }

    async fn rescan(&mut self) {
        self.set_busy(true);
        match self.strom.devices("video_source").await {
            Ok(devices) => {
                info!(count = devices.len(), "scanned capture devices");
                let mut ui = self.ui.lock().expect("ui poisoned");
                ui.devices = devices;
                ui.strom_reachable = true;
                ui.last_error = None;
            }
            Err(err) => {
                let mut ui = self.ui.lock().expect("ui poisoned");
                ui.strom_reachable = false;
                ui.last_error = Some(format!("Strom unreachable: {err}"));
            }
        }
        self.set_busy(false);
    }

    async fn start(&mut self, device: CaptureDevice) -> Result<()> {
        let input_id = session::input_id_for_device(&device.id);
        if self.active.contains_key(&input_id) {
            return Ok(());
        }
        self.set_busy(true);

        // Never reuse a port a configured input already claimed, or one in use here.
        let mut taken: BTreeSet<u16> = session::ports_in_config(&self.cfg);
        taken.extend(self.active.values().map(|a| a.input.uplink.port));
        let port = session::allocate_port(&self.cfg.app.uplink, &taken)?;

        let input = session::input_for_device(&self.cfg.app, &device, port);
        let active = session::start_input(
            &self.strom,
            &self.gateway_id,
            &self.cfg.gateway.name,
            &input,
        )
        .await?;

        self.state.add_input(&input, &self.gateway_id);

        let supervisor = tokio::spawn(supervisor::supervise(
            Arc::clone(&self.state),
            StromClient::new(&self.cfg.strom.url, self.cfg.strom.api_key.as_deref())?,
            input.clone(),
            self.gateway_id.clone(),
            self.cfg.gateway.name.clone(),
        ));

        let registration = match (&self.open_live, self.cfg.open_live.url.as_deref()) {
            (Some(_), Some(_)) => {
                let client = registration::client_from(&self.cfg)?;
                Some(tokio::spawn(registration::reconcile_forever(
                    Arc::clone(&self.state),
                    client,
                    input.clone(),
                    self.cfg.gateway.name.clone(),
                    self.state_path.clone(),
                )))
            }
            _ => None,
        };

        self.tasks.insert(
            input_id.clone(),
            Tasks {
                supervisor,
                registration,
            },
        );

        {
            let mut ui = self.ui.lock().expect("ui poisoned");
            let view = active.view();
            ui.active.insert(
                input_id.clone(),
                ActiveView {
                    label: view.label,
                    listener_address: view.listener_address,
                },
            );
            ui.last_error = None;
        }
        self.active.insert(input_id.clone(), active);
        info!(input = %input_id, port, device = %device.display_name, "started");
        self.set_busy(false);
        Ok(())
    }

    async fn stop(&mut self, input_id: &str) {
        self.set_busy(true);
        if let Some(tasks) = self.tasks.remove(input_id) {
            tasks.supervisor.abort();
            if let Some(r) = tasks.registration {
                r.abort();
            }
        }
        // Mark the source inactive before the flow goes, so Studio does not show a
        // feed as available after it has stopped.
        if let Some(client) = &self.open_live {
            registration::mark_inactive(client, &self.state_path, input_id).await;
        }
        if let Some(active) = self.active.remove(input_id) {
            if let Err(err) = session::stop_input(&self.strom, &active).await {
                warn!(input = %input_id, %err, "could not remove the flow");
            }
        }
        self.state.remove_input(input_id);
        self.ui.lock().expect("ui poisoned").active.remove(input_id);
        info!(input = %input_id, "stopped");
        self.set_busy(false);
    }

    fn fail(&self, err: anyhow::Error) {
        warn!(%err, "command failed");
        self.ui.lock().expect("ui poisoned").last_error = Some(err.to_string());
    }

    fn set_busy(&self, busy: bool) {
        self.ui.lock().expect("ui poisoned").busy = busy;
    }
}
