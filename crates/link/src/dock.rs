//! macOS: RealTimeX Link is a menu bar app (LSUIElement, scripts/bundle-macos.sh), in the Dock
//! only while its window is open, like other menu bar apps with a window.

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the window should be open: set where it is shown or hidden, applied on the main
/// thread (app.rs, logic).
static OPEN: AtomicBool = AtomicBool::new(false);
static APPLIED: AtomicBool = AtomicBool::new(false);

pub fn set_open(open: bool) {
    OPEN.store(open, Ordering::Relaxed);
}

/// On the main thread: in the Dock (and in front) while the window is open, out of it otherwise.
pub fn apply() {
    let open = OPEN.load(Ordering::Relaxed);
    if APPLIED.swap(open, Ordering::Relaxed) == open {
        return;
    }
    #[cfg(target_os = "macos")]
    {
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
}

/// macOS: the app menu says "About RealTimeX Link", "Quit RealTimeX Link": winit names its items
/// after the process, which is the executable's name (rtx-link). Before the window is made.
pub fn name_process() {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSProcessInfo, NSString};
        NSProcessInfo::processInfo().setProcessName(&NSString::from_str("RealTimeX Link"));
    }
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
