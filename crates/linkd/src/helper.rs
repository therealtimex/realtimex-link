//! The long-lived rtx-control process (backend.rs). Normally a child of this daemon. Under Link's
//! Windows service (realtimex-link#10) the daemon runs as SYSTEM in session 0, which has no
//! screen: the helper runs as SYSTEM in the active console session instead (the sign-in screen,
//! the lock screen, or the signed-in person's desktop), with its JSON lines over pipes, and is
//! ended when another session takes the console.

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
}

impl Child {
    pub fn start_kill(&mut self) {
        match self {
            Child::Local(child) => {
                let _ = child.start_kill();
            }
            #[cfg(windows)]
            Child::Session(process) => process.kill(),
        }
    }

    /// Its exit code, once it exited.
    pub async fn wait(&mut self) -> Option<i32> {
        match self {
            Child::Local(child) => child.wait().await.ok().and_then(|status| status.code()),
            #[cfg(windows)]
            Child::Session(process) => process.wait().await,
        }
    }
}

/// Start the helper: into the active console session under the service, else as a child.
pub fn spawn(binary: &Path, local: impl FnOnce() -> std::io::Result<tokio::process::Child>) -> std::io::Result<Spawned> {
    #[cfg(windows)]
    if crate::service::active() {
        return session::spawn(binary);
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

/// The session that has the console now (Windows; None elsewhere or with no console).
pub fn console_session() -> Option<u32> {
    #[cfg(windows)]
    {
        let session = unsafe { crate::service::win::WTSGetActiveConsoleSessionId() };
        (session != crate::service::win::NO_SESSION).then_some(session)
    }
    #[cfg(not(windows))]
    None
}

#[cfg(windows)]
mod session {
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::path::Path;

    use super::{Child, Reader, Spawned, Writer};
    use crate::service::win::{self, Handle, HANDLE};

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

    /// An anonymous pipe: (the child's end, inheritable; ours, not).
    fn pipe(child_reads: bool) -> std::io::Result<(Handle, Handle)> {
        let attributes = win::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<win::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { win::CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read, write) = (Handle(read), Handle(write));
        let (child, ours) = if child_reads { (read, write) } else { (write, read) };
        unsafe { win::SetHandleInformation(ours.0, win::HANDLE_FLAG_INHERIT, 0) };
        Ok((child, ours))
    }

    /// Our end of a pipe as an async file.
    fn file(handle: Handle) -> tokio::fs::File {
        let raw = handle.0 as RawHandle;
        std::mem::forget(handle);
        // SAFETY: an owned handle, handed over whole.
        tokio::fs::File::from_std(unsafe { std::fs::File::from_raw_handle(raw) })
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

    pub fn spawn(binary: &Path) -> std::io::Result<Spawned> {
        let session = super::console_session()
            .ok_or_else(|| std::io::Error::other("no session has the console"))?;
        let token = console_token(session)?;
        let (child_in, our_in) = pipe(true)?;
        let (child_out, our_out) = pipe(false)?;
        let (child_err, our_err) = pipe(false)?;
        let mut desktop = win::wide(r"winsta0\default");
        let mut startup: win::STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<win::STARTUPINFOW>() as u32;
        startup.lpDesktop = desktop.as_mut_ptr();
        startup.dwFlags = win::STARTF_USESTDHANDLES;
        startup.hStdInput = child_in.0;
        startup.hStdOutput = child_out.0;
        startup.hStdError = child_err.0;
        // An absolute path, never a PATH lookup as SYSTEM (SD C2).
        let application = win::wide(&binary.to_string_lossy());
        let mut command_line = win::wide(&format!("\"{}\"", binary.display()));
        let directory = binary.parent().map(|dir| win::wide(&dir.to_string_lossy()));
        let mut information: win::PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            win::CreateProcessAsUserW(
                token.0,
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
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
        // The child holds its ends now; closing ours lets its exit end our reads.
        drop((child_in, child_out, child_err));
        let stdin: Writer = Box::new(file(our_in));
        let stdout: Reader = Box::new(file(our_out));
        let stderr: Reader = Box::new(file(our_err));
        Ok(Spawned {
            child: Child::Session(Process {
                handle: Handle(information.hProcess),
            }),
            stdin,
            stdout,
            stderr: Some(stderr),
            session: Some(session),
        })
    }
}
