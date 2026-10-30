//! The supervisor's and the bridge's decisions as pure functions (#11 Tests): who may turn it
//! off, when the root copy is refreshed and the owner's daemon restarted, the owner's
//! environment, and what a bridge passes in each kind of session.

use std::time::Duration;

/// Who may turn it off with no prompt (M4; consult §2): the owner, any administrator, and root.
pub fn may_disable(peer_uid: u32, owner_uid: Option<u32>, peer_is_admin: bool) -> bool {
    peer_uid == 0 || peer_is_admin || owner_uid == Some(peer_uid)
}

fn newer(candidate: &str, than: &str) -> bool {
    crate::install::version_key(candidate) > crate::install::version_key(than)
}

/// Whether the supervisor refreshes the root copy from `/Applications` (M2): only to a newer
/// version, never a downgrade, and never from a bundle that doesn't say which it is.
pub fn refresh_wanted(root_copy: Option<&str>, applications: Option<&str>) -> bool {
    match (root_copy, applications) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(root), Some(installed)) => newer(installed, root),
    }
}

/// Whether the supervisor restarts the owner's daemon (Addendum 1): the `/Applications` bundle
/// is another version than the one running, so an update's daemon restarted before the new
/// bundle was in place (launchd's KeepAlive) doesn't keep running the old one. A missing bundle
/// (an update's move) waits for the next tick.
pub fn restart_owner_daemon(running: &str, applications: Option<&str>) -> bool {
    applications.is_some_and(|installed| installed != running)
}

/// How long `/Applications/RealTimeX Link.app` may be missing before it counts as uninstalled
/// (M3): longer than an update's move.
pub const UNINSTALLED_AFTER: Duration = Duration::from_secs(10 * 60);

/// Whether a bundle missing for `missing_for` means Link was uninstalled.
pub fn uninstalled(missing_for: Option<Duration>) -> bool {
    missing_for.is_some_and(|missing| missing >= UNINSTALLED_AFTER)
}

/// The owner's daemon's restart delay after `failures` quick exits in a row (M3): 2 s doubling
/// to 60 s.
pub fn backoff(failures: u32) -> Duration {
    Duration::from_secs((2u64 << failures.min(5)).min(60))
}

/// The owner's daemon's whole environment (M3), from their passwd entry only: no root TMPDIR,
/// and nothing from the supervisor's own.
pub fn owner_environment(home: &str, name: &str) -> Vec<(&'static str, String)> {
    vec![
        ("HOME", home.to_string()),
        ("USER", name.to_string()),
        ("LOGNAME", name.to_string()),
        ("PATH", super::plist::FIXED_PATH.to_string()),
    ]
}

/// The owner's standard folder under their home (M3, M6; C5).
pub fn standard_folder(home: &str) -> std::path::PathBuf {
    std::path::Path::new(home).join("Library/Application Support/RealTimeX Link")
}

/// Whose session a bridge serves (M5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    LoginWindow,
    /// The owner's own Aqua session.
    Owner,
    /// Another user's Aqua session (Fast User Switching).
    Other,
}

impl Session {
    pub fn of(uid: u32, login_window: bool, owner_uid: u32) -> Session {
        if login_window || uid == 0 {
            Session::LoginWindow
        } else if uid == owner_uid {
            Session::Owner
        } else {
            Session::Other
        }
    }
}

/// Whether a bridge passes `op` to its helper in that session (M5): `Ok`, or the refusal code.
/// The login window passes capture, input, text clipboard and the live view only; another
/// user's session also images and reading copies; files and permission prompts only the
/// owner's own (pasted files sit in the owner's 0700 temporary folder).
pub fn bridge_allows(session: Session, op: &str) -> Result<(), &'static str> {
    const LOGIN_WINDOW: [&str; 12] = [
        "start", "stop", "status", "screenshot", "move", "button", "keysym", "key", "scroll",
        "paste_text", "clipboard_seq", "permissions",
    ];
    const OTHER_USER: [&str; 2] = ["paste_image", "clipboard_read"];
    const OWNER_ONLY: [&str; 2] = ["paste_files", "request_permission"];
    let allowed = LOGIN_WINDOW.contains(&op)
        || op.starts_with("peer_")
        || (session != Session::LoginWindow && OTHER_USER.contains(&op))
        || (session == Session::Owner && OWNER_ONLY.contains(&op));
    match (allowed, op) {
        (true, _) => Ok(()),
        (false, "paste_files") => Err(crate::unattended::FILES_NEED_OWNER_SESSION),
        (false, _) => Err("not-in-this-session"),
    }
}

/// Whether a Secure Input holder is the login window's own UI, by its executable's path (I4,
/// Addendum 6): loginwindow, or SecurityAgent (the login window's password field).
pub fn login_ui(path: &str) -> bool {
    path == "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow"
        || (path.starts_with("/System/Library/Frameworks/Security.framework/")
            && path.ends_with("/SecurityAgent.bundle/Contents/MacOS/SecurityAgent"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_login_windows_own_ui_routes_input_to_the_job() {
        assert!(login_ui("/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow"));
        assert!(login_ui(
            "/System/Library/Frameworks/Security.framework/Versions/A/MachServices/SecurityAgent.bundle/Contents/MacOS/SecurityAgent"
        ));
        // By path, not name: a copy elsewhere doesn't count.
        assert!(!login_ui("/tmp/loginwindow"));
        assert!(!login_ui("/Users/a/SecurityAgent.bundle/Contents/MacOS/SecurityAgent"));
        assert!(!login_ui("/Applications/Terminal.app/Contents/MacOS/Terminal"));
    }

    #[test]
    fn the_owner_an_admin_and_root_may_turn_it_off() {
        assert!(may_disable(501, Some(501), false));
        assert!(may_disable(502, Some(501), true));
        assert!(may_disable(0, Some(501), false));
        assert!(!may_disable(502, Some(501), false));
        assert!(!may_disable(502, None, false));
    }

    #[test]
    fn the_root_copy_only_moves_forward() {
        assert!(refresh_wanted(None, Some("0.3.1")));
        assert!(refresh_wanted(Some("0.3.1"), Some("0.3.2")));
        assert!(refresh_wanted(Some("0.3.9"), Some("0.3.10")));
        assert!(!refresh_wanted(Some("0.3.1"), Some("0.3.1")));
        assert!(!refresh_wanted(Some("0.3.2"), Some("0.3.1")));
        assert!(!refresh_wanted(Some("0.3.1"), None));
    }

    #[test]
    fn an_upgrade_while_on_restarts_the_owners_daemon() {
        assert!(restart_owner_daemon("0.3.1", Some("0.3.2")));
        // A downgrade installed by hand: restarted too; the refresh then keeps the root copy.
        assert!(restart_owner_daemon("0.3.2", Some("0.3.1")));
        assert!(!restart_owner_daemon("0.3.1", Some("0.3.1")));
        // In the middle of an update's move: wait.
        assert!(!restart_owner_daemon("0.3.1", None));
    }

    #[test]
    fn only_a_long_absence_is_an_uninstall_and_restarts_back_off() {
        assert!(!uninstalled(None));
        assert!(!uninstalled(Some(Duration::from_secs(30))));
        assert!(uninstalled(Some(UNINSTALLED_AFTER)));
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(4), Duration::from_secs(32));
        assert_eq!(backoff(9), Duration::from_secs(60));
    }

    #[test]
    fn the_owners_environment_is_only_their_passwd_entry() {
        let environment = owner_environment("/Users/rta", "rta");
        let names: Vec<&str> = environment.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["HOME", "USER", "LOGNAME", "PATH"]);
        assert_eq!(environment[0].1, "/Users/rta");
        assert_eq!(environment[3].1, "/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(
            standard_folder("/Users/rta"),
            std::path::Path::new("/Users/rta/Library/Application Support/RealTimeX Link")
        );
    }

    #[test]
    fn each_session_passes_what_m5_lists() {
        assert_eq!(Session::of(0, true, 501), Session::LoginWindow);
        assert_eq!(Session::of(0, false, 501), Session::LoginWindow);
        assert_eq!(Session::of(501, false, 501), Session::Owner);
        assert_eq!(Session::of(502, false, 501), Session::Other);
        for op in ["start", "screenshot", "move", "button", "keysym", "key", "scroll", "paste_text", "peer_offer"] {
            assert_eq!(bridge_allows(Session::LoginWindow, op), Ok(()), "{op}");
        }
        assert!(bridge_allows(Session::LoginWindow, "paste_image").is_err());
        assert!(bridge_allows(Session::LoginWindow, "clipboard_read").is_err());
        assert_eq!(bridge_allows(Session::Other, "paste_image"), Ok(()));
        assert_eq!(bridge_allows(Session::Other, "clipboard_read"), Ok(()));
        assert_eq!(
            bridge_allows(Session::Other, "paste_files"),
            Err("files-need-owner-session")
        );
        assert_eq!(
            bridge_allows(Session::LoginWindow, "paste_files"),
            Err("files-need-owner-session")
        );
        assert!(bridge_allows(Session::Other, "request_permission").is_err());
        assert_eq!(bridge_allows(Session::Owner, "paste_files"), Ok(()));
        assert_eq!(bridge_allows(Session::Owner, "request_permission"), Ok(()));
        assert!(bridge_allows(Session::Owner, "format_disk").is_err());
    }
}
