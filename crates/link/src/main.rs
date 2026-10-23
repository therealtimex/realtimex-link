//! RealTimeX Link (#2753): the window and menu bar / tray icon for this computer's hosting. The
//! work is rtx-linkd's, which keeps running when the window is closed; Quit in the menu stops it.
//! Layout and copy follow the approved prototype (artifacts/realtimex-link-v3).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod app;
mod desktop;
mod tray;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("RealTimeX Link")
            .with_inner_size([440.0, 720.0])
            .with_min_inner_size([380.0, 480.0])
            // RealTimeX Link's icon (assets/icon.svg, made by scripts/icons.py).
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
