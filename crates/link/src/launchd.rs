//! macOS unattended access (realtimex-link#11, SD M1): launchd starts this, the bundle's main
//! executable, so TCC credits RealTimeX Link (`ai.realtimex.link`) for rtx-linkd and its
//! rtx-control, as it does for the login item today. `--launchd <role> …` touches no AppKit:
//! it starts rtx-linkd (`launchd-<role> …`) as its child, passes SIGTERM on and exits with the
//! child's status.

use std::sync::atomic::{AtomicI32, Ordering};

/// The child's pid, for the SIGTERM handler.
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn pass_on(signal: libc::c_int) {
    let child = CHILD.load(Ordering::Relaxed);
    if child > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(child, signal) };
    }
}

pub fn run(role: &str, rest: &[String]) -> ! {
    match role {
        "probe" => crate::probe::serve(rest),
        "probe-child" => crate::probe::child(rest),
        _ => {}
    }
    let Some(linkd) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("rtx-linkd")))
    else {
        eprintln!("rtx-link --launchd: no rtx-linkd next to this executable");
        std::process::exit(1);
    };
    let mut child = match std::process::Command::new(&linkd)
        .arg(format!("launchd-{role}"))
        .args(rest)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("rtx-link --launchd: starting {}: {error}", linkd.display());
            std::process::exit(1);
        }
    };
    CHILD.store(child.id() as i32, Ordering::Relaxed);
    // SAFETY: the handler only reads an atomic and calls kill.
    unsafe {
        libc::signal(libc::SIGTERM, pass_on as libc::sighandler_t);
        libc::signal(libc::SIGINT, pass_on as libc::sighandler_t);
    }
    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    };
    std::process::exit(code);
}
