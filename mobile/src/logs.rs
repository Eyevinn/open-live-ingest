//! The phone's log file (docs/specs/ios-app.md, sections 7.1 and 8.2).
//!
//! The library's `tracing` output at `log.level`, without colour, goes to `app.log`
//! in the log folder Swift names (`Library/Logs` in the app's sandbox), with one
//! rotated copy, as the desktop app keeps its own (desktop spec, section 7, and
//! `app/src/logs.rs`, whose rotation this is): at start, an `app.log` over 10 MB
//! becomes `app.log.1`, replacing the previous copy.
//!
//! The core adds one line per command and per event, by name only: message bodies
//! are never logged, so `media.start`'s `uplink.passphrase`, the one secret that
//! crosses to Swift, never reaches the file.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

pub const FILE_NAME: &str = "app.log";
/// The size over which `app.log` is set aside at start.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

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
    std::fs::rename(log, rotated(log))?;
    Ok(true)
}

/// The log file opened for this launch.
#[derive(Debug)]
pub struct Opened {
    pub path: PathBuf,
    pub file: File,
    /// Why the old log could not be set aside, if it could not. The new lines are
    /// appended to it then.
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

/// A subscriber writing to `file` as the CLI writes to the terminal, without colour,
/// at `level` (`log.level`: `info` unless the settings say otherwise).
pub fn subscriber(file: File, level: &str) -> impl tracing::Subscriber + Send + Sync {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .finish()
}

/// Logs the process to `app.log` in `dir`, at `level`, once: a second call, or a
/// process that already has a global subscriber, leaves logging as it is. Returns
/// the log's path, or why it could not be opened.
pub fn init(dir: &Path, level: &str) -> io::Result<PathBuf> {
    let opened = open_in(dir, MAX_BYTES)?;
    if tracing::subscriber::set_global_default(subscriber(opened.file, level)).is_ok() {
        if let Some(err) = opened.rotation_failed {
            tracing::warn!("could not set the previous log aside: {err}");
        }
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            "open-live-ingest-mobile started"
        );
    }
    Ok(opened.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oli-mobile-logs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_log_over_the_limit_is_set_aside_once_replacing_the_old_copy() {
        let dir = scratch("rotate");
        let log = dir.join(FILE_NAME);
        let opened = open_in(&dir, 10).unwrap();
        assert_eq!(opened.path, log);
        writeln!(&opened.file, "first launch, a long line").unwrap();
        std::fs::write(rotated(&log), "an older copy").unwrap();

        let opened = open_in(&dir, 10).unwrap();
        assert!(opened.rotation_failed.is_none());
        assert_eq!(
            std::fs::read_to_string(rotated(&log)).unwrap(),
            "first launch, a long line\n"
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
        writeln!(&opened.file, "short").unwrap();

        // Under the limit: appended to, not rotated.
        let opened = open_in(&dir, 10).unwrap();
        writeln!(&opened.file, "more").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "short\nmore\n");
        assert!(!rotate(&dir.join("absent.log"), 10).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_rotated_copy_is_app_log_1() {
        assert_eq!(
            rotated(Path::new("/x/Library/Logs/app.log")),
            Path::new("/x/Library/Logs/app.log.1")
        );
    }
}
