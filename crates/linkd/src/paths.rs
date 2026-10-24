//! Where the daemon keeps its state: the same folder RealTimeX looks in
//! (server/utils/remoteHost/paths.js), per user, shared by the standalone app and the one
//! RealTimeX installs, so there is one daemon and one App ID.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const APP_NAME: &str = "RealTimeX Link";

/// Application Support on macOS, %APPDATA% on Windows, $XDG_CONFIG_HOME (~/.config) on Linux.
/// RTX_LINK_DATA_DIR overrides it (tests, a second development instance).
pub fn default_data_dir_for(
    platform: &str,
    env: &dyn Fn(&str) -> Option<String>,
    home: &str,
) -> String {
    if let Some(dir) = env("RTX_LINK_DATA_DIR").filter(|dir| !dir.is_empty()) {
        return dir;
    }
    match platform {
        "macos" => format!("{home}/Library/Application Support/{APP_NAME}"),
        "windows" => {
            let roaming = env("APPDATA")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| format!("{home}\\AppData\\Roaming"));
            format!("{roaming}\\{APP_NAME}")
        }
        _ => {
            let config = env("XDG_CONFIG_HOME")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| format!("{home}/.config"));
            format!("{config}/realtimex-link")
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    let home = home_dir();
    PathBuf::from(default_data_dir_for(
        std::env::consts::OS,
        &|name| std::env::var(name).ok(),
        &home,
    ))
}

/// The folder this daemon runs on, set once at start (daemon.rs).
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn remember(dir: &Path) {
    let _ = DATA_DIR.set(dir.to_path_buf());
}

/// This computer's Link: the standard folder, not a copy with a folder of its own (development,
/// tests). Only it changes things outside its folder, such as starting at login.
pub fn is_standard() -> bool {
    let standard = PathBuf::from(default_data_dir_for(
        std::env::consts::OS,
        &|name| {
            (name != "RTX_LINK_DATA_DIR")
                .then(|| std::env::var(name).ok())
                .flatten()
        },
        &home_dir(),
    ));
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    DATA_DIR.get().is_some_and(|dir| same(dir, &standard))
}

pub fn home_dir() -> String {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(name).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn matches_the_javascript_daemon() {
        assert_eq!(
            default_data_dir_for("macos", &env_of(&[]), "/Users/me"),
            "/Users/me/Library/Application Support/RealTimeX Link"
        );
        assert_eq!(
            default_data_dir_for(
                "windows",
                &env_of(&[("APPDATA", "C:\\Users\\me\\AppData\\Roaming")]),
                "C:\\Users\\me"
            ),
            "C:\\Users\\me\\AppData\\Roaming\\RealTimeX Link"
        );
        assert_eq!(
            default_data_dir_for("linux", &env_of(&[]), "/home/me"),
            "/home/me/.config/realtimex-link"
        );
        assert_eq!(
            default_data_dir_for("linux", &env_of(&[("XDG_CONFIG_HOME", "/x")]), "/home/me"),
            "/x/realtimex-link"
        );
        assert_eq!(
            default_data_dir_for(
                "linux",
                &env_of(&[("RTX_LINK_DATA_DIR", "/tmp/h")]),
                "/home/me"
            ),
            "/tmp/h"
        );
    }
}
