//! The Strom on this machine: adopted if one is listening, started headless if not.
//!
//! The rule is **adopt, never replace**. A Strom already answering at the configured
//! URL is used as it is and never stopped, because it may be a service this box
//! depends on or another operator's session. Only a Strom this process started is a
//! Strom this process may stop, and it is stopped on the way out so a camera does
//! not stay open with nothing supervising it.

use crate::config::{self, Config};
use crate::gateway::{Event, Events};
use crate::strom::{Reachability, StromClient};
use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A freshly started Strom loads GStreamer and probes for hardware first.
const READY_TIMEOUT: Duration = Duration::from_secs(45);
const READY_POLL: Duration = Duration::from_millis(400);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub enum LocalStrom {
    /// Already running. Left alone entirely.
    Adopted,
    /// Started here, and stopped when this is dropped, which is told to `events`.
    Managed {
        child: Child,
        log_path: PathBuf,
        events: Events,
    },
}

impl Drop for LocalStrom {
    fn drop(&mut self) {
        let LocalStrom::Managed { child, events, .. } = self else {
            return;
        };
        // Already exited and reaped: its pid may belong to another process by now.
        if let Ok(Some(_)) = child.try_wait() {
            std::fs::remove_file(config::strom_pidfile()).ok();
            return;
        }
        let pid = child.id();
        info!(pid, "stopping the Strom we started");
        // Unix: SIGTERM, then the wait below gives it time to exit cleanly. Windows
        // has no SIGTERM, so it is ended hard (TerminateProcess) through the handle we
        // hold. That is fine here: the teardown has already removed the flows and the
        // Open Live sources by the time this runs.
        #[cfg(not(windows))]
        signal_term(pid);
        #[cfg(windows)]
        let _ = child.kill();
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
        events.emit(Event::StromStopped);
    }
}

/// Whether a Strom this process starts gets no console window (Windows). Set once by
/// a GUI-subsystem app through [`start_without_console_window`]; the CLI leaves it
/// off and keeps its console behaviour.
static NO_CONSOLE_WINDOW: AtomicBool = AtomicBool::new(false);

/// Windows `CREATE_NEW_PROCESS_GROUP`: Strom gets a Ctrl-C of its own, not the
/// gateway's.
#[cfg_attr(not(windows), allow(dead_code))]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// Windows `CREATE_NO_WINDOW`: a console program started from a GUI process would
/// otherwise get a console window of its own.
#[cfg_attr(not(windows), allow(dead_code))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// For an app with no console of its own (a Windows GUI-subsystem executable): every
/// Strom this process starts from now on is started with `CREATE_NO_WINDOW`, so no
/// console window opens next to the app. Has no effect elsewhere than on Windows.
pub fn start_without_console_window() {
    NO_CONSOLE_WINDOW.store(true, Ordering::Relaxed);
}

/// The creation flags a Strom is started with on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn creation_flags(no_console_window: bool) -> u32 {
    if no_console_window {
        CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    } else {
        CREATE_NEW_PROCESS_GROUP
    }
}

/// Returns a usable local Strom, starting one if nothing is listening.
pub async fn ensure_running(cfg: &Config, events: &Events) -> Result<LocalStrom> {
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
    let log_path = log_path(cfg);
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let log_err = log.try_clone().context("duplicating the log file handle")?;

    events.emit(Event::StromStarting {
        port,
        log_path: log_path.clone(),
    });
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(creation_flags(NO_CONSOLE_WINDOW.load(Ordering::Relaxed)));
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

    let mut managed = LocalStrom::Managed {
        child,
        log_path,
        events: events.clone(),
    };
    wait_until_ready(&client, &mut managed, events).await?;
    Ok(managed)
}

async fn wait_until_ready(
    client: &StromClient,
    strom: &mut LocalStrom,
    events: &Events,
) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let LocalStrom::Managed {
            child, log_path, ..
        } = &mut *strom
        {
            if let Some(status) = child
                .try_wait()
                .context("checking on the Strom we started")?
            {
                bail!(
                    "Strom exited during startup ({status}).{}",
                    tail_of_log(log_path)
                );
            }
        }
        match client.probe().await {
            Reachability::Ok { video_sources } => {
                events.emit(Event::StromReady { video_sources });
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
/// GStreamer that Strom links against where Apple's bundle layout puts each kind of
/// code, since `codesign` accepts nested code only there:
///
/// ```text
/// Open Live Ingest.app/Contents/
///     MacOS/open-live-ingest-app   the desktop app, the bundle's main executable
///     MacOS/open-live-ingest       the CLI
///     MacOS/strom
///     Frameworks/                  GStreamer and GLib libraries
///     PlugIns/gstreamer/           GStreamer plugins
///     Helpers/gst-plugin-scanner   the plugin scanner
/// ```
///
/// No directory in those places may have a dot in its name: `codesign` takes such a
/// directory for a nested bundle and rejects the app.
///
/// The paths below are relative to `Contents`, the parent of the directory holding
/// the executables. `.github/scripts/build-macos-app.sh` puts the files exactly
/// there (`PLUGIN_DIR_REL`, `SCANNER_REL`); a test below keeps the two in step.
///
/// A `strom` next to the gateway counts as bundled only when this directory exists
/// relative to it. That is what tells the app apart from a plain install such as
/// `install.sh`'s, which puts both executables in `/usr/local/bin` or `~/.local/bin`
/// with no GStreamer of their own.
pub const BUNDLE_PLUGIN_DIR: &str = "PlugIns/gstreamer";
/// See [`BUNDLE_PLUGIN_DIR`]. An executable, so it sits with the bundle's helper tools.
pub const BUNDLE_PLUGIN_SCANNER: &str = "Helpers/gst-plugin-scanner";
/// The GStreamer registry of a bundled Strom, kept in the managed Strom's data
/// directory so it never shares a registry with another GStreamer on the machine.
pub const BUNDLE_REGISTRY_FILE: &str = "gstreamer-registry.bin";

#[cfg(not(windows))]
const INSTALL_HINT: &str =
    "Install Strom, or set [strom] binary to its path:\n    curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash";
// install.ps1 (#42) is not on main yet, so the hint leads with the MSI; #42 adds
// its `irm ... | iex` line here.
#[cfg(windows)]
const INSTALL_HINT: &str = "Install Strom with the Strom MSI from https://github.com/Eyevinn/strom/releases,\nor set [strom] binary to the path of strom.exe.";

/// Where the Strom MSI puts `strom.exe`, relative to `%ProgramFiles%` (Eyevinn/strom
/// `installer/windows/Product.wxs`: ProgramFiles64Folder\Strom\bin).
const MSI_STROM: [&str; 3] = ["Strom", "bin", "strom.exe"];

/// Where the Strom to start was found, which decides how it is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StromOrigin {
    /// `strom.binary` names a path.
    Configured,
    /// Next to the gateway's own executable, inside the app bundle: the parent of
    /// that directory (`contents`, the bundle's `Contents`) holds
    /// [`BUNDLE_PLUGIN_DIR`]. Runs with the bundle's GStreamer only.
    Bundled { contents: PathBuf },
    /// Next to the gateway's own executable, but with no bundle around it, as after
    /// `install.sh`. Brings its own GStreamer, like a Strom on `PATH`.
    Beside,
    /// Found on `PATH`.
    Path,
    /// In the Strom MSI's install folder, `%ProgramFiles%\Strom\bin` (Windows), for a
    /// console opened before the MSI added that folder to the system `PATH`. That
    /// Strom finds its own GStreamer under `Strom\gstreamer`.
    Installed,
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
            StromOrigin::Bundled { .. } => "the one bundled with open-live-ingest",
            StromOrigin::Beside => "the one next to open-live-ingest",
            StromOrigin::Path => "found on PATH",
            StromOrigin::Installed => "the Strom MSI's install folder",
        }
    }

    /// The GStreamer environment for starting this Strom. Empty unless it is the
    /// bundled one: a Strom on `PATH`, set explicitly, or next to the gateway outside
    /// a bundle brings its own GStreamer.
    /// For the bundled one, plugins and the scanner come from the bundle only, and
    /// the registry lives in `data_dir`, so a Homebrew GStreamer is never mixed in.
    pub fn gst_env(&self, data_dir: &Path) -> Vec<(&'static str, OsString)> {
        let StromOrigin::Bundled { contents } = &self.origin else {
            return Vec::new();
        };
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
/// one on `PATH`, else (Windows) the one the Strom MSI installed.
pub fn locate_binary(configured: &str) -> Result<LocatedStrom> {
    let exe_dir = own_exe_dir();
    let program_files = if cfg!(windows) {
        std::env::var_os("ProgramFiles")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else {
        None
    };
    let located = resolve_binary(
        configured,
        exe_dir.as_deref(),
        std::env::var_os("PATH").as_deref(),
        program_files.as_deref(),
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
/// deliberate choice and is looked up on `PATH` alone. A Strom found next to the
/// executable is [`StromOrigin::Bundled`] only when the bundle layout is there
/// (see [`bundle_contents`]), and [`StromOrigin::Beside`] otherwise.
///
/// Order: explicit path > next to the executable > `PATH` > the Strom MSI's folder
/// under `program_files` (passed only on Windows, and only for the default name).
/// On Windows a value containing `\` is a path too, and a name without an extension
/// is looked up as `<name>.exe` only (see [`file_name`]).
fn resolve_binary(
    configured: &str,
    exe_dir: Option<&Path>,
    path_var: Option<&OsStr>,
    program_files: Option<&Path>,
) -> Result<LocatedStrom> {
    let candidate = Path::new(configured);
    let is_path = configured.contains('/') || (cfg!(windows) && configured.contains('\\'));
    if candidate.is_absolute() || is_path {
        if candidate.is_file() {
            return Ok(LocatedStrom {
                path: candidate.to_path_buf(),
                origin: StromOrigin::Configured,
            });
        }
        bail!("no Strom executable at {configured}.\n\n{INSTALL_HINT}");
    }
    // Strom's own name, in whatever case it was configured, is looked up by its
    // canonical spelling (`strom.exe` on Windows). Windows matches `STROM.EXE`
    // case-insensitively but the path would keep the configured casing, so the log
    // and the setup check would name a file that is not spelled that way on disk.
    let file = if is_default_name(configured) {
        file_name(config::DEFAULT_STROM_BINARY)
    } else {
        file_name(configured)
    };
    if is_default_name(configured) {
        if let Some(dir) = exe_dir {
            let beside = dir.join(&file);
            if is_executable(&beside) {
                let origin = match bundle_contents(dir) {
                    Some(contents) => StromOrigin::Bundled { contents },
                    None => StromOrigin::Beside,
                };
                return Ok(LocatedStrom {
                    path: beside,
                    origin,
                });
            }
        }
    }
    let on_path = std::env::split_paths(path_var.unwrap_or_default())
        .map(|dir| dir.join(&file))
        .find(|full| full.is_file());
    if let Some(path) = on_path {
        return Ok(LocatedStrom {
            path,
            origin: StromOrigin::Path,
        });
    }
    if is_default_name(configured) {
        if let Some(program_files) = program_files {
            let installed: PathBuf = MSI_STROM
                .iter()
                .fold(program_files.to_path_buf(), |p, part| p.join(part));
            if installed.is_file() {
                return Ok(LocatedStrom {
                    path: installed,
                    origin: StromOrigin::Installed,
                });
            }
        }
    }
    bail!("could not find {configured:?} on PATH.\n\n{INSTALL_HINT}")
}

/// Whether `configured` is Strom's own name, the only one looked up next to the
/// executable and in the MSI folder. On Windows also `strom.exe`, in any case, as
/// Windows file names are case-insensitive.
fn is_default_name(configured: &str) -> bool {
    if cfg!(windows) {
        configured.eq_ignore_ascii_case(config::DEFAULT_STROM_BINARY)
            || configured.eq_ignore_ascii_case(&format!("{}.exe", config::DEFAULT_STROM_BINARY))
    } else {
        configured == config::DEFAULT_STROM_BINARY
    }
}

/// The file a command name stands for. On Windows a name without an extension is
/// `<name>.exe` only: Windows never runs an extensionless file as a command, so a
/// bare `strom` (a shell script in a Git-for-Windows dir, say) must not shadow a
/// `strom.exe` later on `PATH` or next to the executable.
fn file_name(name: &str) -> String {
    if cfg!(windows) && Path::new(name).extension().is_none() {
        format!("{name}{}", std::env::consts::EXE_SUFFIX)
    } else {
        name.to_string()
    }
}

/// The bundle's `Contents` directory when `exe_dir` sits inside the app bundle,
/// that is when its parent holds [`BUNDLE_PLUGIN_DIR`]. None for a plain directory
/// such as `/usr/local/bin`, where pointing GStreamer at a plugin directory that does
/// not exist would leave Strom with no plugins at all.
fn bundle_contents(exe_dir: &Path) -> Option<PathBuf> {
    let contents = exe_dir.parent()?;
    contents
        .join(BUNDLE_PLUGIN_DIR)
        .is_dir()
        .then(|| contents.to_path_buf())
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

/// Where a Strom the gateway starts writes its output (`<data_dir>/strom.log`),
/// replaced at each start. An adopted Strom logs wherever it was started to.
pub fn log_path(cfg: &Config) -> PathBuf {
    data_dir(cfg).join("strom.log")
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
pub fn stop_recorded(events: &Events) {
    let pidfile = config::strom_pidfile();
    let Some(pid) = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    if crate::run::process_named(pid, "strom") {
        events.emit(Event::StromLeftOverStopping { pid });
        signal_term(pid);
    }
    std::fs::remove_file(&pidfile).ok();
}

/// Reclaims a Strom left behind by a gateway that ended without its teardown (a
/// crash, a force quit): when `strom.pid` names a running Strom, it is stopped and
/// waited for, so the Strom the next session starts is one it owns and stops again.
/// Adopting the leftover instead would leave a camera open after the next Stop,
/// because an adopted Strom is never stopped.
///
/// Nothing is stopped while another gateway is running (see
/// [`crate::run::running_gateway`]): its Strom is not left behind, it is in use. That
/// check is made here, after `strom.pid` is read, not by the caller beforehand. `up`
/// writes `gateway.pid` before it starts Strom and writes `strom.pid`, so a
/// `strom.pid` that belongs to a running gateway, or to one that is still starting,
/// is always covered by a `gateway.pid` by the time it is read here: a gateway that
/// starts while the caller is busy (the app waiting on the macOS camera prompt) is
/// never mistaken for a crash's leftover.
/// Returns whether a Strom was stopped. Blocks for up to [`STOP_TIMEOUT`].
pub fn reclaim_recorded(events: &Events) -> bool {
    reclaim_recorded_at(&config::strom_pidfile(), &config::pidfile(), events)
}

fn reclaim_recorded_at(pidfile: &Path, gateway_pidfile: &Path, events: &Events) -> bool {
    let Some(pid) = read_pid(pidfile) else {
        return false;
    };
    // Only after the read: see above.
    if let Some(gateway) = crate::run::running_gateway_in(gateway_pidfile) {
        info!(
            pid,
            gateway, "a gateway is running: its Strom is not reclaimed"
        );
        return false;
    }
    if !crate::run::process_named(pid, "strom") {
        remove_if_still(pidfile, pid);
        return false;
    }
    info!(pid, "stopping the Strom an earlier session left behind");
    events.emit(Event::StromLeftOverStopping { pid });
    signal_term(pid);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while crate::run::process_named(pid, "strom") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    if crate::run::process_named(pid, "strom") {
        warn!(pid, "the Strom left behind did not stop in time");
    }
    remove_if_still(pidfile, pid);
    true
}

fn read_pid(pidfile: &Path) -> Option<u32> {
    std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Removes `pidfile` only while it still names `pid`: a gateway that started in the
/// meantime has recorded its own Strom there.
fn remove_if_still(pidfile: &Path, pid: u32) {
    if read_pid(pidfile) == Some(pid) {
        std::fs::remove_file(pidfile).ok();
    }
}

#[cfg(unix)]
fn signal_term(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// Windows has no SIGTERM, so a Strom known only by its pid is ended hard, with its
/// child processes. This is only used after the flows are gone (`down` removes them
/// first), so nothing is left half torn down. CTRL_BREAK_EVENT is not used because it
/// has not been checked that Strom shuts down cleanly on it.
#[cfg(windows)]
fn signal_term(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .output();
}

#[cfg(not(any(unix, windows)))]
fn signal_term(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's launch option adds `CREATE_NO_WINDOW`; the CLI keeps today's flags.
    #[test]
    fn a_gui_app_starts_strom_without_a_console_window() {
        assert_eq!(creation_flags(false), 0x0000_0200);
        assert_eq!(creation_flags(true), 0x0800_0200);
    }

    /// A pidfile naming no Strom is stale: removed, and nothing is stopped.
    #[test]
    fn a_stale_strom_pidfile_is_removed_and_nothing_stopped() {
        let dir = std::env::temp_dir().join(format!("oli-reclaim-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("strom.pid");
        let no_gateway = dir.join("gateway.pid");
        // This test process: alive, but not a Strom.
        std::fs::write(&pidfile, std::process::id().to_string()).unwrap();
        let (events, seen) = recording();
        assert!(!reclaim_recorded_at(&pidfile, &no_gateway, &events));
        assert!(!pidfile.exists());
        assert!(seen.lock().unwrap().is_empty());
        assert!(
            !reclaim_recorded_at(&pidfile, &no_gateway, &events),
            "no pidfile at all"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Events that are kept, to be looked at after.
    fn recording() -> (Events, std::sync::Arc<std::sync::Mutex<Vec<Event>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events = {
            let seen = seen.clone();
            Events::new(move |e| seen.lock().unwrap().push(e))
        };
        (events, seen)
    }

    /// Whether this machine can tell processes apart by name: process names come from
    /// `ps`, which every CI runner has; a bare container may not, and then there is
    /// nothing to check against.
    #[cfg(unix)]
    fn ps_works() -> bool {
        Command::new("ps").arg("-p").arg("1").output().is_ok()
    }

    /// A process named `name`, started from `dir`, that runs until it is stopped. It
    /// stands in for a Strom or a gateway. The test fails here, saying so, if it
    /// exits at once rather than later on an assertion that cannot tell why.
    #[cfg(unix)]
    fn spawn_named(dir: &Path, name: &str) -> std::process::Child {
        let fake = write_stand_in(dir, name);
        let mut child = spawn_retrying(&fake);
        std::thread::sleep(Duration::from_millis(200));
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the stand-in {} exited at once ({status})", fake.display());
        }
        child
    }

    /// On Linux, a shell script named `name` that sleeps in a loop. The kernel names
    /// a process after the file it executed, so `ps -o comm=` shows `name`, as long as
    /// the script never `exec`s. A copy of `sleep` would not do: on Ubuntu 26.04
    /// `sleep` is the uutils coreutils multicall binary, which picks the utility from
    /// the name it was started under and exits at once as `strom`.
    #[cfg(target_os = "linux")]
    fn write_stand_in(dir: &Path, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join(name);
        std::fs::write(&fake, "#!/bin/sh\nwhile :; do sleep 1; done\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    /// Elsewhere (macOS), a copy of `sleep` named `name`, as before: `sleep` there is a
    /// single-purpose binary that runs under any name.
    #[cfg(all(unix, not(target_os = "linux")))]
    fn write_stand_in(dir: &Path, name: &str) -> std::path::PathBuf {
        let fake = dir.join(name);
        let sleep = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
            .expect("a sleep executable");
        std::fs::copy(sleep, &fake).unwrap();
        fake
    }

    /// Runs `fake` (with `60` for `sleep`; a script ignores it). A file just
    /// written may be briefly "text file busy" while another test thread forks with
    /// its write handle still open, so that is retried.
    #[cfg(unix)]
    fn spawn_retrying(fake: &Path) -> std::process::Child {
        let started = Instant::now();
        loop {
            match Command::new(fake).arg("60").spawn() {
                Ok(child) => return child,
                Err(err)
                    if err.raw_os_error() == Some(libc::ETXTBSY)
                        && started.elapsed() < Duration::from_secs(5) =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("starting {}: {err}", fake.display()),
            }
        }
    }

    /// Waits until `seen` holds: `ps` may not show a new process's name at once.
    #[cfg(unix)]
    fn wait_until(what: &str, seen: impl Fn() -> bool) {
        let started = Instant::now();
        while !seen() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{what} never showed up"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    fn a_strom(pid: u32) -> bool {
        crate::run::process_named(pid, "strom")
    }

    /// A Strom left running by a crashed session is stopped and waited for, not
    /// adopted.
    #[cfg(unix)]
    #[test]
    fn a_strom_left_behind_is_stopped_and_waited_for() {
        if !ps_works() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("oli-reclaim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = spawn_named(&dir, "strom");
        wait_until("the fake Strom", || a_strom(child.id()));
        let pidfile = dir.join("strom.pid");
        std::fs::write(&pidfile, child.id().to_string()).unwrap();
        let (events, seen) = recording();
        // Reaped on a thread, as a parent would: an unreaped child stays a zombie
        // that `ps` still lists.
        let pid = child.id();
        let reaper = std::thread::spawn(move || child.wait());
        assert!(reclaim_recorded_at(
            &pidfile,
            &dir.join("gateway.pid"),
            &events
        ));
        assert!(!pidfile.exists());
        assert!(!a_strom(pid));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Event::StromLeftOverStopping { pid }]
        );
        reaper.join().unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The Strom `strom.pid` names belongs to a gateway that `gateway.pid` names and
    /// that is running (one started while the app waited on its checks): it is not
    /// signalled, and its `strom.pid` is left for that gateway.
    #[cfg(unix)]
    #[test]
    fn the_strom_of_a_running_gateway_is_never_reclaimed() {
        if !ps_works() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("oli-reclaim-gw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gateway_pidfile = dir.join("gateway.pid");
        let mut strom = spawn_named(&dir, "strom");
        let mut gateway = spawn_named(&dir, env!("CARGO_PKG_NAME"));
        std::fs::write(&gateway_pidfile, gateway.id().to_string()).unwrap();
        wait_until("the fake Strom", || a_strom(strom.id()));
        wait_until("the fake gateway", || {
            crate::run::running_gateway_in(&gateway_pidfile) == Some(gateway.id())
        });
        let pidfile = dir.join("strom.pid");
        std::fs::write(&pidfile, strom.id().to_string()).unwrap();
        let (events, seen) = recording();

        assert!(!reclaim_recorded_at(&pidfile, &gateway_pidfile, &events));
        assert!(a_strom(strom.id()), "the gateway's Strom is still running");
        assert_eq!(
            std::fs::read_to_string(&pidfile).unwrap(),
            strom.id().to_string()
        );
        assert!(seen.lock().unwrap().is_empty());

        // Once that gateway has gone, the same Strom is a leftover again.
        gateway.kill().unwrap();
        gateway.wait().unwrap();
        let pid = strom.id();
        let reaper = std::thread::spawn(move || strom.wait());
        assert!(reclaim_recorded_at(&pidfile, &gateway_pidfile, &events));
        assert!(!a_strom(pid));
        assert!(!pidfile.exists());
        reaper.join().unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A gateway that started after `strom.pid` was read and recorded its own Strom
    /// there keeps that record when the stale one it replaced is cleaned up.
    #[test]
    fn a_newer_strom_pid_is_not_removed() {
        let dir = std::env::temp_dir().join(format!("oli-reclaim-newer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("strom.pid");
        std::fs::write(&pidfile, "4242").unwrap();
        remove_if_still(&pidfile, 4141);
        assert_eq!(std::fs::read_to_string(&pidfile).unwrap(), "4242");
        remove_if_still(&pidfile, 4242);
        assert!(!pidfile.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

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

        /// An executable file at `rel` plus the platform's executable suffix (`.exe`
        /// on Windows, where an extensionless `strom` is never picked), with its
        /// directories.
        fn exe(&self, rel: &str) -> PathBuf {
            let path = self
                .0
                .join(format!("{rel}{}", std::env::consts::EXE_SUFFIX));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_managed_strom_that_exits_during_startup_is_reported_at_once() {
        let scratch = Scratch::new("early-exit");
        let strom = scratch.exe("strom");
        std::fs::write(
            &strom,
            "#!/bin/sh\necho 'dyld: Library not loaded'\nexit 1\n",
        )
        .unwrap();
        let log_path = scratch.0.join("strom.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let child = Command::new(&strom)
            .stdout(Stdio::from(log))
            .spawn()
            .unwrap();
        // A port nothing listens on, so the probe stays unreachable.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = StromClient::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        // Not dropped: `Drop` would remove the user's real Strom pidfile.
        let mut managed = std::mem::ManuallyDrop::new(LocalStrom::Managed {
            child,
            log_path,
            events: Events::discard(),
        });

        let started = Instant::now();
        let err = wait_until_ready(&client, &mut managed, &Events::discard())
            .await
            .unwrap_err()
            .to_string();

        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
        assert!(err.contains("Strom exited during startup"), "{err}");
        assert!(err.contains("exit status: 1"), "{err}");
        assert!(err.contains("dyld: Library not loaded"), "{err}");
    }

    #[test]
    fn an_explicit_path_wins_then_the_strom_next_to_us_then_path() {
        let scratch = Scratch::new("order");
        let contents = scratch.0.join("App.app/Contents");
        let macos = contents.join("MacOS");
        std::fs::create_dir_all(contents.join(BUNDLE_PLUGIN_DIR)).unwrap();
        let bundled = scratch.exe("App.app/Contents/MacOS/strom");
        let on_path = scratch.exe("bin/strom");
        let explicit = scratch.exe("elsewhere/strom");
        let path_var = std::env::join_paths([scratch.0.join("bin")]).unwrap();

        // An explicit path wins over a strom next to the executable.
        let got = resolve_binary(
            explicit.to_str().unwrap(),
            Some(&macos),
            Some(&path_var),
            None,
        )
        .unwrap();
        assert_eq!(got.path, explicit);
        assert_eq!(got.origin, StromOrigin::Configured);

        // The default name finds the strom next to the executable before PATH.
        let got = resolve_binary("strom", Some(&macos), Some(&path_var), None).unwrap();
        assert_eq!(got.path, bundled);
        assert_eq!(
            got.origin,
            StromOrigin::Bundled {
                contents: contents.clone()
            }
        );

        // With nothing next to the executable, PATH.
        let empty_dir = scratch.0.join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let got = resolve_binary("strom", Some(&empty_dir), Some(&path_var), None).unwrap();
        assert_eq!(got.path, on_path);
        assert_eq!(got.origin, StromOrigin::Path);
        let got = resolve_binary("strom", None, Some(&path_var), None).unwrap();
        assert_eq!(got.origin, StromOrigin::Path);

        // A missing explicit path still errors, and is not rescued by the bundle.
        let missing = scratch.0.join("nowhere/strom");
        let err = resolve_binary(
            missing.to_str().unwrap(),
            Some(&macos),
            Some(&path_var),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(missing.to_str().unwrap()), "{err}");

        // Nothing anywhere is an error naming the binary.
        assert!(resolve_binary("strom", Some(&empty_dir), None, None).is_err());
    }

    /// Any name other than the default is a deliberate choice, looked up on PATH only.
    #[test]
    fn a_non_default_name_is_not_looked_up_next_to_us() {
        let scratch = Scratch::new("name");
        let macos = scratch.0.join("MacOS");
        scratch.exe("MacOS/strom-dev");
        let on_path = scratch.exe("bin/strom-dev");
        let path_var = std::env::join_paths([scratch.0.join("bin")]).unwrap();
        let got = resolve_binary("strom-dev", Some(&macos), Some(&path_var), None).unwrap();
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
        let got = resolve_binary("strom", Some(&macos), Some(&path_var), None).unwrap();
        assert_eq!(got.path, on_path);
    }

    /// `install.sh` puts the gateway and Strom side by side in a plain `bin`
    /// directory. That Strom is still preferred, but it is not bundled: it must keep
    /// its own GStreamer, so it gets no GST_* environment.
    #[test]
    fn a_strom_next_to_us_in_a_plain_bin_dir_is_not_bundled() {
        let scratch = Scratch::new("plainbin");
        let bin = scratch.0.join("usr/local/bin");
        let beside = scratch.exe("usr/local/bin/strom");
        scratch.exe("usr/local/bin/open-live-ingest");
        assert!(!scratch.0.join("usr/local").join(BUNDLE_PLUGIN_DIR).exists());

        let got = resolve_binary("strom", Some(&bin), None, None).unwrap();
        assert_eq!(got.path, beside);
        assert_eq!(got.origin, StromOrigin::Beside);
        assert_eq!(got.reason(), "the one next to open-live-ingest");
        let env = got.gst_env(&scratch.0.join("strom-data"));
        assert!(env.is_empty(), "{env:?}");

        // A MacOS directory without the bundle's plugin directory is not a bundle either.
        let macos = scratch.0.join("Broken.app/Contents/MacOS");
        scratch.exe("Broken.app/Contents/MacOS/strom");
        let got = resolve_binary("strom", Some(&macos), None, None).unwrap();
        assert_eq!(got.origin, StromOrigin::Beside);
        assert!(got.gst_env(&scratch.0.join("strom-data")).is_empty());
    }

    #[test]
    fn only_a_bundled_strom_gets_the_bundles_gstreamer() {
        let data_dir = PathBuf::from("/data/strom-data");
        let macos = PathBuf::from("/Applications/Open Live Ingest.app/Contents/MacOS");
        let contents = PathBuf::from("/Applications/Open Live Ingest.app/Contents");

        for origin in [
            StromOrigin::Configured,
            StromOrigin::Path,
            StromOrigin::Beside,
            StromOrigin::Installed,
        ] {
            let strom = LocatedStrom {
                path: PathBuf::from("/usr/local/bin/strom"),
                origin,
            };
            assert!(strom.gst_env(&data_dir).is_empty());
        }

        let strom = LocatedStrom {
            path: macos.join("strom"),
            origin: StromOrigin::Bundled {
                contents: contents.clone(),
            },
        };
        let env: std::collections::HashMap<_, _> = strom.gst_env(&data_dir).into_iter().collect();
        let plugins = contents.join("PlugIns/gstreamer");
        let scanner = contents.join("Helpers/gst-plugin-scanner");
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

    #[cfg(windows)]
    #[test]
    fn on_windows_a_name_on_path_is_found_with_its_exe_extension() {
        let found = locate_binary("cmd").expect("cmd.exe is on PATH");
        let file = found
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert_eq!(file, "cmd.exe");
    }

    /// A relative Windows path is a path, not a name to look up on PATH.
    #[cfg(windows)]
    #[test]
    fn on_windows_a_value_with_a_backslash_is_a_path() {
        assert_eq!(
            locate_binary(r"src\main.rs")
                .expect("cargo test runs in the crate root")
                .path,
            PathBuf::from(r"src\main.rs")
        );
        let err = locate_binary(r"no-such-dir\cmd").unwrap_err().to_string();
        assert!(
            err.contains(r"no Strom executable at no-such-dir\cmd"),
            "{err}"
        );
    }

    /// The MSI folder is the last resort: after PATH, only for Strom's own name, and
    /// never reported as found on PATH. `locate_binary` passes it only on Windows;
    /// injected here, so every OS runs this.
    #[test]
    fn the_msi_folder_is_tried_only_after_path_misses() {
        let scratch = Scratch::new("msi");
        let program_files = scratch.0.join("Program Files");
        let installed = program_files.join("Strom").join("bin").join("strom.exe");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "").unwrap();
        let on_path = scratch.exe("bin/strom");
        let path_var = std::env::join_paths([scratch.0.join("bin")]).unwrap();
        let empty_dir = scratch.0.join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let got = resolve_binary(
            "strom",
            Some(&empty_dir),
            Some(&path_var),
            Some(&program_files),
        )
        .unwrap();
        assert_eq!(got.path, on_path);
        assert_eq!(got.origin, StromOrigin::Path);

        let got = resolve_binary("strom", Some(&empty_dir), None, Some(&program_files)).unwrap();
        assert_eq!(got.path, installed);
        assert_eq!(got.origin, StromOrigin::Installed);
        assert_ne!(got.reason(), "found on PATH");
        assert!(got.gst_env(&scratch.0.join("strom-data")).is_empty());

        // Another name is a deliberate choice and never resolves to the MSI's Strom.
        assert!(resolve_binary("strom-dev", Some(&empty_dir), None, Some(&program_files)).is_err());
        // Without a Program Files folder (every OS but Windows), nothing.
        assert!(resolve_binary("strom", Some(&empty_dir), None, None).is_err());
    }

    /// The Windows zip/installer layout: `strom.exe` next to `open-live-ingest.exe`.
    /// A bare `strom` there is not a command on Windows and is never picked.
    #[cfg(windows)]
    #[test]
    fn on_windows_strom_exe_next_to_us_is_found_and_a_bare_strom_is_not() {
        let scratch = Scratch::new("winbeside");
        let app = scratch.0.join("app");
        let beside = scratch.exe("app/strom");
        assert!(beside.ends_with("strom.exe"));
        for name in ["strom", "strom.exe", "STROM.EXE"] {
            let got = resolve_binary(name, Some(&app), None, None).unwrap();
            assert_eq!(got.path, app.join("strom.exe"), "{name}");
            assert_eq!(got.origin, StromOrigin::Beside, "{name}");
        }
        // The PATH lookup reports the canonical spelling too.
        let app_on_path = std::env::join_paths([&app]).unwrap();
        let got = resolve_binary("STROM.EXE", None, Some(&app_on_path), None).unwrap();
        assert_eq!(got.path, app.join("strom.exe"));
        assert_eq!(got.origin, StromOrigin::Path);

        let bare_dir = scratch.0.join("bare");
        std::fs::create_dir_all(&bare_dir).unwrap();
        std::fs::write(bare_dir.join("strom"), "").unwrap();
        let path_var = std::env::join_paths([&bare_dir]).unwrap();
        assert!(resolve_binary("strom", Some(&bare_dir), Some(&path_var), None).is_err());
    }

    /// With `PATH=C:\tools;C:\Program Files\Strom\bin` and an extensionless
    /// `C:\tools\strom`, the real `strom.exe` later on PATH is the one chosen.
    #[cfg(windows)]
    #[test]
    fn on_windows_strom_exe_later_on_path_beats_a_bare_strom_earlier() {
        let scratch = Scratch::new("winpath");
        let tools = scratch.0.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("strom"), "#!/bin/sh\n").unwrap();
        let real = scratch.exe("strom-bin/strom");
        let path_var = std::env::join_paths([tools, scratch.0.join("strom-bin")]).unwrap();
        let got = resolve_binary("strom", None, Some(&path_var), None).unwrap();
        assert_eq!(got.path, real);
        assert_eq!(got.origin, StromOrigin::Path);
    }

    #[test]
    fn a_managed_strom_gets_its_own_data_directory_unless_told_otherwise() {
        let mut cfg = Config::default();
        assert!(data_dir(&cfg).ends_with("strom-data"));
        cfg.strom.data_dir = Some("/tmp/somewhere".to_string());
        assert_eq!(data_dir(&cfg), PathBuf::from("/tmp/somewhere"));
    }

    #[test]
    fn strom_logs_into_its_data_directory() {
        let mut cfg = Config::default();
        assert!(log_path(&cfg).ends_with(Path::new("strom-data").join("strom.log")));
        cfg.strom.data_dir = Some("/tmp/somewhere".to_string());
        assert_eq!(
            log_path(&cfg),
            Path::new("/tmp/somewhere").join("strom.log")
        );
    }

    /// The value of `NAME="..."` in the script that builds the macOS app.
    fn app_script_value(name: &str) -> &'static str {
        let script = include_str!("../.github/scripts/build-macos-app.sh");
        let prefix = format!("{name}=\"");
        let line = script
            .lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("build-macos-app.sh sets no {name}"));
        let value = &line[prefix.len()..];
        &value[..value.find('"').expect("closing quote")]
    }

    #[test]
    fn the_app_script_puts_plugins_and_scanner_where_we_look() {
        assert_eq!(app_script_value("PLUGIN_DIR_REL"), BUNDLE_PLUGIN_DIR);
        assert_eq!(app_script_value("SCANNER_REL"), BUNDLE_PLUGIN_SCANNER);
    }

    /// `codesign` accepts nested code only in a bundle's code locations, and takes a
    /// directory there whose name has a dot (such as `gstreamer-1.0`) for a nested
    /// bundle, which rejects the whole app.
    #[test]
    fn the_bundle_layout_is_one_codesign_accepts() {
        for (path, location) in [
            (BUNDLE_PLUGIN_DIR, "PlugIns"),
            (BUNDLE_PLUGIN_SCANNER, "Helpers"),
        ] {
            let parts: Vec<&str> = path.split('/').collect();
            assert_eq!(parts[0], location, "{path}");
            let dirs = if path == BUNDLE_PLUGIN_DIR {
                &parts[..]
            } else {
                &parts[..parts.len() - 1]
            };
            for dir in dirs {
                assert!(!dir.contains('.'), "{path}: directory {dir:?} has a dot");
            }
        }
    }
}
