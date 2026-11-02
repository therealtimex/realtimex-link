//! The accessibility runner (realtimex-ai-app#3055 §2.2): rtx-control runs the pinned
//! agent-desktop CLI for rtx-linkd, here because rtx-control already runs in the right session,
//! at the right integrity level and under Link's TCC identity on every platform.
//!
//! ```text
//! {"id":30,"op":"ax","argv":["snapshot","--window-id=w-42","--max-depth=30"],"timeoutMs":10000,"stateDir":"/…"}
//!   -> {"id":30,"ok":true,"result":<agent-desktop's JSON envelope>}
//!    | {"id":30,"ok":false,"code":"timeout"|"unavailable"|"busy"|"ax-failed"|"ax-crashed"|"bad-request","error":"…","retryAfterMs"?}
//! ```
//!
//! - **Not serial:** the call runs on a worker thread and its reply comes back to the control loop
//!   (`Msg::AxDone`), which writes it by id, so screenshots, input and the live view keep flowing.
//!   One agent-desktop at a time: a second call while one runs is `busy` (rtx-linkd queues).
//! - **A second wall behind rtx-linkd:** `argv[0]` must be an allow-listed subcommand; every flag
//!   must be on that subcommand's list, given as `--name` or `--name=value`; `--headed` and every
//!   other flag, and short flags, are refused; values come after `--` only, and a ref must look
//!   like `@<snapshot>:e<n>`.
//! - **Timeouts** with a hard kill: its own process group (Unix, `killpg`), or a job object that
//!   kills on close (Windows). Output over 4 MB is killed too (`ax-failed`). stderr is kept to
//!   64 KB and only its first line goes to this process's log (it may hold UI text).
//! - **Crashes:** no JSON envelope is `ax-crashed` (rtx-linkd answers reads `unavailable` and
//!   actions `ax-failed`, delivery unknown). Three crashes or timeouts within 60 s switch `ax` off
//!   for 5 minutes (`unavailable`, `retryAfterMs` the time left).
//! - **Version:** before its first call, each process checks `agent-desktop version` is the pinned
//!   one, with a budget of its own (`VERSION_CHECK`; rtx-linkd waits that much longer), so the
//!   check never eats into the call's deadline. A pass is kept for the process; a failure answers
//!   `unavailable` ("reinstall RealTimeX Link") and is checked again after `VERSION_RETRY`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// third_party/agent-desktop.json's version (a test keeps them together).
pub const PINNED_VERSION: &str = "1.0.2";
pub const MAX_STDOUT: usize = 4 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
const BREAKER_FAILURES: usize = 3;
const BREAKER_WINDOW: Duration = Duration::from_secs(60);
const BREAKER_OPEN: Duration = Duration::from_secs(5 * 60);
/// After the CLI exits, its output is read at most this much longer (a process it started may
/// keep the pipe open).
const DRAIN: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(10);
/// The version check's own budget (rtx-linkd's `VERSION_CHECK` waits for it).
pub const VERSION_CHECK: Duration = Duration::from_secs(5);
/// After a failed version check, `ax` is off this long, then checked again.
pub const VERSION_RETRY: Duration = Duration::from_secs(60);

/// The subcommands rtx-linkd may run, each with the flags it may pass and how many values
/// (after `--`) it takes; the first value of a ref command is a ref.
struct Allowed {
    name: &'static str,
    flags: &'static [&'static str],
    values: usize,
    reference: bool,
    /// The longest it may run.
    cap: Duration,
}

const LIST: Duration = Duration::from_secs(5);
const SNAPSHOT: Duration = Duration::from_secs(10);
const READ: Duration = Duration::from_secs(8);
const ACT: Duration = Duration::from_secs(8);

const ALLOWED: &[Allowed] = &[
    Allowed { name: "version", flags: &[], values: 0, reference: false, cap: LIST },
    Allowed { name: "status", flags: &[], values: 0, reference: false, cap: LIST },
    Allowed { name: "list-apps", flags: &["timeout-ms"], values: 0, reference: false, cap: LIST },
    Allowed { name: "list-windows", flags: &["app", "timeout-ms"], values: 0, reference: false, cap: LIST },
    Allowed {
        name: "snapshot",
        flags: &["window-id", "app", "root", "max-depth", "interactive-only", "include-bounds", "compact", "timeout-ms"],
        values: 0,
        reference: false,
        cap: SNAPSHOT,
    },
    Allowed {
        name: "find",
        flags: &["window-id", "app", "root", "role", "name", "text", "value", "state", "exact", "limit", "first", "timeout-ms"],
        values: 0,
        reference: false,
        cap: READ,
    },
    Allowed { name: "get", flags: &["property", "timeout-ms"], values: 1, reference: true, cap: READ },
    Allowed { name: "is", flags: &["property", "timeout-ms"], values: 1, reference: true, cap: READ },
    Allowed { name: "click", flags: &["timeout-ms"], values: 1, reference: true, cap: ACT },
    Allowed { name: "toggle", flags: &["timeout-ms"], values: 1, reference: true, cap: ACT },
    Allowed { name: "expand", flags: &["timeout-ms"], values: 1, reference: true, cap: ACT },
    Allowed { name: "collapse", flags: &["timeout-ms"], values: 1, reference: true, cap: ACT },
    Allowed { name: "scroll-to", flags: &["timeout-ms"], values: 1, reference: true, cap: ACT },
    Allowed { name: "set-value", flags: &["timeout-ms"], values: 2, reference: true, cap: ACT },
    Allowed { name: "type", flags: &["timeout-ms"], values: 2, reference: true, cap: ACT },
    Allowed { name: "select", flags: &["timeout-ms"], values: 2, reference: true, cap: ACT },
];

/// `@<snapshot>:e<n>`.
pub fn is_ref(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('@') else {
        return false;
    };
    let Some((snapshot, element)) = rest.split_once(':') else {
        return false;
    };
    let Some(number) = element.strip_prefix('e') else {
        return false;
    };
    !snapshot.is_empty()
        && snapshot
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

/// The argv checked against the allow-list; the subcommand's longest run.
pub fn check_argv(argv: &[String]) -> Result<Duration, String> {
    let Some(first) = argv.first() else {
        return Err("no subcommand".into());
    };
    let allowed = ALLOWED
        .iter()
        .find(|a| a.name == first)
        .ok_or_else(|| format!("{first} is not an allowed subcommand"))?;
    let mut values = Vec::new();
    let mut after_separator = false;
    for arg in &argv[1..] {
        if after_separator {
            values.push(arg);
            continue;
        }
        if arg == "--" {
            after_separator = true;
            continue;
        }
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(format!("{arg:?}: values go after --, flags as --name"));
        };
        let name = flag.split_once('=').map_or(flag, |(name, _)| name);
        if name == "headed" || !allowed.flags.contains(&name) {
            return Err(format!("--{name} is not allowed for {first}"));
        }
    }
    if values.len() != allowed.values {
        return Err(format!("{first} takes {} value(s)", allowed.values));
    }
    if allowed.reference && !values.first().is_some_and(|r| is_ref(r)) {
        return Err(format!("{first} needs a ref (@<snapshot>:e<n>)"));
    }
    Ok(allowed.cap)
}

/// Crashes and timeouts in the last minute; open: when `ax` is back.
#[derive(Default)]
pub struct Breaker {
    failures: Vec<Instant>,
    open_until: Option<Instant>,
}

impl Breaker {
    /// How long `ax` stays off, when it is.
    pub fn open(&mut self, now: Instant) -> Option<Duration> {
        match self.open_until {
            Some(until) if until > now => Some(until - now),
            Some(_) => {
                self.open_until = None;
                self.failures.clear();
                None
            }
            None => None,
        }
    }

    pub fn failed(&mut self, now: Instant) {
        self.failures.retain(|at| now.duration_since(*at) < BREAKER_WINDOW);
        self.failures.push(now);
        if self.failures.len() >= BREAKER_FAILURES {
            self.open_until = Some(now + BREAKER_OPEN);
        }
    }
}

/// How one run ended.
#[derive(Debug, PartialEq)]
pub enum Ended {
    Exited { code: Option<i32> },
    TimedOut,
    TooLarge,
    NotStarted(String),
}

pub struct Run {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    /// stderr's first line (for this process's log only).
    pub stderr: String,
}

/// The process group (Unix) of the CLI running now: killed when rtx-control ends.
static RUNNING: Mutex<Option<u32>> = Mutex::new(None);

/// rtx-control is ending (stop, stall, a session change): the running CLI goes with it.
pub fn kill_running() {
    let pid = RUNNING.lock().unwrap_or_else(|p| p.into_inner()).take();
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: killpg only sends a signal; the group is the child's own (process_group(0)).
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// The environment for the CLI: rtx-control's, without Link's own variables or anything that
/// looks like a secret, with its state directory.
fn child_env(command: &mut Command, state_dir: Option<&Path>, binary: &Path) {
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("RTX_")
            || upper.contains("TOKEN")
            || upper.contains("SECRET")
            || upper.contains("PASSWORD")
            || upper.starts_with("AGENT_DESKTOP")
        {
            command.env_remove(&name);
        }
    }
    if let Some(dir) = state_dir {
        command.env("AGENT_DESKTOP_HOME", dir);
    }
    // Its macOS helper sits next to it (bundled), never one found elsewhere.
    if let Some(helper) = binary.parent().map(|dir| dir.join("agent-desktop-macos-helper")) {
        if helper.is_file() {
            command.env("AGENT_DESKTOP_MACOS_HELPER_PATH", helper);
        }
    }
}

fn reader<R: Read + Send + 'static>(mut from: R, max: usize, over: Option<Arc<AtomicBool>>) -> mpsc::Receiver<Vec<u8>> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match from.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = max.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..n.min(room)]);
                    if n > room {
                        if let Some(over) = &over {
                            over.store(true, Ordering::SeqCst);
                            break;
                        }
                        // stderr: the rest is read and dropped, so the child never blocks on it.
                    }
                }
            }
        }
        let _ = send.send(kept);
    });
    receive
}

#[cfg(windows)]
mod job {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job that kills every process in it when closed.
    pub struct Job(HANDLE);

    impl Job {
        pub fn for_child(child: &std::process::Child) -> Option<Self> {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: plain Win32 calls on a handle this function owns; the child's handle is
            // valid while `child` lives.
            unsafe {
                let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set.is_err() || AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).is_err() {
                    let _ = CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is this job's own; closing it kills what is still in it.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    // SAFETY: a job handle may be used and closed from any thread.
    unsafe impl Send for Job {}
}

/// Run the CLI once: `argv` as separate arguments (no shell), at most `timeout`, at most
/// MAX_STDOUT of output.
pub fn run_cli(binary: &Path, argv: &[String], timeout: Duration, state_dir: Option<&Path>) -> Run {
    run_capped(binary, argv, timeout, state_dir, MAX_STDOUT)
}

fn run_capped(binary: &Path, argv: &[String], timeout: Duration, state_dir: Option<&Path>, max: usize) -> Run {
    let mut command = Command::new(binary);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    child_env(&mut command, state_dir, binary);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashes up.
        command.creation_flags(0x0800_0000);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Run {
                ended: Ended::NotStarted(error.to_string()),
                stdout: Vec::new(),
                stderr: String::new(),
            }
        }
    };
    #[cfg(windows)]
    let job = job::Job::for_child(&child);
    *RUNNING.lock().unwrap_or_else(|p| p.into_inner()) = Some(child.id());
    let over = Arc::new(AtomicBool::new(false));
    let stdout = reader(child.stdout.take().expect("piped"), max, Some(over.clone()));
    let stderr = reader(child.stderr.take().expect("piped"), MAX_STDERR, None);
    let deadline = Instant::now() + timeout;
    let kill = |child: &mut std::process::Child| {
        #[cfg(unix)]
        // SAFETY: as in kill_running.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let ended = loop {
        if over.load(Ordering::SeqCst) {
            kill(&mut child);
            break Ended::TooLarge;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ended::Exited { code: status.code() },
            Ok(None) => {}
            Err(_) => {
                kill(&mut child);
                break Ended::Exited { code: None };
            }
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            break Ended::TimedOut;
        }
        std::thread::sleep(POLL);
    };
    // What it started (agent-desktop's own) goes with it too.
    #[cfg(unix)]
    // SAFETY: as in kill_running; a group that is gone is ESRCH, ignored.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    drop(job);
    *RUNNING.lock().unwrap_or_else(|p| p.into_inner()) = None;
    let stdout = stdout.recv_timeout(DRAIN).unwrap_or_default();
    let stderr = stderr.recv_timeout(DRAIN).unwrap_or_default();
    let ended = if over.load(Ordering::SeqCst) { Ended::TooLarge } else { ended };
    Run {
        ended,
        stdout,
        stderr: String::from_utf8_lossy(&stderr)
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(300)
            .collect(),
    }
}

/// agent-desktop's JSON envelope from its output: the last line that is a JSON object with `ok`.
pub fn envelope(stdout: &[u8]) -> Option<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|value| value.get("ok").is_some_and(Value::is_boolean))
}

fn failure(code: &str, message: impl Into<String>) -> Value {
    json!({ "ok": false, "code": code, "error": message.into() })
}

/// One call's reply (without its id), and whether it counts against the breaker.
pub struct Reply {
    pub body: Value,
    pub failed: bool,
}

/// The reply to a finished run.
pub fn reply_of(run: Run) -> Reply {
    if !run.stderr.is_empty() {
        eprintln!("[ax] agent-desktop: {}", run.stderr);
    }
    match run.ended {
        Ended::TimedOut => Reply {
            body: failure("timeout", "agent-desktop didn't answer in time and was stopped"),
            failed: true,
        },
        Ended::TooLarge => Reply {
            body: failure("ax-failed", "tree too large: use maxDepth or a narrower root"),
            failed: false,
        },
        Ended::NotStarted(error) => Reply {
            body: failure("unavailable", format!("agent-desktop couldn't start: {error}")),
            failed: true,
        },
        Ended::Exited { code } => match envelope(&run.stdout) {
            Some(envelope) => Reply {
                body: json!({ "ok": true, "result": envelope }),
                failed: false,
            },
            None => Reply {
                body: failure(
                    "ax-crashed",
                    format!(
                        "agent-desktop ended without an answer (exit {})",
                        code.map_or_else(|| "signal".to_string(), |c| c.to_string())
                    ),
                ),
                failed: true,
            },
        },
    }
}

/// Where the CLI is: RTX_AGENT_DESKTOP_BIN, else next to this executable.
pub fn binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RTX_AGENT_DESKTOP_BIN").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "agent-desktop.exe" } else { "agent-desktop" };
    let candidate = std::env::current_exe().ok()?.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

/// Whether `version`'s answer is the pinned version.
pub fn is_pinned(envelope: &Value) -> bool {
    let data = &envelope["data"];
    let version = data["version"]
        .as_str()
        .or_else(|| data["cli_version"].as_str())
        .or_else(|| data.as_str())
        .unwrap_or("");
    version.trim_start_matches('v') == PINNED_VERSION
}

/// This process's version check: not run yet, passed (kept), or failed at a time (run again
/// after VERSION_RETRY).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Version {
    #[default]
    Unchecked,
    Pinned,
    Failed(Instant),
}

impl Version {
    /// Whether the check runs before this call, or how long `ax` stays off.
    pub fn due(self, now: Instant) -> Result<bool, Duration> {
        match self {
            Version::Unchecked => Ok(true),
            Version::Pinned => Ok(false),
            Version::Failed(at) => {
                let again = at + VERSION_RETRY;
                if now >= again {
                    Ok(true)
                } else {
                    Err(again - now)
                }
            }
        }
    }

    /// The check's result, as of `now`.
    pub fn checked(pinned: bool, now: Instant) -> Self {
        if pinned {
            Version::Pinned
        } else {
            Version::Failed(now)
        }
    }
}

fn wrong_version() -> Value {
    failure(
        "unavailable",
        format!("agent-desktop isn't version {PINNED_VERSION}: reinstall RealTimeX Link"),
    )
}

/// A finished call, for the control loop to answer.
pub struct Done {
    pub id: Value,
    pub reply: Reply,
}

/// The runner's state in the control loop.
pub struct Ax {
    binary: Option<PathBuf>,
    busy: Arc<AtomicBool>,
    /// The version check of this process.
    version: Arc<Mutex<Version>>,
    breaker: Breaker,
}

impl Default for Ax {
    fn default() -> Self {
        Self {
            binary: None,
            busy: Arc::new(AtomicBool::new(false)),
            version: Arc::new(Mutex::new(Version::Unchecked)),
            breaker: Breaker::default(),
        }
    }
}

impl Ax {
    #[cfg(test)]
    fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary: Some(binary),
            ..Self::default()
        }
    }

    /// Start a call. `Some(reply)` answers now (refused, busy, off); None: `done` gets the reply
    /// from a worker thread later.
    pub fn start(&mut self, id: Value, request: &Value, done: impl FnOnce(Done) + Send + 'static) -> Option<Value> {
        let argv: Option<Vec<String>> = request["argv"].as_array().and_then(|list| {
            list.iter()
                .map(|a| a.as_str().map(str::to_string))
                .collect()
        });
        let Some(argv) = argv else {
            return Some(failure("bad-request", "argv must be a list of strings"));
        };
        let cap = match check_argv(&argv) {
            Ok(cap) => cap,
            Err(reason) => return Some(failure("bad-request", reason)),
        };
        if let Some(left) = self.breaker.open(Instant::now()) {
            let mut body = failure(
                "unavailable",
                "agent-desktop failed repeatedly; accessibility is off for a few minutes",
            );
            body["retryAfterMs"] = json!(left.as_millis() as u64);
            return Some(body);
        }
        let Some(binary) = self.binary.clone().or_else(binary) else {
            return Some(failure(
                "unavailable",
                "agent-desktop isn't installed with RealTimeX Link: reinstall RealTimeX Link",
            ));
        };
        // A failed version check: off until it is due again.
        let check = self.version.lock().unwrap_or_else(|p| p.into_inner()).due(Instant::now());
        let check = match check {
            Ok(check) => check,
            Err(left) => {
                let mut body = wrong_version();
                body["retryAfterMs"] = json!(left.as_millis() as u64);
                return Some(body);
            }
        };
        if self.busy.swap(true, Ordering::SeqCst) {
            let mut body = failure("busy", "Another accessibility call is running");
            body["retryAfterMs"] = json!(500);
            return Some(body);
        }
        let timeout = request["timeoutMs"]
            .as_u64()
            .map_or(cap, Duration::from_millis)
            .clamp(Duration::from_millis(100), cap);
        let state_dir = request["stateDir"]
            .as_str()
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute());
        let (busy, version) = (self.busy.clone(), self.version.clone());
        std::thread::spawn(move || {
            if let Some(dir) = &state_dir {
                let _ = std::fs::create_dir_all(dir);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
                }
            }
            // Its own budget, before the call's: the call keeps its whole deadline.
            let pinned = !check || {
                let run = run_cli(&binary, &["version".to_string()], VERSION_CHECK, state_dir.as_deref());
                let ok = envelope(&run.stdout).is_some_and(|e| is_pinned(&e));
                if !ok {
                    eprintln!(
                        "[ax] agent-desktop isn't version {PINNED_VERSION}: accessibility is off for {} s",
                        VERSION_RETRY.as_secs()
                    );
                }
                *version.lock().unwrap_or_else(|p| p.into_inner()) = Version::checked(ok, Instant::now());
                ok
            };
            let reply = if !pinned {
                let mut body = wrong_version();
                body["retryAfterMs"] = json!(VERSION_RETRY.as_millis() as u64);
                Reply { body, failed: false }
            } else {
                reply_of(run_cli(&binary, &argv, timeout, state_dir.as_deref()))
            };
            busy.store(false, Ordering::SeqCst);
            done(Done { id, reply });
        });
        None
    }

    /// A call ended: crashes and timeouts count against the breaker.
    pub fn finished(&mut self, done: &Done) {
        if done.reply.failed {
            self.breaker.failed(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_pin_and_this_version_agree() {
        let pin: Value =
            serde_json::from_str(include_str!("../../../third_party/agent-desktop.json")).unwrap();
        assert_eq!(pin["version"], PINNED_VERSION);
    }

    #[test]
    fn argv_is_held_to_the_allow_list() {
        assert!(check_argv(&strings(&["snapshot", "--window-id=w-42", "--max-depth=30", "--interactive-only"])).is_ok());
        assert!(check_argv(&strings(&["click", "--", "@s1:e7"])).is_ok());
        assert!(check_argv(&strings(&["type", "--timeout-ms=8000", "--", "@s1:e7", "--headed"])).is_ok());
        // --headed, anywhere a flag goes, and press / key chords never.
        assert!(check_argv(&strings(&["snapshot", "--headed"])).is_err());
        assert!(check_argv(&strings(&["click", "--headed", "--", "@s1:e7"])).is_err());
        assert!(check_argv(&strings(&["press", "--", "cmd+q"])).is_err());
        assert!(check_argv(&strings(&["screenshot"])).is_err());
        assert!(check_argv(&strings(&["launch", "--", "Terminal"])).is_err());
        assert!(check_argv(&strings(&["batch", "--", "[]"])).is_err());
        // A flag of another subcommand, a short flag, a value before --.
        assert!(check_argv(&strings(&["click", "--window-id=w-1", "--", "@s1:e7"])).is_err());
        assert!(check_argv(&strings(&["snapshot", "-i"])).is_err());
        assert!(check_argv(&strings(&["click", "@s1:e7"])).is_err());
        // A ref must look like one.
        assert!(check_argv(&strings(&["click", "--", "@e7"])).is_err());
        assert!(check_argv(&strings(&["get", "--property=value", "--", "../etc"])).is_err());
        assert!(check_argv(&strings(&["set-value", "--", "@s1:e7"])).is_err());
        assert!(check_argv(&[]).is_err());
        assert!(is_ref("@s2rzl61oy5saop:e7"));
        assert!(!is_ref("@s1:e"));
        assert!(!is_ref("@s 1:e1"));
    }

    #[test]
    fn three_failures_in_a_minute_turn_it_off_for_five_minutes() {
        let mut breaker = Breaker::default();
        let start = Instant::now();
        breaker.failed(start);
        breaker.failed(start + Duration::from_secs(70));
        breaker.failed(start + Duration::from_secs(80));
        // The first one is over a minute old: still on.
        assert_eq!(breaker.open(start + Duration::from_secs(81)), None);
        breaker.failed(start + Duration::from_secs(90));
        let left = breaker.open(start + Duration::from_secs(91)).unwrap();
        assert!(left > Duration::from_secs(298) && left <= BREAKER_OPEN);
        assert_eq!(breaker.open(start + Duration::from_secs(91) + BREAKER_OPEN), None);
    }

    #[test]
    fn the_version_check_backs_off_then_tries_again() {
        let start = Instant::now();
        assert_eq!(Version::Unchecked.due(start), Ok(true));
        assert_eq!(Version::Pinned.due(start), Ok(false));
        let failed = Version::checked(false, start);
        let left = failed.due(start + Duration::from_secs(10)).unwrap_err();
        assert_eq!(left, VERSION_RETRY - Duration::from_secs(10));
        assert_eq!(failed.due(start + VERSION_RETRY), Ok(true));
        assert_eq!(Version::checked(true, start), Version::Pinned);
    }

    #[test]
    fn replies_map_how_it_ended() {
        let run = |ended, stdout: &str| Run { ended, stdout: stdout.as_bytes().to_vec(), stderr: String::new() };
        let ok = reply_of(run(Ended::Exited { code: Some(1) }, "log line\n{\"ok\":false,\"error\":{\"code\":\"STALE_REF\"}}\n"));
        assert_eq!(ok.body["ok"], true);
        assert_eq!(ok.body["result"]["error"]["code"], "STALE_REF");
        assert!(!ok.failed);
        let crashed = reply_of(run(Ended::Exited { code: Some(101) }, "thread panicked"));
        assert_eq!(crashed.body["code"], "ax-crashed");
        assert!(crashed.failed);
        let late = reply_of(run(Ended::TimedOut, ""));
        assert_eq!((late.body["code"].as_str(), late.failed), (Some("timeout"), true));
        let big = reply_of(run(Ended::TooLarge, ""));
        assert_eq!(big.body["code"], "ax-failed");
        assert!(is_pinned(&json!({ "ok": true, "data": { "version": "1.0.2" } })));
        assert!(!is_pinned(&json!({ "ok": true, "data": { "version": "1.0.3" } })));
    }

    #[cfg(unix)]
    mod fake {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// A fake agent-desktop: `version` answers the pinned version; anything else runs `body`.
        pub fn cli(name: &str, body: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("rtx-ax-{}-{name}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("agent-desktop");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = version ]; then echo '{{\"ok\":true,\"data\":{{\"version\":\"{PINNED_VERSION}\"}}}}'; exit 0; fi\n{body}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[test]
        fn a_cli_that_hangs_is_killed_with_what_it_started() {
            let marker = std::env::temp_dir().join(format!("rtx-ax-{}-child", std::process::id()));
            let _ = std::fs::remove_file(&marker);
            // It starts a child that would write the marker later, then hangs.
            let cli = cli("hang", &format!("(sleep 2; touch '{}') &\nsleep 30", marker.display()));
            let started = Instant::now();
            let run = run_cli(&cli, &strings(&["list-apps"]), Duration::from_millis(300), None);
            assert_eq!(run.ended, Ended::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
            std::thread::sleep(Duration::from_millis(2500));
            assert!(!marker.exists(), "the CLI's own child outlived the kill");
        }

        #[test]
        fn output_over_the_cap_is_cut_and_refused() {
            let cli = cli("big", "yes aaaaaaaaaaaaaaaa | head -c 300000; sleep 5");
            let run = run_capped(&cli, &strings(&["snapshot"]), Duration::from_secs(5), None, 100_000);
            assert_eq!(run.ended, Ended::TooLarge);
            assert!(run.stdout.len() <= 100_000);
            assert_eq!(reply_of(run).body["code"], "ax-failed");
        }

        #[test]
        fn the_envelope_and_the_state_directory_reach_the_caller() {
            let cli = cli("ok", "echo \"{\\\"ok\\\":true,\\\"data\\\":{\\\"home\\\":\\\"$AGENT_DESKTOP_HOME\\\",\\\"argv\\\":\\\"$*\\\",\\\"secret\\\":\\\"$RTX_SECRET_FOR_TEST\\\"}}\"");
            std::env::set_var("RTX_SECRET_FOR_TEST", "s3cret");
            let dir = std::env::temp_dir().join("rtx-ax-state");
            let run = run_cli(&cli, &strings(&["click", "--", "@s1:e2"]), Duration::from_secs(5), Some(&dir));
            let reply = reply_of(run);
            let data = &reply.body["result"]["data"];
            assert_eq!(data["home"], dir.display().to_string());
            assert_eq!(data["argv"], "click -- @s1:e2");
            // Link's own variables don't reach it.
            assert_eq!(data["secret"], "");
        }

        /// The reply comes later, by id, while the control loop goes on (here: at once).
        #[test]
        fn calls_reply_out_of_order_and_one_at_a_time() {
            let cli = cli("slow", "sleep 0.5; echo '{\"ok\":true,\"data\":[]}'");
            let mut ax = Ax::with_binary(cli);
            let (send, receive) = mpsc::channel();
            let request = json!({ "op": "ax", "argv": ["list-apps"], "timeoutMs": 4000 });
            let started = Instant::now();
            let first = send.clone();
            assert!(ax.start(json!(7), &request, move |done| first.send(done).unwrap()).is_none());
            // Not waiting for it: a screenshot's reply would go out now.
            assert!(started.elapsed() < Duration::from_millis(200));
            // A second call while the first runs is busy.
            let second = send.clone();
            let busy = ax.start(json!(8), &request, move |done| second.send(done).unwrap()).unwrap();
            assert_eq!(busy["code"], "busy");
            let done = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(done.id, json!(7));
            assert_eq!(done.reply.body["ok"], true);
            ax.finished(&done);
            // Free again.
            let third = send;
            assert!(ax.start(json!(9), &request, move |done| third.send(done).unwrap()).is_none());
            assert_eq!(receive.recv_timeout(Duration::from_secs(5)).unwrap().id, json!(9));
        }

        #[test]
        fn a_crashing_cli_switches_ax_off() {
            let cli = cli("crash", "exit 3");
            let mut ax = Ax::with_binary(cli);
            let (send, receive) = mpsc::channel();
            let request = json!({ "op": "ax", "argv": ["list-apps"] });
            for id in 0..3 {
                let send = send.clone();
                assert!(ax.start(json!(id), &request, move |done| send.send(done).unwrap()).is_none());
                let done = receive.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(done.reply.body["code"], "ax-crashed");
                ax.finished(&done);
            }
            let off = ax.start(json!(3), &request, |_| {}).unwrap();
            assert_eq!(off["code"], "unavailable");
            assert!(off["retryAfterMs"].as_u64().unwrap() > 0);
        }

        /// A version that fails once is checked again after VERSION_RETRY, and a pass is kept.
        #[test]
        fn a_failed_version_check_is_tried_again_later() {
            let dir = std::env::temp_dir().join(format!("rtx-ax-{}-flaky", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("agent-desktop");
            let seen = dir.join("seen");
            // The first `version` answers nothing (not ready yet); later ones the pinned version.
            std::fs::write(
                &path,
                format!(
                    concat!(
                        "#!/bin/sh\n",
                        "if [ \"$1\" = version ]; then\n",
                        "  if [ -e '{seen}' ]; then echo '{{\"ok\":true,\"data\":{{\"version\":\"{pinned}\"}}}}'; else touch '{seen}'; fi\n",
                        "  exit 0\n",
                        "fi\n",
                        "echo \"$1\" >> '{calls}'\n",
                        "echo '{{\"ok\":true,\"data\":[]}}'\n",
                    ),
                    pinned = PINNED_VERSION,
                    seen = seen.display(),
                    calls = dir.join("calls").display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut ax = Ax::with_binary(path);
            let (send, receive) = mpsc::channel();
            let request = json!({ "argv": ["list-apps"] });
            let first = send.clone();
            assert!(ax.start(json!(1), &request, move |done| first.send(done).unwrap()).is_none());
            let done = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(done.reply.body["code"], "unavailable");
            assert!(!done.reply.failed);
            // Off for now, answered at once (no CLI run), with the time left.
            let off = ax.start(json!(2), &request, |_| {}).unwrap();
            assert_eq!(off["code"], "unavailable");
            let left = off["retryAfterMs"].as_u64().unwrap();
            assert!(left > 50_000 && left <= VERSION_RETRY.as_millis() as u64, "{left}");
            assert!(!dir.join("calls").exists());
            // A minute later it is checked again, passes, and the call runs.
            *ax.version.lock().unwrap() = Version::Failed(Instant::now() - VERSION_RETRY);
            let second = send.clone();
            assert!(ax.start(json!(3), &request, move |done| second.send(done).unwrap()).is_none());
            let done = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(done.reply.body["ok"], true);
            assert_eq!(*ax.version.lock().unwrap(), Version::Pinned);
            // Kept: the next call doesn't check again.
            let _ = std::fs::remove_file(&seen);
            let third = send;
            assert!(ax.start(json!(4), &request, move |done| third.send(done).unwrap()).is_none());
            assert_eq!(receive.recv_timeout(Duration::from_secs(5)).unwrap().reply.body["ok"], true);
            assert_eq!(std::fs::read_to_string(dir.join("calls")).unwrap().lines().count(), 2);
        }

        #[test]
        fn another_version_is_unavailable() {
            let dir = std::env::temp_dir().join(format!("rtx-ax-{}-old", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("agent-desktop");
            std::fs::write(&path, "#!/bin/sh\necho '{\"ok\":true,\"data\":{\"version\":\"0.9.0\"}}'\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut ax = Ax::with_binary(path);
            let (send, receive) = mpsc::channel();
            ax.start(json!(1), &json!({ "argv": ["list-apps"] }), move |done| send.send(done).unwrap());
            let done = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(done.reply.body["code"], "unavailable");
        }
    }
}
