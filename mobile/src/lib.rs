//! Open Live Ingest for iPhone: the Rust core and the C ABI Swift calls it through
//! (docs/specs/ios-app.md, section 8; ADR-002, section 2).
//!
//! Four hand-written `extern "C"` functions, declared in `include/oli.h`, carry the
//! desktop app's JSON message envelope: [`oli_core_new`], [`oli_core_send`],
//! [`oli_core_free`] and [`oli_string_free`]. Everything behind them is plain Rust in
//! [`core`], tested on the host. These shims are the crate's only `unsafe` code.
//!
//! - The core owns a multi-threaded Tokio runtime; [`oli_core_send`] never waits on
//!   the network. Replies and events come back through the `deliver` callback, on a
//!   core thread, as one UTF-8 JSON string that is valid only during the call.
//! - Every shim catches panics: a panic becomes the `{"event":"fatal"}` message, and
//!   nothing unwinds into Swift. A panic in a task on the core's runtime is the same
//!   event ([`core`]).
//! - A null or non-UTF-8 argument is an error (a reply with `"id": null`, or a null
//!   core from [`oli_core_new`]), never undefined behaviour.

pub mod core;
pub mod logs;
/// The fake Open Source Cloud and Open Live the tests run against. Also built with the
/// `fake-servers` feature, for `examples/fake_servers.rs`, which the iOS UI tests use
/// as a host process; never part of the app.
#[cfg(any(test, feature = "fake-servers"))]
#[doc(hidden)]
pub mod testing;

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;

/// Called on a core thread with one UTF-8 JSON message (a reply or an event). The
/// string is valid only during the call; Swift copies it.
pub type OliDeliver = extern "C" fn(ctx: *mut c_void, json: *const c_char);

/// The core, opaque to Swift.
pub struct OliCore {
    core: core::Core,
    sink: Sink,
}

// What `oli_core_send` relies on, proved by the compiler rather than said: a core is
// used from any thread Swift calls on, through shared references only.
const _: fn() = || {
    fn shared_across_threads<T: Send + Sync>() {}
    shared_across_threads::<OliCore>();
};

/// Swift's callback and its context pointer.
#[derive(Clone, Copy)]
struct Sink {
    deliver: OliDeliver,
    ctx: *mut c_void,
}

// SAFETY: `ctx` is Swift's own pointer, which the core never dereferences: it is only
// handed back to `deliver`. Swift's side of the contract (oli.h) is that `deliver`
// may be called on any thread, with that pointer, until `oli_core_free` returns.
unsafe impl Send for Sink {}
// SAFETY: as for `Send`: the pointer is only passed back, never read or written here.
unsafe impl Sync for Sink {}

impl Sink {
    fn deliver(&self, json: &str) {
        // serde_json escapes every control character, so JSON holds no NUL byte.
        let Ok(json) = CString::new(json) else {
            return;
        };
        (self.deliver)(self.ctx, json.as_ptr());
    }
}

/// A string argument, or why it cannot be read.
///
/// # Safety
/// `s` is null or points to a NUL-terminated string that stays valid for `'a`.
unsafe fn text<'a>(s: *const c_char) -> Result<&'a str, &'static str> {
    if s.is_null() {
        return Err("a null string");
    }
    // SAFETY: not null, and NUL-terminated and valid for 'a by the caller's contract.
    unsafe { CStr::from_ptr(s) }
        .to_str()
        .map_err(|_| "a string that is not UTF-8")
}

/// The reply to a message that could not be read at all.
fn unreadable(why: &str) -> String {
    serde_json::json!({
        "id": null,
        "error": {"message": format!("The app sent {why}. Restart the app."), "action": null},
    })
    .to_string()
}

/// Starts the core: the settings in `<app_support_dir>/open-live-ingest/`, the log
/// in `log_dir/app.log`, replies and events through `deliver` with `ctx`. Null on
/// failure: a null or non-UTF-8 argument, a null `deliver`, settings that cannot be
/// written on the first launch, or a runtime that cannot start.
///
/// # Safety
/// `app_support_dir` and `log_dir` are null or NUL-terminated strings valid for the
/// call. `deliver` is null or a function that may be called on any thread, with
/// `ctx`, until [`oli_core_free`] returns.
#[no_mangle]
pub unsafe extern "C" fn oli_core_new(
    app_support_dir: *const c_char,
    log_dir: *const c_char,
    deliver: Option<OliDeliver>,
    ctx: *mut c_void,
) -> *mut OliCore {
    let made = catch_unwind(AssertUnwindSafe(|| {
        let deliver = deliver?;
        // SAFETY: valid for this call by the caller's contract.
        let (dir, logs) = unsafe { (text(app_support_dir).ok()?, text(log_dir).ok()?) };
        let level = open_live_ingest::api::load_from(
            &PathBuf::from(dir)
                .join("open-live-ingest")
                .join("gateway.toml"),
        )
        .map_or_else(|_| "info".to_string(), |s| s.log.level);
        if let Err(err) = logs::init(&PathBuf::from(logs), &level) {
            // Still usable without a log; the app's Share logs then has nothing.
            eprintln!("open-live-ingest-mobile: could not open the log: {err}");
        }
        let sink = Sink { deliver, ctx };
        let to_swift: core::Deliver = Arc::new(move |json: String| sink.deliver(&json));
        match core::Core::new(core::Options::new(&PathBuf::from(dir)), to_swift) {
            Ok(core) => Some(Box::into_raw(Box::new(OliCore { core, sink }))),
            Err(err) => {
                tracing::error!("the core could not start: {err:#}");
                None
            }
        }
    }));
    match made {
        Ok(Some(core)) => core,
        Ok(None) => std::ptr::null_mut(),
        Err(_) => {
            if let Some(deliver) = deliver {
                Sink { deliver, ctx }.deliver(&core::fatal_json());
            }
            std::ptr::null_mut()
        }
    }
}

/// Hands the core one command (section 8.2). Returns at once; the reply follows
/// through `deliver`. A null core is ignored; a null or non-UTF-8 `json` gets an
/// error reply with `"id": null`.
///
/// # Safety
/// `core` is null or a pointer [`oli_core_new`] returned that has not been freed.
/// `json` is null or a NUL-terminated string valid for the call.
#[no_mangle]
pub unsafe extern "C" fn oli_core_send(core: *mut OliCore, json: *const c_char) {
    // SAFETY: null or a live core from oli_core_new, by the caller's contract. Only
    // shared references are taken; the core is Sync.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    let sent = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: valid for this call by the caller's contract.
        match unsafe { text(json) } {
            Ok(json) => core.core.send(json),
            Err(why) => {
                tracing::warn!("unreadable message: {why}");
                core.sink.deliver(&unreadable(why));
            }
        }
    }));
    if sent.is_err() {
        tracing::error!("a command panicked");
        core.sink.deliver(&core::fatal_json());
    }
}

/// Stops what is live, waiting at most four seconds for its teardown (the source is
/// kept, as at Stop), then frees the core. No delivery follows once it returns: it
/// closes delivery and waits for one still under way to return (`core::Core::shutdown`).
/// A null core is ignored.
///
/// # Safety
/// `core` is null or a pointer [`oli_core_new`] returned that has not been freed, and
/// no other call with it is running or follows. Not to be called from within
/// `deliver`: the core cannot wait on itself there, and stops without waiting.
#[no_mangle]
pub unsafe extern "C" fn oli_core_free(core: *mut OliCore) {
    if core.is_null() {
        return;
    }
    // SAFETY: from Box::into_raw in oli_core_new, not freed yet, and not used again,
    // by the caller's contract.
    let mut core = unsafe { Box::from_raw(core) };
    let sink = core.sink;
    let freed = catch_unwind(AssertUnwindSafe(move || {
        core.core.shutdown(core::SHUTDOWN_WITHIN);
        drop(core);
    }));
    if freed.is_err() {
        tracing::error!("freeing the core panicked");
        sink.deliver(&core::fatal_json());
    }
}

/// Frees a string the core returned. Reserved: nothing returns an owned string
/// today, so Swift never calls it with anything but null.
///
/// # Safety
/// `s` is null or a pointer the core returned as an owned string, not freed yet.
#[no_mangle]
pub unsafe extern "C" fn oli_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    let _ = catch_unwind(|| {
        // SAFETY: made by CString::into_raw in this crate and not freed yet, by the
        // caller's contract.
        drop(unsafe { CString::from_raw(s) });
    });
}
