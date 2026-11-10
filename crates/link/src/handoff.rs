//! macOS: the launchd job owns the tray (realtimex-ai-app#3241 Q5). The LaunchAgent
//! (rtx-linkd's autostart.rs) has `KeepAlive {SuccessfulExit=false}`, which only revives a
//! process launchd started. So an rtx-link started any other way (RealTimeX's `open -n -g -a
//! "RealTimeX Link.app" --args --background`, a person opening Link.app) asks launchd to run the
//! job, shows the job's window when it was a window open, and exits 0 without claiming a role.
//! Any step that fails is logged and this process carries on as before (it claims).
//! `--launchd <role>` (#11) runs before this and is unaffected.

use std::path::Path;

/// The LaunchAgent's label (autostart.rs LABEL).
pub const LABEL: &str = "ai.realtimex.link.agent";
/// The app autostart.rs names in today's LaunchAgent (ASSOCIATED_APP).
#[cfg(any(target_os = "macos", test))]
const ASSOCIATED_APP: &str = "co.realtimex.ai";

/// The program a LaunchAgent starts: the first `<string>` after the first `<array>`
/// (autostart.rs launch_agent_program).
#[cfg(any(target_os = "macos", test))]
fn agent_program(text: &str) -> Option<String> {
    let start = text.find("<array>")? + "<array>".len();
    let rest = &text[start..];
    let first = rest.find("<string>")? + "<string>".len();
    let end = rest[first..].find("</string>")? + first;
    Some(
        rest[first..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// Today's LaunchAgent: it names RealTimeX.AI and keeps the tray alive (autostart.rs
/// in_current_form). An older one is rewritten by rtx-linkd first; until then, no hand-off.
#[cfg(any(target_os = "macos", test))]
fn in_current_form(text: &str) -> bool {
    let names_realtimex = text
        .find("<key>AssociatedBundleIdentifiers</key>")
        .is_some_and(|at| {
            text[at..].split_once("<string>").is_some_and(|(_, rest)| {
                rest.starts_with(&format!("{ASSOCIATED_APP}</string>"))
            })
        });
    names_realtimex && text.contains("<key>KeepAlive</key>")
}

/// Whether this process hands its role to launchd: this computer's own Link, not the job itself
/// (launchd sets XPC_SERVICE_NAME to the job's label), and a LaunchAgent in today's form that
/// starts this very executable (a build or a copy elsewhere never hands off to the installed one).
#[cfg(any(target_os = "macos", test))]
fn hands_off(xpc_service: Option<&str>, default_instance: bool, plist: Option<&str>, me: &Path) -> bool {
    if !default_instance || xpc_service == Some(LABEL) {
        return false;
    }
    let Some(plist) = plist.filter(|text| in_current_form(text)) else {
        return false;
    };
    agent_program(plist).is_some_and(|program| {
        let program = Path::new(&program);
        match (program.canonicalize(), me.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => program == me,
        }
    })
}

/// The job's pid from `launchctl print`: its `pid = N` line, while it runs.
#[cfg(any(target_os = "macos", test))]
fn job_pid(print: &str) -> Option<u32> {
    print
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = ")?.trim().parse().ok())
        .filter(|pid| *pid > 1)
}

#[cfg(target_os = "macos")]
mod run {
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::LABEL;

    const WAIT: Duration = Duration::from_secs(10);

    fn launchctl(args: &[&str]) -> Option<Output> {
        Command::new("/bin/launchctl")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()
    }

    fn log(message: &str) {
        eprintln!("rtx-link: launchd hand-off: {message}; carrying on");
    }

    /// Hand the tray to the launchd job and exit 0, or return to carry on as before.
    pub fn run(background: bool) {
        let plist_path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"));
        let plist = std::fs::read_to_string(&plist_path).ok();
        let Ok(me) = std::env::current_exe() else { return };
        let xpc = std::env::var("XPC_SERVICE_NAME").ok();
        if !super::hands_off(xpc.as_deref(), crate::api::is_default_instance(), plist.as_deref(), &me) {
            return;
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let service = format!("{domain}/{LABEL}");
        let loaded = launchctl(&["print", &service]).is_some_and(|out| out.status.success());
        // Loaded: kickstart without -k leaves a running job alone. Not loaded: bootstrap, and
        // RunAtLoad starts it.
        let step = if loaded {
            launchctl(&["kickstart", &service])
        } else {
            launchctl(&["bootstrap", &domain, &plist_path.to_string_lossy()])
        };
        match step {
            Some(out) if out.status.success() => {}
            Some(out) => {
                let what = if loaded { "kickstart" } else { "bootstrap" };
                log(&format!(
                    "launchctl {what} failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                return;
            }
            None => return log("launchctl didn't run"),
        }
        let started = Instant::now();
        let job = loop {
            let pid = launchctl(&["print", &service])
                .filter(|out| out.status.success())
                .and_then(|out| super::job_pid(&String::from_utf8_lossy(&out.stdout)));
            match pid {
                Some(pid) => break pid,
                None if started.elapsed() > WAIT => return log("the job didn't start"),
                None => std::thread::sleep(Duration::from_millis(250)),
            }
        };
        if !background && !show_window(job, started) {
            return log("no window answered");
        }
        std::process::exit(0);
    }

    /// A window open: once the daemon answers (the job's tray starts it), the role goes to the
    /// job's pid first, so this process never claims it, then the running window is asked to
    /// show itself. Asked once more a little later, in case the job's first poll came after the
    /// first ask (a window counts only the asks after its first read).
    fn show_window(job: u32, started: Instant) -> bool {
        let me = std::process::id();
        loop {
            if let Ok(api) = crate::api::Api::running() {
                let _ = api.post(
                    "/v1/window/open",
                    json!({ "pid": job, "role": "window", "background": true }),
                );
                let ask = || {
                    api.post(
                        "/v1/window/open",
                        json!({ "pid": me, "role": "window", "background": false }),
                    )
                };
                // owner false: another process (the job's, or a tray from before) holds it, and
                // was asked to show itself.
                if ask().is_ok_and(|answer| answer["owner"] == false) {
                    std::thread::sleep(Duration::from_secs(2));
                    let _ = ask();
                    return true;
                }
                // owner true: the job is gone and this process holds the role now; it carries on.
                return false;
            }
            if started.elapsed() > WAIT {
                return false;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(target_os = "macos")]
pub use run::run;

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRAM: &str = "/Applications/RealTimeX Link.app/Contents/MacOS/rtx-link";

    fn today(program: &str) -> String {
        format!(
            r#"<dict>
  <key>Label</key>
  <string>ai.realtimex.link.agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--background</string>
  </array>
  <key>AssociatedBundleIdentifiers</key>
  <string>co.realtimex.ai</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
</dict>"#
        )
    }

    #[test]
    fn only_a_process_outside_the_job_hands_off_to_todays_agent() {
        let me = Path::new(PROGRAM);
        let plist = today(PROGRAM);
        assert!(hands_off(None, true, Some(&plist), me));
        assert!(hands_off(Some("application.co.realtimex.ai.123"), true, Some(&plist), me));
        // The job itself, another instance, no agent: as before.
        assert!(!hands_off(Some(LABEL), true, Some(&plist), me));
        assert!(!hands_off(None, false, Some(&plist), me));
        assert!(!hands_off(None, true, None, me));
        // An agent from before #3241 (no KeepAlive), or before #11.
        let older = plist.replace("<key>KeepAlive</key>", "<key>Other</key>");
        assert!(!hands_off(None, true, Some(&older), me));
        let oldest = plist.replace("co.realtimex.ai", "com.example");
        assert!(!hands_off(None, true, Some(&oldest), me));
        // A build or a copy elsewhere never hands off to the installed one.
        assert!(!hands_off(None, true, Some(&plist), Path::new("/Users/a/link/target/debug/rtx-link")));
        // XML escapes in the program are read back.
        let amp = "/Applications/R&D/rtx-link";
        assert!(hands_off(None, true, Some(&today("/Applications/R&amp;D/rtx-link")), Path::new(amp)));
    }

    #[test]
    fn the_jobs_pid_comes_from_launchctl_print() {
        let running = "gui/501/ai.realtimex.link.agent = {\n\tactive count = 1\n\tpath = /Users/a/Library/LaunchAgents/ai.realtimex.link.agent.plist\n\tstate = running\n\n\tprogram = /Applications/RealTimeX Link.app/Contents/MacOS/rtx-link\n\tpid = 81234\n\timmediate reason = speculative\n}";
        assert_eq!(job_pid(running), Some(81234));
        let waiting = running.replace("\tpid = 81234\n", "").replace("running", "not running");
        assert_eq!(job_pid(&waiting), None);
        assert_eq!(job_pid("\tpid = 1\n"), None);
        assert_eq!(job_pid(""), None);
    }
}
