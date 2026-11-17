//! Open Live Ingest, the library.
//!
//! Streams the capture devices on a machine into Open Live. Strom does the media:
//! capture, encode, mux, SRT. This drives it over HTTP and registers the result with
//! Open Live. See docs/DESIGN.md for the reasoning.
//!
//! Used by the `open-live-ingest` command line tool (`src/main.rs`) and meant for an
//! app as well, so it makes no process-wide assumptions: it never prints, never reads
//! stdin, never calls `std::process::exit` and installs no signal handler. Progress is
//! reported as [`gateway::Event`]s; `run::up` stops when a future its caller passes
//! resolves, and [`session::Session`] runs it on a task of its own, with a handle to
//! stop it and a channel of its events. `tests/library_boundary.rs` keeps it that way.

pub mod config;
pub mod devices;
pub mod flow;
pub mod gateway;
pub mod heartbeat;
pub mod local_strom;
pub mod openlive;
pub mod osc;
pub mod run;
pub mod session;
pub mod strom;
