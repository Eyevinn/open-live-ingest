//! Windows: a logout or shutdown, and one instance per user session.
//!
//! tao leaves `WM_QUERYENDSESSION` unhandled and calls `std::process::exit(0)` on
//! `WM_ENDSESSION`. So [`subclass`] subclasses the app's own window
//! (`SetWindowSubclass`) and, while live, answers `WM_QUERYENDSESSION` by setting a
//! shutdown block reason, running the bounded teardown, and then letting the session
//! end. Windows does not wait for a dialog here, so nothing is asked.
//!
//! [`single_instance`] is the named mutex `Local\open-live-ingest-app`: a second launch
//! brings the window of the first to the front and exits.

use std::sync::Arc;
use windows::core::HSTRING;
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, WM_NCDESTROY,
    WM_QUERYENDSESSION,
};

/// The mutex that makes the app single instance in a user session.
pub const MUTEX_NAME: &str = r"Local\open-live-ingest-app";

/// What Windows shows for the app while it holds the logout or shutdown up.
pub const BLOCK_REASON: &str = "Stopping the live feeds";

/// The subclass's id on the window ("OLI").
const SUBCLASS_ID: usize = 0x4f4c49;

/// What the subclass needs of the app: whether anything is live, and the bounded
/// teardown, which blocks until it is done or its deadline has passed.
pub trait Session: Send + Sync {
    fn is_live(&self) -> bool;
    fn end(&self);
}

impl Session for super::QuitHandle {
    fn is_live(&self) -> bool {
        !self.may_terminate()
    }
    fn end(&self) {
        self.end_session()
    }
}

/// Subclasses `hwnd` so that `WM_QUERYENDSESSION` stops `session` first. The subclass
/// removes itself when the window is destroyed.
pub fn subclass(hwnd: HWND, session: Arc<dyn Session>) -> Result<(), String> {
    let data = Box::into_raw(Box::new(session));
    // SAFETY: `data` is a leaked box the subclass procedure owns from here on; it is
    // freed in WM_NCDESTROY, the last message the window gets, or below on failure.
    let added = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, data as usize) };
    if added.as_bool() {
        Ok(())
    } else {
        // SAFETY: not handed to the window, so still ours.
        drop(unsafe { Box::from_raw(data) });
        Err("could not subclass the window".to_string())
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            // SAFETY: `data` is the box `subclass` handed over, alive until
            // WM_NCDESTROY.
            let session = unsafe { &*(data as *const Arc<dyn Session>) };
            if session.is_live() {
                tracing::info!("Windows is ending the session while live: stopping first");
                let reason = HSTRING::from(BLOCK_REASON);
                // SAFETY: `hwnd` is this thread's top-level window.
                let blocked = unsafe { ShutdownBlockReasonCreate(hwnd, &reason) }.is_ok();
                session.end();
                if blocked {
                    // SAFETY: as above.
                    let _ = unsafe { ShutdownBlockReasonDestroy(hwnd) };
                }
            }
            // TRUE: the session may end. Windows does not wait for a dialog here.
            return LRESULT(1);
        }
        WM_NCDESTROY => {
            // SAFETY: the last message the window gets: the subclass and its data go.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), id);
                drop(Box::from_raw(data as *mut Arc<dyn Session>));
            }
        }
        _ => {}
    }
    // SAFETY: the next procedure in the chain, with the message unchanged.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Makes this the one instance of the app in the user session. False when another is
/// running: its window (found by `class` and `title`) is brought to the front, and
/// this launch should exit. When the mutex cannot be made at all the app starts
/// anyway: a second window is better than none.
pub fn single_instance(class: &str, title: &str) -> bool {
    if claim(MUTEX_NAME) {
        return true;
    }
    tracing::info!("the app is already running: showing its window");
    bring_to_front(class, title);
    false
}

/// Claims the named mutex `name`. False when another process (or an earlier claim)
/// holds it already. The handle is never closed: it holds the name for the life of
/// the process.
fn claim(name: &str) -> bool {
    let name = HSTRING::from(name);
    // SAFETY: a named mutex with default security, not owned at creation.
    match unsafe { CreateMutexW(None, false, &name) } {
        Ok(_handle) => {
            // SAFETY: read right after CreateMutexW, which sets it when the mutex
            // already existed and still returns a handle to it.
            let last = unsafe { GetLastError() };
            last != ERROR_ALREADY_EXISTS
        }
        Err(e) => {
            tracing::warn!("could not make the single-instance mutex: {e}");
            true
        }
    }
}

/// Brings the window of `class` and `title` to the front, restored if minimised.
fn bring_to_front(class: &str, title: &str) {
    let (class, title) = (HSTRING::from(class), HSTRING::from(title));
    // SAFETY: plain window lookups and activation on a window of this user's session.
    unsafe {
        let Ok(hwnd) = FindWindowW(&class, &title) else {
            tracing::warn!("the running app's window was not found");
            return;
        };
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use windows::core::w;
    use windows::Win32::System::Shutdown::ShutdownBlockReasonQuery;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SendMessageW, ENDSESSION_LOGOFF, WINDOW_EX_STYLE,
        WS_OVERLAPPEDWINDOW,
    };

    /// A session that is live until it is ended, and notes the shutdown block reason
    /// the window has while it ends.
    struct FakeSession {
        hwnd: Mutex<Option<isize>>,
        live: AtomicBool,
        ended: AtomicUsize,
        reason_while_ending: Mutex<Option<String>>,
    }

    impl Session for FakeSession {
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }
        fn end(&self) {
            self.ended.fetch_add(1, Ordering::SeqCst);
            let hwnd = HWND(self.hwnd.lock().unwrap().unwrap() as *mut _);
            *self.reason_while_ending.lock().unwrap() = block_reason(hwnd);
            self.live.store(false, Ordering::SeqCst);
        }
    }

    fn block_reason(hwnd: HWND) -> Option<String> {
        let mut len = 0u32;
        // SAFETY: a size query, then a buffer of that size.
        unsafe {
            ShutdownBlockReasonQuery(hwnd, None, &mut len).ok()?;
            let mut buf = vec![0u16; len as usize];
            ShutdownBlockReasonQuery(hwnd, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len)
                .ok()?;
            Some(
                String::from_utf16_lossy(&buf)
                    .trim_end_matches('\0')
                    .to_string(),
            )
        }
    }

    /// A hidden top-level window, as the app's is to Windows.
    fn window() -> HWND {
        // SAFETY: a plain STATIC window, never shown, destroyed by the test.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Open Live Ingest test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                200,
                100,
                None,
                None,
                None,
                None,
            )
            .expect("a test window")
        }
    }

    fn query_end_session(hwnd: HWND) -> LRESULT {
        // SAFETY: sent to this thread's own window, so handled synchronously here.
        unsafe {
            SendMessageW(
                hwnd,
                WM_QUERYENDSESSION,
                Some(WPARAM(0)),
                Some(LPARAM(ENDSESSION_LOGOFF as isize)),
            )
        }
    }

    #[test]
    fn query_end_session_while_live_stops_under_a_block_reason_then_allows_it() {
        let hwnd = window();
        let session = Arc::new(FakeSession {
            hwnd: Mutex::new(Some(hwnd.0 as isize)),
            live: AtomicBool::new(true),
            ended: AtomicUsize::new(0),
            reason_while_ending: Mutex::new(None),
        });
        subclass(hwnd, session.clone()).unwrap();

        assert_eq!(query_end_session(hwnd), LRESULT(1), "the session may end");
        assert_eq!(session.ended.load(Ordering::SeqCst), 1);
        assert_eq!(
            session.reason_while_ending.lock().unwrap().as_deref(),
            Some(BLOCK_REASON)
        );
        assert_eq!(block_reason(hwnd), None, "the block reason is gone after");

        // Not live any more: allowed at once, nothing stopped again.
        assert_eq!(query_end_session(hwnd), LRESULT(1));
        assert_eq!(session.ended.load(Ordering::SeqCst), 1);

        // SAFETY: the test's own window; the subclass frees its data on WM_NCDESTROY.
        unsafe { DestroyWindow(hwnd).unwrap() };
        assert_eq!(Arc::strong_count(&session), 1, "the subclass let go of it");
    }

    #[test]
    fn a_second_claim_on_the_name_is_refused() {
        assert_eq!(MUTEX_NAME, r"Local\open-live-ingest-app");
        // A name of the test's own, so a running app does not get in the way.
        let name = format!(r"Local\open-live-ingest-app-test-{}", std::process::id());
        assert!(claim(&name), "the first claim");
        assert!(!claim(&name), "a second launch finds it taken");
    }
}
