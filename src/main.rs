//! Open Live Ingest.
//!
//! Streams the capture devices on this machine into Open Live. Run it and it asks for
//! whatever it needs, checks the answers, remembers them, then registers every device
//! it finds and starts sending. Built to be driven over SSH: prompts on a plain
//! terminal, no window, no full-screen UI.
//!
//! Strom does the media: capture, encode, mux, SRT. This drives it over HTTP and
//! registers the result with Open Live. See docs/DESIGN.md for the reasoning.
//!
//! This is the command line layer over the `open_live_ingest` library: argument
//! parsing, logging, the setup prompts, signal handling, and rendering the library's
//! events and reports as text. Everything else is the library's.

mod setup;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use open_live_ingest::gateway::{Event, Events};
use open_live_ingest::run::{self, Stop};
use open_live_ingest::session::{Inputs, Session};
use open_live_ingest::{config, devices};
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

        /// Send an audio source with a camera, both by id or name fragment, e.g.
        /// `--audio "FaceTime=USB Microphone"`. Repeat for more cameras. One audio
        /// source goes with one camera. Replaces the settings' [[input]] audio for
        /// this run.
        #[arg(long = "audio", value_name = "CAMERA=AUDIO")]
        audio: Vec<devices::AudioPairing>,

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
        audio: Vec::new(),
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
            audio,
            test,
            reconfigure,
        } => {
            if setup::configure(&mut cfg, reconfigure).await? {
                config::save(&path, &cfg)?;
                setup::report_saved(&path);
            }
            config::validate(&cfg)?;
            up(
                cfg,
                Inputs {
                    include_virtual: all,
                    selection: devices,
                    audio,
                    test_pattern: test,
                },
            )
            .await
        }
        Command::Down => {
            config::validate(&cfg)?;
            run::down(&cfg, &reporter()).await
        }
        Command::Status { json } => {
            config::validate(&cfg)?;
            let report = run::status(&cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", run::render_status(&report));
            }
            Ok(())
        }
        // Listing devices needs only Strom, so an unfinished Open Live setup must not
        // stand in the way.
        Command::Devices { json } => {
            let report = run::devices(&cfg, &reporter()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", run::render_devices(&report));
            }
            Ok(())
        }
    }
}

/// `up`: a session, its events printed as they arrive, stopped on Ctrl-C or SIGTERM
/// (or, on Windows, the console closing). Returns how the session ended, so the exit
/// code is the one `up` has always had.
async fn up(cfg: config::Config, inputs: Inputs) -> Result<()> {
    let (mut session, mut events) = Session::start(cfg, inputs);
    let print = reporter();
    // The signal handlers go in once the inputs are up, as they did when `up` polled
    // them from its loop: until then a Ctrl-C ends the process the default way.
    let mut stop: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Stop>>>> = None;
    let mut stopping = false;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if matches!(event, Event::Summary { .. }) && stop.is_none() && !stopping {
                        stop = Some(Box::pin(stop_signal()));
                    }
                    print.emit(event);
                }
                // The session has ended and every event it emitted is printed.
                None => return session.wait().await,
            },
            why = async { stop.as_mut().expect("guarded").await }, if stop.is_some() => {
                // Polled no more: further signals are swallowed by the handlers,
                // which stay installed, as before.
                stop = None;
                stopping = true;
                session.request_stop(why);
            }
        }
    }
}

/// The library's events, printed to stdout as the lines the gateway has always
/// printed.
///
/// A failed write is ignored rather than a panic, as `println!` would: events arrive
/// during teardown, including from the drop of a managed Strom, and on Windows that
/// runs while the console is closing, logging off or shutting down, when Microsoft
/// documents that console I/O may not work reliably. A panic there would skip the
/// rest of the cleanup.
fn reporter() -> Events {
    Events::new(|event| {
        if let Some(text) = render(&event) {
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
    })
}

/// The text for one event, newline included, or None for an event the log already
/// reports: warnings are logged through `tracing` where they happen.
fn render(event: &Event) -> Option<String> {
    Some(match event {
        Event::StromStarting { port, log_path } => format!(
            "Starting Strom on port {port} (logging to {}).\n",
            log_path.display()
        ),
        Event::StromReady { video_sources } => {
            format!("  Strom is up; it can see {video_sources} video source(s).\n")
        }
        Event::StromStopped => "  strom — stopped\n".to_string(),
        Event::StromLeftOverStopping { pid } => {
            format!("  strom (pid {pid}) — stopping the one an earlier run started\n")
        }
        Event::BringingUp {
            inputs,
            host,
            ports,
            port_source,
            total_mbps,
        } => format!(
            "Bringing up {inputs} input(s) to {host}, SRT ports {ports} from {port_source}, about {total_mbps:.0} Mbps in total.\n"
        ),
        Event::LeftoverSourceRemoved { name, port } => format!(
            "  removed Open Live source {name:?}, left over from an earlier run (it held port {port})\n"
        ),
        Event::ConflictingFlowRemoved { name, ours } => {
            let why = if *ours {
                "left over from an earlier run"
            } else {
                "which was holding one of these devices"
            };
            format!("  removed Strom flow {name:?}, {why}\n")
        }
        Event::Summary { inputs } => {
            let mut out = format!("\n{:<36} {:<6} REGISTERED ADDRESS\n", "INPUT", "PORT");
            for row in inputs {
                out += &format!(
                    "{:<36} {:<6} {}\n",
                    run::truncate(&row.name, 36),
                    row.port,
                    row.address
                );
            }
            out += "\nAssign these in Studio and activate the production; they read as inactive until\n";
            out += "something is receiving them. Ctrl-C stops and removes them.\n\n";
            out
        }
        Event::HeartbeatStarted { gateway_id } => {
            format!("Status is pushed to Open Live as gateway {gateway_id}.\n")
        }
        Event::Input { input, state } => format!("  {input} — {}\n", state.describe()),
        Event::Stopping => "\nStopping.\n".to_string(),
        Event::FlowRemoved { input } => format!("  {input} — flow stopped and removed\n"),
        Event::SourceRemoved { input } => {
            format!("  {input} — source removed from Open Live\n")
        }
        Event::SourceKept { input, reason } => format!("  {}\n", kept_message(input, reason)),
        Event::AskingRunningToStop { pid } => {
            format!("Asking the running gateway (pid {pid}) to stop.\n")
        }
        Event::RunningDidNotStop => {
            "It has not finished stopping; removing what is left anyway.\n".to_string()
        }
        Event::StromUnreachable { url, error } => {
            format!("Strom at {url} is not reachable ({error}); leaving its flows alone.\n")
        }
        Event::NothingToRemove => "Nothing of ours left to remove.\n".to_string(),
        // A failure is printed once, as `Error:` when `up` returns it.
        Event::Registering { .. }
        | Event::Registered { .. }
        | Event::ConnectWarning { .. }
        | Event::InputFailed { .. }
        | Event::Warning { .. }
        | Event::OscSignInEnds { .. }
        | Event::Stats { .. }
        | Event::Stopped
        | Event::Failed { .. } => return None,
    })
}

/// The line for a source Open Live would not remove, `reason` being its refusal.
fn kept_message(name: &str, reason: &str) -> String {
    format!(
        "{name} stays in Open Live: {reason}. Deactivate that production, then run `open-live-ingest down`."
    )
}

/// Resolves on Ctrl-C or SIGTERM. SIGHUP is deliberately swallowed: closing an SSH
/// session, or a link dropping mid-show, must not tear down a venue's feeds. The
/// signal streams live for the whole run, so a signal is never lost between polls.
#[cfg(unix)]
async fn stop_signal() -> Stop {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    async fn recv(sig: &mut Option<Signal>) {
        match sig {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    }

    let mut interrupt = signal(SignalKind::interrupt()).ok();
    let mut terminate = signal(SignalKind::terminate()).ok();
    let mut hangup = signal(SignalKind::hangup()).ok();
    if interrupt.is_none() && terminate.is_none() {
        let _ = tokio::signal::ctrl_c().await;
        return Stop::Interrupt;
    }
    loop {
        tokio::select! {
            _ = recv(&mut interrupt) => return Stop::Interrupt,
            _ = recv(&mut terminate) => return Stop::Interrupt,
            _ = recv(&mut hangup) => tracing::info!("ignoring SIGHUP so the feeds survive a closed session"),
        }
    }
}

/// Resolves on Ctrl-C, Ctrl-Break, closing the console window, logoff or shutdown.
/// The last three come with a deadline: Windows ends the process about five seconds
/// later whatever it is doing. The streams live for the whole run, as on Unix.
#[cfg(windows)]
async fn stop_signal() -> Stop {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};

    // Each event has its own stream type; one that could not be registered never fires.
    macro_rules! recv {
        ($stream:expr) => {
            async {
                match $stream.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }
        };
    }

    let mut interrupt = ctrl_c().ok();
    let mut brk = ctrl_break().ok();
    let mut close = ctrl_close().ok();
    let mut logoff = ctrl_logoff().ok();
    let mut shutdown = ctrl_shutdown().ok();
    if interrupt.is_none() && brk.is_none() {
        let _ = tokio::signal::ctrl_c().await;
        return Stop::Interrupt;
    }
    tokio::select! {
        _ = recv!(interrupt) => Stop::Interrupt,
        _ = recv!(brk) => Stop::Interrupt,
        _ = recv!(close) => Stop::Ending,
        _ = recv!(logoff) => Stop::Ending,
        _ = recv!(shutdown) => Stop::Ending,
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
    fn up_takes_repeated_audio_pairings_and_refuses_a_malformed_one() {
        let cli = Cli::try_parse_from([
            "open-live-ingest",
            "up",
            "--audio",
            "FaceTime=USB Microphone",
            "--audio",
            "DeckLink=dev-5e1a",
        ])
        .expect("parses");
        let Some(Command::Up { audio, .. }) = cli.command else {
            panic!("not up");
        };
        assert_eq!(
            audio,
            [
                "FaceTime=USB Microphone".parse().unwrap(),
                "DeckLink=dev-5e1a"
                    .parse::<devices::AudioPairing>()
                    .unwrap(),
            ]
        );
        assert!(Cli::try_parse_from(["open-live-ingest", "up", "--audio", "FaceTime"]).is_err());
    }

    #[test]
    fn status_and_devices_take_json() {
        for sub in ["status", "devices"] {
            Cli::try_parse_from(["open-live-ingest", sub, "--json"]).expect(sub);
        }
    }

    /// The line printed when Open Live keeps a source names the production holding
    /// it and what to do, rather than a bare status.
    #[test]
    fn a_kept_source_names_the_production_and_the_way_out() {
        let refused = open_live_ingest::openlive::Refused {
            what: "DELETE source".into(),
            status: reqwest::StatusCode::CONFLICT,
            reason: Some(r#"Source is in use by active production "Morning Show""#.into()),
        };
        assert_eq!(
            kept_message("Venue — Camera", &refused.explain()),
            r#"Venue — Camera stays in Open Live: Source is in use by active production "Morning Show". Deactivate that production, then run `open-live-ingest down`."#
        );
        let bare = open_live_ingest::openlive::Refused {
            reason: None,
            ..refused
        };
        assert!(kept_message("Venue — Camera", &bare.explain()).contains("HTTP 409 Conflict"));
    }

    /// The lines the golden tests do not reach, worded exactly as the gateway printed
    /// them before its output became events.
    #[test]
    fn events_render_as_the_lines_printed_before() {
        use open_live_ingest::gateway::{InputState, SummaryRow};
        let cases = [
            (
                Event::StromStarting {
                    port: 8080,
                    log_path: PathBuf::from("strom-data").join("strom.log"),
                },
                format!(
                    "Starting Strom on port 8080 (logging to {}).\n",
                    PathBuf::from("strom-data").join("strom.log").display()
                ),
            ),
            (
                Event::StromReady { video_sources: 2 },
                "  Strom is up; it can see 2 video source(s).\n".to_string(),
            ),
            (Event::StromStopped, "  strom — stopped\n".to_string()),
            (
                Event::StromLeftOverStopping { pid: 4242 },
                "  strom (pid 4242) — stopping the one an earlier run started\n".to_string(),
            ),
            (
                Event::LeftoverSourceRemoved {
                    name: "Venue — Camera".to_string(),
                    port: 47110,
                },
                "  removed Open Live source \"Venue — Camera\", left over from an earlier run (it held port 47110)\n".to_string(),
            ),
            (
                Event::ConflictingFlowRemoved {
                    name: "Studio cam".to_string(),
                    ours: false,
                },
                "  removed Strom flow \"Studio cam\", which was holding one of these devices\n"
                    .to_string(),
            ),
            (
                Event::HeartbeatStarted {
                    gateway_id: "gw-1".to_string(),
                },
                "Status is pushed to Open Live as gateway gw-1.\n".to_string(),
            ),
            (
                Event::Input {
                    input: "Venue — Camera".to_string(),
                    state: InputState::OnAir,
                },
                "  Venue — Camera — on air, the far end is receiving\n".to_string(),
            ),
            (
                Event::SourceKept {
                    input: "Venue — Camera".to_string(),
                    reason: "HTTP 409 Conflict".to_string(),
                },
                "  Venue — Camera stays in Open Live: HTTP 409 Conflict. Deactivate that production, then run `open-live-ingest down`.\n".to_string(),
            ),
            (
                Event::AskingRunningToStop { pid: 4242 },
                "Asking the running gateway (pid 4242) to stop.\n".to_string(),
            ),
            (
                Event::RunningDidNotStop,
                "It has not finished stopping; removing what is left anyway.\n".to_string(),
            ),
            (
                Event::StromUnreachable {
                    url: "http://127.0.0.1:8080".to_string(),
                    error: "GET flows".to_string(),
                },
                "Strom at http://127.0.0.1:8080 is not reachable (GET flows); leaving its flows alone.\n"
                    .to_string(),
            ),
        ];
        for (event, text) in cases {
            assert_eq!(render(&event).as_deref(), Some(text.as_str()), "{event:?}");
        }

        // A name longer than the column is cut, as before.
        let summary = render(&Event::Summary {
            inputs: vec![SummaryRow {
                name: "Venue — a camera with a name far too long for the column".to_string(),
                port: 47110,
                address: "srt://:47110?mode=listener".to_string(),
            }],
        })
        .unwrap();
        let row = summary.lines().nth(2).unwrap();
        assert_eq!(
            row,
            "Venue — a camera with a name far to… 47110  srt://:47110?mode=listener"
        );
    }

    /// Warnings are already in the log, so the CLI does not print them twice.
    #[test]
    fn warnings_and_registrations_print_nothing() {
        for event in [
            Event::Warning {
                message: "x".to_string(),
            },
            Event::InputFailed {
                input: "x".to_string(),
                reason: "y".to_string(),
            },
            Event::ConnectWarning {
                input: "x".to_string(),
                warning: "srt-listener-unsupported".to_string(),
            },
            Event::Registering {
                input: "x".to_string(),
            },
            Event::Registered {
                input: "x".to_string(),
                source_id: "s".to_string(),
            },
            Event::Stats { inputs: Vec::new() },
            Event::Stopped,
            Event::Failed {
                reason: "y".to_string(),
            },
        ] {
            assert_eq!(render(&event), None, "{event:?}");
        }
    }
}
