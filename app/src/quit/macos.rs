//! macOS: Dock → Quit, a logout and a restart reach the app as
//! `-[NSApplication terminate:]`, which asks the application delegate's
//! `applicationShouldTerminate:`. tao 0.37's delegate class implements none (ADR-001),
//! so [`install`] adds one to it at start with the Objective-C runtime
//! (`class_addMethod`). It answers `NSTerminateNow` when the app may end and otherwise
//! `NSTerminateCancel`, asking the quit question instead; on a logout macOS then says
//! that Open Live Ingest cancelled it. If the method cannot be added, the window runs
//! the bounded teardown on `applicationWillTerminate:` (tao's `LoopDestroyed`).

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send, sel};
use std::sync::{Arc, Mutex};

/// `NSApplicationTerminateReply`.
pub const NS_TERMINATE_CANCEL: usize = 0;
pub const NS_TERMINATE_NOW: usize = 1;

/// Decides whether the app may end now. When it may not, it asks the quit question
/// (by posting to the event loop: it runs on the main thread, outside the loop's
/// handler) and returns false.
pub type ShouldTerminate = Arc<dyn Fn() -> bool + Send + Sync>;

static HOOK: Mutex<Option<ShouldTerminate>> = Mutex::new(None);

/// Sets what the added method asks, and adds `applicationShouldTerminate:` to the
/// class of the application's delegate. Call on the main thread once tao has made
/// its delegate (when the event loop is created). An error names what failed.
pub fn install(hook: ShouldTerminate) -> Result<(), String> {
    *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    let class = delegate_class().ok_or("the application has no delegate")?;
    add_to(class)
}

/// Whether the application's delegate answers `applicationShouldTerminate:`.
pub fn is_installed() -> bool {
    delegate_class().is_some_and(|class| class.responds_to(sel!(applicationShouldTerminate:)))
}

/// The class of `NSApp.delegate`: tao's delegate class, found from the object rather
/// than by its name.
pub fn delegate_class() -> Option<&'static AnyClass> {
    // SAFETY: `+sharedApplication` returns the application (never nil) and
    // `-delegate` an object or nil; both are plain object getters.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let app = app.as_ref()?;
        let delegate: *mut AnyObject = msg_send![app, delegate];
        delegate.as_ref().map(|d| d.class())
    }
}

/// Adds `applicationShouldTerminate:` to `class`. Fails when the class has one of its
/// own already, which `class_addMethod` does not replace.
pub fn add_to(class: &AnyClass) -> Result<(), String> {
    let imp: unsafe extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) -> usize =
        should_terminate;
    // SAFETY: the implementation matches the type encoding: it returns an
    // NSApplicationTerminateReply (NSUInteger, `Q`) and takes self (`@`), the selector
    // (`:`) and the sender (`@`). `class_addMethod` takes the IMP as an untyped function
    // pointer, as every runtime method is called through objc_msgSend's cast.
    let added = unsafe {
        objc2::ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            sel!(applicationShouldTerminate:),
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) -> usize,
                Imp,
            >(imp),
            c"Q@:@".as_ptr(),
        )
    };
    if added.as_bool() {
        Ok(())
    } else {
        Err(format!(
            "could not add applicationShouldTerminate: to {}",
            class.name().to_string_lossy()
        ))
    }
}

/// `-applicationShouldTerminate:`. Without a hook (not set yet) the app may end, as it
/// would without the method.
unsafe extern "C-unwind" fn should_terminate(
    _this: &AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) -> usize {
    let hook = HOOK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let may = hook.is_none_or(|hook| hook());
    if may {
        NS_TERMINATE_NOW
    } else {
        tracing::info!("quit from macOS while live: asking first");
        NS_TERMINATE_CANCEL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::rc::Retained;
    use objc2::runtime::{ClassBuilder, NSObject};
    use objc2::ClassType;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// An NSObject subclass standing in for tao's delegate class, which also has no
    /// `applicationShouldTerminate:` of its own.
    fn delegate_like(name: &std::ffi::CStr) -> &'static AnyClass {
        ClassBuilder::new(name, NSObject::class())
            .expect("a fresh class name")
            .register()
    }

    #[test]
    fn the_method_is_added_and_asks_the_hook() {
        let class = delegate_like(c"OliTestDelegate");
        assert!(!class.responds_to(sel!(applicationShouldTerminate:)));
        add_to(class).unwrap();
        assert!(class.responds_to(sel!(applicationShouldTerminate:)));

        let live = Arc::new(AtomicBool::new(true));
        let asked = Arc::new(AtomicUsize::new(0));
        let (l, a) = (live.clone(), asked.clone());
        *HOOK.lock().unwrap() = Some(Arc::new(move || {
            a.fetch_add(1, Ordering::SeqCst);
            !l.load(Ordering::SeqCst)
        }));

        // SAFETY: a plain NSObject subclass; `new` returns a retained instance, and
        // the added method takes one object argument and returns an NSUInteger.
        unsafe {
            let delegate: Retained<AnyObject> = msg_send![class, new];
            let nil: *mut AnyObject = std::ptr::null_mut();
            let reply: usize = msg_send![&*delegate, applicationShouldTerminate: nil];
            assert_eq!(reply, NS_TERMINATE_CANCEL, "live: cancelled, and asked");
            live.store(false, Ordering::SeqCst);
            let reply: usize = msg_send![&*delegate, applicationShouldTerminate: nil];
            assert_eq!(reply, NS_TERMINATE_NOW, "not live: may end");
        }
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        *HOOK.lock().unwrap() = None;
    }

    /// The fallback's trigger: a class that has the method already is not changed.
    #[test]
    fn adding_it_twice_fails() {
        let class = delegate_like(c"OliTestDelegateTwice");
        add_to(class).unwrap();
        let err = add_to(class).unwrap_err();
        assert!(err.contains("OliTestDelegateTwice"), "{err}");
    }
}
