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
    let session = crate::helper::console_session()
        .ok_or_else(|| ApiError::coded(409, "no-console", "Nobody is at this computer's screen"))?;
    let user = win::session_user(session).ok_or_else(|| {
        ApiError::coded(
            409,
            "nobody-signed-in",
            "Files can be pasted once someone signs in on this computer",
        )
    })?;
    let base = crate::unattended::machine_dir().join("transfers");
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
    let base = crate::unattended::machine_dir().join("updates");
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

/// The service's log, next to its choice: it has no console.
#[cfg(windows)]
fn log_line(line: &str) {
    use std::io::Write;
    let dir = crate::unattended::machine_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("service.log"))
    {
        let _ = writeln!(file, "{} {line}", chrono_like_now());
    }
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
        PROFILE_UNAVAILABLE, SERVICE_NAME, STANDARD_ONLY,
    };

    /// Who may use the pipe: SYSTEM and administrators fully, signed-in users to ask; never
    /// over the network (SD: no SMB access).
    const PIPE_SDDL: &str = "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";
    /// Why the service doesn't run the daemon although it is on.
    const OWNER_NOT_ADMIN: &str = "owner-not-admin";

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
        log_line("service started");
        let shared = Arc::new(Shared::default());
        tokio::spawn(pipe_server(shared.clone()));
        // The folder this process ran a daemon on: process-wide state (paths) is set once, so
        // another owner's folder needs a fresh process (the SCM restarts the service).
        let mut served: Option<PathBuf> = None;
        loop {
            let config = Config::load(&machine_dir());
            let daemon = if config.enabled {
                if served.as_ref().is_some_and(|dir| !crate::paths::same_folder(dir, &config.data_dir, "windows")) {
                    log_line("another owner's folder: restarting the service");
                    std::process::exit(1);
                }
                start_owner_daemon(&config, &shared).await
            } else {
                shared.set_idle(None);
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
                // Not running although on: try again now and then (a profile that comes back).
                () = tokio::time::sleep(Duration::from_secs(60)), if daemon.is_none() && config.enabled => false,
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
        for _ in 0..30 {
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
        log_line("the owner's own daemon kept the folder: idle until the next change");
        shared.set_idle(Some(PROFILE_UNAVAILABLE));
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
        Some(Caller { sid, admin })
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

    fn decide(request: &Request, caller: &Caller, shared: &Shared) -> Reply {
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
                if !standard.is_some_and(|folder| crate::paths::same_folder(&folder, data_dir, "windows")) {
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
