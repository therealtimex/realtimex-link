//! macOS: RealTimeX Link is a menu bar app (LSUIElement, scripts/bundle-macos.sh), in the Dock
//! only while its window is open, like other menu bar apps with a window.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Whether the window should be open: set where it is shown or hidden, applied on the main
/// thread (app.rs, logic).
static OPEN: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static APPLIED: AtomicBool = AtomicBool::new(false);
/// The first frame is yet to come (apply).
static FIRST: AtomicBool = AtomicBool::new(true);
/// The window of a --background start, until it is first opened (veil): its NSView (macOS) or
/// HWND (Windows); 0 for none.
static VEILED: AtomicUsize = AtomicUsize::new(0);

pub fn set_open(open: bool) {
    OPEN.store(open, Ordering::Relaxed);
}

/// A --background start's window (main.rs) stays off the screen. eframe shows the window once its
/// first frame is painted, whatever with_visible says (eframe 0.37, epi_integration.rs,
/// post_rendering), and the window flashed up at login (realtimex-ai-app#2836 QA): until it is
/// first opened it is see-through (macOS) or cloaked (Windows), and the first frame hides it
/// again (apply). Before the first frame, on the main thread.
pub fn veil(cc: &eframe::CreationContext<'_>) {
    if OPEN.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let raw = match cc.window_handle().map(|handle| handle.as_raw()) {
            #[cfg(target_os = "macos")]
            Ok(RawWindowHandle::AppKit(handle)) => handle.ns_view.as_ptr() as usize,
            #[cfg(windows)]
            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get() as usize,
            _ => return,
        };
        if see_through(raw, true) {
            VEILED.store(raw, Ordering::Relaxed);
            #[cfg(target_os = "macos")]
            never_key_while_veiled();
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = cc;
}

/// The window `raw` (veil) see-through or cloaked, or back to normal; false when that failed.
#[cfg(target_os = "macos")]
fn see_through(raw: usize, on: bool) -> bool {
    use objc2_app_kit::NSView;
    if objc2::MainThreadMarker::new().is_none() {
        return false;
    }
    // SAFETY: the window's own NSView (winit's, alive as long as the window, which is for this
    // process's life), used on the main thread only.
    let view = unsafe { &*(raw as *const NSView) };
    let Some(window) = view.window() else {
        return false;
    };
    window.setAlphaValue(if on { 0.0 } else { 1.0 });
    // Nor does it take a click in the moment it is on the screen.
    window.setIgnoresMouseEvents(on);
    true
}

/// macOS: eframe's first show is winit's makeKeyAndOrderFront, which would make the veiled window
/// the key window for the moment it is on the screen. winit's window class always answers yes to
/// becoming key (and main); while a window is veiled, the answer is no, so it never takes the
/// keyboard. Once opened (apply), the answer is yes again.
#[cfg(target_os = "macos")]
fn never_key_while_veiled() {
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    extern "C-unwind" fn can_become(_this: &AnyObject, _cmd: Sel) -> Bool {
        Bool::new(VEILED.load(Ordering::Relaxed) == 0)
    }
    let Some(class) = AnyClass::get(c"WinitWindow") else {
        return;
    };
    for selector in [
        objc2::sel!(canBecomeKeyWindow),
        objc2::sel!(canBecomeMainWindow),
    ] {
        if let Some(method) = class.instance_method(selector) {
            // SAFETY: the same signature as winit's methods (BOOL, no arguments), and as safe:
            // it reads an atomic. Only on the main thread, before the first frame.
            unsafe {
                let imp = std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(
                    can_become,
                );
                method.set_implementation(imp);
            }
        }
    }
}

#[cfg(windows)]
fn see_through(raw: usize, on: bool) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
    let value = i32::from(on);
    // SAFETY: the window's own HWND (winit's, alive for this process's life), and a BOOL, as
    // DWMWA_CLOAK expects (egui-winit cloaks the same way, set_maximized_while_hidden).
    let result = unsafe {
        DwmSetWindowAttribute(
            raw as _,
            DWMWA_CLOAK as u32,
            (&raw const value).cast(),
            size_of::<i32>() as u32,
        )
    };
    result >= 0
}

/// On the main thread, every frame: in the Dock (and in front) while the window is open, out of
/// it otherwise (macOS). The first frame hides a window started hidden (veil); one that is opened
/// is no longer see-through, before it is shown.
pub fn apply(ctx: &eframe::egui::Context) {
    let open = OPEN.load(Ordering::Relaxed);
    if FIRST.swap(false, Ordering::Relaxed) && !open {
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
    }
    if open {
        let raw = VEILED.swap(0, Ordering::Relaxed);
        #[cfg(any(target_os = "macos", windows))]
        if raw != 0 {
            see_through(raw, false);
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = raw;
    }
    #[cfg(target_os = "macos")]
    if APPLIED.swap(open, Ordering::Relaxed) != open {
        show_in_dock(open);
    }
}

#[cfg(target_os = "macos")]
fn show_in_dock(open: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(main) = MainThreadMarker::new() else {
        APPLIED.store(!open, Ordering::Relaxed);
        return;
    };
    let app = NSApplication::sharedApplication(main);
    app.setActivationPolicy(if open {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    });
    if open {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// macOS: the app menu says "About RealTimeX Link", "Quit RealTimeX Link": winit names its items
/// after the process, which is the executable's name (rtx-link). Before the window is made. Another
/// instance's name carries its label (instance.rs): "Quit RealTimeX Link (dev)".
pub fn name_process(name: &str) {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSProcessInfo, NSString};
        NSProcessInfo::processInfo().setProcessName(&NSString::from_str(name));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = name;
}

/// macOS: whether "Double-click a window's title bar to" is Minimize (the header's double-click,
/// app.rs). Zoom, the default, and "Do Nothing" leave the window as it is.
pub fn double_click_minimizes() -> bool {
    #[cfg(target_os = "macos")]
    {
        let answer = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleActionOnDoubleClick"])
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(answer) = answer {
            return String::from_utf8_lossy(&answer.stdout).trim() == "Minimize";
        }
    }
    false
}
