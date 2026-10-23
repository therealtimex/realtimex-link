//! RealTimeX Link (#2753): the window and menu bar / tray icon for this computer's hosting. The
//! work is rtx-linkd's, which keeps running when the window is closed; Quit in the menu stops it.
//! Layout and copy follow the approved prototype (artifacts/realtimex-link-v3).

#![cfg_attr(windows, windows_subsystem = "windows")]

// The glibc 2.34 floor on Linux (no code to call).
use rtx_glibc_compat as _;

mod api;
mod app;
mod desktop;
#[cfg(target_os = "linux")]
mod icon;
mod tray;

fn main() -> eframe::Result {
    // --background: started at login (rtx-linkd's autostart.rs) or by RealTimeX. The menu bar /
    // tray icon and the daemon, no window until it is opened from the icon. On Linux the icon is
    // a process of its own (icon.rs).
    let background = std::env::args().any(|arg| arg == "--background");
    #[cfg(target_os = "linux")]
    if background {
        return icon::run();
    }
    // One window (with its icon, except on Linux) at a time: when RealTimeX Link already runs,
    // it shows its window and this one exits. Without the daemon (or an older one), it goes on.
    if let Ok(api) = api::Api::ensure() {
        let answer = api.post(
            "/v1/window/open",
            serde_json::json!({ "pid": std::process::id(), "role": "window" }),
        );
        if answer.as_ref().is_ok_and(|answer| answer["owner"] == false) {
            return Ok(());
        }
        // Linux: the window was opened on its own (the applications menu); its icon comes too.
        #[cfg(target_os = "linux")]
        if answer.is_ok_and(|answer| answer["tray"] == false) {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).arg("--background").spawn();
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("RealTimeX Link")
            .with_visible(!background)
            // The height then follows the content (app.rs, fit_height).
            .with_inner_size([440.0, 520.0])
            .with_min_inner_size([380.0, 360.0])
            // RealTimeX Link's icon (RealTimeX's with assets/badge.svg, made by scripts/icons.py).
            .with_icon(std::sync::Arc::new(eframe::egui::IconData {
                rgba: include_bytes!("../icons/window-256.rgba").to_vec(),
                width: 256,
                height: 256,
            })),
        ..Default::default()
    };
    eframe::run_native(
        "RealTimeX Link",
        options,
        Box::new(|cc| Ok(Box::new(app::LinkApp::new(cc)))),
    )
}
