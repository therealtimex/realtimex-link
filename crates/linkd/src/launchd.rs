//! macOS unattended access (realtimex-link#11): what launchd runs, through `rtx-link
//! --launchd <role>` (SD M1), so TCC credits RealTimeX Link for everything below it.
//!
//! `launchd-session --spike <socket>` is SD's spike S1–S5 only: it serves rtx-control's JSON
//! lines, one connection at a time, on a root-only Unix socket, so the login window, the lock
//! screen and the grants can be tried from SSH before the supervisor and the bridge exist.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};

pub fn run(role: &str, args: &[String]) -> ! {
    let spike = args
        .iter()
        .position(|arg| arg == "--spike")
        .and_then(|i| args.get(i + 1));
    match (role, spike) {
        ("launchd-session", Some(socket)) => spike(socket),
        _ => {
            eprintln!("rtx-linkd {role}: not available in this build");
            std::process::exit(2);
        }
    }
}

fn spike(socket: &str) -> ! {
    let _ = std::fs::remove_file(socket);
    let listener = match UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("spike: binding {socket}: {error}");
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    eprintln!(
        "spike: uid {} euid {} pid {} on {socket}",
        unsafe { libc::getuid() },
        unsafe { libc::geteuid() },
        std::process::id()
    );
    for stream in listener.incoming().flatten() {
        if let Err(error) = serve(stream) {
            eprintln!("spike: {error}");
        }
    }
    std::process::exit(0);
}

/// One connection: a new rtx-control, its stdin and stdout carried as lines.
fn serve(stream: UnixStream) -> std::io::Result<()> {
    let control = std::env::current_exe()?
        .parent()
        .map(|dir| dir.join("rtx-control"))
        .ok_or_else(|| std::io::Error::other("no folder"))?;
    let mut child = Command::new(control)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut to_child = child.stdin.take().expect("piped");
    let from_child = child.stdout.take().expect("piped");
    let mut to_peer = stream.try_clone()?;
    let out = std::thread::spawn(move || {
        for line in BufReader::new(from_child).lines().map_while(Result::ok) {
            if writeln!(to_peer, "{line}").is_err() {
                break;
            }
        }
    });
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if writeln!(to_child, "{line}").is_err() {
            break;
        }
    }
    drop(to_child);
    let _ = child.kill();
    let _ = child.wait();
    let _ = out.join();
    Ok(())
}
