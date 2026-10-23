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
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

/// Copy with the desktop's own tool (wl-copy on Wayland, xclip on X11); without one, the window
/// opens instead, where the App ID and password can be copied.
fn copy(text: String) {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let (program, args): (&str, &[&str]) = if wayland {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
        Err(_) => open_window(),
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
        tray.update(lock(&shared).overview.as_ref());
        std::thread::sleep(Duration::from_millis(500));
    }
}
