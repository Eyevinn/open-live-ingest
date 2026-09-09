//! Interactive setup: anything missing is asked for, checked there and then, and
//! written to the settings file. Line-oriented prompts, because this runs over SSH on
//! whatever terminal the venue's laptop has: lists are picked with the arrow keys,
//! colour steps back to plain text on a dumb terminal or under NO_COLOR, and nothing
//! takes over the screen.
//!
//! Checking each answer as it is given is the point. A wrong credential caught at
//! the prompt costs a retype; the same mistake surfacing later looks like a feed that
//! silently goes nowhere.

use crate::config::{AuthMode, Config, UplinkMode};
use crate::openlive::OpenLiveClient;
use crate::osc;
use crate::strom::{Reachability, StromClient};
use anyhow::{bail, Context, Result};
use console::{style, Emoji, Style};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};
use std::io::IsTerminal;
use std::path::Path;

const OK: Emoji<'_, '_> = Emoji("✔", "ok");
const WARN: Emoji<'_, '_> = Emoji("!", "!");
const FAIL: Emoji<'_, '_> = Emoji("✘", "x");

/// dialoguer's colourful theme, with its grey parts lifted: the default in brackets
/// and the hints are what an operator has to read, and bright black disappears on
/// many terminal backgrounds.
fn theme() -> ColorfulTheme {
    ColorfulTheme {
        hint_style: Style::new().for_stderr().cyan(),
        prompt_suffix: style("›".to_string()).for_stderr(),
        success_suffix: style("·".to_string()).for_stderr(),
        ..ColorfulTheme::default()
    }
}

/// A section of related questions.
fn section(title: &str) {
    println!("\n{}", style(title).bold().cyan());
}

/// Something checked out.
fn good(msg: impl std::fmt::Display) {
    println!("  {} {msg}", style(OK).green());
}

/// Worth knowing, not wrong. Plain text: dimmed text is unreadable on some terminals.
fn note(msg: impl std::fmt::Display) {
    println!("  {msg}");
}

/// Setup can continue, but the operator should look at this.
fn warn(msg: impl std::fmt::Display) {
    println!("  {} {}", style(WARN).yellow().bold(), style(msg).yellow());
}

/// A check failed.
fn fail(msg: impl std::fmt::Display) {
    println!("  {} {}", style(FAIL).red().bold(), style(msg).red());
}

/// Reads a line, showing a default that Enter accepts. An optional field with no
/// default may be left blank.
async fn ask(label: &str, default: Option<&str>) -> Result<String> {
    let label = label.to_string();
    let default = default.map(str::to_string).filter(|d| !d.is_empty());
    tokio::task::spawn_blocking(move || {
        let theme = theme();
        let mut input = Input::<String>::with_theme(&theme)
            .with_prompt(label)
            .allow_empty(true);
        if let Some(default) = default {
            input = input.default(default);
        }
        let answer: String = input.interact_text().context("reading from the terminal")?;
        Ok(answer.trim().to_string())
    })
    .await
    .context("prompt task")?
}

/// Reads a secret without echoing it. Blank is allowed, and means "none" or "keep".
async fn ask_secret(label: &str) -> Result<String> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || {
        Password::with_theme(&theme())
            .with_prompt(label)
            .allow_empty_password(true)
            .interact()
            .context("reading the credential")
    })
    .await
    .context("prompt task")?
}

/// One entry in a list: a short title, and a dimmed hint shown beside it while the
/// list is open. Only the title is echoed once picked.
struct Choice {
    title: String,
    hint: String,
}

fn choice(title: impl Into<String>, hint: impl Into<String>) -> Choice {
    Choice {
        title: title.into(),
        hint: hint.into(),
    }
}

/// Picks one entry with the arrow keys. Returns its index.
async fn ask_choice(label: &str, options: Vec<Choice>, default: usize) -> Result<usize> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || {
        let items: Vec<String> = options
            .iter()
            .map(|c| format!("{}  {}", style(&c.title).bold(), c.hint))
            .collect();
        let picked = Select::with_theme(&theme())
            .with_prompt(&label)
            .items(&items)
            .default(default)
            .report(false)
            .interact()
            .context("reading from the terminal")?;
        println!(
            "{} {} · {}",
            style(OK).green(),
            style(&label).bold(),
            options[picked].title
        );
        Ok(picked)
    })
    .await
    .context("prompt task")?
}

async fn ask_number(label: &str, default: u32) -> Result<u32> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || {
        Input::<u32>::with_theme(&theme())
            .with_prompt(label)
            .default(default)
            .interact_text()
            .context("reading from the terminal")
    })
    .await
    .context("prompt task")?
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

/// Whether anything still has to be asked for. `osc_login` says whether the OSC CLI
/// has a saved login, which serves in place of a token in the settings.
fn needs_setup(cfg: &Config, osc_login: bool) -> bool {
    if cfg.gateway.name.trim().is_empty() {
        return true;
    }
    // With registration off there is nothing to register, so no address or credential
    // is needed, and a non-interactive run must not refuse over settings it will not use.
    if !cfg.open_live.register {
        return false;
    }
    cfg.open_live.url.is_none()
        || (cfg.open_live.auth_mode == AuthMode::Osc
            && cfg.open_live.api_key.is_none()
            && !osc_login)
}

/// Fills in whatever is missing. Returns true when something changed and the file
/// should be written.
pub async fn configure(cfg: &mut Config, force: bool) -> Result<bool> {
    if !force && !needs_setup(cfg, osc::saved_token().is_some()) {
        return Ok(false);
    }
    // Refusing beats hanging on a closed stdin: a prompt in a service or a cron job
    // would otherwise block forever with nothing to explain why.
    if !std::io::stdin().is_terminal() {
        bail!("settings are incomplete and there is no terminal to ask on. Run it interactively once, or fill in the settings file.");
    }

    println!("{}", style("Open Live Gateway setup").bold());
    note("Enter accepts the highlighted value. Lists are picked with the arrow keys.");

    section("Gateway");
    let default_name = Some(cfg.gateway.name.trim())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(crate::config::hostname)
        .unwrap_or_else(|| "gateway".to_string());
    cfg.gateway.name = ask("Name for this gateway", Some(&default_name)).await?;

    section("Open Live");
    // Hosting first, because on Open Source Cloud the address is picked from a list
    // rather than typed, and the credential is needed to fetch that list.
    let hosting_default = match cfg.open_live.url.as_deref() {
        Some(url) if infer_auth_mode(url) == AuthMode::Direct => 1,
        _ => 0,
    };
    let hosting = ask_choice(
        "Where does Open Live run?",
        vec![
            choice("Open Source Cloud", "an osaas.io instance"),
            choice("Self-hosted", "your own Open Live, reached by URL"),
        ],
        hosting_default,
    )
    .await?;
    if hosting == 0 {
        let token = osc_authenticate(cfg).await?;
        cfg.open_live.auth_mode = AuthMode::Osc;
        cfg.open_live.url = Some(choose_instance(cfg.open_live.url.as_deref(), &token).await?);
    } else {
        let url = ask(
            "Open Live URL",
            cfg.open_live.url.as_deref().or(Some("https://")),
        )
        .await?;
        cfg.open_live.auth_mode = infer_auth_mode(&url);
        cfg.open_live.url = Some(url);
        match cfg.open_live.auth_mode {
            AuthMode::Osc => {
                note(
                    "that is an Open Source Cloud address, so it needs an Open Source Cloud login",
                );
                osc_authenticate(cfg).await?;
            }
            AuthMode::Direct => {
                note("a self-hosted Open Live may need an API key, or none at all");
                if let Some(secret) =
                    blank_to_none(ask_secret("Open Live API key (blank if it needs none)").await?)
                {
                    cfg.open_live.api_key = Some(secret);
                }
            }
        }
    }
    // Checked before asking anything else, so a bad address or credential is
    // corrected while the operator is still looking at it.
    match check_open_live(cfg).await {
        Ok(Some(host)) => {
            if cfg.uplink.host.is_none() {
                good(format!("reached Open Live; feeds will go to its Strom at {host}"));
            } else {
                good(format!("reached Open Live; its Strom is {host}"));
            }
        }
        Ok(None) => warn(
            "reached Open Live, but it did not report a Strom host; set uplink.host in the settings file",
        ),
        Err(err) => fail(format!("could not reach Open Live: {err:#}")),
    }

    section("Local Strom");
    note("the Strom on this machine, which does the capturing and encoding");
    cfg.strom.url = ask("Strom URL", Some(&cfg.strom.url)).await?;
    match probe_strom(cfg).await {
        Reachability::Ok { video_sources } => good(format!(
            "reached Strom; it can see {video_sources} video source(s)"
        )),
        Reachability::NeedsCredential => {
            warn("Strom is there but wants a credential (it runs with STROM_API_KEY set)");
            cfg.strom.api_key = blank_to_none(ask_secret("Strom API key").await?);
            match probe_strom(cfg).await {
                Reachability::Ok { video_sources } => good(format!(
                    "reached Strom; it can see {video_sources} video source(s)"
                )),
                Reachability::NeedsCredential => fail("Strom still refuses that credential"),
                Reachability::Unreachable(err) => fail(format!("could not reach Strom: {err}")),
            }
        }
        Reachability::Unreachable(err) => fail(format!("could not reach Strom: {err}")),
    }

    section("Uplink");
    let mode = ask_choice(
        "Which end dials?",
        vec![
            choice(
                "Caller",
                "this machine dials the cloud; nothing inbound here",
            ),
            choice(
                "Listener",
                "the cloud dials this machine; needs a public address and inbound UDP",
            ),
        ],
        match cfg.uplink.mode {
            UplinkMode::Caller => 0,
            UplinkMode::Listener => 1,
        },
    )
    .await?;
    cfg.uplink.mode = if mode == 1 {
        UplinkMode::Listener
    } else {
        UplinkMode::Caller
    };
    if cfg.uplink.mode == UplinkMode::Listener {
        cfg.uplink.public_host = blank_to_none(
            ask(
                "This machine's address as the cloud sees it",
                cfg.uplink.public_host.as_deref(),
            )
            .await?,
        );
    }
    cfg.uplink.port_range = ask("SRT port range", Some(&cfg.uplink.port_range)).await?;
    cfg.uplink.latency_ms = ask_number("SRT latency (ms)", cfg.uplink.latency_ms).await?;

    section("Capture");
    note("blank takes whatever each device offers, which suits most devices");
    cfg.capture.video_resolution = blank_to_none(
        ask(
            "Resolution, e.g. 1280x720",
            cfg.capture.video_resolution.as_deref(),
        )
        .await?,
    );
    cfg.capture.video_framerate = blank_to_none(
        ask(
            "Framerate, e.g. 25/1",
            cfg.capture.video_framerate.as_deref(),
        )
        .await?,
    );
    cfg.video.bitrate_kbps = ask_number("Video bitrate (kbps)", cfg.video.bitrate_kbps).await?;

    Ok(true)
}

/// Says where the settings went, in the same voice as the prompts.
pub fn report_saved(path: &Path) {
    println!();
    good(format!("saved to {}", path.display()));
    println!();
}

/// Three ways in, and the operator says which: the OSC CLI's environment variable,
/// the CLI's browser login, or a pasted personal access token. The first two keep the
/// secret out of the settings file, so a stored token is dropped when one of them is
/// chosen; it would otherwise take precedence and silently be the credential in use.
/// Returns the token, which setup needs right away to list the instances.
async fn osc_authenticate(cfg: &mut Config) -> Result<String> {
    let env_state = if osc::env_token().is_some() {
        "set"
    } else {
        "not set"
    };
    let options = || {
        vec![
            choice(osc::ENV_TOKEN, format!("from the environment, {env_state}")),
            choice(
                "OSC CLI login",
                format!(
                    "`{}`; opens a browser here, where you pick the workspace",
                    osc::LOGIN_COMMAND
                ),
            ),
            choice(
                "Personal access token",
                "from the OSC web console; stored in the settings",
            ),
        ]
    };
    let default = if osc::env_token().is_some() {
        0
    } else if cfg.open_live.api_key.is_some() {
        2
    } else {
        1
    };
    loop {
        let picked = ask_choice("Authenticate with", options(), default).await?;
        let token = match picked {
            0 => match osc::env_token() {
                Some(token) => {
                    cfg.open_live.api_key = None;
                    token
                }
                None => {
                    warn(format!("{} is not set in this environment", osc::ENV_TOKEN));
                    continue;
                }
            },
            1 => {
                let current =
                    osc::login_token().filter(|t| !osc::inspect(t).is_some_and(|i| i.is_expired()));
                if current.is_none() {
                    if let Err(err) = osc::login().await {
                        fail(format!("OSC CLI login failed: {err:#}"));
                        continue;
                    }
                }
                match osc::login_token() {
                    Some(token) => {
                        cfg.open_live.api_key = None;
                        if osc::env_token().is_some() {
                            warn(format!(
                                "{} is also set, and takes precedence over the login when the gateway runs",
                                osc::ENV_TOKEN
                            ));
                        }
                        token
                    }
                    None => {
                        fail("the OSC CLI saved no login");
                        continue;
                    }
                }
            }
            _ => {
                let label = if cfg.open_live.api_key.is_some() {
                    "Personal access token (blank keeps the stored one)"
                } else {
                    "Personal access token"
                };
                match blank_to_none(ask_secret(label).await?)
                    .or_else(|| cfg.open_live.api_key.clone())
                {
                    Some(token) => {
                        cfg.open_live.api_key = Some(token.clone());
                        token
                    }
                    None => continue,
                }
            }
        };
        describe_token(&token, picked == 1);
        return Ok(token);
    }
}

/// Which workspace the token landed in, and how long a login has left. The workspace
/// is what the operator chose in the browser, so it is worth reading back.
fn describe_token(token: &str, is_login: bool) {
    let Some(info) = osc::inspect(token) else {
        return;
    };
    match (&info.workspace, info.remaining()) {
        (Some(workspace), Some(left)) => good(format!(
            "workspace {}, token valid for another {} minute(s)",
            style(workspace).bold(),
            left.as_secs() / 60
        )),
        (Some(workspace), None) => good(format!("workspace {}", style(workspace).bold())),
        (None, Some(left)) => good(format!(
            "token valid for another {} minute(s)",
            left.as_secs() / 60
        )),
        (None, None) => {}
    }
    if is_login && info.remaining().is_some() {
        warn(format!(
            "a login lasts an hour; for a show, set {} or store a personal access token instead",
            osc::ENV_TOKEN
        ));
    }
}

/// Picks the Open Live instance from the workspace's list, the way `osc list` shows
/// it. A typed address stays available: the list may be empty, the platform may be
/// unreachable during setup, or the instance may live in another workspace.
async fn choose_instance(current: Option<&str>, token: &str) -> Result<String> {
    match osc::open_live_instances(token).await {
        Ok(instances) if !instances.is_empty() => {
            let mut options: Vec<Choice> =
                instances.iter().map(|i| choice(&i.name, &i.url)).collect();
            options.push(choice("Another address", "typed in"));
            let default = instances
                .iter()
                .position(|i| Some(i.url.as_str()) == current)
                .unwrap_or(0);
            let picked = ask_choice("Open Live instance", options, default).await?;
            if let Some(instance) = instances.get(picked) {
                return Ok(instance.url.clone());
            }
        }
        Ok(_) => warn("this workspace has no Open Live instances"),
        Err(err) => fail(format!("could not list Open Live instances: {err:#}")),
    }
    ask("Open Live URL", current.or(Some("https://"))).await
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
        assert!(needs_setup(&cfg, false));

        let cfg = named();
        assert!(needs_setup(&cfg, false), "no Open Live URL");

        let mut cfg = named();
        cfg.open_live.url = Some("https://x.osaas.io".to_string());
        cfg.open_live.auth_mode = AuthMode::Osc;
        assert!(
            needs_setup(&cfg, false),
            "an OSC deployment cannot work without a token"
        );
        assert!(
            !needs_setup(&cfg, true),
            "the OSC CLI's login stands in for a token"
        );
    }

    /// A self-hosted Open Live can leave API_KEY unset, and registration off needs
    /// nothing from Open Live at all, so neither may re-prompt on every run.
    #[test]
    fn setup_is_not_needed_for_a_complete_config() {
        let mut cfg = named();
        cfg.open_live.url = Some("http://127.0.0.1:3000".to_string());
        assert!(!needs_setup(&cfg, false));

        let mut cfg = named();
        cfg.open_live.register = false;
        assert!(!needs_setup(&cfg, false));
    }
}
