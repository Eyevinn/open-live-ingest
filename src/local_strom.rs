//! The Strom on this machine: adopted if one is listening, started headless if not.
//!
//! The rule is **adopt, never replace**. A Strom already answering at the configured
//! URL is used as it is and never stopped, because it may be a service this box
//! depends on or another operator's session. Only a Strom this process started is a
//! Strom this process may stop, and it is stopped on the way out so a camera does
//! not stay open with nothing supervising it.

use crate::config::{self, Config};
use crate::strom::{Reachability, StromClient};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A freshly started Strom loads GStreamer and probes for hardware first.
const READY_TIMEOUT: Duration = Duration::from_secs(45);
const READY_POLL: Duration = Duration::from_millis(400);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub enum LocalStrom {
    /// Already running. Left alone entirely.
    Adopted,
    /// Started here, and stopped when this is dropped.
    Managed { child: Child, log_path: PathBuf },
}

impl Drop for LocalStrom {
    fn drop(&mut self) {
        let LocalStrom::Managed { child, .. } = self else {
            return;
        };
        let pid = child.id();
        info!(pid, "stopping the Strom we started");
        signal_term(pid);
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200))
                }
                Ok(None) => {
                    warn!(pid, "Strom did not stop in time, killing it");
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Err(err) => {
                    warn!(pid, %err, "could not wait for Strom");
                    break;
                }
            }
        }
        std::fs::remove_file(config::strom_pidfile()).ok();
        println!("  strom — stopped");
    }
}

/// Returns a usable local Strom, starting one if nothing is listening.
pub async fn ensure_running(cfg: &Config) -> Result<LocalStrom> {
    let client = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
    match client.probe().await {
        Reachability::Ok { .. } => {
            info!(url = %cfg.strom.url, "using the Strom already running");
            return Ok(LocalStrom::Adopted);
        }
        // A running Strom that wants a credential is still a running Strom. Starting
        // a second one would fight it for the port.
        Reachability::NeedsCredential => bail!(
            "Strom at {} wants a credential.\n\nIt runs with STROM_API_KEY set. Run `open-live-gateway setup` and enter that key when asked.",
            cfg.strom.url
        ),
        Reachability::Unreachable(_) => {}
    }

    if !cfg.strom.manage {
        bail!(
            "nothing is listening at {}, and [strom] manage is off.\n\n\
             Start Strom yourself, for example:\n    strom --headless --port 8080\n\n\
             or set manage = true to have the gateway run one.",
            cfg.strom.url
        );
    }

    let binary = locate_binary(&cfg.strom.binary)?;
    let port = port_of(&cfg.strom.url).with_context(|| {
        format!(
            "strom.url {:?} names no port, so there is nothing to start a Strom on",
            cfg.strom.url
        )
    })?;
    let data_dir = data_dir(cfg);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating Strom's data directory at {}", data_dir.display()))?;
    let log_path = data_dir.join("strom.log");
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let log_err = log.try_clone().context("duplicating the log file handle")?;

    println!(
        "Starting Strom on port {port} (logging to {}).",
        log_path.display()
    );
    let child = Command::new(&binary)
        .args(["--headless", "--port", &port.to_string(), "--data-dir"])
        .arg(&data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;

    // Recorded so a `down` after a hard kill can still stop it. Written before the
    // readiness wait, so a failure there cannot leak a running Strom either.
    let pidfile = config::strom_pidfile();
    if let Some(parent) = pidfile.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&pidfile, child.id().to_string()).ok();

    let managed = LocalStrom::Managed { child, log_path };
    wait_until_ready(&client, &managed).await?;
    Ok(managed)
}

async fn wait_until_ready(client: &StromClient, strom: &LocalStrom) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match client.probe().await {
            Reachability::Ok { video_sources } => {
                println!("  Strom is up; it can see {video_sources} video source(s).");
                return Ok(());
            }
            // We set no key on the Strom we started, so this is someone else's on the port.
            Reachability::NeedsCredential => {
                bail!("the Strom on that port wants a credential, so it is not the one we started")
            }
            Reachability::Unreachable(_) if Instant::now() < deadline => {
                tokio::time::sleep(READY_POLL).await
            }
            Reachability::Unreachable(err) => {
                let tail = match strom {
                    LocalStrom::Managed { log_path, .. } => tail_of_log(log_path),
                    LocalStrom::Adopted => String::new(),
                };
                bail!(
                    "Strom did not become ready within {}s ({err}).{tail}",
                    READY_TIMEOUT.as_secs()
                );
            }
        }
    }
}

/// The last few lines of Strom's log, to put the reason next to the failure.
fn tail_of_log(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let tail: Vec<&str> = text.lines().rev().take(8).collect();
    if tail.is_empty() {
        return String::new();
    }
    let mut out = format!("\n\nThe end of {}:\n", path.display());
    for line in tail.into_iter().rev() {
        out.push_str("    ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// An explicit path, or a name on `PATH`.
fn locate_binary(configured: &str) -> Result<PathBuf> {
    const INSTALL_HINT: &str =
        "Install Strom, or set [strom] binary to its path:\n    curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash";
    let candidate = Path::new(configured);
    if candidate.is_absolute() || configured.contains('/') {
        if candidate.is_file() {
            return Ok(candidate.to_path_buf());
        }
        bail!("no Strom executable at {configured}.\n\n{INSTALL_HINT}");
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(configured))
        .find(|full| full.is_file())
        .with_context(|| format!("could not find {configured:?} on PATH.\n\n{INSTALL_HINT}"))
}

/// Beside our own settings by default, so a managed Strom never writes into an
/// existing install's data directory.
fn data_dir(cfg: &Config) -> PathBuf {
    match cfg
        .strom
        .data_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(dir) => PathBuf::from(dir),
        None => config::user_dir().join("strom-data"),
    }
}

/// The explicit port in a URL. None when the URL relies on the scheme default, in
/// which case a Strom started on a guessed port would never be the one probed.
pub fn port_of(url: &str) -> Option<u16> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next()?;
    host.rsplit_once(':')?.1.parse().ok()
}

/// Stops a Strom recorded as started by an earlier run, for the case where `up` was
/// killed hard and left it holding the cameras. Checked against the process name,
/// because pids are reused after a reboot. An adopted Strom is never recorded, so it
/// is never stopped.
pub fn stop_recorded() {
    let pidfile = config::strom_pidfile();
    let Some(pid) = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    if crate::run::process_named(pid, "strom") {
        println!("  strom (pid {pid}) — stopping the one an earlier run started");
        signal_term(pid);
    }
    std::fs::remove_file(&pidfile).ok();
}

#[cfg(unix)]
fn signal_term(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn signal_term(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_is_read_from_the_url_and_a_default_port_is_not_guessed() {
        assert_eq!(port_of("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(port_of("https://strom.example.com:9443/api"), Some(9443));
        assert_eq!(port_of("http://strom.example.com"), None);
    }

    /// A path that does not exist must say so, rather than being looked up on PATH
    /// under its basename and silently finding something else.
    #[test]
    fn binaries_are_located_by_path_or_on_path() {
        assert!(locate_binary("/nonexistent/strom")
            .unwrap_err()
            .to_string()
            .contains("/nonexistent/strom"));
        assert!(locate_binary("definitely-not-a-real-binary-xyz").is_err());
        assert!(locate_binary("sh").expect("sh is on PATH").ends_with("sh"));
    }

    #[test]
    fn a_managed_strom_gets_its_own_data_directory_unless_told_otherwise() {
        let mut cfg = Config::default();
        assert!(data_dir(&cfg).ends_with("strom-data"));
        cfg.strom.data_dir = Some("/tmp/somewhere".to_string());
        assert_eq!(data_dir(&cfg), PathBuf::from("/tmp/somewhere"));
    }
}
