//! The supervisor (#11 M2–M4, M6): `rtx-link --launchd service` → `rtx-linkd launchd-service`,
//! root, from boot, while Start with this computer is on.
//!
//! Trust premise: an administrator approved this owner with their password (consult §2: any
//! admin may, and the owner needn't be one). So the owner's processes reaching the login window
//! and other users' console sessions through Link is what that admin approved. What must hold:
//! no user but the owner and administrators can influence what root runs or reads, and the
//! owner reaches root only through this socket's fixed requests and newer bundles our team
//! signed. So this process reads only root-owned files, never opens the owner's folder, does no
//! networking and no file work for peers, and runs the owner's daemon as the owner.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::apply::{self, APPLICATIONS_APP};
use super::{os, policy, sign};
use crate::unattended::{
    Config, Reply, Request, MACOS_MACHINE_DIR, OS_ADMIN_REQUIRED, OWNER_DAEMON_RUNNING,
    PROFILE_UNAVAILABLE,
};

/// The owner's daemon, for the SIGTERM handler.
static DAEMON: AtomicI32 = AtomicI32::new(0);
const LOG_DIR: &str = "/Library/Logs/RealTimeX Link";
/// How often the presence and version checks run (M3; Addendum 1).
const TICK: Duration = Duration::from_secs(15);

fn log(line: &str) {
    let dir = Path::new(LOG_DIR);
    if os::root_dir(dir, 0o755).is_err() {
        return;
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(dir.join("service.log"))
    {
        let _ = writeln!(file, "{seconds} {line}");
    }
}

/// The choice, read only from root-owned files (M8).
fn config() -> Config {
    let dir = Path::new(MACOS_MACHINE_DIR);
    if crate::unattended::root_only(dir) && crate::unattended::root_only(&dir.join("unattended.json")) {
        Config::load(dir)
    } else {
        Config::default()
    }
}

#[derive(Default)]
struct State {
    /// Why the owner's daemon isn't running although it is on.
    idle: Option<String>,
    /// This end of the socketpair the owner's daemon got as fd 3: bridges go to it (M4).
    bridges: Option<OwnedFd>,
}

struct Supervisor {
    config: Config,
    team: String,
    state: Mutex<State>,
    stopping: AtomicBool,
}

extern "C" fn on_term(_signal: libc::c_int) {
    let daemon = DAEMON.load(Ordering::Relaxed);
    // SAFETY: kill and _exit are async-signal-safe.
    unsafe {
        if daemon > 0 {
            libc::kill(daemon, libc::SIGTERM);
        }
        libc::_exit(0);
    }
}

pub fn run(args: &[String]) -> ! {
    // SAFETY: a plain getter.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("rtx-linkd launchd-service runs as root, under launchd");
        std::process::exit(2);
    }
    let Some(fd) = args
        .iter()
        .position(|arg| arg == "--listen-fd")
        .and_then(|i| args.get(i + 1))
        .and_then(|fd| fd.parse::<RawFd>().ok())
    else {
        eprintln!("rtx-linkd launchd-service needs --listen-fd (rtx-link checks the socket in)");
        std::process::exit(2);
    };
    // SAFETY: the listening socket rtx-link checked in with launchd and handed down.
    let listener = unsafe { UnixListener::from_raw_fd(fd) };
    let _ = os::set_cloexec(fd, true);
    let config = config();
    let Some(team) = sign::own_team() else {
        log("this copy isn't signed by a Developer ID team: nothing to run");
        std::process::exit(0);
    };
    if !config.enabled || config.owner_uid.is_none() {
        log("off: nothing to run");
        std::process::exit(0);
    }
    // SAFETY: the handler only kills the daemon and exits.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as libc::sighandler_t);
    }
    log(&format!("started for uid {}", config.owner_uid.unwrap_or_default()));
    let supervisor = Arc::new(Supervisor {
        config,
        team,
        state: Mutex::new(State::default()),
        stopping: AtomicBool::new(false),
    });
    let daemon = supervisor.clone();
    std::thread::spawn(move || daemon.owner_daemon());
    for stream in listener.incoming().flatten() {
        let supervisor = supervisor.clone();
        std::thread::spawn(move || supervisor.answer(stream));
    }
    std::process::exit(1);
}

impl Supervisor {
    fn set_idle(&self, why: Option<&str>) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).idle = why.map(str::to_string);
    }

    fn idle(&self) -> Option<String> {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).idle.clone()
    }

    /// Run the owner's daemon as the owner, again and again (M3), keeping the root copy current
    /// (M2) and restarting the daemon after an update (Addendum 1).
    fn owner_daemon(&self) {
        let uid = self.config.owner_uid.unwrap_or_default();
        let mut missing_since: Option<Instant> = None;
        let mut failures = 0u32;
        let mut owner_waits = 0u32;
        while !self.stopping.load(Ordering::Relaxed) {
            let installed = apply::bundle_version(Path::new(APPLICATIONS_APP));
            if installed.is_none() {
                let since = *missing_since.get_or_insert_with(Instant::now);
                if policy::uninstalled(Some(since.elapsed())) {
                    log("RealTimeX Link was removed from Applications: turning off");
                    self.turn_off();
                }
                std::thread::sleep(TICK);
                continue;
            }
            missing_since = None;
            let root = apply::bundle_version(Path::new(super::plist::ROOT_APP));
            if policy::refresh_wanted(root.as_deref(), installed.as_deref()) {
                match apply::install_root_copy(&self.team) {
                    Ok(()) => {
                        log(&format!("root copy refreshed to {}", installed.as_deref().unwrap_or("?")));
                        // launchd starts this job again, from the new copy; the bridges see the
                        // new version and restart (M5).
                        std::process::exit(1);
                    }
                    Err(error) => log(&format!("the root copy wasn't refreshed: {error:#}")),
                }
            }
            let Some(owner) = os::user(uid) else {
                self.set_idle(Some(PROFILE_UNAVAILABLE));
                std::thread::sleep(Duration::from_secs(60));
                continue;
            };
            let started = Instant::now();
            let outcome = self.run_daemon_once(&owner, installed.as_deref().unwrap_or_default());
            if self.stopping.load(Ordering::Relaxed) {
                return;
            }
            if !matches!(outcome, Outcome::OwnerRunning) {
                owner_waits = 0;
            }
            let wait = match outcome {
                Outcome::Uninstalled => {
                    self.turn_off();
                    return;
                }
                Outcome::Restart => Duration::from_secs(1),
                // Right after turning on, the owner's own daemon is still handing over: try
                // again soon, then less often.
                Outcome::OwnerRunning => {
                    owner_waits += 1;
                    Duration::from_secs(if owner_waits <= 10 { 1 } else { 5 })
                }
                Outcome::Unavailable => Duration::from_secs(60),
                Outcome::Exited => {
                    failures = if started.elapsed() > Duration::from_secs(60) { 0 } else { failures + 1 };
                    policy::backoff(failures)
                }
            };
            std::thread::sleep(wait);
        }
    }

    fn run_daemon_once(&self, owner: &os::User, version: &str) -> Outcome {
        let Ok((ours, theirs)) = os::socketpair(libc::SOCK_STREAM) else {
            return Outcome::Exited;
        };
        let groups = os::groups_of(owner);
        let (uid, gid, theirs_fd) = (owner.uid, owner.gid, theirs.as_raw_fd());
        let folder = policy::standard_folder(&owner.home);
        let mut command = std::process::Command::new(format!("{APPLICATIONS_APP}/Contents/MacOS/rtx-linkd"));
        command
            .arg("--data-dir")
            .arg(&folder)
            .args(["--supervised-fd", "3"])
            .env_clear()
            .envs(policy::owner_environment(&owner.home, &owner.name))
            .current_dir("/")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // SAFETY: only async-signal-safe calls between fork and exec: dup2, fcntl, setgroups,
        // setgid, setuid, getuid. The owner's groups were looked up before the fork.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(theirs_fd, 3) < 0
                    || libc::fcntl(3, libc::F_SETFD, 0) < 0
                    || libc::setgroups(groups.len() as libc::c_int, groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                    || libc::getuid() != uid
                    || libc::setuid(0) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                log(&format!("the owner's daemon didn't start: {error}"));
                return Outcome::Exited;
            }
        };
        drop(theirs);
        DAEMON.store(child.id() as i32, Ordering::Relaxed);
        // Its log lines (it logs nothing typed: I7) into this one.
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    log(&format!("daemon: {}", line.chars().take(400).collect::<String>()));
                }
            });
        }
        // Its first line: ready, or why not (main.rs).
        let first = child.stdout.take().map(|mut stdout| {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while line.len() < 4096 && stdout.read(&mut byte).is_ok_and(|n| n == 1) && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            // The rest of its output goes nowhere.
            std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
            serde_json::from_slice::<serde_json::Value>(&line).unwrap_or_default()
        });
        let code = first.as_ref().and_then(|line| line.get("code")).and_then(|code| code.as_str());
        if first.as_ref().is_some_and(|line| line.get("ready").is_some()) {
            self.set_idle(None);
            self.state.lock().unwrap_or_else(|p| p.into_inner()).bridges = Some(ours);
            log("owner's daemon started");
        } else {
            let _ = child.wait();
            DAEMON.store(0, Ordering::Relaxed);
            return match code {
                Some("already-running") => {
                    self.set_idle(Some(OWNER_DAEMON_RUNNING));
                    Outcome::OwnerRunning
                }
                Some(PROFILE_UNAVAILABLE) => {
                    self.set_idle(Some(PROFILE_UNAVAILABLE));
                    Outcome::Unavailable
                }
                _ => Outcome::Exited,
            };
        }
        struct Daemon<'a>(&'a mut std::process::Child);
        impl policy::Running for Daemon<'_> {
            fn exited(&mut self) -> bool {
                !matches!(self.0.try_wait(), Ok(None))
            }
            fn stop(&mut self) {
                stop(self.0);
            }
        }
        let watched = policy::watch(
            &mut Daemon(&mut child),
            version,
            TICK,
            || apply::bundle_version(Path::new(APPLICATIONS_APP)),
            Instant::now,
            || self.stopping.load(Ordering::Relaxed),
            std::thread::sleep,
        );
        let outcome = match watched {
            policy::Watched::Exited => Outcome::Exited,
            policy::Watched::Restart => {
                log("RealTimeX Link was updated: restarted the owner's daemon");
                Outcome::Restart
            }
            policy::Watched::Uninstalled => {
                log("RealTimeX Link was removed from Applications: turning off");
                Outcome::Uninstalled
            }
        };
        self.state.lock().unwrap_or_else(|p| p.into_inner()).bridges = None;
        let _ = child.wait();
        DAEMON.store(0, Ordering::Relaxed);
        log("owner's daemon stopped");
        outcome
    }

    /// One connection: its peer checked (M4), then one request line.
    fn answer(&self, stream: UnixStream) {
        // An accepted socket keeps the listener's non-blocking mode on macOS.
        let _ = stream.set_nonblocking(false);
        let Ok(peer) = os::peer(stream.as_raw_fd()) else { return };
        let allowed = sign::requirement(&self.team, &sign::PEER_IDENTIFIERS)
            .is_some_and(|requirement| sign::peer_satisfies(&peer.token, &requirement));
        if !allowed {
            // Closed without an answer.
            return;
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let Some(line) = read_line(&stream) else { return };
        let Ok(request) = serde_json::from_slice::<Request>(&line) else {
            reply(&stream, &Reply::refused("bad-request", "not a request"));
            return;
        };
        match request {
            Request::Status { data_dir } => {
                let enabled = self.config.runs(&data_dir);
                reply(
                    &stream,
                    &Reply {
                        enabled,
                        // Any administrator may turn it on, and the owner or one off (consult §2).
                        can_change: true,
                        idle: if enabled { self.idle() } else { None },
                        ..Reply::default()
                    },
                );
            }
            Request::Disable { .. } => {
                if !policy::may_disable(peer.uid, self.config.owner_uid, os::is_admin(peer.uid)) {
                    reply(
                        &stream,
                        &Reply::refused(OS_ADMIN_REQUIRED, "The owner or an administrator can turn this off"),
                    );
                    return;
                }
                if let Err(error) = apply::write_off() {
                    reply(&stream, &Reply::refused("io", error.to_string()));
                    return;
                }
                log(&format!("turned off by uid {}", peer.uid));
                let off = Reply { enabled: false, can_change: true, ..Reply::default() };
                // The owner's daemon asking for itself (POST /v1/unattended) needs the answer before
                // it stops, or its caller gets none; anyone else hears once it has stopped (C4).
                if peer.pid() as i32 == DAEMON.load(Ordering::Relaxed) {
                    reply(&stream, &off);
                    drop(stream);
                    std::thread::sleep(Duration::from_millis(500));
                    self.stop_daemon();
                } else {
                    self.stop_daemon();
                    reply(&stream, &off);
                    drop(stream);
                }
                apply::remove_everything();
                std::process::exit(0);
            }
            Request::Enable { .. } => reply(
                &stream,
                &Reply::refused("bad-request", "Turning on goes through the administrator's prompt"),
            ),
            Request::Bridge { version, on_console } => {
                // The hello's timeout isn't the daemon's to keep.
                let _ = stream.set_read_timeout(None);
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                let Some(bridges) = state.bridges.as_ref() else {
                    // No daemon to give it to: the bridge tries again.
                    return;
                };
                let hello = serde_json::json!({
                    "version": version,
                    "onConsole": on_console,
                    "uid": peer.uid,
                    "pid": peer.pid(),
                    "auditSession": peer.audit_session(),
                    "loginWindow": peer.uid == 0,
                });
                if let Err(error) = os::send_fd(bridges.as_raw_fd(), stream.as_raw_fd(), hello.to_string().as_bytes()) {
                    log(&format!("a bridge couldn't be handed on: {error}"));
                }
            }
        }
    }

    fn stop_daemon(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        let daemon = DAEMON.load(Ordering::Relaxed);
        if daemon > 0 {
            // SAFETY: the pid of this process's own child.
            unsafe { libc::kill(daemon, libc::SIGTERM) };
            let until = Instant::now() + Duration::from_secs(10);
            while DAEMON.load(Ordering::Relaxed) != 0 && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// The off path from inside (an uninstall): as `disable`, with nobody to answer.
    fn turn_off(&self) {
        let _ = apply::write_off();
        self.stop_daemon();
        apply::remove_everything();
        std::process::exit(0);
    }
}

enum Outcome {
    /// Stopped for an update: start the new one at once.
    Restart,
    /// /Applications has been without Link for ten minutes: turn off.
    Uninstalled,
    OwnerRunning,
    Unavailable,
    Exited,
}

fn stop(child: &mut std::process::Child) {
    // SAFETY: the pid of this process's own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    for _ in 0..100 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
}

/// One line, byte by byte, so nothing after it is read away from a bridge's connection.
fn read_line(mut stream: &UnixStream) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return Some(line),
            Ok(1) if line.len() < 64 * 1024 => line.push(byte[0]),
            _ => return None,
        }
    }
}

fn reply(mut stream: &UnixStream, reply: &Reply) {
    let mut text = serde_json::to_string(reply).unwrap_or_default();
    text.push('\n');
    let _ = stream.write_all(text.as_bytes());
}
