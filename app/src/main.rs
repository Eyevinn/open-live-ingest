//! Open Live Ingest, the desktop app (macOS and Windows).
//!
//! One window with the system webview, showing the three compiled-in files of `ui/`.
//! The page talks to the app core through the IPC dispatcher in `ipc.rs`; the window,
//! the event loop and the menu bar are `window.rs`. `--version` answers before any of
//! that exists. See docs/specs/desktop-app.md.

// A GUI executable: no console window of its own on Windows (spec, section 4).
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", windows))]
mod window;

use std::process::ExitCode;

fn main() -> ExitCode {
    if open_live_ingest_app::wants_version(std::env::args_os().skip(1)) {
        println!("{}", open_live_ingest_app::version_line());
        return ExitCode::SUCCESS;
    }
    // To app.log (spec, section 7), at the settings' log level.
    open_live_ingest_app::logs::init(&open_live_ingest::config::default_path());
    run()
}

#[cfg(any(target_os = "macos", windows))]
fn run() -> ExitCode {
    window::run()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn run() -> ExitCode {
    eprintln!(
        "The Open Live Ingest app runs on macOS and Windows. Use the open-live-ingest command here."
    );
    ExitCode::FAILURE
}
