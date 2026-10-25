//! RealTimeX itself on this computer: open it, or its download page when it is not installed.

use std::path::PathBuf;
use std::process::{Command, Stdio};

const DOWNLOAD_URL: &str = "https://realtimex.ai";

/// The installed RealTimeX, where its installers put it.
pub fn realtimex_app() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_default();
        vec![
            "/Applications/RealTimeX.AI.app".into(),
            PathBuf::from(home).join("Applications/RealTimeX.AI.app"),
        ]
    } else if cfg!(windows) {
        let mut list = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            list.push(PathBuf::from(local).join("Programs\\RealTimeX.AI\\RealTimeX.AI.exe"));
        }
        list.push("C:\\Program Files\\RealTimeX.AI\\RealTimeX.AI.exe".into());
        list
    } else {
        vec!["/opt/RealTimeX.AI/RealTimeX.AI".into()]
    };
    candidates.into_iter().find(|path| path.exists())
}

fn spawn(program: &str, args: &[&str]) {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: this window has no console, so `cmd /C start` would open one.
        command.creation_flags(0x0800_0000);
    }
    let _ = command.spawn();
}

pub fn open_url(url: &str) {
    if cfg!(target_os = "macos") {
        spawn("open", &[url]);
    } else if cfg!(windows) {
        spawn("cmd", &["/C", "start", "", url]);
    } else {
        spawn("xdg-open", &[url]);
    }
}

/// Open RealTimeX, or its download page when it is not on this computer.
pub fn open_realtimex() {
    match realtimex_app() {
        Some(app) if cfg!(target_os = "macos") => spawn("open", &[&app.to_string_lossy()]),
        Some(app) => spawn(&app.to_string_lossy(), &[]),
        None => open_url(DOWNLOAD_URL),
    }
}

/// macOS: the Privacy & Security pane for a permission.
pub fn open_privacy_settings(name: &str) {
    let pane = if name == "accessibility" {
        "Privacy_Accessibility"
    } else {
        "Privacy_ScreenCapture"
    };
    open_url(&format!(
        "x-apple.systempreferences:com.apple.preference.security?{pane}"
    ));
}
