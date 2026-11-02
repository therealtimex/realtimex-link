//! Windows on Windows (realtimex-ai-app#3055): visible, unowned top-level windows, front to
//! back (rtx-screens' list), and focusing one.
//!
//! Windows lets a process set the foreground window only in some cases; attaching to the
//! foreground thread's input first (AttachThreadInput) is the usual way through. Whether it
//! worked is checked: the window must be in front within FOCUS_CHECK. A window of an elevated
//! process can't be focused from Link at medium integrity (UIPI): that is `input-blocked`.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
    SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE,
};

use crate::platform::{refused, Listed};
use crate::winid::WindowId;

/// How long a focused window has to come to the front.
const FOCUS_CHECK: Duration = Duration::from_millis(500);

fn hwnd(raw: u64) -> HWND {
    HWND(raw as usize as *mut c_void)
}

/// Whether `window` has an owner (a dialog or tool window of another window).
fn owned(window: HWND) -> bool {
    // SAFETY: a plain query; an error is no owner.
    unsafe { GetWindow(window, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid())
}

pub fn list_windows() -> Result<Vec<Listed>> {
    // SAFETY: a plain getter.
    let foreground = unsafe { GetForegroundWindow() };
    Ok(rtx_screens::win::list_windows()?
        .into_iter()
        .filter(|info| !owned(hwnd(info.id as u64)))
        .map(|info| Listed {
            id: WindowId::Win(info.id as u64),
            focused: hwnd(info.id as u64) == foreground,
            info,
        })
        .collect())
}

/// Whether process `pid` runs elevated; None when that can't be read.
fn elevated(pid: u32) -> Option<bool> {
    // SAFETY: handles this function opens and closes; an out struct of the right size.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(process);
        opened.ok()?;
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let read = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        read.ok()?;
        Some(elevation.TokenIsElevated != 0)
    }
}

pub fn focus_window(window: &Listed) -> Result<()> {
    let WindowId::Win(raw) = window.id else {
        return Err(refused("window-not-found", "Not a window of this computer"));
    };
    let target = hwnd(raw);
    // SAFETY: plain Win32 calls on a window handle; a closed one fails harmlessly.
    unsafe {
        if !IsWindow(Some(target)).as_bool() {
            return Err(refused("window-not-found", "The window is closed"));
        }
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        let front = GetForegroundWindow();
        let front_thread = GetWindowThreadProcessId(front, None);
        let me = GetCurrentThreadId();
        let attached = front_thread != 0
            && front_thread != me
            && AttachThreadInput(me, front_thread, true).as_bool();
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
        if attached {
            let _ = AttachThreadInput(me, front_thread, false);
        }
    }
    let start = Instant::now();
    loop {
        // SAFETY: a plain getter.
        if unsafe { GetForegroundWindow() } == target {
            return Ok(());
        }
        if start.elapsed() >= FOCUS_CHECK {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if elevated(window.info.pid as u32) == Some(true) {
        return Err(refused(
            "input-blocked",
            "The window belongs to an app running as administrator, which Link can't bring forward",
        ));
    }
    Err(refused(
        "focus-refused",
        "Windows did not bring the window to the front",
    ))
}

/// Whether the input desktop is not the user's own (the lock or sign-in screen, a UAC prompt):
/// window titles must not be read from behind it.
pub fn screen_locked() -> bool {
    !super::desktop::input_desktop_is_default()
}
