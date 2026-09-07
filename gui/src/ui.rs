//! The window. Reads shared state, sends commands, holds no logic of its own.

use crate::controller::{Cmd, Ui};
use crate::settings::Form;
use eframe::egui;
use open_live_gateway::state::SharedState;
use open_live_gateway_types::config::UplinkMode;
use open_live_gateway_types::status::InputState;
use open_live_gateway_types::GatewayConfig;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub struct App {
    cfg: GatewayConfig,
    config_path: std::path::PathBuf,
    form: Form,
    settings_open: bool,
    saved_note: Option<String>,
    state: Arc<SharedState>,
    ui: Ui,
    tx: mpsc::UnboundedSender<Cmd>,
    runtime: tokio::runtime::Handle,
    shutting_down: bool,
}

impl App {
    pub fn new(
        cfg: GatewayConfig,
        config_path: std::path::PathBuf,
        state: Arc<SharedState>,
        ui: Ui,
        tx: mpsc::UnboundedSender<Cmd>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let form = Form::from_config(&cfg);
        // Nothing to connect to yet means a first run: open the form rather than
        // showing an empty device list with no hint about why.
        let settings_open = cfg.open_live.url.is_none();
        Self {
            form,
            settings_open,
            saved_note: None,
            config_path,
            cfg,
            state,
            ui,
            tx,
            runtime,
            shutting_down: false,
        }
    }
}

/// How an input's state should read to an operator.
///
/// `Stalled` is the one that needs care: nothing is receiving the feed yet, which is
/// the normal condition between starting a device and activating the production that
/// consumes it. Calling that "error" would send people hunting for a fault that isn't
/// there — the confusion this project spent an afternoon on.
fn describe(state: InputState) -> (egui::Color32, &'static str, &'static str) {
    match state {
        InputState::Running => (
            egui::Color32::from_rgb(70, 180, 90),
            "on air",
            "the far end is receiving this feed",
        ),
        InputState::Stalled => (
            egui::Color32::from_rgb(220, 170, 60),
            "waiting for a receiver",
            "assign this source to a mixer input in Studio and activate the production",
        ),
        InputState::Starting | InputState::Provisioning => (
            egui::Color32::from_rgb(120, 150, 200),
            "starting",
            "building the flow in Strom",
        ),
        InputState::Unknown => (
            egui::Color32::from_rgb(200, 130, 60),
            "Strom unreachable",
            "the feed may still be running — this app has lost contact with Strom",
        ),
        InputState::Failed => (
            egui::Color32::from_rgb(200, 80, 80),
            "failed",
            "see the message below",
        ),
        InputState::Idle => (egui::Color32::GRAY, "idle", ""),
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Status comes from background tasks, so repaint on a timer rather than only
        // on input.
        root.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));

        let snapshot = self.state.snapshot();
        let (devices, active, strom_ok, cloud_host, last_error, busy) = {
            let ui = self.ui.lock().expect("ui poisoned");
            (
                ui.devices.clone(),
                ui.active.clone(),
                ui.strom_reachable,
                ui.cloud_host.clone(),
                ui.last_error.clone(),
                ui.busy,
            )
        };

        egui::Panel::top("header").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                dot(ui, strom_ok);
                ui.label("Strom");
                ui.monospace(&self.cfg.strom.url);
            });
            ui.horizontal(|ui| {
                let registering = self.cfg.open_live.register && self.cfg.open_live.url.is_some();
                dot(ui, registering);
                ui.label("Open Live");
                match self.cfg.open_live.url.as_deref() {
                    Some(url) if registering => ui.monospace(url),
                    _ => ui.weak("not registering — feeds will not appear in Studio"),
                };
            });
            ui.horizontal(|ui| {
                let configured = self
                    .cfg
                    .app
                    .uplink
                    .host
                    .clone()
                    .filter(|h| !h.trim().is_empty());
                let target = configured.or(cloud_host);
                dot(ui, target.is_some());
                ui.label("Sending to");
                match target {
                    Some(host) => ui.monospace(host),
                    None => ui.weak("unknown — Open Live has not reported its Strom host"),
                };
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("footer").show(root, |ui| {
            ui.add_space(4.0);
            if let Some(err) = &last_error {
                ui.colored_label(egui::Color32::from_rgb(200, 80, 80), err);
            } else {
                ui.weak("Closing this window stops every feed and removes its flow from Strom.");
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let streaming = !active.is_empty();
                let header = if self.settings_open {
                    "Settings \u{25BE}"
                } else {
                    "Settings \u{25B8}"
                };
                if ui.selectable_label(self.settings_open, header).clicked() {
                    self.settings_open = !self.settings_open;
                }
                if self.settings_open {
                    self.settings_form(ui, streaming);
                    ui.add_space(10.0);
                }

                ui.horizontal(|ui| {
                    ui.heading("Capture devices");
                    if ui.add_enabled(!busy, egui::Button::new("Rescan")).clicked() {
                        let _ = self.tx.send(Cmd::Rescan);
                    }
                    if busy {
                        ui.spinner();
                    }
                });
                ui.separator();

                if devices.is_empty() {
                    ui.weak(if strom_ok {
                        "Strom reports no video sources on this machine."
                    } else {
                        "Cannot reach Strom. Is it running?"
                    });
                }

                for device in &devices {
                    let input_id = open_live_gateway::session::input_id_for_device(&device.id);
                    let is_active = active.contains_key(&input_id);
                    ui.horizontal(|ui| {
                        ui.label(&device.display_name);
                        if let Some(p) = &device.provider {
                            ui.weak(format!("[{p}]"));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if is_active {
                                if ui.add_enabled(!busy, egui::Button::new("Stop")).clicked() {
                                    let _ = self.tx.send(Cmd::Stop(input_id.clone()));
                                }
                            } else if ui.add_enabled(!busy, egui::Button::new("Start")).clicked() {
                                let _ = self.tx.send(Cmd::Start(device.clone()));
                            }
                        });
                    });
                }

                if !active.is_empty() {
                    ui.add_space(14.0);
                    ui.heading("Streaming");
                    ui.separator();
                }

                for (input_id, view) in &active {
                    let status = snapshot.inputs.iter().find(|i| &i.id == input_id);
                    let state = status.map(|s| s.state).unwrap_or(InputState::Idle);
                    let (colour, label, hint) = describe(state);

                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.strong(&view.label);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.colored_label(colour, label);
                                },
                            );
                        });
                        if !hint.is_empty() {
                            ui.weak(hint);
                        }
                        if let Some(uplink) = status.and_then(|s| s.uplink.as_ref()) {
                            let rate = uplink.send_rate_mbps.unwrap_or(0.0);
                            let rtt = uplink.rtt_ms.unwrap_or(0.0);
                            ui.monospace(format!(
                                "{rate:.2} Mbps   rtt {rtt:.1} ms   retrans {}",
                                uplink.packets_retransmitted.unwrap_or(0)
                            ));
                        }
                        ui.monospace(&view.listener_address);
                        if let Some(err) = status.and_then(|s| s.last_error.as_deref()) {
                            ui.colored_label(egui::Color32::from_rgb(200, 140, 60), err);
                        }
                    });
                }
            });
        });
    }

    fn on_exit(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        // Block until the controller has torn every flow down. Without this the
        // process can exit first and leave flows running in Strom.
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(ack_tx)).is_ok() {
            let _ = self.runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(15), ack_rx).await
            });
        }
    }
}

impl App {
    fn settings_form(&mut self, ui: &mut egui::Ui, streaming: bool) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            if streaming {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 170, 60),
                    "Stop every feed before changing these — a live input keeps the \
                     connection it was started with.",
                );
                ui.add_space(4.0);
            }
            ui.add_enabled_ui(!streaming, |ui| {
                egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.gateway_name)
                            .hint_text("shown on the sources in Studio"),
                    );
                    ui.end_row();

                    ui.label("Open Live URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.open_live_url)
                            .hint_text("https://open-live.example.com"),
                    );
                    ui.end_row();

                    ui.label("Credential");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.open_live_key)
                            .password(true)
                            .hint_text("OSC personal access token, or an API key"),
                    );
                    ui.end_row();

                    ui.label("Credential kind");
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut self.form.auth_mode,
                            "osc".to_string(),
                            "OSC token",
                        );
                        ui.selectable_value(
                            &mut self.form.auth_mode,
                            "direct".to_string(),
                            "API key",
                        );
                    });
                    ui.end_row();

                    ui.label("Register sources");
                    ui.checkbox(&mut self.form.register, "show these feeds in Studio");
                    ui.end_row();

                    ui.label("Local Strom");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.strom_url)
                            .hint_text("http://127.0.0.1:8080"),
                    );
                    ui.end_row();

                    ui.label("Cloud Strom host");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.cloud_host)
                            .hint_text("leave blank to ask Open Live"),
                    );
                    ui.end_row();

                    ui.label("Uplink");
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut self.form.uplink_mode,
                            UplinkMode::Caller,
                            "we dial out",
                        );
                        ui.selectable_value(
                            &mut self.form.uplink_mode,
                            UplinkMode::Listener,
                            "cloud dials us",
                        );
                        ui.selectable_value(
                            &mut self.form.uplink_mode,
                            UplinkMode::Rendezvous,
                            "both dial",
                        );
                    });
                    ui.end_row();

                    if matches!(
                        self.form.uplink_mode,
                        UplinkMode::Listener | UplinkMode::Rendezvous
                    ) {
                        ui.label("Our public address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.public_host)
                                .hint_text("this machine, as the cloud sees it"),
                        );
                        ui.end_row();
                    }

                    ui.label("SRT ports");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.port_range)
                            .hint_text("9000-9100"),
                    );
                    ui.end_row();

                    ui.label("SRT latency (ms)");
                    ui.add(egui::TextEdit::singleline(&mut self.form.latency_ms));
                    ui.end_row();

                    ui.label("Capture format");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.resolution)
                                .desired_width(90.0)
                                .hint_text("1280x720"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.framerate)
                                .desired_width(60.0)
                                .hint_text("25/1"),
                        );
                    });
                    ui.end_row();

                    ui.label("Bitrate (kbps)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.bitrate_kbps).desired_width(80.0),
                    );
                    ui.end_row();
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.save_settings();
                    }
                    if ui.button("Revert").clicked() {
                        self.form = Form::from_config(&self.cfg);
                        self.saved_note = None;
                    }
                    if let Some(note) = &self.saved_note {
                        ui.weak(note);
                    }
                });
                ui.weak(format!("Stored in {}", self.config_path.display()));
            });
        });
    }

    fn save_settings(&mut self) {
        match self.form.to_config(&self.cfg) {
            Ok(cfg) => match open_live_gateway::config::save(&self.config_path, &cfg) {
                Ok(()) => {
                    self.cfg = cfg.clone();
                    self.saved_note = Some("saved".to_string());
                    let _ = self.tx.send(Cmd::Reconfigure(Box::new(cfg)));
                    self.ui.lock().expect("ui poisoned").last_error = None;
                }
                Err(err) => {
                    self.ui.lock().expect("ui poisoned").last_error = Some(format!("{err:#}"));
                }
            },
            Err(msg) => {
                self.ui.lock().expect("ui poisoned").last_error = Some(msg);
            }
        }
    }
}

fn dot(ui: &mut egui::Ui, ok: bool) {
    let colour = if ok {
        egui::Color32::from_rgb(70, 180, 90)
    } else {
        egui::Color32::from_rgb(180, 80, 80)
    };
    ui.colored_label(colour, "\u{25CF}");
}
