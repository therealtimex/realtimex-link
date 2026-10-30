//! macOS unattended access (realtimex-link#11, SD M1): launchd starts this, the bundle's main
//! executable, so TCC credits RealTimeX Link (`ai.realtimex.link`) for rtx-linkd and its
//! rtx-control, as it does for the login item today. `--launchd <role>` touches no AppKit: it
//! starts rtx-linkd (`launchd-<role> …`) as its child, passes SIGTERM on and exits with the
//! child's status.
//! - `service`: checks the supervisor's socket in with launchd (only the job process can) and
//!   hands it down as `--listen-fd`.
//! - `session`: makes the login-window input channel (job.rs; Addendum 5) and hands one end
//!   down as `--input-fd`, to rtx-linkd and from it to rtx-control only (C1).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
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

extern "C" {
    fn launch_activate_socket(name: *const libc::c_char, fds: *mut *mut libc::c_int, count: *mut libc::size_t) -> libc::c_int;
}

/// The supervisor's listening socket, from launchd (`Sockets` → `Listeners` in its plist).
fn checked_in_socket() -> Option<RawFd> {
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut count: libc::size_t = 0;
    // SAFETY: launchd fills a malloc'd array of `count` fds, freed here; extra fds are closed.
    unsafe {
        if launch_activate_socket(c"Listeners".as_ptr(), &mut fds, &mut count) != 0 || fds.is_null() {
            return None;
        }
        let list = std::slice::from_raw_parts(fds, count).to_vec();
        libc::free(fds.cast());
        for &extra in list.iter().skip(1) {
            libc::close(extra);
        }
        // Inherited by the one intended exec only, as fd 3 (C1).
        if let Some(&fd) = list.first() {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        list.first().copied()
    }
}

fn socketpair_seqpacket() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: two out fds; both close on exec until the one intended exec (C1).
    unsafe {
        if libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) != 0 {
            return None;
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Some((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

pub fn run(role: &str, rest: &[String]) -> ! {
    let Some(linkd) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("rtx-linkd")))
    else {
        eprintln!("rtx-link --launchd: no rtx-linkd next to this executable");
        std::process::exit(1);
    };
    let mut command = std::process::Command::new(&linkd);
    command.arg(format!("launchd-{role}")).args(rest);
    // The fd the child gets as 3, inherited by that exec only.
    let mut handed: Option<RawFd> = None;
    let mut channel: Option<OwnedFd> = None;
    match role {
        "service" => match checked_in_socket() {
            Some(fd) => {
                handed = Some(fd);
                command.args(["--listen-fd", "3"]);
            }
            None => {
                eprintln!("rtx-link --launchd service: launchd gave no socket");
                std::process::exit(1);
            }
        },
        "session" => {
            if let Some((ours, theirs)) = socketpair_seqpacket() {
                handed = Some(theirs.as_raw_fd());
                command.args(["--input-fd", "3"]);
                channel = Some(ours);
                // Kept open (and owned) until the child started.
                std::mem::forget(theirs);
            }
        }
        _ => {}
    }
    if let Some(fd) = handed {
        // SAFETY: dup2 and fcntl only, between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("rtx-link --launchd: starting {}: {error}", linkd.display());
            std::process::exit(1);
        }
    };
    if let Some(fd) = handed {
        // SAFETY: this process's copy; the child has its own now.
        unsafe { libc::close(fd) };
    }
    CHILD.store(child.id() as i32, Ordering::Relaxed);
    // SAFETY: the handler only reads an atomic and calls kill.
    unsafe {
        libc::signal(libc::SIGTERM, pass_on as libc::sighandler_t);
        libc::signal(libc::SIGINT, pass_on as libc::sighandler_t);
    }
    if let Some(channel) = channel {
        std::thread::spawn(move || {
            if !crate::job::serve(channel) {
                // A malformed datagram (C2): end the job; launchd starts it again.
                let child = CHILD.load(Ordering::Relaxed);
                // SAFETY: the pid of this process's own child.
                unsafe { libc::kill(child, libc::SIGTERM) };
                std::process::exit(1);
            }
        });
    }
    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    };
    std::process::exit(code);
}
