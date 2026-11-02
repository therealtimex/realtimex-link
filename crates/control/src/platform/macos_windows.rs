//! Windows on macOS (realtimex-ai-app#3055): the list from ScreenCaptureKit (normal windows,
//! layer 0), the focused one and focusing from the Accessibility API, which Remote Control's
//! input already needs granted.
//!
//! Focusing activates the app (NSRunningApplication), raises the window (AXRaise, after
//! un-minimizing it) and then checks that it is the focused window within FOCUS_CHECK.

use std::ffi::{c_char, c_void, CStr};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::platform::{refused, Listed};
use crate::winid::WindowId;

type CFTypeRef = *const c_void;

/// How long a focused window has to become the focused one.
const FOCUS_CHECK: Duration = Duration::from_millis(500);
const AX_SUCCESS: i32 = 0;
const UTF8: u32 = 0x0800_0100;
/// NSApplicationActivateIgnoringOtherApps.
const ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFTypeRef) -> i32;
    /// The CGWindowID of a window element (HIServices; not in the headers, used widely).
    fn _AXUIElementGetWindow(element: CFTypeRef, window: *mut u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    fn CFRelease(object: CFTypeRef);
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    fn CFStringCreateWithCString(
        allocator: CFTypeRef,
        text: *const c_char,
        encoding: u32,
    ) -> CFTypeRef;
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> bool;
}

/// A CoreFoundation object this code owns: released when dropped.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an object this code created or copied, released once.
            unsafe { CFRelease(self.0) };
        }
    }
}

fn string(text: &CStr) -> Owned {
    // SAFETY: a NUL-terminated string; the result is owned.
    Owned(unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) })
}

/// An attribute's value, when the element has one.
fn attribute(element: CFTypeRef, name: &CStr) -> Option<Owned> {
    let key = string(name);
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: a valid element and key; a copied value is owned.
    let error = unsafe { AXUIElementCopyAttributeValue(element, key.0, &mut value) };
    (error == AX_SUCCESS && !value.is_null()).then_some(Owned(value))
}

fn set_bool(element: CFTypeRef, name: &CStr, on: bool) -> bool {
    let key = string(name);
    // SAFETY: the CFBoolean constants live for the process.
    let value = unsafe {
        if on {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    };
    // SAFETY: a valid element, key and value.
    unsafe { AXUIElementSetAttributeValue(element, key.0, value) == AX_SUCCESS }
}

fn is_true(value: &Owned) -> bool {
    // SAFETY: two CF objects.
    unsafe { CFEqual(value.0, kCFBooleanTrue) }
}

fn window_number(element: CFTypeRef) -> Option<u32> {
    let mut id = 0u32;
    // SAFETY: a valid element and an out pointer.
    (unsafe { _AXUIElementGetWindow(element, &mut id) } == AX_SUCCESS).then_some(id)
}

/// The focused window of the focused app, by its CGWindowID; None without Accessibility.
fn focused_window() -> Option<u32> {
    // SAFETY: the system-wide element is owned.
    let system = Owned(unsafe { AXUIElementCreateSystemWide() });
    let app = attribute(system.0, c"AXFocusedApplication")?;
    let window = attribute(app.0, c"AXFocusedWindow")?;
    window_number(window.0)
}

/// Normal app windows (layer 0), front to back as ScreenCaptureKit lists them.
pub fn list_windows() -> Result<Vec<Listed>> {
    if !rtx_screens::macos::supported() {
        return Err(refused(
            "unsupported",
            "Listing windows needs macOS 14 or later",
        ));
    }
    let windows = super::capture_bounded(rtx_screens::macos::list_windows)?;
    let focused = focused_window();
    Ok(windows
        .into_iter()
        .filter(|window| !window.floating)
        .map(|info| Listed {
            id: WindowId::Mac(info.id),
            focused: focused == Some(info.id),
            info,
        })
        .collect())
}

/// Bring `window` to the front and check that it is focused.
pub fn focus_window(window: &Listed) -> Result<()> {
    let WindowId::Mac(id) = window.id else {
        return Err(refused("window-not-found", "Not a window of this computer"));
    };
    // SAFETY: a plain check.
    if !unsafe { AXIsProcessTrusted() } {
        return Err(refused(
            "permission-missing",
            "Focusing a window needs Accessibility for RealTimeX Link",
        ));
    }
    let pid = window.info.pid;
    // SAFETY: the application element is owned.
    let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
    let target = attribute(app.0, c"AXWindows").and_then(|list| {
        // SAFETY: AXWindows is a CFArray of window elements, alive while `list` is.
        let count = unsafe { CFArrayGetCount(list.0) };
        (0..count)
            .map(|index| unsafe { CFArrayGetValueAtIndex(list.0, index) })
            .find(|element| window_number(*element) == Some(id))
            .map(|element| {
                // Kept past the list: the next calls use it after `list` is gone.
                unsafe { CFRetain(element) };
                Owned(element)
            })
    });
    let Some(target) = target else {
        // Listed, but not among the app's Accessibility windows: on another Space, typically.
        return Err(refused(
            "focus-refused",
            "macOS did not offer this window to focus (it may be on another Space)",
        ));
    };
    if attribute(target.0, c"AXMinimized").is_some_and(|value| is_true(&value)) {
        set_bool(target.0, c"AXMinimized", false);
    }
    activate(pid);
    set_bool(app.0, c"AXFrontmost", true);
    let raise = string(c"AXRaise");
    // SAFETY: a valid element and action name.
    unsafe { AXUIElementPerformAction(target.0, raise.0) };
    set_bool(target.0, c"AXMain", true);
    let start = Instant::now();
    loop {
        if focused_window() == Some(id) {
            return Ok(());
        }
        if start.elapsed() >= FOCUS_CHECK {
            return Err(refused(
                "focus-refused",
                "macOS did not bring the window to the front",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(object: CFTypeRef) -> CFTypeRef;
}

/// Activate the app `pid` (NSRunningApplication), over whichever is in front.
fn activate(pid: i32) {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    let Some(class) = AnyClass::get(c"NSRunningApplication") else {
        return;
    };
    // SAFETY: a class method returning an autoreleased app or nil, then one message to it.
    unsafe {
        let app: *mut AnyObject = msg_send![class, runningApplicationWithProcessIdentifier: pid];
        if !app.is_null() {
            let _: Bool = msg_send![app, activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS];
        }
    }
}
