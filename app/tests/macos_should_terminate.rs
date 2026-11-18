//! macOS: once the app's event loop has started, tao's application delegate answers
//! `applicationShouldTerminate:` (docs/specs/desktop-app.md, section 8), so Dock → Quit
//! and a logout while live ask first. tao 0.37 implements no such method, so this
//! fails if the app stops adding it, or if a tao upgrade starts implementing one of
//! its own (which `class_addMethod` would not replace).
//!
//! No test harness: AppKit's event loop must run on the main thread, which libtest
//! keeps for itself. Elsewhere this test has nothing to check.

#[cfg(target_os = "macos")]
fn main() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use open_live_ingest_app::quit::macos;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};

    const NAME: &str = "the_delegate_answers_application_should_terminate_after_start";

    fn fail(why: &str) -> ! {
        println!("test {NAME} ... FAILED\n{why}");
        std::process::exit(1)
    }

    /// Asks the delegate as `-[NSApplication terminate:]` does.
    fn ask_delegate() -> usize {
        // SAFETY: the application and its delegate, as AppKit holds them; the method
        // takes the sender and returns an NSApplicationTerminateReply (NSUInteger).
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let delegate: *mut AnyObject = msg_send![app, delegate];
            let delegate = delegate.as_ref().unwrap_or_else(|| fail("no delegate"));
            msg_send![delegate, applicationShouldTerminate: app]
        }
    }

    // AppKit that never finishes launching fails the test rather than the CI job's
    // time limit.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        fail("the event loop did not start within 120 s");
    });

    let event_loop = EventLoopBuilder::<()>::with_user_event().build();
    if macos::is_installed() {
        fail("tao's delegate already has an applicationShouldTerminate: of its own");
    }
    let live = Arc::new(AtomicBool::new(true));
    println!("running 1 test");
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::NewEvents(StartCause::Init) = event {
            // What the window does at start.
            let hook_live = live.clone();
            if let Err(e) = macos::install(Arc::new(move || !hook_live.load(Ordering::SeqCst))) {
                fail(&e);
            }
            if !macos::is_installed() {
                fail("the delegate does not answer applicationShouldTerminate:");
            }
            if ask_delegate() != macos::NS_TERMINATE_CANCEL {
                fail("live, but macOS was not told to cancel");
            }
            live.store(false, Ordering::SeqCst);
            if ask_delegate() != macos::NS_TERMINATE_NOW {
                fail("not live, but macOS was not told to terminate");
            }
            println!("test {NAME} ... ok\n\ntest result: ok. 1 passed; 0 failed");
            std::process::exit(0);
        }
    })
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only: skipped");
}
