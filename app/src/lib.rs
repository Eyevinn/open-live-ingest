//! Open Live Ingest, the desktop app's core.
//!
//! Everything here is plain Rust with no window and no webview, so it is unit tested
//! with `cargo test` on any OS: the custom protocol that serves the page, its
//! Content-Security-Policy, the navigation filter, the IPC dispatcher, the models
//! behind the window's steps (`where_to_send`, `what_to_send`), and the log file
//! (`logs`). The window itself (tao, wry, muda) is `src/main.rs`. See
//! docs/specs/desktop-app.md and
//! docs/decisions/ADR-001-desktop-app-toolkit.md.

pub mod ipc;
pub mod logs;
pub mod navigation;
pub mod protocol;
#[cfg(test)]
mod testing;
pub mod what_to_send;
pub mod where_to_send;

/// The version both the app and the CLI of one release report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether the command line asks for the version only. Answered before any window or
/// webview exists, so it works on a headless machine and in a packaging check. Any
/// other argument is ignored: the app is started from Finder or the Start menu, which
/// may pass arguments of their own.
pub fn wants_version<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    args.into_iter()
        .any(|a| a.as_ref() == "--version" || a.as_ref() == "-V")
}

/// What `--version` prints, in the CLI's form (`<name> <version>`).
pub fn version_line() -> String {
    format!("open-live-ingest-app {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_asked_for_with_either_flag() {
        assert!(wants_version(["--version"]));
        assert!(wants_version(["-V"]));
        assert!(wants_version(["-psn_0_12345", "--version"]));
    }

    #[test]
    fn anything_else_opens_the_window() {
        assert!(!wants_version(Vec::<String>::new()));
        assert!(!wants_version(["-psn_0_12345"]));
        assert!(!wants_version(["--versions", "version", "-v"]));
    }

    #[test]
    fn the_version_line_names_the_app_and_the_workspace_version() {
        assert_eq!(
            version_line(),
            format!("open-live-ingest-app {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(!VERSION.is_empty());
    }
}
