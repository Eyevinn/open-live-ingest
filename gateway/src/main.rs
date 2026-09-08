//! Open Live Gateway.
//!
//! Streams the capture devices on this machine into Open Live. Run it and it asks for
//! whatever it needs, checks the answers, remembers them, then registers every device
//! it finds and starts sending. Built to be driven over SSH: prompts on a plain
//! terminal, no window, no full-screen UI.
//!
//! Strom does the media — capture, encode, mux, SRT — and this drives it. See
//! docs/DESIGN.md.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use open_live_gateway::{config, identity, prompt, runner};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "open-live-gateway", version, about)]
struct Cli {
    /// Settings file. Defaults to a per-user location and is written on first setup.
    #[arg(short, long, env = "OLG_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Log level (`trace`, `debug`, `info`, `warn`, `error`).
    #[arg(long, env = "OLG_LOG_LEVEL", global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Register every capture device with Open Live and stream it. Stays running:
    /// Ctrl-C stops and removes the feeds, while a closed SSH session does not.
    Up {
        /// Include virtual devices, which are skipped by default because they
        /// produce nothing unless another application is running.
        #[arg(long)]
        all: bool,

        /// Only these devices, by id or name, e.g. `--devices "FaceTime,DeckLink"`.
        #[arg(long)]
        devices: Option<String>,

        /// Ask for every setting again, even the ones already stored.
        #[arg(long)]
        reconfigure: bool,
    },
    /// Stop a running gateway and remove its flows and Open Live sources.
    Down,
    /// Report what is running, from Strom and Open Live directly.
    Status,
    /// List the capture devices Strom can see, without starting anything.
    Devices,
    /// Ask for settings and store them without starting anything.
    Setup,
    /// Check the settings file and exit.
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(config::default_path);

    let mut cfg = config::load_or_default(&config_path)
        .with_context(|| format!("loading settings from {}", config_path.display()))?;
    if let Some(level) = &cli.log_level {
        cfg.log.level = level.clone();
    }
    config::init_tracing(&cfg.log.level);

    let command = cli.command.unwrap_or(Command::Up {
        all: false,
        devices: None,
        reconfigure: false,
    });

    match command {
        Command::Check => {
            config::validate(&cfg)?;
            println!(
                "Settings at {} are valid ({} declared input(s)).",
                config_path.display(),
                cfg.inputs.len()
            );
            Ok(())
        }

        Command::Setup => {
            if prompt::configure(&mut cfg, true).await? {
                config::save(&config_path, &cfg)?;
                println!("\nSaved to {}.", config_path.display());
            }
            Ok(())
        }

        Command::Up {
            all,
            devices,
            reconfigure,
        } => {
            if prompt::configure(&mut cfg, reconfigure).await? {
                config::save(&config_path, &cfg)?;
                println!("\nSaved to {}.\n", config_path.display());
            }
            config::validate(&cfg)?;
            let gateway_id = identity::resolve_gateway_id(&cfg);
            runner::up(cfg, gateway_id, all, devices).await
        }

        Command::Down => {
            config::validate(&cfg)?;
            let gateway_id = identity::resolve_gateway_id(&cfg);
            runner::down(cfg, gateway_id).await
        }

        Command::Devices => {
            config::validate(&cfg)?;
            runner::devices(cfg).await
        }

        Command::Status => {
            config::validate(&cfg)?;
            let gateway_id = identity::resolve_gateway_id(&cfg);
            runner::status(cfg, gateway_id).await
        }
    }
}
