//! Reopen RealTimeX after a crash (realtimex-ai-app#3241, SD Q2–Q4). A thread of the rtx-link
//! process that holds the tray (the window owner on macOS and Windows, `icon::run` on Linux), for
//! this computer's own Link only. Never in rtx-linkd: under unattended mode that runs outside the
//! person's GUI session, and would start RealTimeX in the wrong place or as the wrong user.
//!
//! The installed RealTimeX writes `app-session.json` in Link's default data folder when it starts
//! (`{schema: 1, capabilities: ["clean-exit"], pid, exit: null, …}`), and rewrites it with `exit`
//! before any exit it means. The watcher:
//! - supervises a pid only when the lease passes ([`parse_lease`]), the OS says the pid is the
//!   installed RealTimeX's main executable ([`expected_executable`], [`same_executable`]), the lease
//!   is no older than the process ([`lease_is_fresh`]), and it saw that pid alive itself;
//! - on its death, with no `exit` for it, waits 3, 10 or 30 s and starts the fixed target from
//!   `desktop::realtimex_app()` with no arguments ([`launch_command`]), and stops at the 4th
//!   unannounced exit in 10 minutes ([`decide`]);
//! - keeps its state in `app-supervision.json` and one `supervise:` line per decision in
//!   `app-supervision.log`, which rtx-linkd reports (GET /v1/supervision).
//!
//! Nothing from the lease, the local API or the settings reaches the program or its arguments.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::app::{lock, Shared};
use crate::desktop::Os;

/// Written by the installed RealTimeX, only read here.
pub const LEASE_FILE: &str = "app-session.json";
/// Written here (atomically), read by rtx-linkd.
pub const STATE_FILE: &str = "app-supervision.json";
pub const LOG_FILE: &str = "app-supervision.log";
const MAX_LEASE_BYTES: u64 = 4 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;
/// The log rotates at this size and keeps one `.1`.
const LOG_LIMIT: u64 = 1024 * 1024;
const POLL: Duration = Duration::from_secs(1);
/// The unannounced exits counted (a sliding window), and how long a supervised session must live
/// for the count to start again.
const WINDOW_MS: i64 = 10 * 60 * 1000;
const ALIVE_RESET: Duration = Duration::from_secs(10 * 60);
/// The wait before each relaunch: the grace period on the first exit, then the back-off. The 4th
/// unannounced exit in the window stops.
const DELAYS: [u64; 3] = [3, 10, 30];
/// Never passed on to RealTimeX: they would turn Electron into Node or change how it loads.
pub const ENV_DROPPED: [&str; 4] = [
    "ELECTRON_RUN_AS_NODE",
    "NODE_OPTIONS",
    "ELECTRON_ENABLE_LOGGING",
    "NODE_PATH",
];
/// The bundle's executable when its Info.plist can't say.
const DEFAULT_EXECUTABLE: &str = "RealTimeX.AI";
/// At most this many exit times are kept (the window keeps far fewer).
const MAX_EXITS: usize = 16;
/// The exit reason RealTimeX gives when its server is down for good (its own server restarts gave
/// up). It first asks in the lease (`restart: {reason, nonce}`); a running watcher with the setting
/// on and no crash-loop stop answers with `ack: {pid, nonce}` in app-supervision.json and owes the
/// reopening from then on (kept across a Link restart). Only then does the app quit, and Link
/// reopens it like a crash: counted, with back-off. Without an answer the app keeps its notice.
pub const BACKEND_DOWN: &str = "backend-down";

/// SIGTERM or SIGHUP reached the tray (macOS and Linux logout and shutdown).
static STOPPING: AtomicBool = AtomicBool::new(false);
/// Link's own Quit withdrew its answers ([`forget`]): the watcher writes no more state.
static FORGOTTEN: AtomicBool = AtomicBool::new(false);

// --- the lease --------------------------------------------------------------------------------

/// What Link takes from `app-session.json`. The pid is checked against the OS before it is used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub pid: u32,
    /// For the log only.
    pub version: Option<String>,
    /// `exit.reason` once RealTimeX announced an exit (for the log only), else None.
    pub exit: Option<String>,
    /// `restart.nonce` while RealTimeX asks Link to take over (its server is down for good).
    pub restart: Option<String>,
}

/// A lease Link supervises: at most 4 KB, `schema` 1, `"clean-exit"` among its capabilities, and
/// an integer pid above 1. Anything else (an older or newer app, a damaged file) is None.
pub fn parse_lease(bytes: &[u8]) -> Option<Lease> {
    if bytes.len() as u64 > MAX_LEASE_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("schema")?.as_u64()? != 1 {
        return None;
    }
    let capable = value
        .get("capabilities")?
        .as_array()?
        .iter()
        .any(|capability| capability.as_str() == Some("clean-exit"));
    if !capable {
        return None;
    }
    let pid = value
        .get("pid")?
        .as_u64()
        .filter(|pid| *pid > 1)
        .and_then(|pid| u32::try_from(pid).ok())?;
    let version = value.get("version").and_then(Value::as_str).map(|v| printable(v, 64));
    let exit = match value.get("exit") {
        None | Some(Value::Null) => None,
        Some(exit) => Some(
            exit.get("reason")
                .and_then(Value::as_str)
                .map(|reason| printable(reason, 32))
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    };
    let restart = value
        .get("restart")
        .filter(|restart| restart.get("reason").and_then(Value::as_str) == Some(BACKEND_DOWN))
        .and_then(|restart| restart.get("nonce"))
        .and_then(Value::as_str)
        .map(|nonce| printable(nonce, 64))
        .filter(|nonce| !nonce.is_empty());
    Some(Lease { pid, version, exit, restart })
}

/// Text from the lease as it may appear in a log line: letters, digits and `.-_+` only.
fn printable(text: &str, most: usize) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .take(most)
        .collect()
}

/// The lease file as last read: its mtime (ms since the epoch), size and lease.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LeaseFile {
    mtime_ms: i64,
    size: u64,
    lease: Option<Lease>,
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// The lease, unless the file is missing; one above 4 KB is read as no lease.
fn read_lease(dir: &Path) -> Option<LeaseFile> {
    let path = dir.join(LEASE_FILE);
    let meta = std::fs::metadata(&path).ok().filter(|meta| meta.is_file())?;
    let mtime_ms = modified_ms(&meta)?;
    let size = meta.len();
    let lease = (size <= MAX_LEASE_BYTES)
        .then(|| {
            let mut bytes = Vec::new();
            std::fs::File::open(&path)
                .ok()?
                .take(MAX_LEASE_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            parse_lease(&bytes)
        })
        .flatten();
    Some(LeaseFile { mtime_ms, size, lease })
}

/// The lease was written at or after the process started (whole seconds: Linux knows a start to
/// the tick and the boot time to the second), so a file left behind never adopts a reused pid.
pub fn lease_is_fresh(lease_mtime_ms: i64, process_start_ms: i64) -> bool {
    lease_mtime_ms.div_euclid(1000) >= process_start_ms.div_euclid(1000)
}

// --- the process, from the OS -------------------------------------------------------------------

/// A live process as the OS reports it (never as the app claims).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    /// When it started, ms since the epoch.
    pub start_ms: i64,
    pub exe: PathBuf,
}

#[cfg(target_os = "macos")]
fn process(pid: u32) -> Option<Proc> {
    use std::os::unix::ffi::OsStringExt;
    let pid = i32::try_from(pid).ok()?;
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer and its size are this function's.
    let length = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if length <= 0 {
        return None;
    }
    path.truncate(length as usize);
    // SAFETY: proc_bsdinfo is plain data; proc_pidinfo fills at most its size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if read != size || info.pbi_status == libc::SZOMB {
        return None;
    }
    let start_ms = info.pbi_start_tvsec as i64 * 1000 + info.pbi_start_tvusec as i64 / 1000;
    Some(Proc {
        start_ms,
        exe: PathBuf::from(OsString::from_vec(path)),
    })
}

#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<Proc> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let ticks = stat_start_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let boot = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<i64>().ok())?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as i64;
    if hz <= 0 {
        return None;
    }
    Some(Proc {
        start_ms: boot * 1000 + ticks * 1000 / hz,
        exe,
    })
}

/// `/proc/<pid>/stat` field 22 (the start, in clock ticks after boot); None for a zombie. The
/// command name (field 2) may hold spaces and parentheses, so fields are counted after its last
/// `)`.
#[cfg(any(target_os = "linux", test))]
fn stat_start_ticks(stat: &str) -> Option<i64> {
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    // fields[0] is field 3 (the state).
    fields.get(22 - 3)?.parse().ok()
}

#[cfg(windows)]
fn process(pid: u32) -> Option<Proc> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut name = [0u16; 1024];
    let mut size = name.len() as u32;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle is checked and closed; the buffers and their sizes are this function's.
    let read = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut code = 0u32;
        let read = GetExitCodeProcess(handle, &mut code) != 0
            && code == STILL_ACTIVE as u32
            && QueryFullProcessImageNameW(handle, 0, name.as_mut_ptr(), &mut size) != 0
            && GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        read
    };
    if !read {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(Proc {
        start_ms: filetime_ms(ticks),
        exe: PathBuf::from(String::from_utf16_lossy(&name[..size as usize])),
    })
}

/// A FILETIME (100 ns ticks since 1601) as ms since the Unix epoch.
#[cfg(any(windows, test))]
fn filetime_ms(ticks: u64) -> i64 {
    (ticks as i64 - 116_444_736_000_000_000) / 10_000
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn process(_pid: u32) -> Option<Proc> {
    None
}

// --- the target ---------------------------------------------------------------------------------

/// The main executable of the installed RealTimeX (`desktop::realtimex_app()`): on macOS the
/// bundle's `Contents/MacOS/<CFBundleExecutable>`, elsewhere the target itself.
pub fn expected_executable(os: Os, target: &Path) -> PathBuf {
    if os == Os::MacOs {
        let plist = std::fs::read_to_string(target.join("Contents/Info.plist")).ok();
        target
            .join("Contents/MacOS")
            .join(bundle_executable(plist.as_deref()))
    } else {
        target.to_path_buf()
    }
}

/// CFBundleExecutable from an XML Info.plist; `RealTimeX.AI` when it can't be read (a binary
/// plist, a damaged one, a name that isn't one file's).
pub fn bundle_executable(info_plist: Option<&str>) -> String {
    info_plist
        .and_then(|text| {
            let key = "<key>CFBundleExecutable</key>";
            let rest = text[text.find(key)? + key.len()..].trim_start();
            let rest = rest.strip_prefix("<string>")?;
            let name = rest[..rest.find("</string>")?].trim();
            let one_file = !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains(['/', '\\', '\0', '&', '<']);
            one_file.then(|| name.to_string())
        })
        .unwrap_or_else(|| DEFAULT_EXECUTABLE.to_string())
}

/// Whether two executables are the same file: resolved when both exist, else as written, and
/// without regard to case on macOS and Windows (rtx-linkd's paths::same_folder). A translocated
/// copy, a dev Electron or a copy elsewhere is not the installed one.
pub fn same_executable(a: &Path, b: &Path, ignore_case: bool) -> bool {
    let (a, b) = match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => (a.to_path_buf(), b.to_path_buf()),
    };
    let text = |path: &Path| {
        let text = path.to_string_lossy();
        let text = text.trim_end_matches(['/', '\\']);
        if ignore_case {
            text.to_lowercase()
        } else {
            text.to_string()
        }
    };
    text(&a) == text(&b)
}

fn ignore_case(os: Os) -> bool {
    matches!(os, Os::MacOs | Os::Windows)
}

// --- the launcher -------------------------------------------------------------------------------

/// How RealTimeX is started: only the fixed target, with no arguments of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// Removed from the child's environment.
    pub env_removed: &'static [&'static str],
}

/// - macOS: `/usr/bin/open -n -a <bundle>`. `-n`: otherwise LaunchServices may only activate a
///   running browser instance of the same bundle; a duplicate main process quits on its lock.
/// - Windows: the exe, no arguments, in its own folder.
/// - Linux: the exe, no arguments (its own session, launch()).
pub fn launch_command(os: Os, target: &Path) -> Launch {
    match os {
        Os::MacOs => Launch {
            program: PathBuf::from("/usr/bin/open"),
            args: vec!["-n".into(), "-a".into(), target.as_os_str().to_os_string()],
            cwd: None,
            env_removed: &ENV_DROPPED,
        },
        Os::Windows => Launch {
            program: target.to_path_buf(),
            args: Vec::new(),
            cwd: target.parent().map(Path::to_path_buf),
            env_removed: &ENV_DROPPED,
        },
        Os::Linux => Launch {
            program: target.to_path_buf(),
            args: Vec::new(),
            cwd: None,
            env_removed: &ENV_DROPPED,
        },
    }
}

fn launch(spec: &Launch) -> std::io::Result<()> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in spec.env_removed {
        command.env_remove(name);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; nothing else runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = command.spawn()?;
    // Unix: collected when it ends (open at once, RealTimeX when it quits), so it never lingers
    // as a zombie the watcher would read as alive. Windows: the handle is dropped.
    #[cfg(unix)]
    {
        let mut child = child;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    #[cfg(not(unix))]
    drop(child);
    Ok(())
}

// --- session ending -----------------------------------------------------------------------------

#[cfg(unix)]
extern "C" fn on_session_end(signal: libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
    // Then end as before this handler: the default action, delivered once the handler returns.
    // SAFETY: signal and raise are async-signal-safe.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// macOS and Linux: launchd and session managers send SIGTERM (or SIGHUP) at logout; the watcher
/// sees it before the process ends as it does today.
#[cfg(unix)]
fn watch_session_end() {
    // SAFETY: the handler only stores an atomic, then restores the default and raises again.
    unsafe {
        let handler = on_session_end as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

#[cfg(not(unix))]
fn watch_session_end() {}

fn session_ending() -> bool {
    STOPPING.load(Ordering::SeqCst) || shutting_down()
}

#[cfg(windows)]
fn shutting_down() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    // SAFETY: no pointers.
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

#[cfg(not(windows))]
fn shutting_down() -> bool {
    false
}

// --- the decision (no I/O) ------------------------------------------------------------------------

/// What the watcher remembers between decisions: `stopped` after a crash loop, the unannounced
/// exits in the window (ms since the epoch), and the setting as last seen (None before the first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    pub stopped: bool,
    pub exits: Vec<i64>,
    pub setting: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The setting as read now (a turn from off to on starts the count again).
    Setting,
    /// A session that passes Q2 is watched now.
    Adopted,
    /// The watched session has lived 10 minutes.
    AliveLong,
    /// The watched pid died with `exit` set for it.
    Intended,
    /// The watched pid died without it.
    Unannounced,
    /// A relaunch's wait is over: what holds now, in the order it is checked.
    Due {
        session_ending: bool,
        /// A different live pid whose lease passes Q2.
        other_live: Option<u32>,
        installed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Nothing,
    Watching,
    Intended,
    /// Wait `delay_secs`, then check again (Event::Due); `nth` of 3.
    Relaunch { delay_secs: u64, nth: usize },
    /// The `count`th unannounced exit in 10 minutes: no relaunch until RealTimeX starts normally.
    Stopped { count: usize },
    SettingOff,
    SessionEnding,
    AlreadyRunning(u32),
    NotInstalled,
    Launch,
}

/// The watcher's every decision, from what it saw (`event`), the time (`now`, ms since the epoch)
/// and the setting. Resets: an intended exit, a session alive for 10 minutes, the setting turned
/// off and on, and while stopped a new session (Link didn't start it). A session Link started
/// doesn't reset the count while it is watching.
pub fn decide(memory: &Memory, event: &Event, now: i64, setting: bool) -> (Decision, Memory) {
    let mut next = memory.clone();
    let reset = |next: &mut Memory| {
        next.stopped = false;
        next.exits.clear();
    };
    let decision = match event {
        Event::Setting => {
            if memory.setting == Some(false) && setting {
                reset(&mut next);
            }
            next.setting = Some(setting);
            Decision::Nothing
        }
        Event::Adopted => {
            if memory.stopped {
                reset(&mut next);
            }
            Decision::Watching
        }
        Event::AliveLong => {
            reset(&mut next);
            Decision::Nothing
        }
        Event::Intended => {
            reset(&mut next);
            Decision::Intended
        }
        Event::Unannounced if !setting => Decision::SettingOff,
        Event::Unannounced => {
            next.exits.retain(|at| *at <= now && now - *at < WINDOW_MS);
            next.exits.push(now);
            if next.exits.len() > MAX_EXITS {
                next.exits.drain(..next.exits.len() - MAX_EXITS);
            }
            let count = next.exits.len();
            match DELAYS.get(count - 1) {
                Some(&delay_secs) if !memory.stopped => Decision::Relaunch {
                    delay_secs,
                    nth: count,
                },
                _ => {
                    next.stopped = true;
                    Decision::Stopped { count }
                }
            }
        }
        Event::Due {
            session_ending,
            other_live,
            installed,
        } => {
            if *session_ending {
                Decision::SessionEnding
            } else if !setting {
                Decision::SettingOff
            } else if let Some(pid) = other_live {
                Decision::AlreadyRunning(*pid)
            } else if !installed {
                Decision::NotInstalled
            } else {
                Decision::Launch
            }
        }
    };
    (decision, next)
}

// --- state and log ------------------------------------------------------------------------------

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

fn iso(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn from_iso(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// `app-supervision.json`: `{schema, state, exits, last, enabled, watching}`. RealTimeX reads
/// `enabled`, `state` and `watching.pid` to know Link would reopen it after a server-down exit.
fn state_json(
    memory: &Memory,
    last: &Value,
    enabled: bool,
    watching: Option<u32>,
    ack: Option<&Ack>,
) -> Value {
    json!({
        "schema": 1,
        "state": if memory.stopped { "stopped" } else { "watching" },
        "exits": memory.exits.iter().map(|at| iso(*at)).collect::<Vec<_>>(),
        "last": last,
        "enabled": enabled,
        "watching": watching.map(|pid| json!({ "pid": pid })),
        "ack": ack.map(|ack| json!({
            "pid": ack.pid, "nonce": ack.nonce, "startMs": ack.start_ms, "dueMs": ack.due_ms,
            "diedMs": ack.died_ms,
        })),
    })
}

/// A restart request Link answered: it owes reopening that pid's RealTimeX once it dies.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ack {
    pid: u32,
    nonce: String,
    /// The pid's start time from the OS, so a reused pid is never taken for it.
    start_ms: i64,
    /// Once the pid died and its relaunch was decided (counted once): when it is due (ms since the
    /// epoch). Kept until the relaunch runs, so a Link restart during the wait still reopens.
    due_ms: Option<i64>,
    /// When the death was seen (ms since the epoch), kept with `due_ms`: a replacement session's
    /// lease written since then cancels the relaunch, also after a Link restart.
    died_ms: Option<i64>,
}

fn ack_of(file: Option<&Value>) -> Option<Ack> {
    let ack = file?.get("ack")?;
    Some(Ack {
        pid: u32::try_from(ack.get("pid")?.as_u64()?).ok().filter(|pid| *pid > 1)?,
        nonce: printable(ack.get("nonce")?.as_str()?, 64),
        start_ms: ack.get("startMs")?.as_i64()?,
        due_ms: ack.get("dueMs").and_then(Value::as_i64),
        died_ms: ack.get("diedMs").and_then(Value::as_i64),
    })
}

/// Link's own Quit: it won't watch any more, so no answer it gave may make RealTimeX quit now
/// (nobody would reopen it). Drops `ack` and `watching` from app-supervision.json.
pub fn forget() {
    // First: the watcher writes nothing more (it could put its answer back), then the file.
    FORGOTTEN.store(true, Ordering::SeqCst);
    if !crate::api::is_default_instance() {
        return;
    }
    forget_in(&crate::api::data_dir());
}

fn forget_in(dir: &Path) {
    let path = dir.join(STATE_FILE);
    let Some(mut state) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
    else {
        return;
    };
    state["ack"] = Value::Null;
    state["watching"] = Value::Null;
    let _ = write_atomically(&path, state.to_string().as_bytes());
}

/// The memory and last decision from `app-supervision.json`, so a Link restart (KeepAlive after
/// a kill) keeps the count. Anything unreadable starts afresh.
fn memory_of(file: Option<&Value>) -> (Memory, Value) {
    let Some(file) = file.filter(|file| file.get("schema").and_then(Value::as_u64) == Some(1)) else {
        return (Memory::default(), Value::Null);
    };
    let mut exits: Vec<i64> = file
        .get("exits")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).filter_map(from_iso).collect())
        .unwrap_or_default();
    exits.truncate(MAX_EXITS);
    let memory = Memory {
        stopped: file.get("state").and_then(Value::as_str) == Some("stopped"),
        exits,
        setting: None,
    };
    let last = file.get("last").filter(|last| last.is_object()).cloned().unwrap_or(Value::Null);
    (memory, last)
}

fn load_state(dir: &Path) -> (Memory, Value, Option<Ack>) {
    let path = dir.join(STATE_FILE);
    let file = std::fs::metadata(&path)
        .ok()
        .filter(|meta| meta.len() <= MAX_STATE_BYTES)
        .and_then(|_| std::fs::read(&path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|file| file.get("schema").and_then(Value::as_u64) == Some(1));
    let (memory, last) = memory_of(file.as_ref());
    (memory, last, ack_of(file.as_ref()))
}

/// Written whole to a temporary file in the same folder, then renamed over the old one.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temporary = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// One `supervise:` line, the time after it. The log moves to `.1` once it reaches 1 MB.
fn append_log(dir: &Path, message: &str, now: i64) {
    let path = dir.join(LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() >= LOG_LIMIT) {
        let _ = std::fs::rename(&path, dir.join(format!("{LOG_FILE}.1")));
    }
    let line = format!("supervise: {message} [{}]\n", iso(now));
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

// --- the watcher --------------------------------------------------------------------------------

struct Watched {
    pid: u32,
    start_ms: i64,
    since: Instant,
    /// AliveLong was decided for it.
    long: bool,
}

struct Pending {
    /// The pid that died.
    pid: u32,
    /// When it was seen dead (ms since the epoch): a lease for another pid written since then is a
    /// replacement session.
    died_ms: i64,
    due: Instant,
}

/// What the watcher asks of the computer, so the whole sequence can be tested without one.
struct System {
    process: Box<dyn Fn(u32) -> Option<Proc> + Send>,
    target: Box<dyn Fn() -> Option<PathBuf> + Send>,
    session_ending: Box<dyn Fn() -> bool + Send>,
    launch: Box<dyn FnMut(&Launch) -> std::io::Result<()> + Send>,
}

impl System {
    fn real() -> Self {
        Self {
            process: Box::new(process),
            target: Box::new(crate::desktop::realtimex_app),
            session_ending: Box::new(session_ending),
            launch: Box::new(|spec| launch(spec)),
        }
    }
}

/// While a relaunch waits: has another session taken the dead one's place? (The lease names
/// another pid and was written after the death.)
#[derive(Debug, PartialEq, Eq)]
enum Replacement {
    None,
    /// It already announced its exit (e.g. the person opened RealTimeX and then quit it).
    Intended { pid: u32, reason: String },
    /// It may be running: adopt it if it passes Q2.
    Candidate,
}

fn replacement(file: Option<&LeaseFile>, dead_pid: u32, died_ms: i64) -> Replacement {
    let Some(file) = file else { return Replacement::None };
    let Some(lease) = file.lease.as_ref().filter(|lease| lease.pid != dead_pid) else {
        return Replacement::None;
    };
    // Whole seconds, like lease_is_fresh: an older lease for another pid is not a replacement.
    if file.mtime_ms.div_euclid(1000) < died_ms.div_euclid(1000) {
        return Replacement::None;
    }
    match &lease.exit {
        // A replacement whose server went down too is no reason to cancel: the relaunch stands.
        Some(reason) if reason == BACKEND_DOWN => Replacement::None,
        Some(reason) => Replacement::Intended { pid: lease.pid, reason: reason.clone() },
        None => Replacement::Candidate,
    }
}

struct Watcher {
    dir: PathBuf,
    shared: Shared,
    sys: System,
    os: Os,
    memory: Memory,
    last: Value,
    /// The lease as last read, reused while its mtime and size stay the same.
    cache: Option<LeaseFile>,
    watched: Option<Watched>,
    pending: Option<Pending>,
    /// The pid last logged as not supervised: once per pid.
    refused: Option<u32>,
    /// The setting as last read: on until rtx-linkd says otherwise.
    setting: bool,
    /// The restart request answered (kept in the state file across a Link restart).
    ack: Option<Ack>,
    /// The request last declined (setting off or stopped): logged once.
    declined: Option<String>,
}

/// Start watching RealTimeX, from the process that holds the tray, for this computer's own Link
/// only (another instance never watches the installed app).
pub fn start(shared: Shared) {
    if !crate::api::is_default_instance() {
        return;
    }
    watch_session_end();
    let dir = crate::api::data_dir();
    let _ = std::thread::Builder::new()
        .name("supervise".into())
        .spawn(move || Watcher::new(dir, shared, System::real()).run());
}

impl Watcher {
    fn new(dir: PathBuf, shared: Shared, sys: System) -> Self {
        let (memory, last, ack) = load_state(&dir);
        Self {
            dir,
            shared,
            sys,
            os: Os::current(),
            memory,
            last,
            cache: None,
            watched: None,
            pending: None,
            refused: None,
            setting: true,
            ack,
            declined: None,
        }
    }

    fn run(mut self) {
        while self.tick() {
            std::thread::sleep(POLL);
        }
    }

    /// One look; false when this isn't the default instance after all (the thread ends).
    fn tick(&mut self) -> bool {
        let status = lock(&self.shared).status.clone();
        if let Some(status) = &status {
            if status.pointer("/instance/default") == Some(&Value::Bool(false)) {
                return false;
            }
            if let Some(on) = status.get("reopenRealtimex").and_then(Value::as_bool) {
                self.note_setting(on);
            }
        }
        if let Some(pending) = &self.pending {
            let due = Instant::now() >= pending.due;
            // Keep looking while the relaunch waits: a session started meanwhile cancels it.
            if !self.replaced() && due {
                self.due();
            }
            return true;
        }
        let Some(watched) = &mut self.watched else {
            // Link came back (KeepAlive) after answering a restart request: if that RealTimeX has
            // died meanwhile, the reopening is still owed.
            if let Some(ack) = self.ack.clone() {
                if let Some(due_ms) = ack.due_ms {
                    // Died and counted before Link restarted: the relaunch is still owed, once.
                    let wait = u64::try_from(due_ms - now_ms()).unwrap_or(0);
                    self.pending = Some(Pending {
                        pid: ack.pid,
                        // The real death time, never the deadline: a replacement started and quit
                        // during the wait (even while Link was down) must still cancel it.
                        died_ms: ack.died_ms.unwrap_or(ack.start_ms),
                        due: Instant::now() + Duration::from_millis(wait),
                    });
                    append_log(
                        &self.dir,
                        &format!("server down pid {}; relaunch still owed after a Link restart", ack.pid),
                        now_ms(),
                    );
                    return true;
                }
                if !(self.sys.process)(ack.pid).is_some_and(|p| p.start_ms == ack.start_ms) {
                    self.watched = Some(Watched {
                        pid: ack.pid,
                        start_ms: ack.start_ms,
                        since: Instant::now(),
                        long: false,
                    });
                    self.died();
                    return true;
                }
            }
            if let Some(file) = self.lease() {
                if let Some((lease, proc)) = self.passes(&file, None) {
                    self.adopt(&lease, &proc);
                }
            }
            return true;
        };
        // A start time that no longer matches is a death too (the pid was reused).
        if !(self.sys.process)(watched.pid).is_some_and(|p| p.start_ms == watched.start_ms) {
            self.died();
        } else {
            if !watched.long && watched.since.elapsed() >= ALIVE_RESET {
                watched.long = true;
                self.apply(&Event::AliveLong);
            }
            // While the app may act on Link's answer, notice the switch at once.
            if self.ack.as_ref().is_some_and(|ack| ack.due_ms.is_none()) {
                self.fresh_setting();
            }
            self.answer_restart_request();
        }
        true
    }

    /// The watched RealTimeX asks Link to take over (its server is down for good). Answer only
    /// with the setting on and no crash-loop stop; from then on Link owes the reopening.
    fn answer_restart_request(&mut self) {
        let Some(watched) = &self.watched else { return };
        let (pid, start_ms) = (watched.pid, watched.start_ms);
        let Some(nonce) = self
            .lease()
            .and_then(|file| file.lease)
            .filter(|lease| lease.pid == pid && lease.exit.is_none())
            .and_then(|lease| lease.restart)
        else {
            return;
        };
        if self.ack.as_ref().is_some_and(|ack| ack.pid == pid && ack.nonce == nonce) {
            return;
        }
        let setting = self.fresh_setting();
        // The app never closes itself unless Link will reopen it: not when this exit would be the
        // one that reaches the crash-loop stop.
        let would_stop = matches!(
            decide(&self.memory, &Event::Unannounced, now_ms(), true).0,
            Decision::Stopped { .. }
        );
        if !setting || would_stop {
            if self.declined.as_deref() != Some(nonce.as_str()) {
                self.declined = Some(nonce);
                let why = if !setting {
                    "setting off"
                } else if self.memory.stopped {
                    "crash loop, stopped"
                } else {
                    "a reopening now would reach the crash-loop limit"
                };
                self.record(
                    pid,
                    "declined",
                    why,
                    &format!("server down pid {pid}; {why}, RealTimeX keeps its notice"),
                );
            }
            return;
        }
        self.ack = Some(Ack { pid, nonce, start_ms, due_ms: None, died_ms: None });
        self.record(
            pid,
            "taking-over",
            BACKEND_DOWN,
            &format!("server down pid {pid}; RealTimeX asked to be reopened, taking over"),
        );
    }

    /// The setting from rtx-linkd; a turn is a decision of its own (off then on resets).
    fn note_setting(&mut self, on: bool) {
        let changed = self.setting != on;
        self.setting = on;
        // Turned off before the app acted on Link's answer: withdraw it, so the app keeps its
        // notice (the switch wins over a pending agreement).
        if !on && self.ack.as_ref().is_some_and(|ack| ack.due_ms.is_none()) {
            let ack = self.ack.take();
            if let Some(ack) = ack {
                self.record(
                    ack.pid,
                    "withdrawn",
                    "setting off",
                    &format!("server down pid {}; setting off, answer withdrawn", ack.pid),
                );
            }
        }
        if changed {
            self.save();
        }
        if self.memory.setting != Some(on) {
            self.apply(&Event::Setting);
        }
    }

    /// Read again just before a launch; when rtx-linkd doesn't answer, the last value.
    fn fresh_setting(&mut self) -> bool {
        let api = lock(&self.shared).api.clone();
        if let Some(on) = api
            .and_then(|api| api.get("/v1/status").ok())
            .and_then(|status| status.get("reopenRealtimex").and_then(Value::as_bool))
        {
            self.note_setting(on);
        }
        self.setting
    }

    /// Decide, keep the memory, and write the state when it changed.
    fn apply(&mut self, event: &Event) -> Decision {
        let (decision, memory) = decide(&self.memory, event, now_ms(), self.setting);
        if memory != self.memory {
            let persisted = memory.stopped != self.memory.stopped || memory.exits != self.memory.exits;
            self.memory = memory;
            if persisted {
                self.save();
            }
        }
        decision
    }

    fn save(&self) {
        if FORGOTTEN.load(Ordering::SeqCst) {
            return;
        }
        let watching = self.watched.as_ref().map(|watched| watched.pid);
        let text =
            state_json(&self.memory, &self.last, self.setting, watching, self.ack.as_ref()).to_string();
        if let Err(error) = write_atomically(&self.dir.join(STATE_FILE), text.as_bytes()) {
            eprintln!("supervise: couldn't write {STATE_FILE}: {error}");
        }
    }

    /// A decision for the log and the state's `last`.
    fn record(&mut self, pid: u32, decision: &str, reason: &str, message: &str) {
        let now = now_ms();
        append_log(&self.dir, message, now);
        self.last = json!({ "at": iso(now), "pid": pid, "decision": decision, "reason": reason });
        self.save();
    }

    fn lease(&mut self) -> Option<LeaseFile> {
        let meta = std::fs::metadata(self.dir.join(LEASE_FILE)).ok()?;
        let unchanged = self.cache.as_ref().is_some_and(|cached| {
            Some(cached.mtime_ms) == modified_ms(&meta) && cached.size == meta.len()
        });
        if !unchanged {
            self.cache = read_lease(&self.dir);
        }
        self.cache.clone()
    }

    /// The lease's pid when it passes Q2 (and isn't `except`): alive, the installed RealTimeX's
    /// main executable, and no older than the lease. A mismatch is logged once per pid.
    fn passes(&mut self, file: &LeaseFile, except: Option<u32>) -> Option<(Lease, Proc)> {
        let lease = file.lease.clone().filter(|lease| Some(lease.pid) != except)?;
        let proc = (self.sys.process)(lease.pid)?;
        let target = (self.sys.target)()?;
        let expected = expected_executable(self.os, &target);
        let why = if !same_executable(&proc.exe, &expected, ignore_case(self.os)) {
            format!("{} is not the installed RealTimeX", proc.exe.display())
        } else if !lease_is_fresh(file.mtime_ms, proc.start_ms) {
            format!("{LEASE_FILE} is older than pid {}", lease.pid)
        } else {
            return Some((lease, proc));
        };
        if self.refused != Some(lease.pid) {
            self.refused = Some(lease.pid);
            self.record(lease.pid, "not-supervised", &why, &format!("not supervised: {why}"));
        }
        None
    }

    fn adopt(&mut self, lease: &Lease, proc: &Proc) {
        self.watched = Some(Watched {
            pid: lease.pid,
            start_ms: proc.start_ms,
            since: Instant::now(),
            long: false,
        });
        self.refused = None;
        if self.ack.as_ref().is_some_and(|ack| ack.pid != lease.pid) {
            self.ack = None;
        }
        self.save();
        let version = lease.version.as_deref().unwrap_or("unknown");
        append_log(
            &self.dir,
            &format!("watching RealTimeX pid {} (version {version})", lease.pid),
            now_ms(),
        );
        self.apply(&Event::Adopted);
    }

    /// The watched pid is gone (or its start time changed).
    fn died(&mut self) {
        let Some(watched) = self.watched.take() else { return };
        let pid = watched.pid;
        // An answer for this pid stays (counted, with its due time) until the relaunch runs; any
        // other answer is settled by this death.
        let acked = self.ack.as_ref().is_some_and(|ack| ack.pid == pid && ack.due_ms.is_none());
        if !acked {
            self.ack = None;
        }
        // Read again, past the cache: the announcement may be the last write before the exit.
        self.cache = read_lease(&self.dir);
        let announced = self
            .cache
            .as_ref()
            .and_then(|file| file.lease.as_ref())
            .filter(|lease| lease.pid == pid)
            .and_then(|lease| lease.exit.clone());
        // Its server down for good: reopened like a crash, but named for what it was.
        let what = match announced {
            Some(reason) if reason == BACKEND_DOWN => format!("server down pid {pid}"),
            // Answered, and it died before it could announce: still owed.
            None if acked => format!("server down pid {pid}"),
            Some(reason) => {
                self.ack = None;
                self.apply(&Event::Intended);
                self.record(pid, "intended", &reason, &format!("intended exit pid {pid} ({reason})"));
                return;
            }
            None => format!("unannounced exit pid {pid}"),
        };
        let died_ms = now_ms();
        let decision = self.apply(&Event::Unannounced);
        match &decision {
            Decision::Relaunch { delay_secs, .. } => {
                if let Some(ack) = self.ack.as_mut() {
                    ack.died_ms = Some(died_ms);
                    ack.due_ms = Some(died_ms + (*delay_secs as i64) * 1000);
                }
            }
            _ => self.ack = None,
        }
        match decision {
            Decision::SettingOff => self.record(
                pid,
                "setting-off",
                "",
                &format!("{what}; setting off, no relaunch"),
            ),
            Decision::Relaunch { delay_secs, nth } => {
                let reason = format!("relaunch in {delay_secs}s ({nth} of 3)");
                self.record(pid, "relaunch", &reason, &format!("{what} → {reason}"));
                self.pending = Some(Pending {
                    pid,
                    died_ms,
                    due: Instant::now() + Duration::from_secs(delay_secs),
                });
            }
            Decision::Stopped { count } => {
                let reason = format!("{count} unannounced exits in 10 min");
                self.record(
                    pid,
                    "stopped",
                    &reason,
                    &format!("{what}; crash loop, stopped ({reason})"),
                );
            }
            _ => {}
        }
    }

    /// While a relaunch waits, a session that took the dead one's place cancels it: one that has
    /// already announced its exit counts as an intended exit (the count starts again), and one
    /// that passes Q2 is adopted. True when the pending relaunch was cancelled.
    fn replaced(&mut self) -> bool {
        let Some(pending) = &self.pending else { return false };
        let (dead, died_ms) = (pending.pid, pending.died_ms);
        let file = self.lease();
        match replacement(file.as_ref(), dead, died_ms) {
            Replacement::None => false,
            Replacement::Intended { pid, reason } => {
                self.pending = None;
                self.ack = None;
                self.apply(&Event::Intended);
                self.record(
                    pid,
                    "intended",
                    &reason,
                    &format!("intended exit pid {pid} ({reason}); relaunch cancelled"),
                );
                true
            }
            Replacement::Candidate => {
                let Some((lease, proc)) = file.and_then(|file| self.passes(&file, Some(dead))) else {
                    return false;
                };
                self.pending = None;
                self.ack = None;
                self.record(
                    lease.pid,
                    "already-running",
                    "",
                    &format!("already running (pid {}), no relaunch", lease.pid),
                );
                self.adopt(&lease, &proc);
                true
            }
        }
    }

    /// The wait is over: check again in order, then launch.
    fn due(&mut self) {
        // Past the cache: the replacement may have written just now.
        self.cache = read_lease(&self.dir);
        if self.replaced() {
            return;
        }
        let Some(pending) = self.pending.take() else { return };
        // The owed relaunch is decided now, whatever the outcome.
        self.ack = None;
        let setting = self.fresh_setting();
        self.cache = read_lease(&self.dir);
        let other = self
            .cache
            .clone()
            .and_then(|file| self.passes(&file, Some(pending.pid)));
        let target = (self.sys.target)();
        let event = Event::Due {
            session_ending: (self.sys.session_ending)(),
            other_live: other.as_ref().map(|(lease, _)| lease.pid),
            installed: target.is_some(),
        };
        let (decision, _) = decide(&self.memory, &event, now_ms(), setting);
        let pid = pending.pid;
        match decision {
            Decision::SessionEnding => {
                self.record(pid, "session-ending", "", "session ending, no relaunch")
            }
            Decision::SettingOff => self.record(pid, "setting-off", "", "setting off, no relaunch"),
            Decision::AlreadyRunning(running) => {
                self.record(
                    running,
                    "already-running",
                    "",
                    &format!("already running (pid {running}), no relaunch"),
                );
                if let Some((lease, proc)) = other {
                    self.adopt(&lease, &proc);
                }
            }
            Decision::NotInstalled => {
                self.record(pid, "not-installed", "", "not installed, no relaunch")
            }
            Decision::Launch => {
                let Some(target) = target else { return };
                let spec = launch_command(self.os, &target);
                match (self.sys.launch)(&spec) {
                    Ok(()) => self.record(pid, "relaunch", "relaunched", "relaunched RealTimeX"),
                    Err(error) => self.record(
                        pid,
                        "relaunch",
                        "failed",
                        &format!("relaunch failed: {error}"),
                    ),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60 * 1000;

    /// A folder of its own under the system's temporary folder, removed when dropped (rtx-link has
    /// no tempfile dependency).
    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rtx-link-supervise-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn lease_json(body: Value) -> Vec<u8> {
        body.to_string().into_bytes()
    }

    fn good() -> Value {
        json!({ "schema": 1, "capabilities": ["clean-exit"], "pid": 4242, "sessionId": "x",
                "version": "1.4.2", "startedAt": "2026-11-10T03:00:00.000Z", "exit": null })
    }

    #[test]
    fn a_lease_needs_schema_1_clean_exit_and_a_real_pid() {
        let lease = parse_lease(&lease_json(good())).unwrap();
        assert_eq!(
            lease,
            Lease { pid: 4242, version: Some("1.4.2".into()), exit: None, restart: None }
        );
        let mut announced = good();
        announced["exit"] = json!({ "reason": "quit", "at": "2026-11-10T03:01:00.000Z" });
        assert_eq!(parse_lease(&lease_json(announced)).unwrap().exit.as_deref(), Some("quit"));
        // No reason, or one that isn't text: still an announcement.
        let mut odd = good();
        odd["exit"] = json!({ "reason": 7 });
        assert_eq!(parse_lease(&lease_json(odd)).unwrap().exit.as_deref(), Some("unknown"));

        let refused = |change: &dyn Fn(&mut Value)| {
            let mut body = good();
            change(&mut body);
            parse_lease(&lease_json(body))
        };
        assert!(refused(&|b| b["schema"] = json!(2)).is_none());
        assert!(refused(&|b| b["schema"] = json!("1")).is_none());
        assert!(refused(&|b| { b.as_object_mut().unwrap().remove("schema"); }).is_none());
        assert!(refused(&|b| b["capabilities"] = json!([])).is_none());
        assert!(refused(&|b| b["capabilities"] = json!(["other"])).is_none());
        assert!(refused(&|b| b["capabilities"] = json!("clean-exit")).is_none());
        for pid in [json!(0), json!(1), json!(-5), json!(42.5), json!("4242"), json!(null), json!(u64::MAX)] {
            assert!(refused(&|b| b["pid"] = pid.clone()).is_none(), "{pid}");
        }
        assert!(parse_lease(b"not json").is_none());
        assert!(parse_lease(b"[1]").is_none());
    }

    #[test]
    fn a_lease_above_4_kb_is_refused_and_log_text_is_cleaned() {
        let mut body = good();
        body["padding"] = json!("x".repeat(4096));
        let bytes = lease_json(body);
        assert!(bytes.len() > 4096);
        assert!(parse_lease(&bytes).is_none());
        // Exactly at the cap still parses.
        let mut fits = good();
        let room = 4096 - lease_json(fits.clone()).len() - r#","p":"""#.len();
        fits["p"] = json!("y".repeat(room));
        let bytes = lease_json(fits);
        assert_eq!(bytes.len(), 4096);
        assert!(parse_lease(&bytes).is_some());
        // The file reader caps it too.
        let dir = tempdir();
        let mut big = good();
        big["padding"] = json!("x".repeat(8000));
        std::fs::write(dir.path().join(LEASE_FILE), lease_json(big)).unwrap();
        let file = read_lease(dir.path()).unwrap();
        assert!(file.lease.is_none());
        assert!(file.size > 4096);
        std::fs::write(dir.path().join(LEASE_FILE), lease_json(good())).unwrap();
        assert_eq!(read_lease(dir.path()).unwrap().lease.unwrap().pid, 4242);
        // A version or reason never brings a new line or anything odd into the log.
        let mut sneaky = good();
        sneaky["version"] = json!("1.0\nsupervise: relaunched RealTimeX");
        sneaky["exit"] = json!({ "reason": "quit\r\n(x)" });
        let lease = parse_lease(&lease_json(sneaky)).unwrap();
        assert_eq!(lease.version.as_deref(), Some("1.0superviserelaunchedRealTimeX"));
        assert_eq!(lease.exit.as_deref(), Some("quitx"));
    }

    #[test]
    fn a_lease_older_than_the_process_never_adopts_it() {
        let start = 1_762_743_600_123;
        assert!(lease_is_fresh(start, start));
        assert!(lease_is_fresh(start + 5_000, start));
        // The same second: a start known to the second (Linux) is not after the write.
        assert!(lease_is_fresh(start - 100, start));
        // A file left behind by an earlier process with the same (reused) pid.
        assert!(!lease_is_fresh(start - 1_000, start));
        assert!(!lease_is_fresh(start - 60 * MIN, start));
    }

    #[test]
    fn the_bundles_executable_comes_from_info_plist() {
        let plist = "<dict>\n\t<key>CFBundleExecutable</key>\n\t<string>RealTimeX.AI</string>\n</dict>";
        assert_eq!(bundle_executable(Some(plist)), "RealTimeX.AI");
        let renamed = plist.replace("<string>RealTimeX.AI</string>", "<string>RealTimeX Beta</string>");
        assert_eq!(bundle_executable(Some(&renamed)), "RealTimeX Beta");
        // Unreadable, binary, or a name that isn't one file's: RealTimeX.AI.
        assert_eq!(bundle_executable(None), "RealTimeX.AI");
        assert_eq!(bundle_executable(Some("bplist00\u{1}\u{2}")), "RealTimeX.AI");
        for bad in ["../../bin/sh", "", "..", "a/b"] {
            let text = plist.replace("RealTimeX.AI", bad);
            assert_eq!(bundle_executable(Some(&text)), "RealTimeX.AI", "{bad}");
        }
        let bundle = Path::new("/Applications/RealTimeX.AI.app");
        assert_eq!(
            expected_executable(Os::MacOs, bundle),
            PathBuf::from("/Applications/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI")
        );
        let exe = Path::new("/opt/RealTimeX.AI/RealTimeX.AI");
        assert_eq!(expected_executable(Os::Linux, exe), exe);
        assert_eq!(expected_executable(Os::Windows, exe), exe);
    }

    #[test]
    fn only_the_installed_executable_matches() {
        // Paths that don't exist, so they are compared as written (an installed RealTimeX on a
        // case-insensitive disk would resolve both spellings to one file).
        let installed = Path::new("/rtx-none/Applications/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI");
        assert!(same_executable(installed, installed, true));
        // macOS and Windows ignore case; Linux doesn't.
        let shouted = Path::new("/rtx-none/applications/REALTIMEX.AI.app/Contents/MacOS/realtimex.ai");
        assert!(same_executable(shouted, installed, true));
        assert!(!same_executable(shouted, installed, false));
        let windows = Path::new(r"C:\Users\a\AppData\Local\Programs\RealTimeX.AI\RealTimeX.AI.exe");
        let windows_upper = Path::new(r"C:\USERS\A\APPDATA\LOCAL\PROGRAMS\REALTIMEX.AI\REALTIMEX.AI.EXE");
        assert!(same_executable(windows, windows_upper, true));
        // App Translocation, a dev Electron, a copy elsewhere: not the installed one.
        for other in [
            "/rtx-none/private/var/folders/x/T/AppTranslocation/1234-ABCD/d/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI",
            "/rtx-none/Users/a/realtimex/node_modules/electron/dist/Electron.app/Contents/MacOS/Electron",
            "/rtx-none/Users/a/Downloads/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI",
            "/rtx-none/Applications/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI Helper",
        ] {
            assert!(!same_executable(Path::new(other), installed, true), "{other}");
        }
        // Links are followed when both exist.
        let dir = tempdir();
        let real = dir.path().join("RealTimeX.AI");
        std::fs::write(&real, b"").unwrap();
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(same_executable(&link, &real, false));
        }
    }

    #[test]
    fn the_launch_has_no_arguments_and_a_clean_environment() {
        let bundle = Path::new("/Applications/RealTimeX.AI.app");
        let mac = launch_command(Os::MacOs, bundle);
        assert_eq!(mac.program, PathBuf::from("/usr/bin/open"));
        // -n, or LaunchServices may only activate a browser instance of the same bundle.
        assert_eq!(mac.args, vec![OsString::from("-n"), "-a".into(), bundle.as_os_str().into()]);
        assert_eq!(mac.cwd, None);

        let exe = Path::new(r"C:\Program Files\RealTimeX.AI\RealTimeX.AI.exe");
        let windows = launch_command(Os::Windows, exe);
        assert_eq!(windows.program, exe);
        assert!(windows.args.is_empty());
        assert_eq!(windows.cwd.as_deref(), exe.parent());

        let linux_exe = Path::new("/opt/RealTimeX.AI/RealTimeX.AI");
        let linux = launch_command(Os::Linux, linux_exe);
        assert_eq!(linux.program, linux_exe);
        assert!(linux.args.is_empty());

        for spec in [&mac, &windows, &linux] {
            for name in ["ELECTRON_RUN_AS_NODE", "NODE_OPTIONS", "ELECTRON_ENABLE_LOGGING", "NODE_PATH"] {
                assert!(spec.env_removed.contains(&name), "{name}");
            }
        }
    }

    fn unannounced(memory: &Memory, now: i64) -> (Decision, Memory) {
        decide(memory, &Event::Unannounced, now, true)
    }

    #[test]
    fn back_off_is_3_10_30_then_stop_at_the_4th() {
        let t0 = 1_762_743_600_000;
        let m = Memory::default();
        let (d1, m) = unannounced(&m, t0);
        assert_eq!(d1, Decision::Relaunch { delay_secs: 3, nth: 1 });
        // A session Link started doesn't reset the count while watching.
        let (adopted, m) = decide(&m, &Event::Adopted, t0 + 5_000, true);
        assert_eq!(adopted, Decision::Watching);
        assert_eq!(m.exits.len(), 1);
        let (d2, m) = unannounced(&m, t0 + MIN);
        assert_eq!(d2, Decision::Relaunch { delay_secs: 10, nth: 2 });
        let (d3, m) = unannounced(&m, t0 + 2 * MIN);
        assert_eq!(d3, Decision::Relaunch { delay_secs: 30, nth: 3 });
        let (d4, m) = unannounced(&m, t0 + 3 * MIN);
        assert_eq!(d4, Decision::Stopped { count: 4 });
        assert!(m.stopped);
        // Stopped stays stopped.
        let (d5, m) = unannounced(&m, t0 + 4 * MIN);
        assert_eq!(d5, Decision::Stopped { count: 5 });
        assert!(m.stopped);
    }

    #[test]
    fn the_10_minute_window_slides() {
        let t0 = 1_762_743_600_000;
        let (_, m) = unannounced(&Memory::default(), t0);
        let (_, m) = unannounced(&m, t0 + 4 * MIN);
        let (_, m) = unannounced(&m, t0 + 8 * MIN);
        // The first one is 10 minutes old now: out of the window, so this is the 3rd, not the 4th.
        let (d, m) = unannounced(&m, t0 + 10 * MIN);
        assert_eq!(d, Decision::Relaunch { delay_secs: 30, nth: 3 });
        assert!(!m.stopped);
        // Just inside the window: the 4th.
        let (d, m) = unannounced(&m, t0 + 13 * MIN);
        assert_eq!(d, Decision::Stopped { count: 4 });
        assert!(m.stopped);
        // Times from the future (a clock set back) don't count.
        let future = Memory { exits: vec![t0 + 60 * MIN; 3], ..Memory::default() };
        assert_eq!(unannounced(&future, t0).0, Decision::Relaunch { delay_secs: 3, nth: 1 });
    }

    #[test]
    fn every_reset_starts_the_count_again() {
        let t0 = 1_762_743_600_000;
        let three = Memory { exits: vec![t0, t0 + 1, t0 + 2], setting: Some(true), ..Memory::default() };
        let stopped = Memory { stopped: true, exits: vec![t0; 4], setting: Some(true) };
        let fresh_start = |m: &Memory| {
            assert!(!m.stopped);
            assert!(m.exits.is_empty());
            assert_eq!(unannounced(m, t0 + 3).0, Decision::Relaunch { delay_secs: 3, nth: 1 });
        };
        // An intended exit.
        for m in [&three, &stopped] {
            let (d, next) = decide(m, &Event::Intended, t0 + 3, true);
            assert_eq!(d, Decision::Intended);
            fresh_start(&next);
        }
        // A session alive for 10 minutes.
        fresh_start(&decide(&three, &Event::AliveLong, t0 + 3, true).1);
        // The setting turned off, then on.
        let (_, off) = decide(&stopped, &Event::Setting, t0 + 3, false);
        assert!(off.stopped, "turning off alone doesn't reset");
        let (_, on) = decide(&off, &Event::Setting, t0 + 3, true);
        assert_eq!(on.setting, Some(true));
        fresh_start(&on);
        // On, and on again: nothing changes.
        assert_eq!(decide(&stopped, &Event::Setting, t0 + 3, true).1, stopped);
        // The first read (Link just started) isn't a turn.
        let first = Memory { setting: None, ..stopped.clone() };
        assert!(decide(&first, &Event::Setting, t0 + 3, true).1.stopped);
        // While stopped, a new session (started normally).
        let (d, next) = decide(&stopped, &Event::Adopted, t0 + 3, true);
        assert_eq!(d, Decision::Watching);
        fresh_start(&next);
    }

    #[test]
    fn setting_off_and_the_rechecks_come_in_order() {
        let t0 = 1_762_743_600_000;
        let m = Memory::default();
        // Off at the exit: no relaunch, and it isn't counted.
        let (d, next) = decide(&m, &Event::Unannounced, t0, false);
        assert_eq!(d, Decision::SettingOff);
        assert!(next.exits.is_empty());
        let due = |session_ending, other_live, installed, setting| {
            decide(&m, &Event::Due { session_ending, other_live, installed }, t0, setting).0
        };
        assert_eq!(due(false, None, true, true), Decision::Launch);
        assert_eq!(due(true, Some(7), false, false), Decision::SessionEnding);
        assert_eq!(due(false, Some(7), false, false), Decision::SettingOff);
        assert_eq!(due(false, Some(7), false, true), Decision::AlreadyRunning(7));
        assert_eq!(due(false, None, false, true), Decision::NotInstalled);
    }

    #[test]
    fn the_state_survives_a_restart_and_the_log_rotates() {
        let dir = tempdir();
        let t0 = 1_762_743_600_000;
        let memory = Memory { stopped: true, exits: vec![t0, t0 + MIN], setting: Some(true) };
        let last = json!({ "at": iso(t0), "pid": 4242, "decision": "stopped", "reason": "4 unannounced exits in 10 min" });
        let text = state_json(&memory, &last, true, None, None).to_string();
        write_atomically(&dir.path().join(STATE_FILE), text.as_bytes()).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(written["schema"], 1);
        assert_eq!(written["state"], "stopped");
        assert_eq!(written["exits"][0], "2025-11-10T03:00:00.000Z");
        let (loaded, loaded_last, _) = load_state(dir.path());
        assert_eq!(loaded, Memory { setting: None, ..memory });
        assert_eq!(loaded_last, last);
        // No temporary file is left.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        // Unreadable: afresh.
        assert_eq!(memory_of(Some(&json!({ "schema": 2, "state": "stopped" }))).0, Memory::default());

        append_log(dir.path(), "watching RealTimeX pid 4242 (version 1.4.2)", t0);
        let log = std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap();
        assert!(log.starts_with("supervise: watching RealTimeX pid 4242 (version 1.4.2) ["));
        std::fs::write(dir.path().join(LOG_FILE), vec![b'x'; LOG_LIMIT as usize]).unwrap();
        append_log(dir.path(), "relaunched RealTimeX", t0);
        assert_eq!(std::fs::metadata(dir.path().join(format!("{LOG_FILE}.1"))).unwrap().len(), LOG_LIMIT);
        let log = std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap();
        assert!(log.starts_with("supervise: relaunched RealTimeX ["));
    }

    /// A watcher on a pretend computer: its processes, the installed target and its launches.
    struct Pretend {
        dir: TempDir,
        procs: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u32, Proc>>>,
        launches: std::sync::Arc<std::sync::Mutex<usize>>,
        exe: PathBuf,
        target: PathBuf,
        watcher: Watcher,
    }

    impl Pretend {
        fn new() -> Self {
            use std::sync::{Arc, Mutex};
            let dir = tempdir();
            // Paths that don't exist, so the target is compared as written.
            let target: PathBuf = if cfg!(target_os = "macos") {
                PathBuf::from("/rtx-none/Applications/RealTimeX.AI.app")
            } else {
                PathBuf::from("/rtx-none/opt/RealTimeX.AI/RealTimeX.AI")
            };
            let exe = expected_executable(Os::current(), &target);
            let procs: Arc<Mutex<std::collections::HashMap<u32, Proc>>> = Arc::default();
            let launches = Arc::new(Mutex::new(0usize));
            let shared: Shared = Arc::new(Mutex::new(crate::app::Snapshot::default()));
            lock(&shared).status = Some(json!({ "reopenRealtimex": true, "instance": { "default": true } }));
            let sys = Self::system(&procs, &launches, target.clone());
            let watcher = Watcher::new(dir.path().to_path_buf(), shared, sys);
            Self { dir, procs, launches, exe, target, watcher }
        }

        fn system(
            procs: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u32, Proc>>>,
            launches: &std::sync::Arc<std::sync::Mutex<usize>>,
            target: PathBuf,
        ) -> System {
            System {
                process: {
                    let procs = procs.clone();
                    Box::new(move |pid| procs.lock().unwrap().get(&pid).cloned())
                },
                target: Box::new(move || Some(target.clone())),
                session_ending: Box::new(|| false),
                launch: {
                    let launches = launches.clone();
                    Box::new(move |_| {
                        *launches.lock().unwrap() += 1;
                        Ok(())
                    })
                },
            }
        }

        /// Link dies and comes back (KeepAlive): a new watcher on the same data folder.
        fn restart_link(&mut self) {
            let shared = self.watcher.shared.clone();
            let sys = Self::system(&self.procs, &self.launches, self.target.clone());
            self.watcher = Watcher::new(self.dir.path().to_path_buf(), shared, sys);
        }

        /// RealTimeX asks Link to take over: its server is down for good.
        fn request(&self, pid: u32, nonce: &str) {
            let mut body = good();
            body["pid"] = json!(pid);
            body["restart"] = json!({ "reason": BACKEND_DOWN, "nonce": nonce, "at": iso(now_ms()) });
            std::fs::write(self.dir.path().join(LEASE_FILE), lease_json(body)).unwrap();
        }

        fn state(&self) -> Value {
            serde_json::from_str(&std::fs::read_to_string(self.dir.path().join(STATE_FILE)).unwrap()).unwrap()
        }

        /// RealTimeX `pid` runs (started a few seconds ago) and has written its lease.
        fn start(&mut self, pid: u32) {
            let proc = Proc { start_ms: now_ms() - 5_000, exe: self.exe.clone() };
            self.procs.lock().unwrap().insert(pid, proc);
            self.lease(pid, None);
        }

        fn lease(&self, pid: u32, exit: Option<&str>) {
            let mut body = good();
            body["pid"] = json!(pid);
            if let Some(reason) = exit {
                body["exit"] = json!({ "reason": reason, "at": iso(now_ms()) });
            }
            std::fs::write(self.dir.path().join(LEASE_FILE), lease_json(body)).unwrap();
        }

        fn kill(&self, pid: u32) {
            self.procs.lock().unwrap().remove(&pid);
        }

        /// The relaunch's wait is over now, if one waits.
        fn expire_if_pending(&mut self) {
            if let Some(pending) = self.watcher.pending.as_mut() {
                pending.due = Instant::now();
            }
        }

        /// The relaunch's wait is over now.
        fn expire(&mut self) {
            self.watcher.pending.as_mut().expect("a relaunch waits").due = Instant::now();
        }

        fn launches(&self) -> usize {
            *self.launches.lock().unwrap()
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.path().join(LOG_FILE)).unwrap_or_default()
        }
    }

    #[test]
    fn a_crash_with_nothing_after_it_relaunches_once_the_wait_is_over() {
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        assert_eq!(pc.watcher.watched.as_ref().map(|w| w.pid), Some(100));
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some());
        pc.watcher.tick();
        assert_eq!(pc.launches(), 0, "not before the wait is over");
        pc.expire();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 1);
        assert!(pc.log().contains("relaunched RealTimeX"));
    }

    #[test]
    fn a_session_started_and_quit_during_the_back_off_cancels_the_relaunch() {
        // Review (loop-issue-3241): crash -> relaunch waits -> the person opens RealTimeX -> Quit
        // before the wait is over. That Quit must be honoured: no launch, and the count restarts.
        let mut pc = Pretend::new();
        pc.watcher.memory.exits = vec![now_ms() - 60_000];
        pc.start(100);
        pc.watcher.tick();
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some(), "the 2nd crash waits 10 s");
        assert_eq!(pc.watcher.memory.exits.len(), 2);
        // The person opens RealTimeX: the next look adopts it and cancels the relaunch.
        pc.start(200);
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_none());
        assert_eq!(pc.watcher.watched.as_ref().map(|w| w.pid), Some(200));
        assert!(pc.log().contains("already running (pid 200), no relaunch"));
        // ...and quits it.
        pc.lease(200, Some("quit"));
        pc.kill(200);
        pc.watcher.tick();
        for _ in 0..3 {
            pc.watcher.tick();
        }
        assert_eq!(pc.launches(), 0);
        assert!(pc.log().contains("intended exit pid 200 (quit)"));
        assert!(pc.watcher.memory.exits.is_empty(), "an intended exit starts the count again");
        assert!(!pc.watcher.memory.stopped);
    }

    #[test]
    fn a_session_that_quit_between_two_looks_still_cancels_the_relaunch() {
        // Opened and quit within one poll: Link never saw it alive, but its lease says it quit.
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some());
        pc.lease(200, Some("quit"));
        pc.expire();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 0);
        assert!(pc.watcher.pending.is_none());
        assert!(pc.log().contains("intended exit pid 200 (quit); relaunch cancelled"));
        assert!(pc.watcher.memory.exits.is_empty());
    }

    #[test]
    fn a_server_down_exit_is_reopened_like_a_crash_and_the_app_can_see_link_watching() {
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        // RealTimeX reads these before it quits for a server that stays down.
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(pc.dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state["enabled"], true);
        assert_eq!(state["state"], "watching");
        assert_eq!(state["watching"]["pid"], 100);
        pc.lease(100, Some(BACKEND_DOWN));
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.log().contains("server down pid 100 → relaunch in 3s (1 of 3)"));
        assert_eq!(pc.watcher.memory.exits.len(), 1, "it counts toward the crash loop");
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(pc.dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state["watching"], Value::Null);
        pc.expire();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 1);
        // The setting turned off is in the file at once.
        lock(&pc.watcher.shared).status = Some(json!({ "reopenRealtimex": false }));
        pc.watcher.tick();
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(pc.dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state["enabled"], false);
    }

    #[test]
    fn a_restart_request_is_answered_only_by_a_watcher_that_will_reopen() {
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        let state = pc.state();
        assert_eq!(state["ack"]["pid"], 100);
        assert_eq!(state["ack"]["nonce"], "n-1");
        assert!(pc.log().contains("server down pid 100; RealTimeX asked to be reopened, taking over"));
        // The app announces and quits; Link reopens it like a crash.
        pc.lease(100, Some(BACKEND_DOWN));
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.log().contains("server down pid 100 → relaunch in 3s (1 of 3)"));
        assert!(pc.state()["ack"]["dueMs"].is_i64(), "owed until the relaunch runs");
        pc.expire();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 1);
        assert_eq!(pc.state()["ack"], Value::Null, "settled once it ran");
    }

    #[test]
    fn with_the_setting_off_or_stopped_the_request_is_declined_and_the_app_keeps_its_notice() {
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        lock(&pc.watcher.shared).status = Some(json!({ "reopenRealtimex": false }));
        pc.request(100, "n-1");
        pc.watcher.tick();
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"], Value::Null);
        assert_eq!(pc.log().matches("setting off, RealTimeX keeps its notice").count(), 1, "logged once");
        // Stopped after a crash loop: declined too. (A fresh watcher: turning the setting off and
        // on again would itself reset the stop.)
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.watcher.memory.stopped = true;
        pc.request(100, "n-2");
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"], Value::Null);
        assert!(pc.log().contains("crash loop, stopped, RealTimeX keeps its notice"));
        // Three exits already in the window: this one would be the 4th, so Link wouldn't reopen
        // it, and it doesn't let the app close.
        let mut pc = Pretend::new();
        let now = now_ms();
        pc.watcher.memory.exits = vec![now - 3 * 60_000, now - 2 * 60_000, now - 60_000];
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-3");
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"], Value::Null);
        assert!(pc.log().contains("a reopening now would reach the crash-loop limit, RealTimeX keeps its notice"));
        // Two in the window: the 3rd is still reopened, so it answers.
        let mut pc = Pretend::new();
        pc.watcher.memory.exits = vec![now - 2 * 60_000, now - 60_000];
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-4");
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"]["nonce"], "n-4");
    }

    #[test]
    fn link_dying_across_the_handoff_still_reopens_the_app() {
        // Link answers, then dies; the app quits while Link is down; KeepAlive brings Link back.
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        pc.lease(100, Some(BACKEND_DOWN));
        pc.kill(100);
        pc.restart_link();
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some(), "the owed reopening survived the restart");
        assert!(pc.log().contains("server down pid 100 → relaunch in 3s (1 of 3)"));
        pc.expire();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 1);
        // Died before it could announce: still owed.
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        pc.kill(100);
        pc.restart_link();
        pc.watcher.tick();
        assert!(pc.log().contains("server down pid 100 → relaunch"));
        // Link back while the app still runs: it is adopted again, and the answer stands.
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        pc.restart_link();
        pc.watcher.tick();
        assert_eq!(pc.watcher.watched.as_ref().map(|w| w.pid), Some(100));
        assert_eq!(pc.state()["ack"]["nonce"], "n-1");
    }

    #[test]
    fn link_restarting_during_the_wait_still_reopens_once_and_counts_once() {
        // Review: Link answers, the app exits with backend-down, Link sees the death and waits;
        // then Link crashes and comes back during the wait.
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        pc.lease(100, Some(BACKEND_DOWN));
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some());
        assert_eq!(pc.watcher.memory.exits.len(), 1);
        assert!(pc.state()["ack"]["dueMs"].is_i64(), "the owed relaunch is on disk");
        pc.restart_link();
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some(), "restored");
        assert_eq!(pc.watcher.memory.exits.len(), 1, "not counted again");
        assert!(pc.log().contains("relaunch still owed after a Link restart"));
        pc.expire();
        pc.watcher.tick();
        for _ in 0..3 {
            pc.watcher.tick();
        }
        assert_eq!(pc.launches(), 1, "exactly once");
        assert_eq!(pc.watcher.memory.exits.len(), 1);
        assert_eq!(pc.state()["ack"], Value::Null, "settled once it ran");
        // Restarting again now owes nothing.
        pc.restart_link();
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_none());
    }

    /// Answered, the app exited with backend-down, Link saw the death and waits 30 s (the 3rd).
    fn owed_and_waiting() -> Pretend {
        let mut pc = Pretend::new();
        let now = now_ms();
        pc.watcher.memory.exits = vec![now - 2 * 60_000, now - 60_000];
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        pc.lease(100, Some(BACKEND_DOWN));
        pc.kill(100);
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some());
        assert!(pc.log().contains("server down pid 100 → relaunch in 30s (3 of 3)"));
        pc
    }

    /// The lease's mtime, set back to `ms` (a write in an earlier second than the deadline).
    fn age_lease(pc: &Pretend, ms: i64) {
        let file = std::fs::File::options().write(true).open(pc.dir.path().join(LEASE_FILE)).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(ms as u64)).unwrap();
    }

    #[test]
    fn after_a_link_restart_a_replacement_quit_during_the_wait_still_cancels_the_relaunch() {
        // Review: owed relaunch persisted -> Link restart -> the person opens RealTimeX and quits it
        // before the saved deadline -> deadline: no launch, and the count starts again.
        let mut pc = owed_and_waiting();
        let died = pc.state()["ack"]["diedMs"].as_i64().unwrap();
        assert!(pc.state()["ack"]["dueMs"].as_i64().unwrap() >= died + 30_000);
        pc.restart_link();
        pc.watcher.tick();
        assert!(pc.watcher.pending.is_some(), "restored");
        assert_eq!(pc.watcher.pending.as_ref().unwrap().died_ms, died, "the death time, not the deadline");
        // A replacement starts and quits; its lease is written well before the deadline.
        pc.start(200);
        pc.lease(200, Some("quit"));
        pc.kill(200);
        age_lease(&pc, died + 2_000);
        pc.watcher.tick();
        pc.expire_if_pending();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 0, "the explicit Quit is honoured");
        assert!(pc.watcher.pending.is_none());
        assert_eq!(pc.state()["ack"], Value::Null);
        assert!(pc.watcher.memory.exits.is_empty(), "an intended exit starts the count again");
    }

    #[test]
    fn a_replacement_started_and_quit_while_link_was_down_still_cancels_the_relaunch() {
        let mut pc = owed_and_waiting();
        let died = pc.state()["ack"]["diedMs"].as_i64().unwrap();
        // Link is down: the person opens RealTimeX and quits it.
        pc.start(200);
        pc.lease(200, Some("quit"));
        pc.kill(200);
        age_lease(&pc, died + 2_000);
        pc.restart_link();
        pc.watcher.tick();
        pc.expire_if_pending();
        pc.watcher.tick();
        assert_eq!(pc.launches(), 0);
        assert!(pc.watcher.pending.is_none());
        assert_eq!(pc.state()["ack"], Value::Null);
        assert!(pc.watcher.memory.exits.is_empty());
        assert!(pc.log().contains("intended exit pid 200 (quit); relaunch cancelled"));
    }

    #[test]
    fn turning_reopen_off_after_answering_withdraws_the_answer() {
        // Review: request -> ack -> switch off before the app's next look: the app keeps its notice.
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"]["nonce"], "n-1");
        lock(&pc.watcher.shared).status = Some(json!({ "reopenRealtimex": false }));
        pc.watcher.tick();
        let state = pc.state();
        assert_eq!(state["ack"], Value::Null);
        assert_eq!(state["enabled"], false);
        assert!(pc.log().contains("server down pid 100; setting off, answer withdrawn"));
        // And it isn't answered again while off.
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"], Value::Null);
    }

    #[test]
    fn links_own_quit_withdraws_its_answer() {
        let mut pc = Pretend::new();
        pc.start(100);
        pc.watcher.tick();
        pc.request(100, "n-1");
        pc.watcher.tick();
        assert_eq!(pc.state()["ack"]["nonce"], "n-1");
        forget_in(pc.dir.path());
        let state = pc.state();
        assert_eq!(state["ack"], Value::Null);
        assert_eq!(state["watching"], Value::Null);
        assert_eq!(state["schema"], 1, "the rest is kept");
    }

    #[test]
    fn the_lease_carries_a_restart_request_only_for_a_server_down() {
        let mut body = good();
        body["restart"] = json!({ "reason": BACKEND_DOWN, "nonce": "abc-123", "at": "x" });
        assert_eq!(parse_lease(&lease_json(body.clone())).unwrap().restart.as_deref(), Some("abc-123"));
        body["restart"]["reason"] = json!("other");
        assert_eq!(parse_lease(&lease_json(body.clone())).unwrap().restart, None);
        body["restart"] = json!({ "reason": BACKEND_DOWN, "nonce": "" });
        assert_eq!(parse_lease(&lease_json(body)).unwrap().restart, None);
    }

    #[test]
    fn only_a_lease_for_another_pid_written_after_the_death_replaces_it() {
        let file = |pid: u32, exit: Option<&str>, mtime_ms: i64| LeaseFile {
            mtime_ms,
            size: 100,
            lease: Some(Lease { pid, version: None, exit: exit.map(str::to_string), restart: None }),
        };
        let died = 1_762_743_600_500;
        assert_eq!(replacement(None, 100, died), Replacement::None);
        // The dead pid's own lease, announced or not, is not a replacement.
        assert_eq!(replacement(Some(&file(100, None, died + 5_000)), 100, died), Replacement::None);
        assert_eq!(replacement(Some(&file(100, Some("quit"), died + 5_000)), 100, died), Replacement::None);
        // Another pid, written before the death: left over, not a replacement.
        assert_eq!(replacement(Some(&file(200, Some("quit"), died - 2_000)), 100, died), Replacement::None);
        // Another pid, written since.
        assert_eq!(
            replacement(Some(&file(200, Some("quit"), died + 2_000)), 100, died),
            Replacement::Intended { pid: 200, reason: "quit".into() }
        );
        assert_eq!(replacement(Some(&file(200, None, died + 2_000)), 100, died), Replacement::Candidate);
        // A replacement whose server went down too doesn't cancel the relaunch.
        assert_eq!(replacement(Some(&file(200, Some(BACKEND_DOWN), died + 2_000)), 100, died), Replacement::None);
        // A lease that doesn't parse is nothing.
        let unreadable = LeaseFile { mtime_ms: died + 2_000, size: 5, lease: None };
        assert_eq!(replacement(Some(&unreadable), 100, died), Replacement::None);
    }

    #[test]
    fn linux_start_ticks_skip_the_command_name() {
        let stat = "4242 (RealTimeX (x) y) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 30 0 123456 1000 50";
        assert_eq!(stat_start_ticks(stat), Some(123_456));
        let zombie = stat.replace(") S 1", ") Z 1");
        assert_eq!(stat_start_ticks(&zombie), None);
        assert_eq!(stat_start_ticks("garbage"), None);
    }

    #[test]
    fn windows_creation_times_are_unix_ms() {
        assert_eq!(filetime_ms(116_444_736_000_000_000), 0);
        assert_eq!(filetime_ms(116_444_736_000_000_000 + 15_000_000), 1_500);
    }
}
