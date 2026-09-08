//! Starting and stopping a headless Strom.
//!
//! Strom is not assumed to be running. If nothing answers at the configured URL, one
//! is started here and stopped again on the way out.
//!
//! The rule that matters is **adopt, never replace**: a Strom already listening is
//! used as it is and never stopped, because it may be a service this box depends on
//! or another operator's session. Only a Strom this process started is a Strom this
//! process may kill.

use crate::config;
use crate::strom::client::{Reachability, StromClient};
use anyhow::{bail, Context, Result};
use open_live_gateway_types::GatewayConfig;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long to wait for a freshly started Strom to answer. It loads GStreamer,
/// enumerates devices and probes for hardware first, which is not instant.
const READY_TIMEOUT: Duration = Duration::from_secs(45);
const READY_POLL: Duration = Duration::from_millis(400);

/// How the local Strom came to be.
pub enum LocalStrom {
    /// Already running. Left alone entirely.
    Adopted,
    /// Started here, and stopped when this drops out of scope.
    Managed(Managed),
}

pub struct Managed {
    child: Child,
    log_path: PathBuf,
}

impl LocalStrom {
    /// The pid of a Strom we started, for recording so a later `down` can clean up
    /// after a hard kill.
    pub fn managed_pid(&self) -> Option<u32> {
        match self {
            LocalStrom::Adopted => None,
            LocalStrom::Managed(m) => Some(m.child.id()),
        }
    }

    /// Stops a Strom we started; leaves an adopted one running.
    pub fn shutdown(self) {
        match self {
            LocalStrom::Adopted => {}
            LocalStrom::Managed(mut m) => {
                let pid = m.child.id();
                info!(pid, "stopping the Strom we started");
                stop_pid(pid);

                // Wait for it to go, then insist. Strom holds capture devices and
                // sockets; leaving one behind makes the next run adopt a half-dead
                // engine.
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    match m.child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        Ok(None) => {
                            warn!(pid, "Strom did not stop in time, killing it");
                            let _ = m.child.kill();
                            let _ = m.child.wait();
                            return;
                        }
                        Err(err) => {
                            warn!(pid, %err, "could not wait for Strom");
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Where a managed Strom's output went, for error reporting.
    pub fn log_path(&self) -> Option<&Path> {
        match self {
            LocalStrom::Adopted => None,
            LocalStrom::Managed(m) => Some(&m.log_path),
        }
    }
}

/// Returns a usable local Strom, starting one if nothing is listening.
pub async fn ensure_running(cfg: &GatewayConfig) -> Result<LocalStrom> {
    let client = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;

    match client.probe().await {
        Reachability::Ok { .. } => {
            info!(url = %cfg.strom.url, "using the Strom already running");
            return Ok(LocalStrom::Adopted);
        }
        // A running Strom that wants a credential is still a running Strom. Starting
        // a second one would fight it for the port.
        Reachability::NeedsCredential => bail!(
            "Strom at {url} needs a credential.\n\n\
             It is running with STROM_API_KEY set. Run `open-live-gateway setup` and \
             enter that key when it asks.",
            url = cfg.strom.url
        ),
        Reachability::Unreachable(_) => {}
    }

    if !cfg.strom.manage {
        bail!(
            "nothing is listening at {url}, and [strom] manage is off.\n\n\
             Start Strom yourself, or set manage = true to have the gateway run one.",
            url = cfg.strom.url
        );
    }

    let binary = locate_binary(&cfg.strom.binary)?;
    let port = port_of(&cfg.strom.url).unwrap_or(8080);
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
        .arg("--headless")
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(&data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;

    let managed = LocalStrom::Managed(Managed { child, log_path });
    wait_until_ready(&client, &managed).await?;
    Ok(managed)
}

/// Polls until Strom answers, or gives up with whatever it logged.
async fn wait_until_ready(client: &StromClient, strom: &LocalStrom) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;

    loop {
        match client.probe().await {
            Reachability::Ok { video_sources } => {
                println!("  Strom is up; it can see {video_sources} video source(s).");
                return Ok(());
            }
            // Managing a Strom means we did not set a key on it, so this would be
            // someone else's instance on the port. Better to say so than to retry.
            Reachability::NeedsCredential => {
                bail!("the Strom on that port wants a credential, so it is not the one we started")
            }
            Reachability::Unreachable(_) if Instant::now() < deadline => {
                tokio::time::sleep(READY_POLL).await;
            }
            Reachability::Unreachable(err) => {
                let tail = strom.log_path().map(tail_of_log).unwrap_or_default();
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
    match std::fs::read_to_string(path) {
        Ok(text) => {
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
        Err(_) => String::new(),
    }
}

/// Finds the Strom executable: an explicit path, or a name on `PATH`.
fn locate_binary(configured: &str) -> Result<PathBuf> {
    let candidate = Path::new(configured);
    if candidate.is_absolute() || configured.contains('/') {
        if candidate.is_file() {
            return Ok(candidate.to_path_buf());
        }
        bail!(
            "no Strom executable at {configured}.\n\n\
             Set [strom] binary to where it is installed, or install it:\n    \
             curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash"
        );
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let full = dir.join(configured);
        if full.is_file() {
            return Ok(full);
        }
    }

    bail!(
        "could not find {configured:?} on PATH.\n\n\
         Install Strom, or set [strom] binary to its path:\n    \
         curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash"
    )
}

/// Where a managed Strom keeps its data.
fn data_dir(cfg: &GatewayConfig) -> PathBuf {
    if let Some(dir) = cfg.strom.data_dir.as_deref().filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    // Beside our own settings, so a managed Strom never writes into an existing
    // install's data directory.
    config::default_path()
        .parent()
        .map(|p| p.join("strom-data"))
        .unwrap_or_else(|| PathBuf::from("strom-data"))
}

/// The port in a Strom URL.
pub fn port_of(url: &str) -> Option<u16> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next()?;
    host.rsplit_once(':')?.1.parse().ok()
}

#[cfg(unix)]
fn stop_pid(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn stop_pid(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_read_from_the_url() {
        assert_eq!(port_of("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(port_of("https://strom.example.com:9443/api"), Some(9443));
        assert_eq!(port_of("http://strom.example.com"), None);
    }

    /// A path that does not exist must say so, rather than being looked for on PATH
    /// under its basename and silently finding something else.
    #[test]
    fn an_explicit_path_that_is_missing_is_an_error() {
        let err = locate_binary("/nonexistent/strom").unwrap_err().to_string();
        assert!(err.contains("/nonexistent/strom"));
    }

    #[test]
    fn a_bare_name_not_on_path_is_an_error_naming_it() {
        let err = locate_binary("definitely-not-a-real-binary-xyz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("definitely-not-a-real-binary-xyz"));
    }

    /// Something on PATH is found by name — `sh` stands in for `strom` here.
    #[test]
    fn a_bare_name_on_path_is_found() {
        let found = locate_binary("sh").expect("sh should be on PATH");
        assert!(found.is_file());
        assert!(found.ends_with("sh"));
    }

    #[test]
    fn a_managed_strom_uses_its_own_data_directory_by_default() {
        let cfg = GatewayConfig::default();
        let dir = data_dir(&cfg);
        assert!(dir.ends_with("strom-data"), "got {}", dir.display());
    }

    #[test]
    fn a_configured_data_directory_wins() {
        let mut cfg = GatewayConfig::default();
        cfg.strom.data_dir = Some("/tmp/somewhere".to_string());
        assert_eq!(data_dir(&cfg), PathBuf::from("/tmp/somewhere"));
    }
}
