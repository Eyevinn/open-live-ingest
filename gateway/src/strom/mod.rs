//! Local Strom integration: build the flow, create it, keep it running.
//!
//! The gateway owns no media pipeline. Strom does the capture, encode, mux, and SRT
//! uplink; this module is the thin controller that templates the flow and supervises
//! its lifecycle so a venue box needs no manual work in Strom's editor.

pub mod client;
pub mod flow;
pub mod supervisor;

pub use supervisor::spawn_supervisors;
