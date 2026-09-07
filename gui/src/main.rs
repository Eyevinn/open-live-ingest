//! Open Live Gateway — desktop app.
//!
//! Lists the capture devices Strom can see, and streams the one you pick into Open
//! Live for as long as this window is open. Closing it stops the streams and removes
//! the flows, which is what keeps it simple: there is no daemon, no unattended
//! recovery, and nothing left behind to reconcile next time.
//!
//! For a venue box that must come back on its own after a power cut, use the headless
//! binary and its config file instead.

mod controller;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use controller::{Controller, UiState};
use open_live_gateway::{config, identity, state::SharedState};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Parser)]
#[command(name = "open-live-gateway-gui", version)]
struct Cli {
    /// Path to the configuration file. Only the Strom and Open Live sections and the
    /// [app] defaults are used; inputs are chosen in the window.
    #[arg(short, long, env = "OLG_CONFIG", default_value = "gateway.toml")]
    config: PathBuf,

    #[arg(long, env = "OLG_LOG_LEVEL")]
    log_level: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load(&cli.config, cli.log_level.as_deref())
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    config::init_tracing(&cfg.log.level);

    let gateway_id = identity::resolve_gateway_id(&cfg);
    let state = Arc::new(SharedState::new(gateway_id.clone(), &cfg));
    let ui_state: controller::Ui = Arc::new(Mutex::new(UiState::default()));

    // The async side runs on its own runtime threads; the UI thread only ever reads
    // shared state and sends commands, so a slow network call cannot freeze a frame.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;

    tracing::info!(
        gateway_id = %gateway_id,
        strom = %cfg.strom.url,
        "starting the Open Live Gateway app"
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let controller = Controller::new(
        cfg.clone(),
        gateway_id,
        Arc::clone(&state),
        Arc::clone(&ui_state),
    )?;
    runtime.spawn(controller.run(rx));

    let app = ui::App::new(cfg, state, ui_state, tx, runtime.handle().clone());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([460.0, 420.0])
            .with_title("Open Live Gateway"),
        ..Default::default()
    };

    eframe::run_native(
        "Open Live Gateway",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(())
}
