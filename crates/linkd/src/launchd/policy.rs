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

/// The owner's daemon, as [`watch`] sees it.
pub trait Running {
    /// Whether it exited.
    fn exited(&mut self) -> bool;
    /// Stop it, and wait for it.
    fn stop(&mut self);
}

/// Why [`watch`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watched {
    /// It exited, or the supervisor is stopping.
    Exited,
    /// Another version is in /Applications: stopped, to start the new one (Addendum 1).
    Restart,
    /// /Applications has had no RealTimeX Link for [`UNINSTALLED_AFTER`]: stopped, to turn off (M3).
    Uninstalled,
}

/// Watch the running owner's daemon (M3, Addendum 1; Review of !13): every `tick`, the bundle in
/// /Applications. Another version restarts the daemon; missing for [`UNINSTALLED_AFTER`] in a
/// row means uninstalled, while a shorter gap (an update's move) only waits. The clock,
/// the bundle and the sleep are passed in, so the lifecycle is tested with a fake daemon.
pub fn watch(
    daemon: &mut impl Running,
    version: &str,
    tick: Duration,
    mut installed: impl FnMut() -> Option<String>,
    mut now: impl FnMut() -> std::time::Instant,
    mut stopping: impl FnMut() -> bool,
    mut sleep: impl FnMut(Duration),
) -> Watched {
    let mut next = now() + tick;
    let mut missing_since: Option<std::time::Instant> = None;
    loop {
        if daemon.exited() || stopping() {
            return Watched::Exited;
        }
        let at = now();
        if at >= next {
            next = at + tick;
            match installed() {
                None => {
                    let since = *missing_since.get_or_insert(at);
                    if uninstalled(Some(at.duration_since(since))) {
                        daemon.stop();
                        return Watched::Uninstalled;
                    }
                }
                Some(bundle) => {
                    missing_since = None;
                    if restart_owner_daemon(version, Some(&bundle)) {
                        daemon.stop();
                        return Watched::Restart;
                    }
                }
            }
        }
        sleep(Duration::from_millis(500));
    }
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
/// The login window passes capture (and the display list, realtimex-link#12), input, text
/// clipboard and the live view only; another
/// user's session also images and reading copies; files and permission prompts only the
/// owner's own (pasted files sit in the owner's 0700 temporary folder).
pub fn bridge_allows(session: Session, op: &str) -> Result<(), &'static str> {
    const LOGIN_WINDOW: [&str; 13] = [
        "start", "stop", "status", "screenshot", "displays", "move", "button", "keysym", "key",
        "scroll", "paste_text", "clipboard_seq", "permissions",
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

/// What `launchctl print <job>` says about a job that was just loaded: Some(true) once it runs,
/// Some(false) when launchd has removed it (Login Items refused to spawn it: "Unable to verify
/// trusted spawn"), None while it is still starting. A starting job (`state = xpcproxy`) already
/// has a pid, and the refusal comes after that.
pub fn job_started(print: Option<&str>) -> Option<bool> {
    match print {
        None => Some(false),
        Some(text) => text.lines().any(|line| line.trim() == "state = running").then_some(true),
    }
}

/// How a job that was just loaded came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    Running,
    /// launchd removed it: Login Items refused it.
    Refused,
    /// Still not running after every check: not a success either.
    TimedOut,
}

/// Check `print` (`launchctl print <job>`) up to `checks` times, with `sleep` between, until
/// the job runs or is gone. Only a job seen running counts as started.
pub fn wait_for_start(checks: u32, mut print: impl FnMut() -> Option<String>, mut sleep: impl FnMut()) -> Startup {
    for check in 0..checks {
        match job_started(print().as_deref()) {
            Some(true) => return Startup::Running,
            Some(false) => return Startup::Refused,
            None if check + 1 < checks => sleep(),
            None => {}
        }
    }
    Startup::TimedOut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers `prints` in turn (the last one again after that), counting the sleeps.
    fn wait_over(prints: &[Option<&str>], checks: u32) -> (Startup, usize) {
        let mut answers = prints.iter().map(|print| print.map(str::to_string));
        let mut last = None;
        let mut sleeps = 0;
        let outcome = wait_for_start(
            checks,
            || {
                if let Some(answer) = answers.next() {
                    last = answer;
                }
                last.clone()
            },
            || sleeps += 1,
        );
        (outcome, sleeps)
    }

    #[test]
    fn only_a_job_seen_running_counts_as_started() {
        let starting = Some("x = {\n\tstate = xpcproxy\n\tpid = 25678\n}");
        let running = Some("x = {\n\tstate = running\n\tpid = 25678\n}");
        // Review of !13: still starting at the deadline is not a success.
        assert_eq!(wait_over(&[starting], 40), (Startup::TimedOut, 39));
        assert_eq!(wait_over(&[starting, starting, running], 40), (Startup::Running, 2));
        // QA's F1: launchd removes it after it was starting.
        assert_eq!(wait_over(&[starting, None], 40), (Startup::Refused, 1));
        assert_eq!(wait_over(&[running], 40), (Startup::Running, 0));
    }

    #[test]
    fn a_job_login_items_refused_is_gone_and_only_a_running_one_started() {
        assert_eq!(job_started(None), Some(false));
        assert_eq!(job_started(Some("gui/501/x = {\n\tstate = running\n\tpid = 647\n}")), Some(true));
        assert_eq!(job_started(Some("gui/501/x = {\n\tstate = xpcproxy\n\tpid = 25678\n}")), None);
    }

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

    /// A daemon that runs until stopped, and a clock that moves only when the watcher sleeps.
    struct Fake {
        stopped: bool,
        exits_at: Option<Duration>,
        elapsed: std::rc::Rc<std::cell::Cell<Duration>>,
    }

    impl Running for Fake {
        fn exited(&mut self) -> bool {
            self.exits_at.is_some_and(|at| self.elapsed.get() >= at)
        }
        fn stop(&mut self) {
            self.stopped = true;
        }
    }

    /// Run the watcher with `presence(elapsed)` as /Applications' version; what it returned, when,
    /// and whether it stopped the daemon.
    fn run(presence: impl Fn(Duration) -> Option<&'static str>, exits_at: Option<Duration>) -> (Watched, Duration, bool) {
        let start = std::time::Instant::now();
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let mut daemon = Fake { stopped: false, exits_at, elapsed: elapsed.clone() };
        let (a, b, c) = (elapsed.clone(), elapsed.clone(), elapsed.clone());
        let watched = watch(
            &mut daemon,
            "0.3.1",
            Duration::from_secs(15),
            move || presence(a.get()).map(str::to_string),
            move || start + b.get(),
            || false,
            move |pause| c.set(c.get() + pause),
        );
        (watched, elapsed.get(), daemon.stopped)
    }

    #[test]
    fn a_healthy_daemon_whose_app_was_removed_is_turned_off_after_ten_minutes() {
        // Review of !13: the daemon never exits, the bundle goes at 1 min and doesn't come back.
        let (watched, at, stopped) = run(|t| (t < Duration::from_secs(60)).then_some("0.3.1"), None);
        assert_eq!(watched, Watched::Uninstalled);
        assert!(stopped);
        assert!(at >= Duration::from_secs(60) + UNINSTALLED_AFTER, "{at:?}");
        assert!(at < Duration::from_secs(60) + UNINSTALLED_AFTER + Duration::from_secs(30), "{at:?}");
    }

    #[test]
    fn an_updates_short_gap_only_waits_and_a_new_version_restarts() {
        // Missing for 2 minutes (the move), back as the same version: still running at 20 min.
        let gap = |t: Duration| {
            !(Duration::from_secs(60)..Duration::from_secs(180)).contains(&t)
        };
        let (watched, at, stopped) =
            run(move |t| gap(t).then_some("0.3.1"), Some(Duration::from_secs(20 * 60)));
        assert_eq!((watched, stopped), (Watched::Exited, false));
        assert!(at >= Duration::from_secs(20 * 60));
        // Back as another version: restarted at the first tick that sees it.
        let (watched, at, stopped) = run(
            |t| match t.as_secs() {
                0..60 => Some("0.3.1"),
                60..180 => None,
                _ => Some("0.3.2"),
            },
            None,
        );
        assert_eq!((watched, stopped), (Watched::Restart, true));
        assert!(at < Duration::from_secs(200), "{at:?}");
        // Gaps that add up to more than ten minutes but are each short never count.
        let (watched, _, _) = run(
            |t| (t.as_secs() % 300 >= 120).then_some("0.3.1"),
            Some(Duration::from_secs(40 * 60)),
        );
        assert_eq!(watched, Watched::Exited);
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
        for op in [
            "start", "screenshot", "move", "button", "keysym", "key", "scroll", "paste_text",
            "peer_offer", "displays", "peer_display",
        ] {
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
        // realtimex-link#12: every session lists and switches displays.
        for session in [Session::LoginWindow, Session::Owner, Session::Other] {
            assert_eq!(bridge_allows(session, "displays"), Ok(()));
            assert_eq!(bridge_allows(session, "peer_display"), Ok(()));
        }
    }
}
