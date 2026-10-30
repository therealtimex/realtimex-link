//! The owner's daemon's side of the session bridges (realtimex-link#11 M4, M5, I4), when Link's
//! root supervisor runs it (`--supervised-fd 3`): the supervisor hands each bridge's connection
//! over on fd 3 (SCM_RIGHTS). The daemon listens on no path of its own.
//!
//! The helper runs in whichever session has the console: the login window's bridge, or the
//! bridge of the user whose session is in front, as long as it says it has the console. When
//! that changes, the helper ends (backend.rs follow_console). Input goes through the job when
//! the bridge last said loginwindow holds Secure Input (I4).

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::helper::{Child, Reader, Spawned, Writer};
use crate::launchd::os;
use crate::launchd::policy::Session;

/// How long a bridge has to start its helper.
const START_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Default)]
struct Status {
    on_console: bool,
    secure_input: bool,
}

struct Bridge {
    id: u32,
    uid: u32,
    login_window: bool,
    version: String,
    /// The control connection, for start and stop.
    stream: Mutex<UnixStream>,
    status: Mutex<Status>,
    /// A start waiting for `started` (its connection) or `failed`.
    waiting: Mutex<Option<mpsc::Sender<Result<OwnedFd, String>>>>,
}

static BRIDGES: Mutex<Vec<Arc<Bridge>>> = Mutex::new(Vec::new());
static NEXT: AtomicU32 = AtomicU32::new(1);

fn bridges() -> std::sync::MutexGuard<'static, Vec<Arc<Bridge>>> {
    BRIDGES.lock().unwrap_or_else(|p| p.into_inner())
}

impl Bridge {
    fn send(&self, value: serde_json::Value) -> std::io::Result<()> {
        let mut text = value.to_string();
        text.push('\n');
        self.stream.lock().unwrap_or_else(|p| p.into_inner()).write_all(text.as_bytes())
    }

    fn status(&self) -> Status {
        *self.status.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Take bridges from the supervisor on `fd` for as long as it runs.
pub fn start(fd: RawFd) {
    std::thread::spawn(move || loop {
        match os::recv_with_fd(fd) {
            Ok(Some((Some(connection), hello))) => register(connection, &hello),
            Ok(Some((None, _))) => eprintln!("[unattended] a message from the supervisor without a connection"),
            // The supervisor is gone: so is the reason to run.
            Ok(None) | Err(_) => {
                eprintln!("[unattended] the supervisor closed its connection: stopping");
                std::process::exit(0);
            }
        }
    });
}

fn register(connection: OwnedFd, hello: &[u8]) {
    let hello: serde_json::Value = serde_json::from_slice(hello).unwrap_or_default();
    let stream = UnixStream::from(connection);
    let Ok(writer) = stream.try_clone() else { return };
    let bridge = Arc::new(Bridge {
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        uid: hello.get("uid").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as u32,
        login_window: hello.get("loginWindow").and_then(|v| v.as_bool()).unwrap_or(false),
        version: hello.get("version").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        stream: Mutex::new(writer),
        status: Mutex::new(Status {
            on_console: hello.get("onConsole").and_then(|v| v.as_bool()).unwrap_or(false),
            secure_input: false,
        }),
        waiting: Mutex::new(None),
    });
    // Another version than this daemon's: once the root copy is this version too, that bridge
    // is an old one, and launchd starts it again from the new copy (M5).
    let ours = env!("CARGO_PKG_VERSION");
    if bridge.version != ours {
        let root = crate::install::plist_version(
            &std::fs::read_to_string(format!("{}/Contents/Info.plist", crate::launchd::plist::ROOT_APP))
                .unwrap_or_default(),
        );
        if root.as_deref() == Some(ours) {
            let _ = bridge.send(serde_json::json!({"op": "exit"}));
            return;
        }
    }
    eprintln!(
        "[unattended] a bridge for uid {} (login window: {}, version {})",
        bridge.uid, bridge.login_window, bridge.version
    );
    bridges().push(bridge.clone());
    std::thread::spawn(move || read(bridge, stream));
}

/// A bridge's messages, until its connection closes.
fn read(bridge: Arc<Bridge>, stream: UnixStream) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    loop {
        match os::recv_with_fd(stream.as_raw_fd()) {
            Ok(Some((fd, bytes))) => {
                fds.extend(fd);
                buffer.extend_from_slice(&bytes);
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("[unattended] the bridge for uid {}: {error}", bridge.uid);
                break;
            }
        }
        while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let message: serde_json::Value = serde_json::from_slice(&line).unwrap_or_default();
            match message.get("op").and_then(|op| op.as_str()) {
                Some("status") => {
                    *bridge.status.lock().unwrap_or_else(|p| p.into_inner()) = Status {
                        on_console: message.get("onConsole").and_then(|v| v.as_bool()).unwrap_or(false),
                        secure_input: message.get("secureInput").and_then(|v| v.as_bool()).unwrap_or(false),
                    };
                }
                Some("started") => {
                    let started = if fds.is_empty() {
                        Err("started without a connection".to_string())
                    } else {
                        Ok(fds.remove(0))
                    };
                    if let Some(waiting) = bridge.waiting.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        let _ = waiting.send(started);
                    }
                }
                Some("failed") => {
                    let error = message.get("error").and_then(|v| v.as_str()).unwrap_or("failed").to_string();
                    if let Some(waiting) = bridge.waiting.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        let _ = waiting.send(Err(error));
                    }
                }
                _ => {}
            }
        }
    }
    eprintln!("[unattended] the bridge for uid {} went away", bridge.uid);
    bridges().retain(|other| other.id != bridge.id);
    if let Some(waiting) = bridge.waiting.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = waiting.send(Err("the session's bridge went away".into()));
    }
}

#[link(name = "SystemConfiguration", kind = "framework")]
extern "C" {
    fn SCDynamicStoreCopyConsoleUser(store: *const std::ffi::c_void, uid: *mut u32, gid: *mut u32) -> *const std::ffi::c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const std::ffi::c_void);
}

/// The console user's uid, or None at the login window (M5).
fn console_user() -> Option<u32> {
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: out-pointers to locals; the name is only checked for NULL and released.
    let name = unsafe { SCDynamicStoreCopyConsoleUser(std::ptr::null(), &mut uid, &mut gid) };
    if name.is_null() {
        return None;
    }
    // SAFETY: a CFString this function owns.
    unsafe { CFRelease(name) };
    (uid != 0).then_some(uid)
}

/// The bridge of the session in front, when it says it has the console.
fn console_bridge() -> Option<Arc<Bridge>> {
    let user = console_user();
    bridges()
        .iter()
        .find(|bridge| {
            let at_console = match user {
                None => bridge.login_window,
                Some(uid) => !bridge.login_window && bridge.uid == uid,
            };
            at_console && bridge.status().on_console
        })
        .cloned()
}

/// The console bridge's id, for follow_console.
pub fn console_session() -> Option<u32> {
    console_bridge().map(|bridge| bridge.id)
}

/// Whether an input request goes through the job (I4): loginwindow holds Secure Input in the
/// session in front. Only the daemon decides this (C4).
pub fn via_job(op: &str) -> bool {
    matches!(op, "key" | "keysym" | "move" | "button" | "scroll")
        && console_bridge().is_some_and(|bridge| bridge.status().secure_input)
}

/// Start the helper in the console's session, through its bridge.
pub async fn spawn() -> std::io::Result<Spawned> {
    let bridge = console_bridge().ok_or_else(|| std::io::Error::other("no session has the console"))?;
    if bridge.version != env!("CARGO_PKG_VERSION") {
        return Err(std::io::Error::other(crate::unattended::UNATTENDED_UPDATING));
    }
    // SAFETY: a plain getter; the daemon runs as the owner.
    let owner = unsafe { libc::getuid() };
    let session = match Session::of(bridge.uid, bridge.login_window, owner) {
        Session::LoginWindow => "loginWindow",
        Session::Owner => "owner",
        Session::Other => "other",
    };
    let (sender, receiver) = mpsc::channel();
    *bridge.waiting.lock().unwrap_or_else(|p| p.into_inner()) = Some(sender);
    bridge.send(serde_json::json!({"op": "start", "session": session}))?;
    let started = tokio::task::spawn_blocking(move || receiver.recv_timeout(START_TIMEOUT))
        .await
        .map_err(std::io::Error::other)?
        .map_err(|_| std::io::Error::other("the session's bridge didn't start the helper"))?
        .map_err(std::io::Error::other)?;
    let stream = UnixStream::from(started);
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let (read, write) = stream.into_split();
    let stdin: Writer = Box::new(write);
    let stdout: Reader = Box::new(read);
    Ok(Spawned {
        child: Child::Bridge(InSession { bridge: bridge.clone() }),
        stdin,
        stdout,
        stderr: None,
        session: Some(bridge.id),
    })
}

/// A helper in a bridge's session: stopped through the bridge.
pub struct InSession {
    bridge: Arc<Bridge>,
}

impl InSession {
    pub fn kill(&self) {
        let _ = self.bridge.send(serde_json::json!({"op": "stop"}));
    }
}
