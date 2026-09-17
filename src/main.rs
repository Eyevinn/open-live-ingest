//! Open Live Ingest.
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
mod heartbeat;
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
#[command(name = "open-live-ingest", version, about)]
struct Cli {
    /// Settings file. Defaults to a per-user location and is written on first setup.
    #[arg(short, long, env = "OLI_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Log level (`trace`, `debug`, `info`, `warn`, `error`).
    #[arg(long, env = "OLI_LOG_LEVEL", global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

// Parsed once and matched once; the size of the setup variant is not worth a Box.
#[allow(clippy::large_enum_variant)]
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
    Status {
        /// Machine-readable output, one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// List the capture devices Strom can see, without starting anything.
    Devices {
        /// Machine-readable output, one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Ask for settings and store them without starting anything. Flags pre-answer
    /// the questions; with --non-interactive nothing is asked.
    #[command(
        after_help = "Credentials are never flags. Set OLI_OPEN_LIVE_API_KEY and OLI_STROM_API_KEY, \
or for Open Source Cloud set OSC_ACCESS_TOKEN or run `npx @osaas/cli login`. The gateway token \
that goes with --gateway-id is OLI_OPEN_LIVE_GATEWAY_TOKEN."
    )]
    Setup(setup::Answers),
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
        Command::Setup(answers) => {
            answers.apply(&mut cfg);
            let changed = if answers.non_interactive {
                setup::configure_headless(&mut cfg).await?;
                true
            } else {
                setup::configure(&mut cfg, true).await?
            };
            if changed {
                config::save(&path, &cfg)?;
                setup::report_saved(&path);
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
                setup::report_saved(&path);
            }
            config::validate(&cfg)?;
            run::up(cfg, all, devices, test).await
        }
        Command::Down => {
            config::validate(&cfg)?;
            run::down(cfg).await
        }
        Command::Status { json } => {
            config::validate(&cfg)?;
            run::status(cfg, json).await
        }
        // Listing devices needs only Strom, so an unfinished Open Live setup must not
        // stand in the way.
        Command::Devices { json } => run::devices(cfg, json).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UplinkMode;

    #[test]
    fn setup_takes_its_answers_as_flags_but_never_a_credential() {
        let cli = Cli::try_parse_from([
            "open-live-ingest",
            "setup",
            "--non-interactive",
            "--name",
            "Venue",
            "--open-live-url",
            "https://open-live.example.com",
            "--uplink-mode",
            "listener",
            "--public-host",
            "198.51.100.7",
            "--port-range",
            "47110-47129",
            "--gateway-id",
            "gw-1",
        ])
        .expect("parses");
        let Some(Command::Setup(answers)) = cli.command else {
            panic!("not setup");
        };
        assert!(answers.non_interactive);
        assert_eq!(answers.name.as_deref(), Some("Venue"));
        assert_eq!(answers.uplink_mode, Some(UplinkMode::Listener));
        assert_eq!(answers.public_host.as_deref(), Some("198.51.100.7"));
        assert_eq!(answers.gateway_id.as_deref(), Some("gw-1"));

        for secret in [
            "--open-live-api-key",
            "--strom-api-key",
            "--passphrase",
            "--gateway-token",
        ] {
            assert!(
                Cli::try_parse_from(["open-live-ingest", "setup", secret, "x"]).is_err(),
                "{secret} must not be a flag"
            );
        }
    }

    #[test]
    fn status_and_devices_take_json() {
        for sub in ["status", "devices"] {
            Cli::try_parse_from(["open-live-ingest", sub, "--json"]).expect(sub);
        }
    }
}
