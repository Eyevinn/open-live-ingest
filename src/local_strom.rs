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
use std::ffi::{OsStr, OsString};
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
            "Strom at {} wants a credential.\n\nIt runs with STROM_API_KEY set. Run `open-live-ingest setup` and enter that key when asked.",
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

    let strom = locate_binary(&cfg.strom.binary)?;
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
    let mut command = Command::new(&strom.path);
    command
        .args(["--headless", "--port", &port.to_string(), "--data-dir"])
        .arg(&data_dir)
        .envs(strom.gst_env(&data_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Its own process group, so a Ctrl-C in the terminal reaches only the gateway.
    // Otherwise Strom exits with it, before the teardown has removed the flows; the
    // gateway stops it itself afterwards, when `LocalStrom` is dropped.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("starting {}", strom.path.display()))?;

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

/// The self-contained macOS app ships the gateway and Strom side by side, with the
/// GStreamer that Strom links against under `Frameworks`:
///
/// ```text
/// Open Live Ingest.app/Contents/
///     MacOS/open-live-ingest
///     MacOS/strom
///     Frameworks/                                  GStreamer and GLib libraries
///     Frameworks/gstreamer-1.0/                    GStreamer plugins
///     Frameworks/gstreamer-1.0/gst-plugin-scanner  the plugin scanner
/// ```
///
/// The paths below are relative to `Contents`, the parent of the directory holding
/// the executables. The script that builds the bundle (OL-57b) must put the files
/// exactly there; change both together.
pub const BUNDLE_PLUGIN_DIR: &str = "Frameworks/gstreamer-1.0";
/// See [`BUNDLE_PLUGIN_DIR`]. GStreamer only loads plugin files by their library
/// suffix, so the scanner can sit in the plugin directory.
pub const BUNDLE_PLUGIN_SCANNER: &str = "Frameworks/gstreamer-1.0/gst-plugin-scanner";
/// The GStreamer registry of a bundled Strom, kept in the managed Strom's data
/// directory so it never shares a registry with another GStreamer on the machine.
pub const BUNDLE_REGISTRY_FILE: &str = "gstreamer-registry.bin";

const INSTALL_HINT: &str =
    "Install Strom, or set [strom] binary to its path:\n    curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash";

/// Where the Strom to start was found, which decides how it is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StromOrigin {
    /// `strom.binary` names a path.
    Configured,
    /// Next to the gateway's own executable, as in the app bundle. Runs with the
    /// bundle's GStreamer only.
    Bundled { exe_dir: PathBuf },
    /// Found on `PATH`.
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedStrom {
    pub path: PathBuf,
    pub origin: StromOrigin,
}

impl LocatedStrom {
    /// Why this Strom was chosen, for the log and the setup check.
    pub fn reason(&self) -> &'static str {
        match self.origin {
            StromOrigin::Configured => "the path set in [strom] binary",
            StromOrigin::Bundled { .. } => "the one next to open-live-ingest",
            StromOrigin::Path => "found on PATH",
        }
    }

    /// The GStreamer environment for starting this Strom. Empty unless it is the
    /// bundled one: a Strom on `PATH` or set explicitly brings its own GStreamer.
    /// For the bundled one, plugins and the scanner come from the bundle only, and
    /// the registry lives in `data_dir`, so a Homebrew GStreamer is never mixed in.
    pub fn gst_env(&self, data_dir: &Path) -> Vec<(&'static str, OsString)> {
        let StromOrigin::Bundled { exe_dir } = &self.origin else {
            return Vec::new();
        };
        let contents = exe_dir.parent().unwrap_or(exe_dir);
        let plugins = contents.join(BUNDLE_PLUGIN_DIR).into_os_string();
        let scanner = contents.join(BUNDLE_PLUGIN_SCANNER).into_os_string();
        let registry = data_dir.join(BUNDLE_REGISTRY_FILE).into_os_string();
        vec![
            ("GST_PLUGIN_SYSTEM_PATH_1_0", plugins.clone()),
            ("GST_PLUGIN_SYSTEM_PATH", plugins),
            ("GST_PLUGIN_PATH_1_0", OsString::new()),
            ("GST_PLUGIN_PATH", OsString::new()),
            ("GST_PLUGIN_SCANNER_1_0", scanner.clone()),
            ("GST_PLUGIN_SCANNER", scanner),
            ("GST_REGISTRY_1_0", registry.clone()),
            ("GST_REGISTRY", registry),
        ]
    }
}

/// The Strom to start: an explicit setting, else one next to this executable, else
/// one on `PATH`.
pub fn locate_binary(configured: &str) -> Result<LocatedStrom> {
    let exe_dir = own_exe_dir();
    let located = resolve_binary(
        configured,
        exe_dir.as_deref(),
        std::env::var_os("PATH").as_deref(),
    )?;
    info!(path = %located.path.display(), reason = located.reason(), "chose the Strom to start");
    Ok(located)
}

/// The directory of the running executable with symlinks resolved, so a
/// `/usr/local/bin` link into the app still finds the Strom in the app.
fn own_exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// The lookup order, kept free of the process environment so it can be tested.
/// A path in `configured` always wins and is never looked up elsewhere. Only the
/// default name looks next to the executable before `PATH`; any other name is a
/// deliberate choice and is looked up on `PATH` alone.
fn resolve_binary(
    configured: &str,
    exe_dir: Option<&Path>,
    path_var: Option<&OsStr>,
) -> Result<LocatedStrom> {
    let candidate = Path::new(configured);
    if candidate.is_absolute() || configured.contains('/') {
        if candidate.is_file() {
            return Ok(LocatedStrom {
                path: candidate.to_path_buf(),
                origin: StromOrigin::Configured,
            });
        }
        bail!("no Strom executable at {configured}.\n\n{INSTALL_HINT}");
    }
    if configured == config::DEFAULT_STROM_BINARY {
        if let Some(dir) = exe_dir {
            let beside = dir.join(configured);
            if is_executable(&beside) {
                return Ok(LocatedStrom {
                    path: beside,
                    origin: StromOrigin::Bundled {
                        exe_dir: dir.to_path_buf(),
                    },
                });
            }
        }
    }
    std::env::split_paths(path_var.unwrap_or_default())
        .map(|dir| dir.join(configured))
        .find(|full| full.is_file())
        .map(|path| LocatedStrom {
            path,
            origin: StromOrigin::Path,
        })
        .with_context(|| format!("could not find {configured:?} on PATH.\n\n{INSTALL_HINT}"))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
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
        #[cfg(unix)]
        assert!(locate_binary("sh")
            .expect("sh is on PATH")
            .path
            .ends_with("sh"));
    }

    /// A fresh directory per test, removed again when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("oli-local-strom-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        /// An executable file at `rel`, with its directories.
        fn exe(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_explicit_path_wins_then_the_strom_next_to_us_then_path() {
        let scratch = Scratch::new("order");
        let macos = scratch.0.join("App.app/Contents/MacOS");
        let bundled = scratch.exe("App.app/Contents/MacOS/strom");
        let on_path = scratch.exe("bin/strom");
        let explicit = scratch.exe("elsewhere/strom");
        let path_var = std::env::join_paths([scratch.0.join("bin")]).unwrap();

        // An explicit path wins over a strom next to the executable.
        let got =
            resolve_binary(explicit.to_str().unwrap(), Some(&macos), Some(&path_var)).unwrap();
        assert_eq!(got.path, explicit);
        assert_eq!(got.origin, StromOrigin::Configured);

        // The default name finds the strom next to the executable before PATH.
        let got = resolve_binary("strom", Some(&macos), Some(&path_var)).unwrap();
        assert_eq!(got.path, bundled);
        assert_eq!(
            got.origin,
            StromOrigin::Bundled {
                exe_dir: macos.clone()
            }
        );

        // With nothing next to the executable, PATH.
        let empty_dir = scratch.0.join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let got = resolve_binary("strom", Some(&empty_dir), Some(&path_var)).unwrap();
        assert_eq!(got.path, on_path);
        assert_eq!(got.origin, StromOrigin::Path);
        let got = resolve_binary("strom", None, Some(&path_var)).unwrap();
        assert_eq!(got.origin, StromOrigin::Path);

        // A missing explicit path still errors, and is not rescued by the bundle.
        let missing = scratch.0.join("nowhere/strom");
        let err = resolve_binary(missing.to_str().unwrap(), Some(&macos), Some(&path_var))
            .unwrap_err()
            .to_string();
        assert!(err.contains(missing.to_str().unwrap()), "{err}");

        // Nothing anywhere is an error naming the binary.
        assert!(resolve_binary("strom", Some(&empty_dir), None).is_err());
    }

    /// Any name other than the default is a deliberate choice, looked up on PATH only.
    #[test]
    fn a_non_default_name_is_not_looked_up_next_to_us() {
        let scratch = Scratch::new("name");
        let macos = scratch.0.join("MacOS");
        scratch.exe("MacOS/strom-dev");
        let on_path = scratch.exe("bin/strom-dev");
        let path_var = std::env::join_paths([scratch.0.join("bin")]).unwrap();
        let got = resolve_binary("strom-dev", Some(&macos), Some(&path_var)).unwrap();
        assert_eq!(got.path, on_path);
        assert_eq!(got.origin, StromOrigin::Path);
    }

    #[cfg(unix)]
    #[test]
    fn a_strom_next_to_us_that_is_not_executable_is_skipped() {
        let scratch = Scratch::new("noexec");
        let macos = scratch.0.join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("strom"), "").unwrap();
        let on_path = scratch.exe("bin/strom");
        let path_var = std::env::join_paths([scratch.0.join("bin")]).unwrap();
        let got = resolve_binary("strom", Some(&macos), Some(&path_var)).unwrap();
        assert_eq!(got.path, on_path);
    }

    #[test]
    fn only_a_bundled_strom_gets_the_bundles_gstreamer() {
        let data_dir = PathBuf::from("/data/strom-data");
        let macos = PathBuf::from("/Applications/Open Live Ingest.app/Contents/MacOS");
        let contents = PathBuf::from("/Applications/Open Live Ingest.app/Contents");

        for origin in [StromOrigin::Configured, StromOrigin::Path] {
            let strom = LocatedStrom {
                path: PathBuf::from("/usr/local/bin/strom"),
                origin,
            };
            assert!(strom.gst_env(&data_dir).is_empty());
        }

        let strom = LocatedStrom {
            path: macos.join("strom"),
            origin: StromOrigin::Bundled {
                exe_dir: macos.clone(),
            },
        };
        let env: std::collections::HashMap<_, _> = strom.gst_env(&data_dir).into_iter().collect();
        let plugins = contents.join("Frameworks/gstreamer-1.0");
        let scanner = contents.join("Frameworks/gstreamer-1.0/gst-plugin-scanner");
        let registry = data_dir.join("gstreamer-registry.bin");
        for (key, want) in [
            ("GST_PLUGIN_SYSTEM_PATH_1_0", plugins.as_os_str()),
            ("GST_PLUGIN_SYSTEM_PATH", plugins.as_os_str()),
            ("GST_PLUGIN_PATH_1_0", OsStr::new("")),
            ("GST_PLUGIN_PATH", OsStr::new("")),
            ("GST_PLUGIN_SCANNER_1_0", scanner.as_os_str()),
            ("GST_PLUGIN_SCANNER", scanner.as_os_str()),
            ("GST_REGISTRY_1_0", registry.as_os_str()),
            ("GST_REGISTRY", registry.as_os_str()),
        ] {
            assert_eq!(env.get(key).map(OsString::as_os_str), Some(want), "{key}");
        }
        assert_eq!(env.len(), 8);
        for key in ["GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_SCANNER_1_0"] {
            assert!(Path::new(&env[key]).starts_with(&contents), "{key}");
        }
        assert!(Path::new(&env["GST_REGISTRY_1_0"]).starts_with(&data_dir));
    }

    #[test]
    fn a_managed_strom_gets_its_own_data_directory_unless_told_otherwise() {
        let mut cfg = Config::default();
        assert!(data_dir(&cfg).ends_with("strom-data"));
        cfg.strom.data_dir = Some("/tmp/somewhere".to_string());
        assert_eq!(data_dir(&cfg), PathBuf::from("/tmp/somewhere"));
    }
}
