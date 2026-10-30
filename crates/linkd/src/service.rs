//! `rtx-linkd --service`: Link's Windows service (realtimex-link#10), LocalSystem, installed by
//! the installer. While "Start with this computer" is on (unattended.rs), it runs its owner's
//! daemon on the owner's data folder from boot, so the owner's allowed computers can connect at
//! the sign-in and lock screens; while it is off it only answers its pipe.
//!
//! Trust premise (System Design, issue-10-sd-review.md): the owner is a local administrator,
//! checked when it is turned on and again before every start of the daemon. Their unelevated
//! processes reaching SYSTEM is the accepted no-UAC risk. What must hold: no other user, and no
//! owner who is no longer an administrator, can influence what this SYSTEM daemon writes or runs.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
pub mod win;

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this process is Link's service, running its owner's daemon as SYSTEM.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Where the service keeps a transfer's files for the person at the console (SD C2):
/// `%ProgramData%\RealTimeX Link\transfers\<session>`, made by the service, SYSTEM and
/// administrators only, plus read for that session's signed-in user so their apps can open
/// pasted files. At the sign-in screen there is nobody to give them to.
#[cfg(windows)]
pub fn transfers_dir() -> Result<std::path::PathBuf, crate::error::ApiError> {
    use crate::error::ApiError;
    let machine = trusted_dir()
        .ok_or_else(|| ApiError::coded(503, "machine-dir-unsafe", "RealTimeX Link's ProgramData folder isn't safe to use"))?;
    let session = crate::helper::console_session()
        .ok_or_else(|| ApiError::coded(409, "no-console", "Nobody is at this computer's screen"))?;
    let user = win::session_user(session).ok_or_else(|| {
        ApiError::coded(
            409,
            "nobody-signed-in",
            "Files can be pasted once someone signs in on this computer",
        )
    })?;
    let base = machine.join("transfers");
    match win::create_protected_dir(&base, None) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(ApiError::internal(error)),
    }
    // Named by session and user: a new sign-in on the same session gets a folder of its own.
    let dir = base.join(format!("{session}-{user}"));
    match win::create_protected_dir(&dir, Some(&user)) {
        Ok(()) => Ok(dir),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(dir),
        Err(error) => Err(ApiError::internal(error)),
    }
}

/// A new folder for one update's download (SD C2): SYSTEM and administrators only, under a
/// random name in `%ProgramData%\RealTimeX Link\updates`.
#[cfg(windows)]
pub fn update_dir() -> std::io::Result<std::path::PathBuf> {
    let base = trusted_dir()
        .ok_or_else(|| std::io::Error::other("Link's ProgramData folder isn't safe to use"))?
        .join("updates");
    match win::create_protected_dir(&base, None) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let dir = base.join(crate::util::uuid());
    win::create_protected_dir(&dir, None)?;
    Ok(dir)
}

/// Whether writing into `dir` as SYSTEM would follow a junction or symbolic link (SD C3).
pub fn is_link(dir: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        win::is_link(dir)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        false
    }
}

/// Run as the Windows service: returns when the Service Control Manager stops it.
#[cfg(windows)]
pub fn run() {
    windows_service::run();
}

#[cfg(not(windows))]
pub fn run() {
    eprintln!("rtx-linkd --service runs only as Link's Windows service");
    std::process::exit(2);
}

/// The folder the service may write in: set only once [`prepare_machine_dir`] made it trusted,
/// and never again after it refused it (Review of !12: nothing is written through an unchecked
/// folder).
#[cfg(windows)]
static TRUSTED_DIR: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// Link's ProgramData folder, when it is trusted.
#[cfg(windows)]
pub fn trusted_dir() -> Option<std::path::PathBuf> {
    TRUSTED_DIR.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Make the folder trusted before anything reads or writes there (win::secure_tree): true when
/// it is. A choice found in a folder that wasn't already only SYSTEM's and administrators' is
/// dropped, so the service starts with the setting off. Used by the service at every start and
/// by the installer (`rtx-linkd secure-machine-dir`) before it writes there.
#[cfg(windows)]
pub fn prepare_machine_dir(dir: &std::path::Path) -> bool {
    match win::secure_tree(dir, &SCRATCH) {
        Ok(trust) => {
            *TRUSTED_DIR.write().unwrap_or_else(|p| p.into_inner()) = Some(dir.to_path_buf());
            if trust == win::Trust::Reset {
                log_line("the folder wasn't only SYSTEM's and administrators': its choice was dropped (off)");
            }
            true
        }
        Err(error) => {
            *TRUSTED_DIR.write().unwrap_or_else(|p| p.into_inner()) = None;
            log_line(&format!("Link's ProgramData folder isn't safe, so nothing there is used: {error}"));
            false
        }
    }
}

/// The folders whose contents the service makes and only it uses: transfers and update downloads.
#[cfg(windows)]
const SCRATCH: [&str; 2] = ["updates", "transfers"];

/// How long a transfer or download is kept after it last changed, once the service that made it
/// stopped: a pasted file stays openable for a day.
#[cfg(windows)]
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// At the service's start, after [`prepare_machine_dir`]: what an earlier service left behind
/// and no longer uses. Never from the installer, which runs while the service serves.
#[cfg(windows)]
fn sweep_machine_dir() {
    if let Some(dir) = trusted_dir() {
        win::sweep_stale(&dir, &SCRATCH, STALE_AFTER);
    }
}

/// The service's log: in its trusted folder, else the debugger output (it has no console).
#[cfg(windows)]
fn log_line(line: &str) {
    use std::io::Write;
    let text = format!("{} {line}", chrono_like_now());
    if let Some(dir) = trusted_dir() {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("service.log"))
        {
            let _ = writeln!(file, "{text}");
            return;
        }
    }
    let message = win::wide(&format!("RealTimeX Link Service: {text}\n"));
    unsafe { win::OutputDebugStringW(message.as_ptr()) };
}

/// Seconds since the epoch: enough to order the log without a time crate.
#[cfg(windows)]
fn chrono_like_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(windows)]
mod windows_service {
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::Notify;

    use super::win::{self, Handle};
    use super::{log_line, ACTIVE};
    use crate::unattended::{
        machine_dir, Config, Reply, Request, OS_ADMIN_REQUIRED, PIPE_NAME,
        OWNER_DAEMON_RUNNING, PROFILE_UNAVAILABLE, SERVICE_NAME, STANDARD_ONLY,
    };

    /// Who may use the pipe: SYSTEM and administrators fully, signed-in users to ask; never
    /// over the network (SD: no SMB access).
    const PIPE_SDDL: &str = "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";
    /// Why the service doesn't run the daemon although it is on.
    const OWNER_NOT_ADMIN: &str = "owner-not-admin";
    /// Its ProgramData folder couldn't be made safe: nothing in it is trusted.
    const MACHINE_DIR_UNSAFE: &str = "machine-dir-unsafe";

    struct Status(win::HANDLE);
    // SAFETY: a service status handle is used from any thread.
    unsafe impl Send for Status {}
    unsafe impl Sync for Status {}

    static STATUS: OnceLock<Status> = OnceLock::new();
    static STOP: OnceLock<Arc<Notify>> = OnceLock::new();

    pub fn run() {
        let mut name = win::wide(SERVICE_NAME);
        let table = [
            win::SERVICE_TABLE_ENTRYW {
                lpServiceName: name.as_mut_ptr(),
                lpServiceProc: Some(service_main),
            },
            win::SERVICE_TABLE_ENTRYW {
                lpServiceName: std::ptr::null_mut(),
                lpServiceProc: None,
            },
        ];
        // Blocks until the service stopped.
        if unsafe { win::StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            eprintln!("rtx-linkd --service must be started by the Service Control Manager");
            std::process::exit(2);
        }
    }

    fn report(state: u32) {
        let Some(status) = STATUS.get() else { return };
        let accepted = if state == win::SERVICE_RUNNING {
            win::SERVICE_ACCEPT_STOP | win::SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        };
        let value = win::SERVICE_STATUS {
            dwServiceType: win::SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: win::NO_ERROR,
            dwWaitHint: 10_000,
            ..win::SERVICE_STATUS::default()
        };
        unsafe { win::SetServiceStatus(status.0, &value) };
    }

    unsafe extern "system" fn handler(
        control: u32,
        _event: u32,
        _data: *mut c_void,
        _context: *mut c_void,
    ) -> u32 {
        match control {
            win::SERVICE_CONTROL_STOP | win::SERVICE_CONTROL_SHUTDOWN => {
                report(win::SERVICE_STOP_PENDING);
                if let Some(stop) = STOP.get() {
                    stop.notify_one();
                }
                win::NO_ERROR
            }
            win::SERVICE_CONTROL_INTERROGATE => win::NO_ERROR,
            // ERROR_CALL_NOT_IMPLEMENTED
            _ => 120,
        }
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
        let name = win::wide(SERVICE_NAME);
        let status =
            unsafe { win::RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null_mut()) };
        if status.is_null() {
            return;
        }
        let _ = STATUS.set(Status(status));
        let stop = Arc::new(Notify::new());
        let _ = STOP.set(stop.clone());
        report(win::SERVICE_START_PENDING);
        ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
        match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => {
                report(win::SERVICE_RUNNING);
                runtime.block_on(serve(stop));
                runtime.shutdown_timeout(Duration::from_secs(5));
            }
            Err(error) => log_line(&format!("no runtime: {error}")),
        }
        report(win::SERVICE_STOPPED);
    }

    /// What the pipe and the supervisor share.
    #[derive(Default)]
    struct Shared {
        /// The ProgramData folder was made safe when the service started (win::secure_tree).
        safe: std::sync::atomic::AtomicBool,
        /// Why the daemon isn't running although it is on (Reply::idle).
        idle: Mutex<Option<String>>,
        /// unattended.json changed.
        changed: Notify,
    }

    impl Shared {
        fn set_idle(&self, why: Option<&str>) {
            *self.idle.lock().unwrap_or_else(|p| p.into_inner()) = why.map(str::to_string);
        }
        fn idle(&self) -> Option<String> {
            self.idle.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }
    }

    async fn serve(stop: Arc<Notify>) {
        let shared = Arc::new(Shared::default());
        // Before anything is read or written there, the log included.
        if super::prepare_machine_dir(&machine_dir()) {
            shared.safe.store(true, std::sync::atomic::Ordering::Relaxed);
            super::sweep_machine_dir();
        }
        log_line("service started");
        tokio::spawn(pipe_server(shared.clone()));
        // The folder this process ran a daemon on: process-wide state (paths) is set once, so
        // another owner's folder needs a fresh process (the SCM restarts the service).
        let mut served: Option<PathBuf> = None;
        loop {
            let safe = shared.safe.load(std::sync::atomic::Ordering::Relaxed);
            let config = if safe { Config::load(&machine_dir()) } else { Config::default() };
            if !safe {
                shared.set_idle(Some(MACHINE_DIR_UNSAFE));
            }
            let daemon = if config.enabled {
                if served.as_ref().is_some_and(|dir| !crate::paths::same_folder(dir, &config.data_dir, "windows")) {
                    log_line("another owner's folder: restarting the service");
                    std::process::exit(1);
                }
                start_owner_daemon(&config, &shared).await
            } else {
                if safe {
                    shared.set_idle(None);
                }
                None
            };
            if daemon.is_some() {
                served = Some(config.data_dir.clone());
            }
            let ended = async {
                match &daemon {
                    Some(daemon) => daemon.shutdown.notified().await,
                    None => std::future::pending().await,
                }
            };
            let stopping = tokio::select! {
                () = stop.notified() => true,
                () = shared.changed.notified() => false,
                // Asked to stop through its API (Quit): run it again shortly, as at boot.
                () = ended => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    false
                }
                // Not running although on: try again soon while the owner's daemon steps aside,
                // else now and then (a profile that comes back).
                () = tokio::time::sleep(if shared.idle().as_deref() == Some(OWNER_DAEMON_RUNNING) {
                    Duration::from_secs(5)
                } else {
                    Duration::from_secs(60)
                }), if daemon.is_none() && config.enabled => false,
            };
            if let Some(daemon) = daemon {
                daemon.stop().await;
                log_line("owner's daemon stopped");
            }
            if stopping {
                break;
            }
        }
        log_line("service stopped");
    }

    /// The owner's standard folder: `<profile>\AppData\Roaming\RealTimeX Link`.
    fn standard_folder(sid: &str) -> Option<(PathBuf, PathBuf)> {
        let home = win::profile_home(sid)?;
        let folder = home
            .join("AppData")
            .join("Roaming")
            .join(crate::paths::APP_NAME);
        Some((home, folder))
    }

    /// Run the owner's daemon once the checks hold (SD C1, C3, C5, C6); None and an idle
    /// reason otherwise.
    async fn start_owner_daemon(config: &Config, shared: &Shared) -> Option<crate::daemon::Daemon> {
        match win::account_is_admin(&config.owner_sid) {
            Some(true) => {}
            other => {
                log_line(&format!("owner isn't an administrator ({other:?}): idle"));
                shared.set_idle(Some(OWNER_NOT_ADMIN));
                return None;
            }
        }
        let Some((home, folder)) = standard_folder(&config.owner_sid) else {
            shared.set_idle(Some(PROFILE_UNAVAILABLE));
            return None;
        };
        if !crate::paths::same_folder(&folder, &config.data_dir, "windows")
            || !folder.join("remote-host.db").exists()
            || win::is_link(&folder)
        {
            log_line("owner's folder unavailable: idle");
            shared.set_idle(Some(PROFILE_UNAVAILABLE));
            return None;
        }
        crate::paths::serve_owner(home);
        // The owner's own daemon may still be letting go of the folder (it stops after
        // answering POST /v1/unattended).
        for _ in 0..5 {
            let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|line: &str| log_line(line));
            let started = crate::daemon::start(crate::daemon::Options {
                data_dir: folder.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                port: crate::host::DEFAULT_PORT,
                label: None,
                log,
                backend: None,
            })
            .await;
            match started {
                Ok(daemon) => {
                    shared.set_idle(None);
                    log_line("owner's daemon started");
                    return Some(daemon);
                }
                Err(error) if error.downcast_ref::<crate::lock::LockError>().is_some() => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(error) => {
                    log_line(&format!("owner's daemon didn't start: {error:#}"));
                    shared.set_idle(Some(PROFILE_UNAVAILABLE));
                    return None;
                }
            }
        }
        // Started while the service was stopped: it steps aside when it next asks (main.rs
        // hands_over), and the service tries again soon.
        log_line("the owner's own daemon has the folder: waiting for it to step aside");
        shared.set_idle(Some(OWNER_DAEMON_RUNNING));
        None
    }

    /// The pipe's security attributes, kept alive as long as the pipe is created with them.
    struct PipeSecurity {
        descriptor: *mut c_void,
        attributes: win::SECURITY_ATTRIBUTES,
    }
    // SAFETY: the descriptor is only read by CreateNamedPipe.
    unsafe impl Send for PipeSecurity {}

    impl PipeSecurity {
        fn new() -> Option<Self> {
            let sddl = win::wide(PIPE_SDDL);
            let mut descriptor: *mut c_void = std::ptr::null_mut();
            let ok = unsafe {
                win::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    win::SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then(|| PipeSecurity {
                descriptor,
                attributes: win::SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<win::SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
            })
        }

        fn create(&mut self, first: bool) -> std::io::Result<NamedPipeServer> {
            let mut options = ServerOptions::new();
            options.first_pipe_instance(first);
            // SAFETY: valid SECURITY_ATTRIBUTES for the call.
            unsafe {
                options.create_with_security_attributes_raw(
                    PIPE_NAME,
                    (&mut self.attributes as *mut win::SECURITY_ATTRIBUTES).cast(),
                )
            }
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            unsafe { win::LocalFree(self.descriptor) };
        }
    }

    async fn pipe_server(shared: Arc<Shared>) {
        let Some(mut security) = PipeSecurity::new() else {
            log_line("no pipe security descriptor: the pipe stays closed");
            return;
        };
        let mut server = match security.create(true) {
            Ok(server) => server,
            Err(error) => {
                log_line(&format!("pipe: {error}"));
                return;
            }
        };
        loop {
            let connected = server.connect().await;
            let next = match security.create(false) {
                Ok(next) => next,
                Err(error) => {
                    log_line(&format!("pipe: {error}"));
                    return;
                }
            };
            let client = std::mem::replace(&mut server, next);
            if connected.is_ok() {
                tokio::spawn(answer(client, shared.clone()));
            } else {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    /// Who is on the other end of the pipe.
    struct Caller {
        sid: String,
        admin: bool,
        pid: u32,
    }

    /// The client's user and whether it is an administrator, from its token: impersonated
    /// only for the time it takes to read it, on this thread (no await in between).
    fn caller_of(pipe: &NamedPipeServer) -> Option<Caller> {
        use std::os::windows::io::AsRawHandle;
        let handle = pipe.as_raw_handle() as win::HANDLE;
        if unsafe { win::ImpersonateNamedPipeClient(handle) } == 0 {
            return None;
        }
        let mut token: win::HANDLE = std::ptr::null_mut();
        let opened = unsafe {
            win::OpenThreadToken(
                win::GetCurrentThread(),
                win::TOKEN_QUERY | win::TOKEN_DUPLICATE,
                1,
                &mut token,
            )
        };
        unsafe { win::RevertToSelf() };
        if opened == 0 {
            return None;
        }
        let token = Handle(token);
        let sid = win::token_user(token.0)?;
        let admin = win::Sid::parse("S-1-5-32-544").is_some_and(|admins| win::token_in_group(token.0, &admins));
        let mut pid = 0u32;
        if unsafe { win::GetNamedPipeClientProcessId(handle, &mut pid) } == 0 {
            return None;
        }
        Some(Caller { sid, admin, pid })
    }

    /// One request line in, one reply line out.
    async fn answer(mut pipe: NamedPipeServer, shared: Arc<Shared>) {
        let mut line = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(10), async {
            let mut chunk = [0u8; 512];
            loop {
                let n = pipe.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                line.extend_from_slice(&chunk[..n]);
                if line.contains(&b'\n') || line.len() > 64 * 1024 {
                    break;
                }
            }
            Ok::<_, std::io::Error>(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) || line.is_empty() {
            return;
        }
        // Impersonating needs a read first: the caller is known only now.
        let reply = match (serde_json::from_slice::<Request>(trim(&line)), caller_of(&pipe)) {
            (Ok(request), Some(caller)) => decide(&request, &caller, &shared),
            (Err(error), _) => Reply::refused("bad-request", error.to_string()),
            (_, None) => Reply::refused("no-caller", "couldn't tell who is asking"),
        };
        let mut text = serde_json::to_string(&reply).unwrap_or_default();
        text.push('\n');
        let _ = pipe.write_all(text.as_bytes()).await;
        let _ = pipe.flush().await;
    }

    fn trim(line: &[u8]) -> &[u8] {
        let end = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
        line[..end].strip_suffix(b"\r").unwrap_or(&line[..end])
    }

    /// Whether `pid` is the daemon holding `data_dir`'s lock (SD C5): linkd.json names it, and
    /// the lock can't be taken.
    fn holds_folder(data_dir: &std::path::Path, pid: u32) -> bool {
        let named = std::fs::read(data_dir.join("linkd.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|discovery| discovery.get("pid").and_then(serde_json::Value::as_u64));
        if named != Some(u64::from(pid)) {
            return false;
        }
        match std::fs::File::open(data_dir.join("linkd.lock")) {
            Ok(file) => file.try_lock().is_err(),
            Err(_) => false,
        }
    }

    fn decide(request: &Request, caller: &Caller, shared: &Shared) -> Reply {
        if !shared.safe.load(std::sync::atomic::Ordering::Relaxed) {
            if let Request::Status { .. } = request {
                return Reply {
                    can_change: caller.admin,
                    idle: Some(MACHINE_DIR_UNSAFE.into()),
                    ..Reply::default()
                };
            }
            return Reply::refused(MACHINE_DIR_UNSAFE, "RealTimeX Link's ProgramData folder isn't safe to use");
        }
        let config = Config::load(&machine_dir());
        match request {
            Request::Status { data_dir } => {
                let enabled = config.runs(data_dir);
                Reply {
                    enabled,
                    can_change: caller.admin,
                    idle: if enabled { shared.idle() } else { None },
                    ..Reply::default()
                }
            }
            Request::Enable { data_dir } => {
                if !caller.admin {
                    return Reply::refused(
                        OS_ADMIN_REQUIRED,
                        "An administrator of this computer can change this",
                    );
                }
                let standard = standard_folder(&caller.sid).map(|(_, folder)| folder);
                if !standard.is_some_and(|folder| crate::paths::same_folder(&folder, data_dir, "windows"))
                    || !holds_folder(data_dir, caller.pid)
                {
                    return Reply::refused(
                        STANDARD_ONLY,
                        "Only this computer's own RealTimeX Link can start with it",
                    );
                }
                let on = Config {
                    enabled: true,
                    owner_sid: caller.sid.clone(),
                    data_dir: data_dir.clone(),
                };
                if let Err(error) = on.save(&machine_dir()) {
                    return Reply::refused("io", error.to_string());
                }
                log_line(&format!("turned on by {}", caller.sid));
                shared.changed.notify_one();
                Reply { enabled: true, can_change: true, ..Reply::default() }
            }
            Request::Disable { data_dir } => {
                // The owner (their token reaches the daemon), an administrator, or the service
                // itself (the owner's daemon it runs, answering POST /v1/unattended).
                let allowed = caller.admin
                    || caller.sid == "S-1-5-18"
                    || (caller.sid == config.owner_sid && config.runs(data_dir));
                if !allowed {
                    return Reply::refused(
                        OS_ADMIN_REQUIRED,
                        "An administrator of this computer can change this",
                    );
                }
                if let Err(error) = Config::default().save(&machine_dir()) {
                    return Reply::refused("io", error.to_string());
                }
                log_line(&format!("turned off by {}", caller.sid));
                // Cleared first; the daemon stops right after this answer goes out (C4).
                shared.changed.notify_one();
                Reply { enabled: false, can_change: caller.admin, ..Reply::default() }
            }
        }
    }
}

/// Windows CI (scripts/build.ps1 runs `service::`): the ProgramData folder is made safe whatever
/// another user left in it (Review of !12).
#[cfg(all(test, windows))]
mod tests {


    use super::win;

    fn icacls(path: &std::path::Path) -> String {
        let out = crate::util::command(r"C:\Windows\System32\icacls.exe").arg(path).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn a_planted_grant_junction_and_scratch_folder_are_undone() {
        let root = std::env::temp_dir().join(format!("rtx-secure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("RealTimeX Link");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.txt"), b"not Link's").unwrap();
        std::fs::write(dir.join("unattended.json"), b"{}").unwrap();
        std::fs::write(dir.join("service.log"), b"old").unwrap();
        std::fs::create_dir_all(dir.join("updates").join("old")).unwrap();
        // Another user's explicit grants, on the folder and on a file in it.
        for path in [&dir, &dir.join("service.log")] {
            let granted = crate::util::command(r"C:\Windows\System32\icacls.exe")
                .arg(path)
                .args(["/grant", "*S-1-1-0:(F)"])
                .status()
                .unwrap();
            assert!(granted.success());
        }
        // A junction planted inside, pointing elsewhere.
        let junction = dir.join("planted");
        let made = crate::util::command(r"C:\Windows\System32\cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&elsewhere)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(icacls(&dir).contains("Everyone") || icacls(&dir).contains("S-1-1-0"));

        win::secure_tree(&dir, &["updates", "transfers"]).unwrap();

        assert!(!junction.exists(), "the junction is gone");
        assert!(elsewhere.join("keep.txt").exists(), "what it pointed at is untouched");
        assert!(!dir.join("updates").exists(), "an untrusted tree's scratch folders are emptied");
        // The folder wasn't trusted, so its choice is gone too.
        assert!(!dir.join("unattended.json").exists());
        for path in [&dir, &dir.join("service.log")] {
            let acl = icacls(path);
            assert!(!acl.contains("Everyone") && !acl.contains("S-1-1-0"), "{acl}");
            assert!(acl.contains("SYSTEM"), "{acl}");
        }
        // A link in place of the folder itself is refused, not followed.
        let fake = root.join("fake");
        let made = crate::util::command(r"C:\Windows\System32\cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&fake)
            .arg(&elsewhere)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(win::secure_tree(&fake, &[]).is_err());
        let _ = std::fs::remove_dir(&fake);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rtx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    const ENABLED: &[u8] =
        br#"{"enabled":true,"ownerSid":"S-1-5-21-1-2-3-500","dataDir":"C:/Users/admin/AppData/Roaming/RealTimeX Link"}"#;

    #[test]
    fn a_choice_planted_in_an_untrusted_folder_is_dropped_and_a_trusted_one_kept() {
        let root = scratch("provenance");
        // Someone else made the folder first, writable by everyone, with an enabled choice.
        let planted = root.join("planted");
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::write(planted.join("unattended.json"), ENABLED).unwrap();
        let granted = crate::util::command(r"C:\Windows\System32\icacls.exe")
            .arg(&planted)
            .args(["/grant", "*S-1-1-0:(OI)(CI)(F)"])
            .status()
            .unwrap();
        assert!(granted.success());
        assert!(!win::is_trusted(&planted));
        assert_eq!(win::secure_tree(&planted, &[]).unwrap(), win::Trust::Reset);
        assert!(!planted.join("unattended.json").exists(), "an untrusted choice is dropped");
        assert!(!crate::unattended::Config::load(&planted).enabled, "so it starts off");
        // A folder that is already Link's keeps its choice across starts.
        let trusted = root.join("trusted");
        assert_eq!(win::secure_tree(&trusted, &[]).unwrap(), win::Trust::Kept);
        std::fs::write(trusted.join("unattended.json"), ENABLED).unwrap();
        assert!(win::is_trusted(&trusted));
        assert_eq!(win::secure_tree(&trusted, &[]).unwrap(), win::Trust::Kept);
        assert!(crate::unattended::Config::load(&trusted).enabled);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A conditional allow (SDDL XA) grants like a plain one: a folder whose only stranger's
    /// write grant is one isn't trusted, and its choice goes (Review of !12).
    #[test]
    fn a_conditional_write_grant_makes_the_folder_untrusted() {
        let root = scratch("conditional");
        let conditional = "(XA;OICI;FA;;;WD;(Member_of {SID(WD)}))";
        for on_config in [false, true] {
            let dir = root.join(if on_config { "on-config" } else { "on-folder" });
            assert_eq!(win::secure_tree(&dir, &[]).unwrap(), win::Trust::Kept);
            let config = dir.join("unattended.json");
            std::fs::write(&config, ENABLED).unwrap();
            assert!(win::is_trusted(&dir) && win::is_trusted(&config));
            // Owned by administrators still; only the extra conditional grant differs.
            let target = if on_config { &config } else { &dir };
            win::set_security(target, Some(&format!("{}{conditional}", win::MACHINE_DIR_SDDL)))
                .unwrap();
            assert!(!win::is_trusted(target), "on_config {on_config}");
            assert_eq!(win::secure_tree(&dir, &[]).unwrap(), win::Trust::Reset);
            assert!(!config.exists(), "the enabled choice is dropped");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The start-up path itself (prepare_machine_dir, then the log): nothing is written through
    /// a planted link, on the refusal path or after a repair.
    #[test]
    fn startup_writes_nothing_through_a_planted_link() {
        let root = scratch("startup");
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("sentinel.txt"), b"before").unwrap();
        // The folder itself is a junction: refused, and the log never reaches the target.
        let junction = root.join("RealTimeX Link");
        let made = crate::util::command(r"C:\Windows\System32\cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(!super::prepare_machine_dir(&junction));
        assert!(super::trusted_dir().is_none());
        super::log_line("after a refusal");
        let entries: Vec<_> = std::fs::read_dir(&target).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("sentinel.txt")]);
        assert_eq!(std::fs::read(target.join("sentinel.txt")).unwrap(), b"before");
        std::fs::remove_dir(&junction).unwrap();
        // A real folder whose service.log is a link to a file elsewhere: the link goes, and the
        // log is written to a file of its own.
        let dir = root.join("Link");
        std::fs::create_dir_all(&dir).unwrap();
        let outside = target.join("sentinel.txt");
        let linked = crate::util::command(r"C:\Windows\System32\cmd.exe")
            .args(["/C", "mklink"])
            .arg(dir.join("service.log"))
            .arg(&outside)
            .status()
            .unwrap();
        assert!(linked.success());
        assert!(super::prepare_machine_dir(&dir));
        super::log_line("after a repair");
        assert_eq!(std::fs::read(&outside).unwrap(), b"before", "the link's target is untouched");
        let log = std::fs::read_to_string(dir.join("service.log")).unwrap();
        assert!(log.contains("after a repair"));
        *super::TRUSTED_DIR.write().unwrap() = None;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A trusted tree keeps a live transfer, its reader's access and a running download when it
    /// is secured again (the installer over this version, the service at its start); only what
    /// is stale is swept (Review of !12).
    #[test]
    fn a_trusted_tree_keeps_live_transfers_and_downloads() {
        let root = scratch("live");
        let dir = root.join("RealTimeX Link");
        assert_eq!(win::secure_tree(&dir, &super::SCRATCH).unwrap(), win::Trust::Kept);
        let reader = "S-1-5-32-545";
        let transfers = dir.join("transfers");
        win::create_protected_dir(&transfers, None).unwrap();
        let live = transfers.join("2-reader");
        win::create_protected_dir(&live, Some(reader)).unwrap();
        std::fs::write(live.join("pasted.txt"), b"live").unwrap();
        let download = dir.join("updates").join("now");
        win::create_protected_dir(&dir.join("updates"), None).unwrap();
        win::create_protected_dir(&download, None).unwrap();
        std::fs::write(download.join("setup.exe"), b"setup").unwrap();
        // Held open, as a transfer being written is.
        let open = std::fs::OpenOptions::new().append(true).open(live.join("pasted.txt")).unwrap();
        let before = icacls(&live);
        assert!(before.contains("Users") || before.contains(reader), "{before}");

        assert_eq!(win::secure_tree(&dir, &super::SCRATCH).unwrap(), win::Trust::Kept);
        assert_eq!(std::fs::read(live.join("pasted.txt")).unwrap(), b"live");
        assert_eq!(icacls(&live), before, "the reader keeps access");
        assert!(download.join("setup.exe").exists());
        drop(open);

        // Fresh entries outlive a sweep; with no age left, everything goes.
        win::sweep_stale(&dir, &super::SCRATCH, std::time::Duration::from_secs(3600));
        assert!(live.exists() && download.exists());
        win::sweep_stale(&dir, &super::SCRATCH, std::time::Duration::ZERO);
        assert!(!live.exists() && !download.exists());
        assert!(transfers.exists(), "the folders themselves stay");

        // A scratch folder that isn't trusted itself goes, even in a trusted tree.
        let updates = dir.join("updates");
        std::fs::create_dir_all(updates.join("planted")).unwrap();
        let granted = crate::util::command(r"C:\Windows\System32\icacls.exe")
            .arg(&updates)
            .args(["/grant", "*S-1-1-0:(OI)(CI)(F)"])
            .status()
            .unwrap();
        assert!(granted.success());
        assert_eq!(win::secure_tree(&dir, &super::SCRATCH).unwrap(), win::Trust::Kept);
        assert!(!updates.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
