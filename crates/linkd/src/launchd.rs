//! macOS unattended access (realtimex-link#11; System Design issue-11-sd-design.md). What
//! launchd runs, always through `rtx-link --launchd <role>` (M1), so TCC credits RealTimeX
//! Link for everything below it:
//! - `launchd-service`: the root supervisor (supervisor.rs), which runs the owner's daemon as the
//!   owner and answers the socket;
//! - `launchd-session`: the session bridge (bridge.rs), one per GUI session.
//!
//! And the root steps an administrator's prompt or `sudo` runs (apply.rs):
//! `unattended-apply` and `unattended-remove`.
//!
//! The decisions are pure functions (policy.rs, plist.rs), tested on every platform.

#[cfg(target_os = "macos")]
mod apply;
#[cfg(target_os = "macos")]
mod bridge;
// The job's input datagrams (Addendum 5), shared with rtx-control and rtx-link.
#[cfg(target_os = "macos")]
#[path = "../../control/src/jobinput.rs"]
#[allow(dead_code)]
mod jobinput;
#[cfg(target_os = "macos")]
pub mod os;
pub mod plist;
pub mod policy;
pub mod sign;
#[cfg(target_os = "macos")]
mod supervisor;

/// Run a launchd role, or a root step; never returns.
#[cfg(target_os = "macos")]
pub fn run(role: &str, args: &[String]) -> ! {
    match role {
        "launchd-service" => supervisor::run(args),
        "launchd-session" => bridge::run(args),
        "unattended-apply" | "unattended-remove" => {
            let done = if role == "unattended-apply" { apply::apply(args) } else { apply::remove() };
            match done {
                Ok(()) => {
                    println!("{}", serde_json::json!({ "ok": true }));
                    std::process::exit(0);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let code = (message == crate::unattended::LOGIN_ITEMS_DISABLED).then_some(message.clone());
                    println!("{}", serde_json::json!({ "error": message, "code": code }));
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("rtx-linkd {role}: not a launchd role");
            std::process::exit(2);
        }
    }
}
