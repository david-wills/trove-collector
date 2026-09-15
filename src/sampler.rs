//! Platform sampling for the activity watcher.
//!
//! On macOS this reads, with no running helper process:
//! - the frontmost on-screen window's owning app + title, via
//!   `CGWindowListCopyWindowInfo` (CoreGraphics, thread-safe — no main-thread
//!   requirement, unlike AppKit's NSWorkspace);
//! - seconds since the last user input, via `CGEventSourceSecondsSinceLastEventType`.
//!
//! App-level tracking needs **no** special permission. Window *titles* are
//! empty until the user grants Screen Recording to this binary. Everything
//! degrades gracefully to empty strings.

/// One reading of what the user is doing right now.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Owning app of the frontmost window, e.g. "Code". Empty if unknown.
    pub app: String,
    /// Bundle identifier, when cheaply available. Empty for now.
    pub bundle_id: String,
    /// Frontmost window title. Empty without Screen Recording permission.
    pub title: String,
    /// Seconds since the last keyboard/mouse input.
    pub idle_seconds: f64,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Sample;
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    // CGWindowListOption bits.
    const ON_SCREEN_ONLY: u32 = 1; // kCGWindowListOptionOnScreenOnly
    const EXCLUDE_DESKTOP: u32 = 1 << 4; // kCGWindowListExcludeDesktopElements

    // CGEventSourceStateID::CombinedSessionState, and "any input event".
    const COMBINED_SESSION_STATE: i32 = 0;
    const ANY_INPUT_EVENT: u32 = 0xFFFF_FFFF;

    pub fn sample() -> Sample {
        // The window list is a fresh CF array every call; drain an
        // autorelease pool around it so nothing accumulates in a long-lived
        // headless process.
        objc2::rc::autoreleasepool(|_| {
            let (app, title, bundle_id) = frontmost_window();
            Sample { app, bundle_id, title, idle_seconds: idle_seconds() }
        })
    }

    pub fn screen_recording_ok() -> bool {
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    pub fn request_screen_recording() -> bool {
        unsafe { CGRequestScreenCaptureAccess() }
    }

    fn idle_seconds() -> f64 {
        let s = unsafe { CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT) };
        if s.is_finite() && s >= 0.0 {
            s
        } else {
            0.0
        }
    }

    /// The owning app + title of the frontmost normal (layer-0) window.
    fn frontmost_window() -> (String, String, String) {
        unsafe {
            let arr_ref = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
            if arr_ref.is_null() {
                return (String::new(), String::new(), String::new());
            }
            // The list is front-to-back; the first layer-0 window is frontmost.
            let arr: CFArray = CFArray::wrap_under_create_rule(arr_ref);
            for item in arr.iter() {
                let dict_ref = *item as CFDictionaryRef;
                if dict_ref.is_null() {
                    continue;
                }
                let dict = CFDictionary::<CFString, core_foundation::base::CFType>::wrap_under_get_rule(dict_ref);
                if dict_i64(&dict, "kCGWindowLayer") != Some(0) {
                    continue;
                }
                let app = dict_string(&dict, "kCGWindowOwnerName");
                if app.is_empty() {
                    continue;
                }
                let title = dict_string(&dict, "kCGWindowName");
                return (app, title, String::new());
            }
        }
        (String::new(), String::new(), String::new())
    }

    fn dict_string(dict: &CFDictionary<CFString, core_foundation::base::CFType>, key: &str) -> String {
        match dict.find(&CFString::new(key)) {
            Some(v) => v.downcast::<CFString>().map(|s| s.to_string()).unwrap_or_default(),
            None => String::new(),
        }
    }

    fn dict_i64(dict: &CFDictionary<CFString, core_foundation::base::CFType>, key: &str) -> Option<i64> {
        dict.find(&CFString::new(key))?.downcast::<CFNumber>()?.to_i64()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Sample;

    pub fn sample() -> Sample {
        Sample::default()
    }
    pub fn screen_recording_ok() -> bool {
        false
    }
    pub fn request_screen_recording() -> bool {
        false
    }
}

pub use imp::{request_screen_recording, sample, screen_recording_ok};
