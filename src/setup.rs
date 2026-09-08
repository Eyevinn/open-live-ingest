//! Interactive setup: anything missing is asked for, checked there and then, and
//! written to the settings file. Plain prompts, because this runs over SSH on
//! whatever terminal the venue's laptop has.
//!
//! Checking each answer as it is given is the point. A wrong credential caught at
//! the prompt costs a retype; the same mistake surfacing later looks like a feed that
//! silently goes nowhere.

use crate::config::{AuthMode, Config, UplinkMode};
use crate::openlive::OpenLiveClient;
use crate::strom::{Reachability, StromClient};
use anyhow::{bail, Context, Result};
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
        match ask(label, Some(&default.to_string())).await?.parse() {
            Ok(n) => return Ok(n),
            Err(_) => println!("  please enter a whole number"),
        }
    }
}

fn blank_to_none(answer: String) -> Option<String> {
    Some(answer.trim().to_string()).filter(|s| !s.is_empty())
}

/// Which kind of credential an Open Live address implies. An Open Source Cloud
/// instance sits behind a proxy that rejects a personal access token presented
/// directly, so it needs the exchange; anything else takes a static key. Inferring it
/// spares an operator a question nobody can answer from the words "osc" and "direct".
pub fn infer_auth_mode(url: &str) -> AuthMode {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if host == "osaas.io" || host.ends_with(".osaas.io") {
        AuthMode::Osc
    } else {
        AuthMode::Direct
    }
}

/// Whether anything still has to be asked for.
fn needs_setup(cfg: &Config) -> bool {
    if cfg.gateway.name.trim().is_empty() {
        return true;
    }
    // With registration off there is nothing to register, so no address or credential
    // is needed, and a non-interactive run must not refuse over settings it will not use.
    if !cfg.open_live.register {
        return false;
    }
    cfg.open_live.url.is_none()
        || (cfg.open_live.auth_mode == AuthMode::Osc && cfg.open_live.api_key.is_none())
}

/// Fills in whatever is missing. Returns true when something changed and the file
/// should be written.
pub async fn configure(cfg: &mut Config, force: bool) -> Result<bool> {
    if !force && !needs_setup(cfg) {
        return Ok(false);
    }
    // Refusing beats hanging on a closed stdin: a prompt in a service or a cron job
    // would otherwise block forever with nothing to explain why.
    if !std::io::stdin().is_terminal() {
        bail!("settings are incomplete and there is no terminal to ask on. Run it interactively once, or fill in the settings file.");
    }

    println!("Open Live Gateway setup. Enter accepts the value in brackets.\n");

    let default_name = Some(cfg.gateway.name.trim())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(crate::config::hostname)
        .unwrap_or_else(|| "gateway".to_string());
    cfg.gateway.name = ask("Name for this gateway", Some(&default_name)).await?;

    let url = ask(
        "Open Live URL",
        cfg.open_live.url.as_deref().or(Some("https://")),
    )
    .await?;
    cfg.open_live.auth_mode = infer_auth_mode(&url);
    cfg.open_live.url = Some(url);

    let credential_label = match cfg.open_live.auth_mode {
        AuthMode::Osc => {
            println!(
                "  that is an Open Source Cloud address, so it needs an OSC personal access token"
            );
            "OSC personal access token for Open Live"
        }
        AuthMode::Direct => {
            println!("  self-hosted Open Live: it may need an API key, or none at all");
            "Open Live API key (blank if it needs none)"
        }
    };
    if let Some(secret) = blank_to_none(ask_secret(credential_label).await?) {
        cfg.open_live.api_key = Some(secret);
    }

    // Checked before asking anything else, so a bad address or credential is
    // corrected while the operator is still looking at it.
    match check_open_live(cfg).await {
        Ok(Some(host)) => {
            println!("  reached Open Live; its Strom is {host}");
            if cfg.uplink.host.is_none() {
                println!("  feeds will be sent there");
            }
        }
        Ok(None) => println!("  reached Open Live, but it did not report a Strom host; set uplink.host in the settings file"),
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
            cfg.strom.api_key = blank_to_none(ask_secret("Local Strom API key").await?);
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
        "Uplink direction: this machine dials the cloud (caller), or the cloud dials this machine (listener)",
        &["caller", "listener"],
        cfg.uplink.mode.as_str(),
    )
    .await?;
    cfg.uplink.mode = if mode == "listener" {
        UplinkMode::Listener
    } else {
        UplinkMode::Caller
    };
    if cfg.uplink.mode == UplinkMode::Listener {
        println!("  that direction has the cloud dial this machine, so it needs an address");
        cfg.uplink.public_host = blank_to_none(
            ask(
                "This machine's address as the cloud sees it",
                cfg.uplink.public_host.as_deref(),
            )
            .await?,
        );
    }
    cfg.uplink.port_range = ask("SRT port range", Some(&cfg.uplink.port_range)).await?;
    cfg.uplink.latency_ms = ask_number("SRT latency in ms", cfg.uplink.latency_ms).await?;

    println!("  capture format: blank takes whatever each device offers, which suits most devices");
    cfg.capture.video_resolution = blank_to_none(
        ask(
            "Capture resolution, e.g. 1280x720",
            cfg.capture.video_resolution.as_deref(),
        )
        .await?,
    );
    cfg.capture.video_framerate = blank_to_none(
        ask(
            "Capture framerate, e.g. 25/1",
            cfg.capture.video_framerate.as_deref(),
        )
        .await?,
    );
    cfg.video.bitrate_kbps = ask_number("Video bitrate in kbps", cfg.video.bitrate_kbps).await?;

    Ok(true)
}

/// Confirms the address and credential work, returning the cloud Strom's host.
async fn check_open_live(cfg: &Config) -> Result<Option<String>> {
    let client = OpenLiveClient::new(
        cfg.open_live.url.as_deref().unwrap_or_default(),
        cfg.open_live.auth_mode,
        cfg.open_live.api_key.as_deref(),
    )?;
    // Listing sources exercises the credential; server-info alone might not.
    client.list_sources().await?;
    client.cloud_strom_host().await
}

async fn probe_strom(cfg: &Config) -> Reachability {
    match StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref()) {
        Ok(client) => client.probe().await,
        Err(err) => Reachability::Unreachable(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_osaas_address_implies_the_token_exchange_and_lookalikes_do_not() {
        assert_eq!(
            infer_auth_mode("https://venue.eyevinn-open-live.auto.prod-se.osaas.io"),
            AuthMode::Osc
        );
        assert_eq!(infer_auth_mode("https://osaas.io/"), AuthMode::Osc);
        assert_eq!(infer_auth_mode("http://127.0.0.1:3000"), AuthMode::Direct);
        assert_eq!(
            infer_auth_mode("https://open-live.example.com"),
            AuthMode::Direct
        );
        assert_eq!(
            infer_auth_mode("https://osaas.io.example.com"),
            AuthMode::Direct
        );
        assert_eq!(
            infer_auth_mode("https://myosaas.iohost.net"),
            AuthMode::Direct
        );
    }

    fn named() -> Config {
        let mut cfg = Config::default();
        cfg.gateway.name = "Venue".to_string();
        cfg
    }

    #[test]
    fn setup_is_needed_for_a_missing_name_url_or_osc_token() {
        let mut cfg = named();
        cfg.gateway.name = "   ".to_string();
        assert!(needs_setup(&cfg));

        let cfg = named();
        assert!(needs_setup(&cfg), "no Open Live URL");

        let mut cfg = named();
        cfg.open_live.url = Some("https://x.osaas.io".to_string());
        cfg.open_live.auth_mode = AuthMode::Osc;
        assert!(
            needs_setup(&cfg),
            "an OSC deployment cannot work without a token"
        );
    }

    /// A self-hosted Open Live can leave API_KEY unset, and registration off needs
    /// nothing from Open Live at all, so neither may re-prompt on every run.
    #[test]
    fn setup_is_not_needed_for_a_complete_config() {
        let mut cfg = named();
        cfg.open_live.url = Some("http://127.0.0.1:3000".to_string());
        assert!(!needs_setup(&cfg));

        let mut cfg = named();
        cfg.open_live.register = false;
        assert!(!needs_setup(&cfg));
    }
}
