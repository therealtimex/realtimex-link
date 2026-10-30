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
        &|name| user_env(name),
        &home,
    ))
}

/// The home of the user this daemon serves when Link's service runs it as SYSTEM
/// (realtimex-link#10): taken from the owner's SID in ProfileList (unattended.rs), never from
/// the environment, which is SYSTEM's own (SD C3). Set once, before the daemon starts.
static OWNER_HOME: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

#[cfg_attr(not(windows), allow(dead_code))]
pub fn serve_owner(home: PathBuf) {
    *OWNER_HOME.write().unwrap_or_else(|p| p.into_inner()) = Some(home);
}

fn owner_home() -> Option<PathBuf> {
    OWNER_HOME.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// A variable of the served user's environment: SYSTEM's %APPDATA% names nothing of the
/// owner's, so under the service only the owner's home counts.
fn user_env(name: &str) -> Option<String> {
    if name == "APPDATA" && owner_home().is_some() {
        return None;
    }
    std::env::var(name).ok()
}

/// The folder this daemon runs on, once it started.
pub fn data_dir() -> Option<PathBuf> {
    DATA_DIR.get().cloned()
}

/// The folder this daemon runs on, set once at start (daemon.rs).
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn remember(dir: &Path) {
    let _ = DATA_DIR.set(dir.to_path_buf());
}

/// This computer's Link: the standard folder, not a copy with a folder of its own (development,
/// tests, another RealTimeX's instance: instance.rs). Only it changes things outside its folder,
/// such as starting at login or updating itself. An explicit `--data-dir` that names the standard
/// folder (RealTimeX passes one on Linux and Windows) is the standard folder.
pub fn is_standard() -> bool {
    let standard = PathBuf::from(default_data_dir_for(
        std::env::consts::OS,
        &|name| (name != "RTX_LINK_DATA_DIR").then(|| user_env(name)).flatten(),
        &home_dir(),
    ));
    DATA_DIR
        .get()
        .is_some_and(|dir| same_folder(dir, &standard, std::env::consts::OS))
}

/// Whether `a` and `b` are the same folder: compared resolved (links followed) when both exist,
/// else as written without trailing separators. macOS and Windows file systems ignore case, so the
/// compare does too there (RealTimeX may spell "Application Support" or "AppData" differently).
pub fn same_folder(a: &Path, b: &Path, platform: &str) -> bool {
    let (a, b) = match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => (a.to_path_buf(), b.to_path_buf()),
    };
    let text = |path: &Path| {
        let text = path.to_string_lossy();
        let text = text.trim_end_matches(['/', '\\']);
        if platform == "macos" || platform == "windows" {
            text.to_lowercase()
        } else {
            text.to_string()
        }
    };
    text(&a) == text(&b)
}

/// Where RealTimeX Link is installed, as RealTimeX looks for it (server/utils/remoteHost/
/// linkdBridge.js): the .app on macOS, the folder of executables elsewhere. Linux also has a
/// per-user install (a development run puts one there: /opt needs root).
pub fn install_dirs_for(
    platform: &str,
    env: &dyn Fn(&str) -> Option<String>,
    home: &str,
) -> Vec<String> {
    match platform {
        "macos" => vec![format!("/Applications/{APP_NAME}.app")],
        "windows" => {
            let program_files = env("ProgramW6432")
                .filter(|dir| !dir.is_empty())
                .or_else(|| env("ProgramFiles").filter(|dir| !dir.is_empty()))
                .unwrap_or_else(|| "C:\\Program Files".to_string());
            vec![format!("{program_files}\\{APP_NAME}")]
        }
        _ => {
            let data_home = env("XDG_DATA_HOME")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| format!("{home}/.local/share"));
            vec![
                format!("/opt/{APP_NAME}"),
                format!("{data_home}/realtimex-link"),
            ]
        }
    }
}

/// Whether `install` (install::source(): this executable's .app or folder) is one of the
/// installed copies, not a build or a copy carried inside RealTimeX.
pub fn is_install_dir(install: &Path, dirs: &[String]) -> bool {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    dirs.iter().any(|dir| same(install, Path::new(dir)))
}

/// This executable belongs to RealTimeX Link as installed on this computer.
pub fn running_installed() -> bool {
    let dirs = install_dirs_for(
        std::env::consts::OS,
        &|name| std::env::var(name).ok(),
        &home_dir(),
    );
    crate::install::source().is_ok_and(|install| is_install_dir(&install, &dirs))
}

pub fn home_dir() -> String {
    if let Some(home) = owner_home() {
        return home.to_string_lossy().into_owned();
    }
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
    fn install_dirs_match_realtimex() {
        assert_eq!(
            install_dirs_for("macos", &env_of(&[]), "/Users/me"),
            ["/Applications/RealTimeX Link.app"]
        );
        assert_eq!(
            install_dirs_for(
                "windows",
                &env_of(&[("ProgramW6432", "D:\\Programs"), ("ProgramFiles", "C:\\x86")]),
                "C:\\Users\\me"
            ),
            ["D:\\Programs\\RealTimeX Link"]
        );
        assert_eq!(
            install_dirs_for("windows", &env_of(&[]), "C:\\Users\\me"),
            ["C:\\Program Files\\RealTimeX Link"]
        );
        assert_eq!(
            install_dirs_for("linux", &env_of(&[]), "/home/me"),
            ["/opt/RealTimeX Link", "/home/me/.local/share/realtimex-link"]
        );
    }

    #[test]
    fn only_an_installed_copy_is_the_installed_one() {
        let dirs = install_dirs_for("linux", &env_of(&[]), "/home/me");
        assert!(is_install_dir(Path::new("/opt/RealTimeX Link"), &dirs));
        assert!(is_install_dir(
            Path::new("/home/me/.local/share/realtimex-link"),
            &dirs
        ));
        assert!(!is_install_dir(
            Path::new("/home/me/src/link/target/release"),
            &dirs
        ));
        assert!(!is_install_dir(Path::new("/opt/RealTimeX Link.new"), &dirs));
        // The same folder through a link counts.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        #[cfg(unix)]
        {
            let linked = dir.path().join("linked");
            std::os::unix::fs::symlink(&real, &linked).unwrap();
            assert!(is_install_dir(&linked, &[real.display().to_string()]));
        }
    }

    #[test]
    fn the_default_folder_however_it_is_spelled() {
        let mac = "/Users/me/Library/Application Support/RealTimeX Link";
        assert!(same_folder(
            Path::new(mac),
            Path::new(&format!("{mac}/")),
            "macos"
        ));
        assert!(same_folder(
            Path::new("/Users/me/Library/application support/RealTimeX Link"),
            Path::new(mac),
            "macos"
        ));
        assert!(!same_folder(
            Path::new("/home/me/.config/Realtimex-Link"),
            Path::new("/home/me/.config/realtimex-link"),
            "linux"
        ));
        assert!(same_folder(
            Path::new("C:\\Users\\me\\appdata\\roaming\\RealTimeX Link\\"),
            Path::new("C:\\Users\\me\\AppData\\Roaming\\RealTimeX Link"),
            "windows"
        ));
        assert!(!same_folder(
            Path::new("/nowhere/a"),
            Path::new("/nowhere/b"),
            "linux"
        ));
        // A link to the default folder is the default folder.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        #[cfg(unix)]
        {
            let linked = dir.path().join("linked");
            std::os::unix::fs::symlink(&real, &linked).unwrap();
            assert!(same_folder(&linked, &real, "linux"));
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
