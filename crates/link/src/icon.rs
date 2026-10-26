//! Linux: RealTimeX Link's tray icon on its own (`rtx-link --background`), with no window. A window
//! hidden on Wayland can never be shown again, so here the icon is one process and each window
//! another: Open starts `rtx-link` (one window at a time), and closing that window ends only it.

use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::app::{lock, poll_loop, Snapshot};
use crate::tray::{Actions, Tray};

/// Start the window process (it keeps one window open at a time itself).
pub fn open_window() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let child = Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // Collected when it closes, so no finished window lingers as a zombie.
    if let Ok(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Copy from this long-lived process (clipboard.rs keeps serving the text to whoever pastes it):
/// Wayland's data-control protocol where the compositor has it, else X11 (on GNOME's Wayland
/// through Xwayland, whose clipboard it shares). Failing that, the desktop's own tool; without
/// one, the window opens instead, where the App ID and password can be copied.
fn copy(text: String) {
    if crate::clipboard::copy(&text).is_ok() || copy_with_tool(&text) {
        return;
    }
    open_window();
}

/// wl-copy on Wayland, xclip on X11: each forks to keep serving the text once we have closed its
/// input, so the copy holds when the command returns. Whether it did, by its exit status.
fn copy_with_tool(text: &str) -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let (program, args) = tool(wayland);
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        // Dropped at the end of this block: the tool reads to the end before it forks.
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().is_ok_and(|status| status.success())
}

fn tool(wayland: bool) -> (&'static str, &'static [&'static str]) {
    if wayland {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    }
}

/// Run the icon until Quit (or until another icon is already running).
pub fn run() -> eframe::Result {
    let _ = crate::app::ROLE.set("tray");
    if let Ok(api) = crate::api::Api::ensure() {
        if !crate::app::register(&api) {
            return Ok(());
        }
    }
    let shared = Arc::new(Mutex::new(Snapshot::default()));
    let (refresh, wake) = channel();
    // No window draws here: the context only carries what the shared code expects.
    let ctx = egui::Context::default();
    {
        let (shared, ctx) = (shared.clone(), ctx.clone());
        std::thread::spawn(move || poll_loop(shared, wake, ctx));
    }
    let actions = Actions {
        open: Box::new(open_window),
        copy: Box::new(copy),
    };
    let Ok(tray) = Tray::new(shared.clone(), refresh, ctx, actions) else {
        // No tray on this desktop: nothing to show; the daemon keeps hosting.
        return Ok(());
    };
    loop {
        let state = lock(&shared);
        tray.update(state.overview.as_ref(), state.update.as_ref());
        drop(state);
        std::thread::sleep(Duration::from_millis(500));
    }
}
