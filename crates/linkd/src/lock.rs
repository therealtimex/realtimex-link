//! One hosting daemon per data folder: a standalone RealTimeX Link and the one RealTimeX
//! installed must not both listen and fight over the clipboard. The lock holds the owner's pid;
//! one left by a process that died is taken over.

use std::fs;
use std::path::{Path, PathBuf};

use crate::files;

pub const LOCK_FILE: &str = "linkd.lock";

#[derive(Debug)]
pub enum LockError {
    AlreadyRunning(u32),
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::AlreadyRunning(pid) => {
                write!(f, "RealTimeX Link is already running (pid {pid})")
            }
            LockError::Io(error) => write!(f, "Could not take the RealTimeX Link lock: {error}"),
        }
    }
}

impl std::error::Error for LockError {}

pub struct InstanceLock {
    file: PathBuf,
    pid: u32,
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        Self::acquire_as(data_dir, std::process::id(), alive)
    }

    pub fn acquire_as(
        data_dir: &Path,
        pid: u32,
        is_alive: fn(u32) -> bool,
    ) -> Result<Self, LockError> {
        files::ensure_private_dir(data_dir).map_err(LockError::Io)?;
        let file = data_dir.join(LOCK_FILE);
        for _ in 0..2 {
            match files::create_new(&file, &pid.to_string()) {
                Ok(()) => return Ok(Self { file, pid }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner = fs::read_to_string(&file)
                        .ok()
                        .and_then(|text| text.trim().parse::<u32>().ok())
                        .unwrap_or(0);
                    // A live owner keeps it, this process included: a daemon started twice must not fork.
                    if owner > 0 && is_alive(owner) {
                        return Err(LockError::AlreadyRunning(owner));
                    }
                    // Left by a process that died: take it over once.
                    let _ = fs::remove_file(&file);
                }
                Err(error) => return Err(LockError::Io(error)),
            }
        }
        Err(LockError::Io(std::io::Error::other(
            "the lock file keeps coming back",
        )))
    }

    pub fn release(&self) {
        let ours = fs::read_to_string(&self.file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            == Some(self.pid);
        if ours {
            let _ = fs::remove_file(&self.file);
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    // EPERM: it exists, it is just someone else's.
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: the handle is checked and closed; nothing else is done with it.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Access denied still means it exists.
            return std::io::Error::last_os_error().raw_os_error() == Some(5);
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_owner_and_a_dead_ones_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let first = InstanceLock::acquire_as(dir.path(), 4242, |_| true).unwrap();
        assert!(matches!(
            InstanceLock::acquire_as(dir.path(), 4343, |_| true),
            Err(LockError::AlreadyRunning(4242))
        ));
        std::mem::forget(first);
        let second = InstanceLock::acquire_as(dir.path(), 4343, |_| false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(LOCK_FILE)).unwrap(),
            "4343"
        );
        drop(second);
        assert!(!dir.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn this_process_does_not_take_its_own_lock_twice() {
        let dir = tempfile::tempdir().unwrap();
        let _held = InstanceLock::acquire(dir.path()).unwrap();
        assert!(matches!(
            InstanceLock::acquire(dir.path()),
            Err(LockError::AlreadyRunning(_))
        ));
    }
}
