//! Open Live control-plane integration.
//!
//! Everything here is best-effort. Registration failures are logged and retried; they
//! never touch the media plane, because the venue must not go off air over a failed
//! REST call (docs/DESIGN.md §1).

pub mod client;
pub mod registration;
pub mod token;

pub use registration::spawn_registration;
