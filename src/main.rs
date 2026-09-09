//! Open Live Gateway.
//!
//! Streams the capture devices on this machine into Open Live. Run it and it asks for
//! whatever it needs, checks the answers, remembers them, then registers every device
//! it finds and starts sending. Built to be driven over SSH: prompts on a plain
//! terminal, no window, no full-screen UI.
//!
//! Strom does the media: capture, encode, mux, SRT. This drives it over HTTP and
//! registers the result with Open Live. See docs/DESIGN.md for the reasoning.

mod config;
mod devices;
mod flow;
mod local_strom;
mod openlive;
mod osc;
mod run;
mod setup;
mod strom;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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

        /// Only these devices, by id or name fragment, e.g. `--devices "FaceTime,DeckLink"`.
        #[arg(long)]
        devices: Option<String>,

        /// Stream a test pattern and tone instead of any device, to commission the
        /// link before the cameras arrive.
        #[arg(long)]
        test: bool,

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
    let path = cli.config.clone().unwrap_or_else(config::default_path);
    let mut cfg = config::load_or_default(&path)
        .with_context(|| format!("loading settings from {}", path.display()))?;
    if let Some(level) = &cli.log_level {
        cfg.log.level = level.clone();
    }
    config::init_tracing(&cfg.log.level);

    let command = cli.command.unwrap_or(Command::Up {
        all: false,
        devices: None,
        test: false,
        reconfigure: false,
    });

    match command {
        Command::Check => {
            config::validate(&cfg)?;
            println!("Settings at {} are valid.", path.display());
            Ok(())
        }
        Command::Setup => {
            if setup::configure(&mut cfg, true).await? {
                config::save(&path, &cfg)?;
                println!("\nSaved to {}.", path.display());
            }
            Ok(())
        }
        Command::Up {
            all,
            devices,
            test,
            reconfigure,
        } => {
            if setup::configure(&mut cfg, reconfigure).await? {
                config::save(&path, &cfg)?;
                println!("\nSaved to {}.\n", path.display());
            }
            config::validate(&cfg)?;
            run::up(cfg, all, devices, test).await
        }
        Command::Down => {
            config::validate(&cfg)?;
            run::down(cfg).await
        }
        Command::Status => {
            config::validate(&cfg)?;
            run::status(cfg).await
        }
        // Listing devices needs only Strom, so an unfinished Open Live setup must not
        // stand in the way.
        Command::Devices => run::devices(cfg).await,
    }
}
