//! One hosting daemon per data folder: a standalone RealTimeX Link and the one RealTimeX
//! installed must not both listen and fight over the clipboard. The owner holds an OS lock on
//! the lock file (flock on Unix, LockFileEx on Windows) for as long as it runs. The OS lets go of
//! it when the process ends however it ends (killed at logoff, a crash), so a stale lock can't
//! keep hosting off after a restart, and two starters can't both take one over. The pid in the
//! file is only for people reading it.

use std::fs::{self, File, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::files;

pub const LOCK_FILE: &str = "linkd.lock";

#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock: its pid, or 0 when the file can't tell (Windows doesn't
    /// let others read a locked file).
    AlreadyRunning(u32),
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::AlreadyRunning(0) => write!(f, "RealTimeX Link is already running"),
            LockError::AlreadyRunning(pid) => {
                write!(f, "RealTimeX Link is already running (pid {pid})")
            }
            LockError::Io(error) => write!(f, "Could not take the RealTimeX Link lock: {error}"),
        }
    }
}

impl std::error::Error for LockError {}

pub struct InstanceLock {
    file: File,
    released: AtomicBool,
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        files::ensure_private_dir(data_dir).map_err(LockError::Io)?;
        let path = data_dir.join(LOCK_FILE);
        // Never truncated before it is ours, and never removed: a removed lock file lets one
        // starter lock the old file while another locks a new one.
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut text = String::new();
                let _ = file.read_to_string(&mut text);
                return Err(LockError::AlreadyRunning(
                    text.trim().parse::<u32>().unwrap_or(0),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(LockError::Io(error)),
        }
        let written = (|| -> std::io::Result<()> {
            file.set_len(0)?;
            file.rewind()?;
            file.write_all(std::process::id().to_string().as_bytes())?;
            file.flush()
        })();
        if let Err(error) = written {
            let _ = file.unlock();
            return Err(LockError::Io(error));
        }
        Ok(Self {
            file,
            released: AtomicBool::new(false),
        })
    }

    /// Let go of the lock now (closing the file does too). The pid goes first, while it is ours.
    /// Once: after that the file may already be the next owner's.
    pub fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
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
    fn one_owner_at_a_time_and_the_lock_is_free_again_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = InstanceLock::acquire(dir.path()).unwrap();
        // A second open of the same file is a second owner, even in this process.
        assert!(matches!(
            InstanceLock::acquire(dir.path()),
            Err(LockError::AlreadyRunning(_))
        ));
        drop(first);
        let second = InstanceLock::acquire(dir.path()).unwrap();
        drop(second);
        // The file stays: removing it would let two starters lock different files.
        assert!(dir.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn a_lock_file_left_behind_is_not_an_owner() {
        let dir = tempfile::tempdir().unwrap();
        // A daemon killed without cleanup leaves its pid behind (this process's: alive, too).
        fs::write(
            dir.path().join(LOCK_FILE),
            std::process::id().to_string(),
        )
        .unwrap();
        let held = InstanceLock::acquire(dir.path()).unwrap();
        drop(held);
        fs::write(dir.path().join(LOCK_FILE), "not a pid").unwrap();
        let _held = InstanceLock::acquire(dir.path()).unwrap();
    }

    // Reads the held lock file: Unix only (Windows keeps others out of a locked file).
    #[cfg(unix)]
    #[test]
    fn released_once_so_the_next_owners_pid_stays() {
        let dir = tempfile::tempdir().unwrap();
        let first = InstanceLock::acquire(dir.path()).unwrap();
        // daemon.rs releases on stop, and the lock is dropped later still.
        first.release();
        let second = InstanceLock::acquire(dir.path()).unwrap();
        drop(first);
        drop(second);
        let third = InstanceLock::acquire(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(LOCK_FILE)).unwrap(),
            std::process::id().to_string()
        );
        drop(third);
    }

    #[cfg(unix)]
    #[test]
    fn another_process_holding_it_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let _held = InstanceLock::acquire(dir.path()).unwrap();
        // flock from another process: it can't have the lock while this one holds it.
        let status = crate::util::command("flock")
            .args(["-n"])
            .arg(dir.path().join(LOCK_FILE))
            .arg("true")
            .status();
        if let Ok(status) = status {
            assert!(!status.success());
        }
        match InstanceLock::acquire(dir.path()) {
            Err(LockError::AlreadyRunning(pid)) => assert_eq!(pid, std::process::id()),
            _ => panic!("the lock was taken twice"),
        }
    }
}
