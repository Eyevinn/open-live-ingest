//! The app's log file (docs/specs/desktop-app.md, section 7).
//!
//! The library's `tracing` output at `log.level`, formatted as the CLI's but without
//! colour, goes to `app.log` in the platform's log folder: `~/Library/Logs/open-live-ingest`
//! on macOS (Console.app lists it), `%LOCALAPPDATA%\open-live-ingest\logs` on Windows.
//! The app adds one line per UI command (its name only, `ipc.rs`) and per session
//! event ([`session_event`]). IPC payloads are never logged.
//!
//! Rotation is done once, at start: an `app.log` over 10 MB becomes `app.log.1`,
//! replacing the previous copy, so a show's log survives the next launch and the two
//! together stay at about 20 MB. Own code rather than `tracing-appender`.

use open_live_ingest::config::{self, Config};
use open_live_ingest::gateway::Event;
use open_live_ingest::local_strom;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

pub const FILE_NAME: &str = "app.log";
/// The size over which `app.log` is set aside at start.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The folder the app logs to, from the environment. `None` when the variable it
/// rests on is unset or empty, rather than a relative folder.
pub fn default_dir() -> Option<PathBuf> {
    dir_from(|name| std::env::var_os(name))
}

fn dir_from(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library").join("Logs").join("open-live-ingest"))
    } else if cfg!(windows) {
        // Local, not roaming: logs do not follow the user to another machine.
        var("LOCALAPPDATA").map(|base| base.join("open-live-ingest").join("logs"))
    } else {
        // Not a target of the app; its core is built and tested here.
        Some(config::user_dir().join("logs"))
    }
}

/// `app.log` in the default folder.
pub fn default_path() -> Option<PathBuf> {
    default_dir().map(|dir| dir.join(FILE_NAME))
}

/// The copy `log` is set aside as: `app.log.1` for `app.log`.
pub fn rotated(log: &Path) -> PathBuf {
    let mut name = log.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    log.with_file_name(name)
}

/// Sets `log` aside as [`rotated`] when it is over `max` bytes, replacing the copy
/// set aside before. Returns whether it did. A missing `log` is not an error.
pub fn rotate(log: &Path, max: u64) -> io::Result<bool> {
    let len = match std::fs::metadata(log) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if len <= max {
        return Ok(false);
    }
    let old = rotated(log);
    if std::fs::rename(log, &old).is_err() {
        // A rename onto an existing file can fail on Windows (the old copy open in
        // an editor, say); removing it first is the second try.
        std::fs::remove_file(&old)?;
        std::fs::rename(log, &old)?;
    }
    Ok(true)
}

/// The log file opened for this launch.
#[derive(Debug)]
pub struct Opened {
    pub path: PathBuf,
    pub file: File,
    /// Why the old log could not be set aside, if it could not. The new lines are
    /// appended to it then, and this is worth a warning once logging runs.
    pub rotation_failed: Option<io::Error>,
}

/// Creates `dir`, rotates its `app.log` at `max` bytes, and opens it for appending.
pub fn open_in(dir: &Path, max: u64) -> io::Result<Opened> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    let rotation_failed = rotate(&path, max).err();
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(Opened {
        path,
        file,
        rotation_failed,
    })
}

/// The filter the CLI uses: `RUST_LOG` when set, `log.level` otherwise.
pub fn filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
}

/// A subscriber writing to `file` as the CLI writes to the terminal, without colour.
pub fn subscriber(file: File, filter: EnvFilter) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .finish()
}

/// Starts logging for the app over the settings at `settings_file`: to `app.log`, or
/// to stderr when it cannot be opened. Returns the log's path when there is one.
pub fn init(settings_file: &Path) -> Option<PathBuf> {
    // The settings' error is not logged: a parse error quotes the file, secrets and all.
    let settings = open_live_ingest::api::load_from(settings_file).ok();
    let level = settings
        .as_ref()
        .map_or_else(|| "info".to_string(), |s| s.log.level.clone());
    let opened = default_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no folder for the log"))
        .and_then(|dir| open_in(&dir, MAX_BYTES));
    let path = match opened {
        Ok(opened) => {
            let _ =
                tracing::subscriber::set_global_default(subscriber(opened.file, filter(&level)));
            if let Some(err) = opened.rotation_failed {
                tracing::warn!("could not set the previous log aside: {err}");
            }
            Some(opened.path)
        }
        Err(err) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter(&level))
                .with_writer(io::stderr)
                .init();
            tracing::warn!("could not open the app log, logging to stderr: {err}");
            None
        }
    };
    if let Some(path) = &path {
        tracing::info!(path = %path.display(), "app log");
    }
    match &settings {
        Some(settings) => log_strom_log(settings),
        None => tracing::warn!(
            settings = %settings_file.display(),
            "the settings file could not be read"
        ),
    }
    path
}

/// Where a Strom the app starts writes its own log: Show Strom log reveals it.
pub fn log_strom_log(settings: &Config) {
    tracing::info!(
        path = %local_strom::log_path(settings).display(),
        "Strom log, when the app starts Strom"
    );
}

/// One line for a session event. Per-tick statistics go at `debug`, so a long show
/// does not fill the log with them. The library's events carry no credential: an
/// address in them has its passphrase masked.
pub fn session_event(event: &Event) {
    match event {
        Event::Stats { .. } => tracing::debug!("session event: {event:?}"),
        _ => tracing::info!("session event: {event:?}"),
    }
}

/// Shows `path` selected in Finder (`open -R`) or Explorer (`explorer /select,`), or
/// opens its folder elsewhere. Returns once the viewer is started.
pub fn reveal(path: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut child = reveal_command(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting the file viewer")?;
    // Explorer exits 1 even when it showed the file, so the status says nothing.
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn reveal_command(path: &Path) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut cmd = std::process::Command::new("open");
        cmd.arg("-R").arg(path);
        cmd
    };
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        // One argument, `/select,"<path>"`, exactly: Explorer reads its own command
        // line, and Rust's quoting of the whole argument would hide the path from it.
        // A Windows path cannot hold a `"`.
        let mut cmd = std::process::Command::new("explorer");
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        cmd
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let cmd = {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(path.parent().unwrap_or(path));
        cmd
    };
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Core, Deliver};
    use crate::testing::{self, Scratch};
    use crate::where_to_send::WhereToSend;
    use open_live_ingest::config::UplinkMode;
    use open_live_ingest::keychain::MemoryKeychain;
    use open_live_ingest::session::{Inputs, Session};
    use serde_json::{json, Value};
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::time::Duration;

    fn write(path: &Path, bytes: usize, fill: u8) {
        std::fs::write(path, vec![fill; bytes]).unwrap();
    }

    #[test]
    fn the_rotated_copy_is_app_log_1() {
        assert_eq!(
            rotated(&Path::new("logs").join("app.log")),
            Path::new("logs").join("app.log.1")
        );
    }

    #[test]
    fn a_log_up_to_the_limit_is_kept() {
        let dir = Scratch::new("logs-small");
        let log = dir.path().join(FILE_NAME);
        write(&log, 100, b'a');
        assert!(!rotate(&log, 100).unwrap());
        assert_eq!(std::fs::read(&log).unwrap().len(), 100);
        assert!(!rotated(&log).exists());
    }

    #[test]
    fn a_missing_log_is_nothing_to_rotate() {
        let dir = Scratch::new("logs-missing");
        let log = dir.path().join(FILE_NAME);
        assert!(!rotate(&log, 100).unwrap());
        assert!(!log.exists());
        assert!(!rotated(&log).exists());
    }

    #[test]
    fn a_log_over_the_limit_replaces_the_previous_copy() {
        let dir = Scratch::new("logs-big");
        let log = dir.path().join(FILE_NAME);
        write(&rotated(&log), 50, b'o');
        write(&log, 101, b'n');
        assert!(rotate(&log, 100).unwrap());
        assert!(!log.exists());
        assert_eq!(std::fs::read(rotated(&log)).unwrap(), vec![b'n'; 101]);
    }

    #[test]
    fn the_limit_is_ten_megabytes() {
        assert_eq!(MAX_BYTES, 10 * 1024 * 1024);
        let dir = Scratch::new("logs-limit");
        let log = dir.path().join(FILE_NAME);
        let file = File::create(&log).unwrap();
        file.set_len(MAX_BYTES).unwrap();
        assert!(!rotate(&log, MAX_BYTES).unwrap());
        file.set_len(MAX_BYTES + 1).unwrap();
        drop(file);
        assert!(rotate(&log, MAX_BYTES).unwrap());
        assert_eq!(
            std::fs::metadata(rotated(&log)).unwrap().len(),
            MAX_BYTES + 1
        );
    }

    /// A launch appends to the log of the last one, unless that went over the limit;
    /// then it starts a new one and the last stays as `app.log.1`.
    #[test]
    fn each_launch_appends_and_rotates_at_start() {
        use std::io::Write;
        let dir = Scratch::new("logs-launches");
        let logs = dir.path().join("logs");
        let launch = |line: &str| {
            let mut opened = open_in(&logs, 20).unwrap();
            assert!(opened.rotation_failed.is_none());
            writeln!(opened.file, "{line}").unwrap();
            opened.path
        };
        let log = launch("first launch");
        assert_eq!(log, logs.join(FILE_NAME));
        launch("second launch");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "first launch\nsecond launch\n"
        );
        launch("third launch");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "third launch\n");
        assert_eq!(
            std::fs::read_to_string(rotated(&log)).unwrap(),
            "first launch\nsecond launch\n"
        );
    }

    #[test]
    fn the_log_folder_is_the_platforms() {
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/Users/someone")),
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\someone\AppData\Local")),
            _ => None,
        };
        let dir = dir_from(env).unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(
                dir,
                Path::new("/Users/someone/Library/Logs/open-live-ingest")
            );
        } else if cfg!(windows) {
            assert_eq!(
                dir,
                Path::new(r"C:\Users\someone\AppData\Local\open-live-ingest\logs")
            );
        } else {
            assert!(dir.ends_with(Path::new("open-live-ingest").join("logs")));
        }
        if cfg!(any(target_os = "macos", windows)) {
            assert_eq!(dir_from(|_| None), None);
            assert_eq!(dir_from(|_| Some(OsString::new())), None);
        }
    }

    // -----------------------------------------------------------------------
    // No secret reaches the log
    // -----------------------------------------------------------------------

    /// Runs `up`'s pidfile and any user-dir lookup of this test binary in a scratch
    /// directory, as tests/session.rs does: a Go live writes `gateway.pid` there.
    fn scratch_user_dir() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("oli-app-user-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            for var in ["HOME", "XDG_CONFIG_HOME", "APPDATA", "LOCALAPPDATA"] {
                std::env::set_var(var, &dir);
            }
        });
    }

    /// Strom's flow API with flows in memory; nothing ever receives the uplink.
    fn fake_strom() -> String {
        let flows = Arc::new(Mutex::new(Vec::<Value>::new()));
        testing::serve(Arc::new(move |req| {
            let mut flows = flows.lock().unwrap();
            let path = req.path.as_str();
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            match (req.method.as_str(), path, path.strip_prefix("/api/flows/")) {
                ("GET", _, _) if path.starts_with("/api/discovery/devices") => (200, json!([])),
                ("GET", "/api/flows", _) => (200, json!({ "flows": *flows })),
                ("POST", "/api/flows", _) => {
                    flows.retain(|f| f["id"] != body["id"]);
                    flows.push(body.clone());
                    (201, json!({ "flow": body }))
                }
                (method, _, Some(rest)) => {
                    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
                    let Some(at) = flows.iter().position(|f| f["id"] == id) else {
                        return (404, json!({}));
                    };
                    match (method, action) {
                        ("GET", "") => (200, json!({ "flow": flows[at] })),
                        ("POST", "start" | "stop") => {
                            flows[at]["running"] = json!(action == "start");
                            (200, json!({ "flow": flows[at] }))
                        }
                        ("DELETE", "") => {
                            flows.remove(at);
                            (200, json!({}))
                        }
                        _ => (404, json!({})),
                    }
                }
                _ => (404, json!({})),
            }
        }))
    }

    /// A self-hosted Open Live that wants `Bearer <key>`: server info with one
    /// published port, and sources.
    fn fake_open_live(key: &'static str) -> String {
        let sources = Arc::new(Mutex::new(Vec::<Value>::new()));
        testing::serve(Arc::new(move |req| {
            if req.header("authorization") != Some(&format!("Bearer {key}")) {
                return (401, json!({ "error": "unauthorized" }));
            }
            let mut sources = sources.lock().unwrap();
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let path = req.path.as_str();
            match (
                req.method.as_str(),
                path,
                path.strip_prefix("/api/v1/sources/"),
            ) {
                ("GET", "/api/v1/server-info", _) => (
                    200,
                    json!({"stromHost": "cloud.example.com", "srtPorts": [47260],
                           "srtPortState": "reserved"}),
                ),
                ("GET", "/api/v1/sources", _) => (200, Value::Array(sources.clone())),
                ("POST", "/api/v1/sources", _) => {
                    let address = body["address"].as_str().unwrap_or_default().replacen(
                        "srt://:0",
                        "srt://:47260",
                        1,
                    );
                    let source = json!({
                        "id": format!("src-{}", sources.len() + 1),
                        "name": body["name"],
                        "address": address,
                        "streamType": body["streamType"],
                        "status": body["status"],
                        "latency": body["latency"],
                    });
                    sources.push(source.clone());
                    (201, source)
                }
                ("PATCH", _, Some(id)) => match sources.iter().find(|s| s["id"] == id) {
                    Some(source) => (200, source.clone()),
                    None => (404, json!({})),
                },
                ("DELETE", _, Some(id)) => {
                    sources.retain(|s| s["id"] != id);
                    (200, json!({}))
                }
                _ => (404, json!({})),
            }
        }))
    }

    const OPEN_LIVE_KEY: &str = "OLI-SECRET-OPEN-LIVE-KEY";
    const WRONG_KEY: &str = "OLI-SECRET-WRONG-KEY";
    const PASSPHRASE: &str = "OLI-SECRET-PASSPHRASE";
    const STROM_KEY: &str = "OLI-SECRET-STROM-KEY";
    const GATEWAY_TOKEN: &str = "OLI-SECRET-GATEWAY-TOKEN";

    /// A Go live: the test pattern registered with Open Live over an encrypted
    /// uplink, up, then stopped, with every session event logged as the app logs it.
    async fn go_live(settings: Config) -> Vec<Event> {
        let (handle, mut events) = Session::start(settings, Inputs::test_pattern());
        let mut seen = Vec::new();
        let up = async {
            while let Some(event) = events.recv().await {
                session_event(&event);
                let done = matches!(
                    event,
                    Event::Input { .. } | Event::Failed { .. } | Event::Stopped
                );
                seen.push(event);
                if done {
                    return;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), up)
            .await
            .expect("the input did not come up within 30 s");
        let _ = tokio::time::timeout(Duration::from_secs(60), handle.stop())
            .await
            .expect("stop did not finish within a minute");
        while let Some(event) = events.recv().await {
            session_event(&event);
            seen.push(event);
        }
        seen
    }

    /// Spec section 7: a sign-in, a self-hosted check and a Go live against fakes,
    /// each with credentials known here, write the app's log file through the app's
    /// own subscriber at its most verbose; not one of the credentials is in it.
    #[test]
    fn no_secret_reaches_the_log_file() {
        scratch_user_dir();
        let dir = Scratch::new("logs-secrets");
        let opened = open_in(&dir.path().join("logs"), MAX_BYTES).unwrap();
        let log = opened.path.clone();
        let dispatch = tracing::Dispatch::new(subscriber(opened.file, EnvFilter::new("trace")));

        let osc = testing::FakeOsc::start(&[("venue", "https://venue.example.osaas.io")]);
        let open_live = fake_open_live(OPEN_LIVE_KEY);
        let strom = fake_strom();
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let deliver: Deliver = Arc::new(move |script| {
            let _ = tx.lock().unwrap().send(script);
        });
        let where_to_send = WhereToSend::new(
            dir.settings(),
            Arc::new(MemoryKeychain::new()),
            osc.endpoints(),
            Duration::from_secs(10),
            testing::browser(testing::SIGNED_IN),
        );
        let mut core = Core::with_where_to_send(dir.settings(), where_to_send, deliver);
        let next = || {
            let script = rx.recv_timeout(Duration::from_secs(30)).expect("a reply");
            assert!(!script.contains("\"error\""), "{script}");
            script
        };

        let mut settings = Config::default();
        settings.gateway.id = Some("gw-app-logs".to_string());
        settings.gateway.name = "Log test".to_string();
        settings.strom.url = strom;
        settings.strom.manage = false;
        settings.strom.api_key = Some(STROM_KEY.to_string());
        settings.open_live.url = Some(open_live.clone());
        settings.open_live.api_key = Some(OPEN_LIVE_KEY.to_string());
        settings.open_live.register = true;
        settings.open_live.gateway_id = Some("gw-app-logs".to_string());
        settings.open_live.gateway_token = Some(GATEWAY_TOKEN.to_string());
        settings.uplink.mode = UplinkMode::Caller;
        settings.uplink.passphrase = Some(PASSPHRASE.to_string());

        let events = tracing::dispatcher::with_default(&dispatch, || {
            // The sign-in: a code, a refresh token, an access token and a service
            // token cross the core.
            assert!(core
                .dispatch(r#"{"id": 1, "cmd": "osc_sign_in"}"#)
                .is_none());
            next();
            // The self-hosted check, refused with a wrong key, then passed.
            for (id, key) in [(2, WRONG_KEY), (3, OPEN_LIVE_KEY)] {
                let raw = json!({"id": id, "cmd": "self_hosted",
                                 "args": {"url": open_live, "api_key": key}})
                .to_string();
                assert!(core.dispatch(&raw).is_none());
                let reply = rx.recv_timeout(Duration::from_secs(30)).expect("a reply");
                assert_eq!(reply.contains("\"error\""), key == WRONG_KEY, "{reply}");
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(go_live(settings))
        });
        drop(core);
        assert!(
            events.iter().any(|e| matches!(e, Event::Registered { .. })),
            "{events:?}"
        );
        assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");

        let text = std::fs::read_to_string(&log).unwrap();
        for wanted in [
            "ui command id=1 cmd=\"osc_sign_in\"",
            "ui command id=3 cmd=\"self_hosted\"",
            "session event: Registered",
            "session event: Stopped",
        ] {
            assert!(text.contains(wanted), "no {wanted:?} in the log:\n{text}");
        }
        for secret in [
            testing::CODE,
            testing::ACCESS,
            testing::REFRESH,
            testing::RENEWED,
            testing::SERVICE_TOKEN,
            OPEN_LIVE_KEY,
            WRONG_KEY,
            PASSPHRASE,
            STROM_KEY,
            GATEWAY_TOKEN,
        ] {
            assert!(!text.contains(secret), "{secret} in the log:\n{text}");
        }
    }

    #[test]
    fn show_logs_selects_the_file_in_finder_or_explorer() {
        let log = Path::new("/logs/app.log");
        let cmd = reveal_command(log);
        let args: Vec<_> = cmd.get_args().collect();
        if cfg!(target_os = "macos") {
            assert_eq!(cmd.get_program(), "open");
            assert_eq!(args, ["-R", "/logs/app.log"]);
        } else if cfg!(windows) {
            assert_eq!(cmd.get_program(), "explorer");
        } else {
            assert_eq!(cmd.get_program(), "xdg-open");
            assert_eq!(args, ["/logs"]);
        }
    }
}
