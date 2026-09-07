//! The window. Reads shared state, sends commands, holds no logic of its own.

use crate::controller::{Cmd, Ui};
use eframe::egui;
use open_live_gateway::state::SharedState;
use open_live_gateway_types::status::InputState;
use open_live_gateway_types::GatewayConfig;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub struct App {
    cfg: GatewayConfig,
    state: Arc<SharedState>,
    ui: Ui,
    tx: mpsc::UnboundedSender<Cmd>,
    runtime: tokio::runtime::Handle,
    shutting_down: bool,
}

impl App {
    pub fn new(
        cfg: GatewayConfig,
        state: Arc<SharedState>,
        ui: Ui,
        tx: mpsc::UnboundedSender<Cmd>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
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
        let (devices, active, strom_ok, last_error, busy) = {
            let ui = self.ui.lock().expect("ui poisoned");
            (
                ui.devices.clone(),
                ui.active.clone(),
                ui.strom_reachable,
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

fn dot(ui: &mut egui::Ui, ok: bool) {
    let colour = if ok {
        egui::Color32::from_rgb(70, 180, 90)
    } else {
        egui::Color32::from_rgb(180, 80, 80)
    };
    ui.colored_label(colour, "\u{25CF}");
}
