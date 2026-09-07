//! Open Live Gateway core.
//!
//! Shared by both front ends: the headless daemon, which takes its inputs from a
//! config file and runs unattended under systemd, and the desktop app, which lets an
//! operator pick a capture device and streams it for as long as the window is open.
//!
//! Everything media-related belongs to Strom; this crate templates flows, supervises
//! them, and registers the result with Open Live. See docs/DESIGN.md.

pub mod config;
pub mod control;
pub mod identity;
pub mod openlive;
pub mod session;
pub mod state;
pub mod strom;
