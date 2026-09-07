//! Open Live Gateway — control agent for a venue-side Strom instance.
//!
//! Templates and supervises a Strom flow that captures SDI, encodes it, and pushes it
//! over SRT to a cloud-hosted Strom driven by Open Live; and registers the feed with
//! the Open Live API so operators can assign it to a mixer input. The gateway owns no
//! media pipeline of its own. See docs/DESIGN.md.

use open_live_gateway::{config, control, identity, openlive, state, strom};

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(name = "open-live-gateway", version)]
struct Cli {
    /// Path to the configuration file.
    #[arg(
        short,
        long,
        env = "OLG_CONFIG",
        default_value = "/etc/open-live-gateway/gateway.toml"
    )]
    config: PathBuf,

    /// Log level override (`trace`, `debug`, `info`, `warn`, `error`).
    #[arg(long, env = "OLG_LOG_LEVEL")]
    log_level: Option<String>,

    /// Validate the configuration and exit without starting anything.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load(&cli.config, cli.log_level.as_deref())
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    config::init_tracing(&cfg.log.level);

    // Only the headless daemon needs inputs up front; the desktop app creates them
    // when an operator picks a device.
    if cfg.inputs.is_empty() {
        anyhow::bail!("no inputs configured — the headless daemon needs at least one [[inputs]]");
    }

    if cli.check {
        info!("configuration is valid: {} input(s)", cfg.inputs.len());
        return Ok(());
    }

    let gateway_id = identity::resolve_gateway_id(&cfg);
    info!(gateway_id = %gateway_id, inputs = cfg.inputs.len(), "starting Open Live Gateway");

    let state = Arc::new(state::SharedState::new(gateway_id, &cfg));

    // Flows first: the feed must come up even if Open Live cannot be reached. See
    // the invariant in docs/DESIGN.md §1.
    strom::spawn_supervisors(Arc::clone(&state), &cfg)?;

    if cfg.open_live.register {
        match openlive::spawn_registration(Arc::clone(&state), &cfg) {
            Ok(()) => info!("Open Live registration loop started"),
            // Registration is best-effort by design and must never abort startup.
            Err(err) => warn!(%err, "Open Live registration unavailable, continuing without it"),
        }
    } else {
        info!("Open Live registration disabled by config");
    }

    control::serve(Arc::clone(&state), &cfg.control).await
}
