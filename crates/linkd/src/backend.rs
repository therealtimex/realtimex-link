//! The desktop side: one long-lived rtx-control process speaking JSON lines
//! (crates/control/src/control.rs). The session
//! starts on first use and stops after IDLE_STOP_MS without requests, which also removes GNOME's
//! screen-sharing indicator. Lines without an id are events (peer_state, peer_message).
//!
//! A helper that exits (it crashed, or its screen capture stalled for good: see
//! CONTROL_EXIT_CAPTURE_STALLED) fails the requests it had not answered, and the next request
//! starts a new one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot};

use crate::error::ApiError;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REDETECT_INTERVAL: Duration = Duration::from_secs(30);
/// How often the service checks which session has the console.
const CONSOLE_CHECK: Duration = Duration::from_secs(1);
const IDLE_STOP: Duration = Duration::from_secs(5 * 60);
/// "Someone is using this computer" (#2670): an open live view, or real use this recently.
const IN_USE_WINDOW: Duration = Duration::from_secs(60);
/// A live view's own bookkeeping; not use by itself.
const BOOKKEEPING_OPS: [&str; 2] = ["peer_close", "peer_send"];
/// The JSON-lines protocol this daemon speaks with rtx-control.
pub const CONTROL_PROTOCOL: u64 = 2;
/// rtx-control's exit code when a ScreenCaptureKit request it gave up on never answered, which
/// leaves its screenshots and live view refused until it ends (crates/control/src/main.rs,
/// EXIT_CAPTURE_STALLED). Nothing to do but start a new one, which the next request does.
const CONTROL_EXIT_CAPTURE_STALLED: i32 = 75;
/// How long an exited helper (its stdout closed) may take to be reaped before it is killed.
const EXIT_REAP: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Detected {
    pub binary: Option<PathBuf>,
    pub reason: Option<String>,
    pub code: Option<String>,
}

/// Ask a binary whether it is an rtx-control this daemon can drive (`--check`).
fn check_control_binary(binary: &Path) -> Result<(), String> {
    let output = crate::util::command(binary)
        .arg("--check")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "it did not answer its check".to_string())?;
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let report: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
    if report.get("code").and_then(Value::as_str) == Some("control-moved") {
        return Err("it is rtx-capture, which no longer contains Remote Control".into());
    }
    if report.get("tool").and_then(Value::as_str) != Some("rtx-control") {
        return Err("it did not identify itself as rtx-control".into());
    }
    if report.get("protocol").and_then(Value::as_u64) != Some(CONTROL_PROTOCOL) {
        let theirs = report
            .get("protocol")
            .map_or("unknown".into(), Value::to_string);
        return Err(format!(
            "it is from another release (protocol {theirs}; this daemon needs {CONTROL_PROTOCOL})"
        ));
    }
    if report.get("input") != Some(&Value::Bool(true)) {
        return Err("it has no mouse and keyboard input on this desktop".into());
    }
    if !output.status.success() {
        return Err(format!("its check failed (exit {})", output.status));
    }
    Ok(())
}

/// This daemon's own rtx-control: RTX_REMOTE_CONTROL_BIN when set (empty: none), else the one
/// installed next to rtx-linkd.
fn resolve_binary() -> Detected {
    let name = if cfg!(windows) {
        "rtx-control.exe"
    } else {
        "rtx-control"
    };
    let named = std::env::var_os("RTX_REMOTE_CONTROL_BIN");
    let candidates: Vec<PathBuf> = match &named {
        Some(path) if path.is_empty() => vec![],
        Some(path) => vec![PathBuf::from(path)],
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
            .into_iter()
            .collect(),
    };
    let found: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    if found.is_empty() {
        return Detected {
            binary: None,
            code: Some("control-missing".into()),
            reason: Some(
                "Remote Control needs rtx-control, which is not installed next to rtx-linkd".into(),
            ),
        };
    }
    let mut problem = None;
    for candidate in found {
        match check_control_binary(&candidate) {
            Ok(()) => {
                return Detected {
                    binary: Some(candidate),
                    reason: None,
                    code: None,
                }
            }
            Err(reason) => problem = problem.or(Some(reason)),
        }
    }
    Detected {
        binary: None,
        code: Some("control-incompatible".into()),
        reason: Some(format!(
            "Remote Control can't use the rtx-control it found: {}",
            problem.unwrap_or_default()
        )),
    }
}

fn has_gstreamer_pipewire() -> bool {
    crate::util::command("gst-inspect-1.0")
        .arg("pipewiresrc")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Which helper can drive this desktop, or the reason (and a `code` the UI can act on).
pub fn detect() -> Detected {
    let none = |reason: &str, code: Option<&str>| Detected {
        binary: None,
        reason: Some(reason.into()),
        code: code.map(Into::into),
    };
    if cfg!(target_os = "linux") {
        let var = session_var;
        let session = var("XDG_SESSION_TYPE").to_lowercase();
        let desktop = var("XDG_CURRENT_DESKTOP").to_lowercase();
        let wayland = session == "wayland" || !var("WAYLAND_DISPLAY").is_empty();
        if !wayland && var("DISPLAY").is_empty() {
            return none(
                "Remote Control needs a desktop session on Linux (X11, or GNOME on Wayland)",
                None,
            );
        }
        if wayland && !desktop.contains("gnome") {
            return none("Remote Control on Wayland needs GNOME; other Wayland desktops are not supported yet", None);
        }
        if wayland && !has_gstreamer_pipewire() {
            return none(
                "Remote Control on Linux needs GStreamer with its PipeWire plugin (gstreamer1.0-tools, gstreamer1.0-pipewire)",
                Some("gstreamer-missing"),
            );
        }
    }
    if cfg!(any(target_os = "macos", target_os = "linux", windows)) {
        return resolve_binary();
    }
    none(
        &format!(
            "Remote Control is not available on {} yet",
            std::env::consts::OS
        ),
        None,
    )
}

/// Variables of the desktop session this daemon serves: its own, else the session's (a daemon
/// started outside the desktop, from a terminal or a service, may lack them).
const SESSION_VARS: [&str; 6] = [
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// This user's desktop session environment (Linux): the environment of its desktop shell, found
/// in /proc. Empty when there is none (no one signed in to the desktop yet).
#[cfg(target_os = "linux")]
fn desktop_session_env() -> std::collections::HashMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    const SHELLS: [&str; 5] = [
        "gnome-shell",
        "plasmashell",
        "kwin_wayland",
        "xfce4-session",
        "cinnamon",
    ];
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let dir = entry.path();
        let shell = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        if !SHELLS.contains(&shell.trim()) || entry.metadata().map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(raw) = std::fs::read(dir.join("environ")) else {
            continue;
        };
        return raw
            .split(|byte| *byte == 0)
            .filter_map(|pair| {
                let pair = String::from_utf8_lossy(pair);
                pair.split_once('=')
                    .map(|(name, value)| (name.to_string(), value.to_string()))
            })
            .collect();
    }
    Default::default()
}

#[cfg(not(target_os = "linux"))]
fn desktop_session_env() -> std::collections::HashMap<String, String> {
    Default::default()
}

/// A session variable: this daemon's own, else the desktop session's.
fn session_var(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| desktop_session_env().remove(name))
        .unwrap_or_default()
}

/// Desktop session variables a daemon started outside the desktop may lack.
fn helper_env(command: &mut Command) {
    let session = desktop_session_env();
    for name in SESSION_VARS {
        if std::env::var_os(name).is_none_or(|value| value.is_empty()) {
            if let Some(value) = session.get(name) {
                command.env(name, value);
            }
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let runtime =
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
        command.env("XDG_RUNTIME_DIR", &runtime);
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
            && !session.contains_key("DBUS_SESSION_BUS_ADDRESS")
        {
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={runtime}/bus"),
            );
        }
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// A failed helper answer: 409 with its `code` when the desktop refused the input, else 502.
fn helper_error(message: &Value, fallback: &str) -> ApiError {
    let text = message
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    match message.get("code").and_then(Value::as_str) {
        Some(code) => ApiError::coded(409, code, text),
        None => ApiError::new(502, text),
    }
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, ApiError>>>;

struct Running {
    child: crate::helper::Child,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    started: bool,
    open_peers: HashSet<u64>,
    last_use: Option<Instant>,
    last_stderr: Option<String>,
    idle_deadline: Option<Instant>,
    last_permissions_granted: Option<bool>,
}

pub struct Backend {
    inner: Mutex<Inner>,
    detected: Mutex<(Detected, Instant)>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    generation: AtomicU64,
    events: broadcast::Sender<Value>,
    start_lock: tokio::sync::Mutex<()>,
    /// The running helper's stdin (generation, pipe); one writer at a time.
    stdin: tokio::sync::Mutex<Option<(u64, crate::helper::Writer)>>,
}

impl Backend {
    pub fn new() -> Arc<Self> {
        Self::with_detected(detect())
    }

    pub fn with_detected(detected: Detected) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let backend = Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            detected: Mutex::new((detected, Instant::now())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
            events,
            start_lock: tokio::sync::Mutex::new(()),
            stdin: tokio::sync::Mutex::new(None),
        });
        Self::spawn_idle_watch(&backend);
        backend
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stop the session after IDLE_STOP without requests, unless a live view is open.
    fn spawn_idle_watch(backend: &Arc<Self>) {
        let weak = Arc::downgrade(backend);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(backend) = weak.upgrade() else {
                    return;
                };
                let due = {
                    let mut inner = backend.inner();
                    match inner.idle_deadline {
                        Some(deadline) if Instant::now() >= deadline => {
                            if inner.open_peers.is_empty() {
                                inner.idle_deadline = None;
                                true
                            } else {
                                inner.idle_deadline = Some(Instant::now() + IDLE_STOP);
                                false
                            }
                        }
                        _ => false,
                    }
                };
                if due {
                    let _ = backend.stop().await;
                }
            }
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    fn emit(&self, event: Value) {
        // Ctrl+Alt+Delete from a viewer (realtimex-link#10): only the daemon can send it.
        if event.get("event").and_then(Value::as_str) == Some("secure_attention") {
            crate::secure_attention::send();
            return;
        }
        if event.get("event").and_then(Value::as_str) == Some("peer_state") {
            if let Some(session) = event.get("session").and_then(Value::as_u64) {
                let mut inner = self.inner();
                if event.get("state").and_then(Value::as_str) == Some("closed") {
                    inner.open_peers.remove(&session);
                } else {
                    inner.open_peers.insert(session);
                }
            }
        }
        let _ = self.events.send(event);
    }

    /// Detection is retried while unavailable, e.g. when rtx-control is installed after start.
    pub fn detected(&self) -> Detected {
        let mut detected = self.detected.lock().unwrap_or_else(|p| p.into_inner());
        if detected.0.binary.is_none() && detected.1.elapsed() >= REDETECT_INTERVAL {
            *detected = (detect(), Instant::now());
        }
        detected.0.clone()
    }

    pub fn available(&self) -> bool {
        self.detected().binary.is_some()
    }

    pub fn started(&self) -> bool {
        self.inner().started
    }

    /// True while a live view is open or the computer was used in the last minute.
    pub fn in_use(&self) -> bool {
        let inner = self.inner();
        inner.started
            && (!inner.open_peers.is_empty()
                || inner
                    .last_use
                    .is_some_and(|at| at.elapsed() < IN_USE_WINDOW))
    }

    fn spawn_helper(binary: &Path) -> Result<Child, ApiError> {
        let mut command = crate::util::tokio_command(binary);
        helper_env(&mut command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.spawn().map_err(|error| {
            ApiError::new(502, format!("Remote Control helper did not start: {error}"))
        })
    }

    async fn ensure_child(self: &Arc<Self>) -> Result<(), ApiError> {
        let mut stdin_slot = self.stdin.lock().await;
        if self.inner().running.is_some() && stdin_slot.is_some() {
            return Ok(());
        }
        let detected = self.detected();
        let Some(binary) = detected.binary else {
            return Err(ApiError::new(501, detected.reason.unwrap_or_default()));
        };
        let spawned = crate::helper::spawn(&binary, || {
            Self::spawn_helper(&binary).map_err(|error| std::io::Error::other(error.message))
        })
        .await
        .map_err(|error| {
            ApiError::new(502, format!("Remote Control helper did not start: {error}"))
        })?;
        let crate::helper::Spawned {
            child,
            stdin,
            stdout,
            stderr,
            session,
        } = spawned;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(session) = session {
            self.follow_console(generation, session);
        }
        if let Some(stderr) = stderr {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    eprintln!("[remote-control] helper: {line}");
                    if let Some(backend) = weak.upgrade() {
                        backend.inner().last_stderr = Some(line.chars().take(300).collect());
                    }
                }
            });
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(backend) = weak.upgrade() else {
                    return;
                };
                backend.on_line(&line);
            }
            if let Some(backend) = weak.upgrade() {
                backend.on_exit(generation);
            }
        });
        self.inner().running = Some(Running { child, generation });
        *stdin_slot = Some((generation, stdin));
        Ok(())
    }

    fn on_line(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if message.get("event").is_some() {
            self.emit(message);
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(reply) = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
        else {
            return;
        };
        let result = if message.get("ok") == Some(&Value::Bool(true)) {
            Ok(message)
        } else {
            Err(helper_error(&message, "Remote Control failed"))
        };
        let _ = reply.send(result);
    }

    /// The helper's stdout closed: it is gone. Its unanswered requests fail; with `running`
    /// empty, the next request starts a new helper.
    fn on_exit(&self, generation: u64) {
        let (exited, peers): (Option<Running>, Vec<u64>) = {
            let mut inner = self.inner();
            if inner.running.as_ref().map(|r| r.generation) != Some(generation) {
                return;
            }
            let exited = inner.running.take();
            inner.started = false;
            inner.idle_deadline = None;
            (exited, inner.open_peers.iter().copied().collect())
        };
        if let Some(exited) = exited {
            tokio::spawn(report_exit(exited.child));
        }
        let stderr = self.inner().last_stderr.clone();
        let message = format!(
            "Remote Control helper exited{}",
            stderr.map(|s| format!(": {s}")).unwrap_or_default()
        );
        for (_, reply) in self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
        {
            let _ = reply.send(Err(ApiError::new(502, message.clone())));
        }
        // The helper took its peer sessions with it.
        for session in peers {
            self.emit(json!({ "event": "peer_state", "session": session, "state": "closed" }));
        }
    }

    async fn send(self: &Arc<Self>, op: &str, params: &Value) -> Result<Value, ApiError> {
        self.ensure_child().await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut line = Map::new();
        line.insert("id".into(), json!(id));
        line.insert("op".into(), json!(op));
        if let Some(fields) = params.as_object() {
            for (key, value) in fields {
                if key != "via" {
                    line.insert(key.clone(), value.clone());
                }
            }
        }
        // macOS login window (#11 I4): the job posts this one; only the daemon says so (C4).
        if crate::helper::input_via_job(op) {
            line.insert("via".into(), json!("job"));
        }
        let mut text = Value::Object(line).to_string();
        text.push('\n');
        let (reply, answer) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, reply);
        let written = match self.stdin.lock().await.as_mut() {
            // Flushed: under the service the pipe is an async file that buffers writes.
            Some((_, stdin)) => {
                stdin.write_all(text.as_bytes()).await.is_ok() && stdin.flush().await.is_ok()
            }
            None => false,
        };
        if !written {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(ApiError::new(502, "Remote Control helper exited"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ApiError::new(502, "Remote Control helper exited")),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(ApiError::new(504, format!("Remote Control {op} timed out")))
            }
        }
    }

    fn touch(&self) {
        self.inner().idle_deadline = Some(Instant::now() + IDLE_STOP);
    }

    async fn start(self: &Arc<Self>) -> Result<(), ApiError> {
        self.touch();
        let _guard = self.start_lock.lock().await;
        if self.started() {
            return Ok(());
        }
        self.send("start", &json!({})).await?;
        self.inner().started = true;
        Ok(())
    }

    /// Start the session if needed, then run one primitive op.
    pub async fn call(self: &Arc<Self>, op: &str, params: Value) -> Result<Value, ApiError> {
        if !BOOKKEEPING_OPS.contains(&op) {
            self.inner().last_use = Some(Instant::now());
        }
        self.start().await?;
        match self.send(op, &params).await {
            Err(error) if session_closed(&error.message) => {
                // GNOME can close the session on its own (e.g. the user stops sharing).
                self.inner().started = false;
                self.start().await?;
                self.send(op, &params).await
            }
            other => other,
        }
    }

    pub async fn status(self: &Arc<Self>) -> Value {
        let detected = self.detected();
        if detected.binary.is_none() {
            return json!({ "available": false, "reason": detected.reason });
        }
        if self.inner().running.is_none() {
            return json!({ "available": true, "backend": "rtx-control", "active": false });
        }
        match self.send("status", &json!({})).await {
            Ok(result) => {
                json!({ "available": true, "backend": "rtx-control", "active": result.get("active"), "size": result.get("size") })
            }
            Err(error) => {
                json!({ "available": true, "backend": "rtx-control", "active": false, "error": error.message })
            }
        }
    }

    /// What the desktop still needs granted (macOS); None where nothing needs granting. A fresh
    /// process each time: macOS applies a grant to processes started after it.
    pub async fn permissions(self: &Arc<Self>) -> Result<Option<Value>, ApiError> {
        if !self.available() {
            return Ok(None);
        }
        let result = self.run_once("permissions", json!({})).await?;
        let screen = result
            .get("screenRecording")
            .cloned()
            .unwrap_or(Value::Null);
        let accessibility = result.get("accessibility").cloned().unwrap_or(Value::Null);
        if screen.is_null() && accessibility.is_null() {
            return Ok(None);
        }
        let granted = screen != Value::Bool(false) && accessibility != Value::Bool(false);
        let restart = {
            let mut inner = self.inner();
            let restart =
                granted && inner.last_permissions_granted == Some(false) && inner.running.is_some();
            inner.last_permissions_granted = Some(granted);
            restart
        };
        if restart {
            // Newly granted: restart the long-lived helper so capture and input get it too.
            self.dispose();
        }
        Ok(Some(json!({
            "screenRecording": screen,
            "accessibility": accessibility,
            "app": result.get("app").cloned().unwrap_or(Value::Null),
        })))
    }

    /// One request in a short-lived helper process, outside the long-lived session.
    async fn run_once(&self, op: &str, params: Value) -> Result<Value, ApiError> {
        let Some(binary) = self.detected().binary else {
            return Err(ApiError::new(501, "Remote Control is not available here"));
        };
        let mut child = Self::spawn_helper(&binary)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ApiError::new(502, "no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ApiError::new(502, "no stdout"))?;
        let mut line = json!({ "id": 1, "op": op });
        if let (Some(target), Some(fields)) = (line.as_object_mut(), params.as_object()) {
            target.extend(fields.clone());
        }
        let work = async {
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| ApiError::new(502, e.to_string()))?;
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(text)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if message.get("id").and_then(Value::as_u64) != Some(1) {
                    continue;
                }
                return if message.get("ok") == Some(&Value::Bool(true)) {
                    Ok(message)
                } else {
                    Err(helper_error(
                        &message,
                        &format!("Remote Control {op} failed"),
                    ))
                };
            }
            Err(ApiError::new(502, "Remote Control helper exited"))
        };
        let result = tokio::time::timeout(REQUEST_TIMEOUT, work)
            .await
            .unwrap_or_else(|_| Err(ApiError::new(504, format!("Remote Control {op} timed out"))));
        drop(stdin);
        let _ = child.kill().await;
        result
    }

    /// Show the system prompt for one permission and open its settings pane.
    pub async fn request_permission(&self, name: &str) -> Result<(), ApiError> {
        if !self.available() {
            return Err(ApiError::bad_request("Nothing to grant on this desktop"));
        }
        self.run_once("request_permission", json!({ "name": name }))
            .await
            .map(|_| ())
    }

    pub async fn stop(self: &Arc<Self>) -> Result<(), ApiError> {
        self.inner().idle_deadline = None;
        let running = {
            let mut inner = self.inner();
            if inner.running.is_none() || !inner.started {
                return Ok(());
            }
            inner.started = false;
            true
        };
        if running {
            self.send("stop", &json!({})).await?;
        }
        Ok(())
    }

    /// Under the service, end the helper once another session takes the console (sign-in, user
    /// switch, log off): the next request starts one in that session.
    fn follow_console(self: &Arc<Self>, generation: u64, session: u32) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CONSOLE_CHECK).await;
                let Some(backend) = weak.upgrade() else { return };
                let current = backend.inner().running.as_ref().map(|r| r.generation);
                if current != Some(generation) {
                    return;
                }
                if crate::helper::console_session() != Some(session) {
                    eprintln!("[remote-control] another session has the console: ending the helper");
                    backend.dispose();
                    return;
                }
            }
        });
    }

    /// Kill the helper now; its live views end.
    pub fn dispose(&self) {
        let (running, peers) = {
            let mut inner = self.inner();
            inner.idle_deadline = None;
            inner.started = false;
            (
                inner.running.take(),
                inner.open_peers.drain().collect::<Vec<_>>(),
            )
        };
        if let Some(mut running) = running {
            running.child.start_kill();
        }
        for session in peers {
            self.emit(json!({ "event": "peer_state", "session": session, "state": "closed" }));
        }
    }
}

/// Reap a helper whose stdout closed, and say so when it exited because its screen capture
/// stalled (the log otherwise shows only that it exited).
async fn report_exit(mut child: crate::helper::Child) {
    let waited = tokio::time::timeout(EXIT_REAP, child.wait()).await;
    let code = match waited {
        Ok(code) => code,
        // It closed stdout but keeps running: it is no use any more.
        Err(_) => {
            child.start_kill();
            child.wait().await
        }
    };
    if let Some(code) = code {
        if code == CONTROL_EXIT_CAPTURE_STALLED {
            eprintln!(
                "[remote-control] helper exited: its screen capture stalled; the next request starts a new one"
            );
        }
    }
}

fn session_closed(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("session is not started")
        || lower.contains("session closed")
        || lower.contains("session is closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for rtx-control: answers every request, and exits with rtx-control's stall
    /// code on op "stall". Each start is counted in `spawns` next to it.
    #[cfg(unix)]
    fn fake_helper(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("rtx-control");
        let spawns = dir.join("spawns");
        let script = format!(
            r#"#!/bin/sh
echo started >> '{spawns}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"op":"stall"'*) exit {code} ;;
  esac
  printf '{{"id":%s,"ok":true}}\n' "$id"
done
"#,
            spawns = spawns.display(),
            code = CONTROL_EXIT_CAPTURE_STALLED,
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_helper_that_exits_is_started_again_by_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::with_detected(Detected {
            binary: Some(fake_helper(dir.path())),
            reason: None,
            code: None,
        });
        backend.call("screenshot", json!({})).await.unwrap();
        assert!(backend.started());

        // The helper exits while answering (as rtx-control does when its capture stalls): that
        // request fails at once, not after the request timeout, and the session is over.
        let at = Instant::now();
        let error = backend.call("stall", json!({})).await.unwrap_err();
        assert_eq!(error.status, 502, "{}", error.message);
        assert!(error.message.contains("exited"), "{}", error.message);
        assert!(at.elapsed() < REQUEST_TIMEOUT);
        assert!(!backend.started());

        // The next request starts a new helper and is answered by it; so is the one after.
        backend.call("screenshot", json!({})).await.unwrap();
        backend.call("screenshot", json!({})).await.unwrap();
        assert!(backend.started());
        let spawns = std::fs::read_to_string(dir.path().join("spawns")).unwrap();
        assert_eq!(spawns.lines().count(), 2, "{spawns}");
    }
}
