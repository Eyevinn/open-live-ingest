//! Camera and microphone access on macOS (docs/specs/desktop-app.md, section 4).
//!
//! Launched from Finder the app is the responsible process for the Strom it starts, so
//! macOS asks on behalf of "Open Live Ingest". Before Go live the app asks AVFoundation
//! for its authorisation status (`+[AVCaptureDevice authorizationStatusForMediaType:]`)
//! and requests access when it is not determined yet, so the prompt comes at a
//! predictable moment and a denial is section 10's message rather than a Strom error.
//! Elsewhere there is nothing to ask: access is granted.

use crate::messages::Media;

/// `AVAuthorizationStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

impl Status {
    /// From the `NSInteger` AVFoundation returns. An unknown value is taken as not
    /// allowed: going live would fail later anyway, and this way it says why.
    pub fn from_raw(raw: isize) -> Self {
        match raw {
            0 => Status::NotDetermined,
            1 => Status::Restricted,
            3 => Status::Authorized,
            _ => Status::Denied,
        }
    }
}

/// What has to be allowed for this Go live: the camera when any camera is ticked, the
/// microphone when any audio input is paired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Needs {
    pub camera: bool,
    pub microphone: bool,
}

/// Checks access to what `needs` names, asking `ask` for each medium's status and
/// `request` for access when it is not determined yet. The first medium not allowed
/// is returned. Pure apart from the two callbacks, so the order and the verdicts are
/// tested on every OS.
pub fn check_with(
    needs: Needs,
    status: impl Fn(Media) -> Status,
    request: impl Fn(Media) -> bool,
) -> Result<(), Media> {
    let wanted = [
        (needs.camera, Media::Camera),
        (needs.microphone, Media::Microphone),
    ];
    for media in wanted.into_iter().filter(|(on, _)| *on).map(|(_, m)| m) {
        let allowed = match status(media) {
            Status::Authorized => true,
            Status::NotDetermined => request(media),
            Status::Denied | Status::Restricted => false,
        };
        if !allowed {
            return Err(media);
        }
    }
    Ok(())
}

/// Checks, and where needed requests, access on this machine. Blocks while macOS shows
/// its prompt. Always allowed outside macOS.
pub fn check(needs: Needs) -> Result<(), Media> {
    #[cfg(target_os = "macos")]
    {
        check_with(needs, macos::status, macos::request)
    }
    #[cfg(not(target_os = "macos"))]
    {
        check_with(needs, |_| Status::Authorized, |_| true)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Status;
    use crate::messages::Media;
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use std::time::Duration;

    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        static AVMediaTypeVideo: &'static AnyObject;
        static AVMediaTypeAudio: &'static AnyObject;
    }

    /// How long the macOS prompt may stay unanswered before Go live gives up on it.
    const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

    fn media_type(media: Media) -> &'static AnyObject {
        // SAFETY: AVFoundation's exported NSString constants, linked above.
        unsafe {
            match media {
                Media::Camera => AVMediaTypeVideo,
                Media::Microphone => AVMediaTypeAudio,
            }
        }
    }

    fn capture_device() -> Option<&'static AnyClass> {
        AnyClass::get(c"AVCaptureDevice")
    }

    pub fn status(media: Media) -> Status {
        let Some(class) = capture_device() else {
            tracing::warn!("AVCaptureDevice is not available; taking access as denied");
            return Status::Denied;
        };
        // SAFETY: `+authorizationStatusForMediaType:` takes an AVMediaType (NSString)
        // and returns an AVAuthorizationStatus (NSInteger).
        let raw: isize =
            unsafe { msg_send![class, authorizationStatusForMediaType: media_type(media)] };
        Status::from_raw(raw)
    }

    pub fn request(media: Media) -> bool {
        let Some(class) = capture_device() else {
            return false;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let handler = RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        // SAFETY: `+requestAccessForMediaType:completionHandler:` takes an AVMediaType
        // and a `void (^)(BOOL granted)` block, which it copies and calls once, on an
        // arbitrary queue.
        unsafe {
            let _: () = msg_send![
                class,
                requestAccessForMediaType: media_type(media),
                completionHandler: &*handler
            ];
        }
        rx.recv_timeout(PROMPT_TIMEOUT).unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The status call itself, on a real Mac: it answers without a prompt. What
        /// it answers depends on the machine, so only that it answers is checked.
        #[test]
        fn avfoundation_answers_the_status_of_both_media() {
            assert!(capture_device().is_some(), "AVFoundation is linked");
            for media in [Media::Camera, Media::Microphone] {
                let _ = status(media);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn statuses_read_as_avfoundation_numbers_them() {
        assert_eq!(Status::from_raw(0), Status::NotDetermined);
        assert_eq!(Status::from_raw(1), Status::Restricted);
        assert_eq!(Status::from_raw(2), Status::Denied);
        assert_eq!(Status::from_raw(3), Status::Authorized);
        assert_eq!(Status::from_raw(7), Status::Denied);
    }

    #[test]
    fn nothing_is_asked_for_what_is_not_needed() {
        let asked = RefCell::new(Vec::new());
        let result = check_with(
            Needs {
                camera: true,
                microphone: false,
            },
            |m| {
                asked.borrow_mut().push(m);
                Status::Authorized
            },
            |_| panic!("no request"),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(*asked.borrow(), vec![Media::Camera]);
    }

    #[test]
    fn a_denied_camera_or_microphone_is_named() {
        let denied = |which: Media| {
            move |m: Media| {
                if m == which {
                    Status::Denied
                } else {
                    Status::Authorized
                }
            }
        };
        let both = Needs {
            camera: true,
            microphone: true,
        };
        assert_eq!(
            check_with(both, denied(Media::Camera), |_| true),
            Err(Media::Camera)
        );
        assert_eq!(
            check_with(both, denied(Media::Microphone), |_| true),
            Err(Media::Microphone)
        );
        assert_eq!(
            check_with(both, |_| Status::Restricted, |_| true),
            Err(Media::Camera)
        );
    }

    /// Not determined yet: the app asks, and the answer decides.
    #[test]
    fn access_not_determined_yet_is_requested() {
        let requested = RefCell::new(Vec::new());
        let needs = Needs {
            camera: true,
            microphone: true,
        };
        let granted = check_with(
            needs,
            |_| Status::NotDetermined,
            |m| {
                requested.borrow_mut().push(m);
                true
            },
        );
        assert_eq!(granted, Ok(()));
        assert_eq!(*requested.borrow(), vec![Media::Camera, Media::Microphone]);
        let refused = check_with(needs, |_| Status::NotDetermined, |m| m == Media::Camera);
        assert_eq!(refused, Err(Media::Microphone));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn elsewhere_access_is_granted() {
        assert_eq!(
            check(Needs {
                camera: true,
                microphone: true
            }),
            Ok(())
        );
    }
}
