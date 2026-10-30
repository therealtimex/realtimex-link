//! RealTimeX Link (#2753): the window and menu bar / tray icon for this computer's hosting. The
//! work is rtx-linkd's, which keeps running when the window is closed; Quit in the menu stops it.
//! Layout and copy follow the approved prototype (artifacts/realtimex-link-v3).

#![cfg_attr(windows, windows_subsystem = "windows")]

// The glibc 2.34 floor on Linux (no code to call).
use rtx_glibc_compat as _;

mod api;
mod app;
mod choices;
mod cjk;
mod clipboard;
mod desktop;
mod dock;
mod i18n;
#[cfg(target_os = "linux")]
mod icon;
mod instance;
#[cfg(target_os = "macos")]
mod job;
#[cfg(target_os = "macos")]
mod launchd;
mod theme;
mod tray;

fn main() -> eframe::Result {
    // macOS unattended access (realtimex-link#11): launchd's jobs, before anything AppKit.
    #[cfg(target_os = "macos")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().map(String::as_str) == Some("--launchd") {
            let role = args.get(1).cloned().unwrap_or_default();
            launchd::run(&role, &args[2.min(args.len())..]);
        }
    }
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
    let _ = app::ROLE.set("window");
    if let Ok(api) = api::Api::ensure() {
        // Which instance this is, for the title and the macOS process name (before the window).
        if let Ok(status) = api.get("/v1/status") {
            instance::remember(&status);
        }
        let answer = api.post(
            "/v1/window/open",
            // In the background (login, RealTimeX) it only exits when Link already runs: the
            // running window stays as it is.
            serde_json::json!({ "pid": std::process::id(), "role": "window", "background": background }),
        );
        if answer.as_ref().is_ok_and(|answer| answer["owner"] == false) {
            return Ok(());
        }
        // Linux: the window was opened on its own (the applications menu); its icon comes too.
        #[cfg(target_os = "linux")]
        if answer.is_ok_and(|answer| answer["tray"] == false) {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .arg("--background")
                    .args(api::instance_args())
                    .spawn();
            }
        }
    }
    dock::set_open(!background);
    let instance = instance::current();
    dock::name_process(&instance.short_name());
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title(instance.name())
        // The Wayland app_id and the X11 WM_CLASS: the menu entry's name (realtimex-link.desktop,
        // StartupWMClass), so the dock shows the window under Link's own icon.
        .with_app_id("realtimex-link")
        .with_visible(!background)
        // The height then follows the content (app.rs, fit_height).
        .with_inner_size([440.0, 520.0])
        .with_min_inner_size([380.0, 360.0])
        .with_icon(std::sync::Arc::new(window_icon()));
    // macOS: no title bar, like RealTimeX's window; the traffic lights sit over the header
    // (app.rs), which also moves the window.
    #[cfg(target_os = "macos")]
    let viewport = viewport
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    // macOS: winit activates the app at launch, in front of the app in use (its default), and
    // makes an unbundled one a Dock app. In the background (login, RealTimeX) Link stays a menu
    // bar app in the back: the app in use keeps the keyboard (realtimex-ai-app#2836 QA).
    #[cfg(target_os = "macos")]
    if background {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        options.event_loop_builder = Some(Box::new(|builder| {
            builder
                .with_activation_policy(ActivationPolicy::Accessory)
                .with_activate_ignoring_other_apps(false);
        }));
    }
    eframe::run_native(
        "RealTimeX Link",
        options,
        Box::new(|cc| Ok(Box::new(app::LinkApp::new(cc)))),
    )
}

/// RealTimeX Link's icon (RealTimeX's for the platform with assets/badge.svg, made by
/// scripts/icons.py). None on macOS: eframe would put it in the Dock in place of the .app's
/// icon.icns, which has macOS's rounded shape (eframe leaves the Dock alone for an empty icon).
#[cfg(target_os = "macos")]
fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData::default()
}

#[cfg(not(target_os = "macos"))]
fn window_icon() -> eframe::egui::IconData {
    #[cfg(windows)]
    let rgba = include_bytes!("../icons/window-256.rgba");
    #[cfg(not(windows))]
    let rgba = include_bytes!("../icons/window-linux-256.rgba");
    eframe::egui::IconData {
        rgba: rgba.to_vec(),
        width: 256,
        height: 256,
    }
}
