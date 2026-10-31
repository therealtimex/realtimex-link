//! The long-lived rtx-control process (backend.rs). Normally a child of this daemon. Under Link's
//! Windows service (realtimex-link#10) the daemon runs as SYSTEM in session 0, which has no
//! screen: the helper runs as SYSTEM in the active console session instead (the sign-in screen,
//! the lock screen, or the signed-in person's desktop), with its JSON lines over pipes, and is
//! ended when another session takes the console. Under Link's macOS supervisor (#11) it runs in
//! the console's session through that session's bridge (bridges.rs), the same way.

use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite};

pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;
pub type Reader = Box<dyn AsyncRead + Send + Unpin>;

pub struct Spawned {
    pub child: Child,
    pub stdin: Writer,
    pub stdout: Reader,
    pub stderr: Option<Reader>,
    /// The console session it runs in, when it was started into one.
    pub session: Option<u32>,
}

pub enum Child {
    Local(tokio::process::Child),
    #[cfg(windows)]
    Session(session::Process),
    #[cfg(target_os = "macos")]
    Bridge(crate::bridges::InSession),
}

impl Child {
    pub fn start_kill(&mut self) {
        match self {
            Child::Local(child) => {
                let _ = child.start_kill();
            }
            #[cfg(windows)]
            Child::Session(process) => process.kill(),
            #[cfg(target_os = "macos")]
            Child::Bridge(helper) => helper.kill(),
        }
    }

    /// Its exit code, once it exited.
    pub async fn wait(&mut self) -> Option<i32> {
        match self {
            Child::Local(child) => child.wait().await.ok().and_then(|status| status.code()),
            #[cfg(windows)]
            Child::Session(process) => process.wait().await,
            // Its exit code stays in its session; its lines ending is what counts.
            #[cfg(target_os = "macos")]
            Child::Bridge(_) => None,
        }
    }
}

/// Start the helper: into the active console session under the service, else as a child.
pub async fn spawn(
    binary: &Path,
    local: impl FnOnce() -> std::io::Result<tokio::process::Child>,
) -> std::io::Result<Spawned> {
    #[cfg(windows)]
    if crate::service::active() {
        return session::spawn(binary).await;
    }
    #[cfg(target_os = "macos")]
    if crate::service::supervised() {
        return crate::bridges::spawn().await;
    }
    let _ = binary;
    let mut child = local()?;
    let missing = || std::io::Error::other("the helper has no pipes");
    let stdin: Writer = Box::new(child.stdin.take().ok_or_else(missing)?);
    let stdout: Reader = Box::new(child.stdout.take().ok_or_else(missing)?);
    let stderr = child.stderr.take().map(|stderr| Box::new(stderr) as Reader);
    Ok(Spawned {
        child: Child::Local(child),
        stdin,
        stdout,
        stderr,
        session: None,
    })
}

/// The session that has the console now (Windows; macOS under the supervisor, its bridge;
/// None elsewhere or with no console).
pub fn console_session() -> Option<u32> {
    #[cfg(windows)]
    {
        let session = unsafe { crate::service::win::WTSGetActiveConsoleSessionId() };
        (session != crate::service::win::NO_SESSION).then_some(session)
    }
    #[cfg(target_os = "macos")]
    {
        crate::bridges::console_session()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    None
}

/// Whether an input request goes to the login-window job, not rtx-control's CGEventPost
/// (#11 I4). Only the daemon decides it (C4).
pub fn input_via_job(op: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::service::supervised() && crate::bridges::via_job(op)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = op;
        false
    }
}

#[cfg(windows)]
mod session {
    use std::path::Path;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::{Child, Reader, Spawned, Writer};
    use crate::service::win::{self, Handle, HANDLE};

    /// Only SYSTEM may open the helper's pipes: the helper runs as SYSTEM, nobody else does.
    const HELPER_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)";
    /// How long the helper has to open its pipes.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

    /// A process started into another session, ended on drop.
    pub struct Process {
        handle: Handle,
    }

    impl Process {
        pub fn kill(&self) {
            unsafe { win::TerminateProcess(self.handle.0, 1) };
        }

        pub async fn wait(&self) -> Option<i32> {
            let raw = self.handle.0 as usize;
            tokio::task::spawn_blocking(move || {
                let handle = raw as HANDLE;
                if unsafe { win::WaitForSingleObject(handle, win::INFINITE) } != win::WAIT_OBJECT_0 {
                    return None;
                }
                let mut code = 0u32;
                (unsafe { win::GetExitCodeProcess(handle, &mut code) } != 0).then_some(code as i32)
            })
            .await
            .ok()
            .flatten()
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            self.kill();
        }
    }

    /// One of the helper's pipes, created here first (a name someone else made fails), one
    /// instance, one way.
    fn pipe(name: &str, inbound: bool) -> std::io::Result<NamedPipeServer> {
        let mut security = win::Security::new(HELPER_PIPE_SDDL)?;
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(true)
            .max_instances(1)
            .access_inbound(inbound)
            .access_outbound(!inbound)
            .reject_remote_clients(true);
        // SAFETY: valid SECURITY_ATTRIBUTES, alive for the call.
        unsafe { options.create_with_security_attributes_raw(name, security.attributes()) }
    }

    /// SYSTEM's own token, moved to the console session.
    fn console_token(session: u32) -> std::io::Result<Handle> {
        let mut own: HANDLE = std::ptr::null_mut();
        if unsafe { win::OpenProcessToken(win::GetCurrentProcess(), win::TOKEN_ALL_ACCESS, &mut own) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let own = Handle(own);
        let mut primary: HANDLE = std::ptr::null_mut();
        let duplicated = unsafe {
            win::DuplicateTokenEx(
                own.0,
                win::TOKEN_ALL_ACCESS,
                std::ptr::null(),
                win::SECURITY_IMPERSONATION,
                win::TOKEN_PRIMARY,
                &mut primary,
            )
        };
        if duplicated == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let primary = Handle(primary);
        let set = unsafe {
            win::SetTokenInformation(
                primary.0,
                win::TOKEN_SESSION_ID_CLASS,
                (&session as *const u32).cast(),
                4,
            )
        };
        if set == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(primary)
    }

    /// Start `binary --pipes <name>` as SYSTEM in `session`, inheriting nothing.
    fn start(binary: &Path, name: &str, session: u32) -> std::io::Result<Process> {
        let token = console_token(session)?;
        let mut desktop = win::wide(r"winsta0\default");
        let mut startup: win::STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<win::STARTUPINFOW>() as u32;
        startup.lpDesktop = desktop.as_mut_ptr();
        // An absolute path, never a PATH lookup as SYSTEM (SD C2).
        let application = win::wide(&binary.to_string_lossy());
        let mut command_line = win::wide(&format!("\"{}\" --pipes {name}", binary.display()));
        let directory = binary.parent().map(|dir| win::wide(&dir.to_string_lossy()));
        let mut information: win::PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            win::CreateProcessAsUserW(
                token.0,
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                win::CREATE_NO_WINDOW | win::CREATE_UNICODE_ENVIRONMENT,
                std::ptr::null(),
                directory.as_ref().map_or(std::ptr::null(), |dir| dir.as_ptr()),
                &startup,
                &mut information,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error());
        }
        drop(Handle(information.hThread));
        Ok(Process {
            handle: Handle(information.hProcess),
        })
    }

    /// The helper, as SYSTEM in the console session. It can't inherit handles across sessions
    /// (CreateProcessAsUser), so it opens named pipes this daemon created: `--pipes <name>`.
    pub async fn spawn(binary: &Path) -> std::io::Result<Spawned> {
        let session = super::console_session()
            .ok_or_else(|| std::io::Error::other("no session has the console"))?;
        let name = format!(r"\\.\pipe\RealTimeX-Link-Helper-{}", crate::util::uuid());
        let input = pipe(&format!("{name}-in"), false)?;
        let output = pipe(&format!("{name}-out"), true)?;
        let errors = pipe(&format!("{name}-err"), true)?;
        // Before any await: the Win32 structures it uses aren't Send.
        let process = start(binary, &name, session)?;
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
            input.connect().await?;
            output.connect().await?;
            errors.connect().await?;
            Ok::<_, std::io::Error>(())
        })
        .await;
        match connected {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(std::io::Error::other("the helper didn't open its pipes")),
        }
        let stdin: Writer = Box::new(input);
        let stdout: Reader = Box::new(output);
        let stderr: Reader = Box::new(errors);
        Ok(Spawned {
            child: Child::Session(process),
            stdin,
            stdout,
            stderr: Some(stderr),
            session: Some(session),
        })
    }
}
