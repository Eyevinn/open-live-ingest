//! Shared types for the Open Live Gateway.
//!
//! This crate must not depend on GStreamer, the gateway binary, or any other internal
//! crate — only pure utility crates such as `serde`. Anything API-visible or shared
//! between the media plane, the control API, and the Open Live client belongs here.

pub mod config;
pub mod status;

pub use config::GatewayConfig;
pub use status::GatewayStatus;
