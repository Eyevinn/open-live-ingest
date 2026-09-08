//! Interactive setup.
//!
//! The gateway is configured by running it: anything missing is asked for, checked
//! there and then, and written to the config file. Nothing about it assumes a window
//! or a full-screen terminal, because it has to work over SSH on whatever terminal
//! the venue's laptop happens to have.
//!
//! Checking each answer as it is given is the point. A wrong credential that surfaces
//! at the prompt costs a retype; the same mistake surfacing later looks like a feed
//! that silently goes nowhere, which is a much more expensive afternoon.

use crate::openlive::client::OpenLiveClient;
use crate::openlive::registration;
use crate::strom::client::{Reachability, StromClient};
use anyhow::{bail, Context, Result};
use open_live_gateway_types::config::UplinkMode;
use open_live_gateway_types::GatewayConfig;
use std::io::{IsTerminal, Write};

/// Reads a line, showing a default that Enter accepts.
async fn ask(label: &str, default: Option<&str>) -> Result<String> {
    let prompt = match default {
        Some(d) if !d.is_empty() => format!("{label} ({d}): "),
        _ => format!("{label}: "),
    };
    let default = default.unwrap_or_default().to_string();

    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading from the terminal")?;
        let trimmed = line.trim().to_string();
        Ok(if trimmed.is_empty() { default } else { trimmed })
    })
    .await
    .context("prompt task")?
}

/// Reads a secret without echoing it.
async fn ask_secret(label: &str) -> Result<String> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || {
        rpassword::prompt_password(format!("{label}: ")).context("reading the credential")
    })
    .await
    .context("prompt task")?
}

/// Reads one of a fixed set of answers, re-asking until it is one of them.
async fn ask_choice(label: &str, options: &[&str], default: &str) -> Result<String> {
    let rendered = format!("{label} [{}]", options.join("/"));
    loop {
        let answer = ask(&rendered, Some(default)).await?;
        if options.contains(&answer.as_str()) {
            return Ok(answer);
        }
        println!("  please answer one of: {}", options.join(", "));
    }
}

async fn ask_number(label: &str, default: u32) -> Result<u32> {
    loop {
        let answer = ask(label, Some(&default.to_string())).await?;
        match answer.parse() {
            Ok(n) => return Ok(n),
            Err(_) => println!("  please enter a whole number"),
        }
    }
}

fn blank_to_none(answer: String) -> Option<String> {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Which kind of Open Live credential an address implies.
///
/// An Open Source Cloud instance sits behind a proxy that rejects a personal access
/// token presented directly, so it needs the token exchange; anything else takes a
/// static key. Inferring it spares an operator a question they cannot be expected to
/// answer from the words "osc" and "direct".
pub fn infer_auth_mode(url: &str) -> &'static str {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase();

    if host.ends_with(".osaas.io") || host.ends_with("osaas.io") {
        "osc"
    } else {
        "direct"
    }
}

/// Whether anything still has to be asked for.
fn needs_setup(cfg: &GatewayConfig) -> bool {
    if cfg.gateway.name.trim().is_empty() {
        return true;
    }
    // With registration off there is nothing to register, so no address or credential
    // is needed. Demanding them made a non-interactive run refuse over settings it
    // was never going to use.
    if !cfg.open_live.register {
        return false;
    }
    cfg.open_live.url.is_none()
        || (cfg.open_live.auth_mode == "osc" && cfg.open_live.api_key.is_none())
}

/// Fills in whatever the config is missing, checking answers as they are given.
///
/// Returns true when something changed and the file should be written.
pub async fn configure(cfg: &mut GatewayConfig, force: bool) -> Result<bool> {
    if !force && !needs_setup(cfg) {
        return Ok(false);
    }

    // Refusing beats hanging on a closed stdin: a prompt in a systemd unit or a cron
    // job would otherwise block forever with nothing to explain why.
    if !std::io::stdin().is_terminal() {
        bail!(
            "settings are incomplete and there is no terminal to ask on — \
             run it interactively once, or fill in the config file"
        );
    }

    println!("Open Live Gateway setup. Enter accepts the value in brackets.\n");

    let default_name = if cfg.gateway.name.trim().is_empty() {
        hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "gateway".to_string())
    } else {
        cfg.gateway.name.clone()
    };
    cfg.gateway.name = ask("Name for this gateway", Some(&default_name)).await?;

    cfg.open_live.url = Some(
        ask(
            "Open Live URL",
            cfg.open_live.url.as_deref().or(Some("https://")),
        )
        .await?,
    );
    let url = cfg.open_live.url.clone().unwrap_or_default();
    cfg.open_live.auth_mode = infer_auth_mode(&url).to_string();

    let credential_label = if cfg.open_live.auth_mode == "osc" {
        println!("  that is an Open Source Cloud address, so it needs an OSC token");
        "OSC personal access token for Open Live"
    } else {
        println!("  self-hosted Open Live: it may need an API key, or none at all");
        "Open Live API key (blank if it needs none)"
    };
    let secret = ask_secret(credential_label).await?;
    if !secret.trim().is_empty() {
        cfg.open_live.api_key = Some(secret.trim().to_string());
    }

    // Check it before asking anything else, so a bad URL or credential is corrected
    // while the operator is still looking at it.
    match check_open_live(cfg).await {
        Ok(Some(host)) => {
            println!("  reached Open Live; its Strom is {host}");
            if cfg.app.uplink.host.is_none() {
                println!("  feeds will be sent there");
            }
        }
        Ok(None) => println!("  reached Open Live, but it did not report a Strom host"),
        Err(err) => println!("  could not reach Open Live: {err:#}"),
    }

    cfg.strom.url = ask(
        "Local Strom URL (the Strom on this machine, which does the capturing)",
        Some(&cfg.strom.url),
    )
    .await?;

    match probe_strom(cfg).await {
        Reachability::Ok { video_sources } => {
            println!("  reached Strom; it can see {video_sources} video source(s)")
        }
        Reachability::NeedsCredential => {
            println!("  Strom is there but wants a credential (it runs with STROM_API_KEY set)");
            let secret = ask_secret("Local Strom API key").await?;
            cfg.strom.api_key = Some(secret.trim().to_string()).filter(|s| !s.is_empty());
            match probe_strom(cfg).await {
                Reachability::Ok { video_sources } => {
                    println!("  reached Strom; it can see {video_sources} video source(s)")
                }
                Reachability::NeedsCredential => println!("  Strom still refuses that credential"),
                Reachability::Unreachable(err) => println!("  could not reach Strom: {err}"),
            }
        }
        Reachability::Unreachable(err) => println!("  could not reach Strom: {err}"),
    }

    let mode = ask_choice(
        "Uplink direction: we dial out (caller), the cloud dials us (listener), both (rendezvous)",
        &["caller", "listener", "rendezvous"],
        match cfg.app.uplink.mode {
            UplinkMode::Caller => "caller",
            UplinkMode::Listener => "listener",
            UplinkMode::Rendezvous => "rendezvous",
        },
    )
    .await?;
    cfg.app.uplink.mode = match mode.as_str() {
        "listener" => UplinkMode::Listener,
        "rendezvous" => UplinkMode::Rendezvous,
        _ => UplinkMode::Caller,
    };

    if matches!(
        cfg.app.uplink.mode,
        UplinkMode::Listener | UplinkMode::Rendezvous
    ) {
        println!("  that direction has the cloud dial this machine, so it needs an address");
        let host = ask(
            "This machine's address as the cloud sees it",
            cfg.app.uplink.public_host.as_deref(),
        )
        .await?;
        cfg.app.uplink.public_host = Some(host).filter(|h| !h.trim().is_empty());
    }

    cfg.app.uplink.port_range = ask("SRT port range", Some(&cfg.app.uplink.port_range)).await?;
    cfg.app.uplink.latency_ms = ask_number("SRT latency in ms", cfg.app.uplink.latency_ms).await?;

    // Blank is the right answer for almost everyone: many capture devices advertise
    // exactly one format, and asking for a different one fails negotiation rather
    // than being converted.
    println!("  capture format: blank takes whatever each device offers");
    cfg.app.video_resolution = blank_to_none(
        ask(
            "Capture resolution, e.g. 1280x720",
            cfg.app.video_resolution.as_deref(),
        )
        .await?,
    );
    cfg.app.video_framerate = blank_to_none(
        ask(
            "Capture framerate, e.g. 25/1",
            cfg.app.video_framerate.as_deref(),
        )
        .await?,
    );
    cfg.app.video.bitrate_kbps =
        ask_number("Video bitrate in kbps", cfg.app.video.bitrate_kbps).await?;

    Ok(true)
}

/// Confirms the Open Live address and credential work, returning its Strom's host.
async fn check_open_live(cfg: &GatewayConfig) -> Result<Option<String>> {
    let client: OpenLiveClient = registration::client_from(cfg)?;
    // Listing sources exercises the credential; server-info alone might not.
    client.list_sources().await?;
    client.cloud_strom_host().await
}

/// Checks the local Strom, distinguishing "wants a credential" from "not there".
async fn probe_strom(cfg: &GatewayConfig) -> Reachability {
    match StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref()) {
        Ok(client) => client.probe().await,
        Err(err) => Reachability::Unreachable(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_osaas_address_implies_the_token_exchange() {
        assert_eq!(
            infer_auth_mode("https://ludde-prodda.eyevinn-open-live.auto.prod-se.osaas.io"),
            "osc"
        );
        assert_eq!(infer_auth_mode("https://open-live.osaas.io/"), "osc");
    }

    #[test]
    fn anything_else_takes_a_static_key() {
        assert_eq!(infer_auth_mode("http://127.0.0.1:3000"), "direct");
        assert_eq!(infer_auth_mode("https://open-live.example.com"), "direct");
        assert_eq!(infer_auth_mode("http://localhost:3000/api"), "direct");
    }

    /// A hostname that merely mentions osaas must not trigger the exchange — only the
    /// real domain does, or a self-hosted instance would be asked for the wrong thing.
    #[test]
    fn a_lookalike_hostname_is_not_treated_as_osc() {
        assert_eq!(infer_auth_mode("https://osaas.io.example.com"), "direct");
        assert_eq!(infer_auth_mode("https://myosaas.iohost.net"), "direct");
    }

    #[test]
    fn a_config_missing_the_open_live_url_needs_setup() {
        let mut cfg = GatewayConfig::default();
        cfg.gateway.name = "Venue".to_string();
        assert!(needs_setup(&cfg));
    }

    #[test]
    fn an_osc_config_without_a_token_needs_setup() {
        let mut cfg = GatewayConfig::default();
        cfg.gateway.name = "Venue".to_string();
        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        cfg.open_live.auth_mode = "osc".to_string();
        assert!(
            needs_setup(&cfg),
            "an OSC deployment cannot work without a PAT"
        );
    }

    /// A self-hosted Open Live can leave API_KEY unset, so `direct` mode with no
    /// credential is complete and must not re-prompt on every run.
    #[test]
    fn a_direct_config_without_a_key_is_complete() {
        let mut cfg = GatewayConfig::default();
        cfg.gateway.name = "Venue".to_string();
        cfg.open_live.url = Some("http://127.0.0.1:3000".to_string());
        cfg.open_live.auth_mode = "direct".to_string();
        assert!(!needs_setup(&cfg));
    }

    /// Registration off means no Open Live settings are needed at all — a gateway
    /// streaming to a Strom without registering is a legitimate setup.
    #[test]
    fn registration_off_needs_nothing_from_open_live() {
        let mut cfg = GatewayConfig::default();
        cfg.gateway.name = "Nuc1".to_string();
        cfg.open_live.register = false;
        cfg.open_live.url = None;
        cfg.open_live.api_key = None;
        assert!(!needs_setup(&cfg));
    }

    #[test]
    fn an_unnamed_gateway_needs_setup() {
        let mut cfg = GatewayConfig::default();
        cfg.open_live.url = Some("http://127.0.0.1:3000".to_string());
        cfg.open_live.auth_mode = "direct".to_string();
        cfg.gateway.name = "   ".to_string();
        assert!(needs_setup(&cfg));
    }
}
