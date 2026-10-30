//! The input desktop (realtimex-link#10): Windows shows the sign-in and lock screens on the
//! Winlogon desktop and UAC prompts on a secure desktop, not on the one rtx-control started on.
//! Capture and SendInput only reach the desktop the calling thread is on, so before each request
//! the control loop moves its thread to whichever desktop receives input now.
//!
//! Only a process running as SYSTEM may open Winlogon's desktop: under Link's service it does.
//! Run as the signed-in user, opening it fails, the thread stays where it is, and input there is
//! refused as before (`input-blocked`).

use std::cell::RefCell;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

/// GENERIC_ALL: capture, input and the clipboard on that desktop.
const DESKTOP_ALL: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0x1000_0000);

thread_local! {
    /// The desktop this thread was moved to, kept open while the thread is on it.
    static CURRENT: RefCell<Option<(HDESK, String)>> = const { RefCell::new(None) };
}

/// Move this thread to the input desktop when it changed. Returns its name after a move.
pub fn follow_input_desktop() -> Option<String> {
    // SAFETY: plain Win32 calls on handles this function owns.
    let desktop = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ALL) }.ok()?;
    let Some(name) = desktop_name(desktop) else {
        let _ = unsafe { CloseDesktop(desktop) };
        return None;
    };
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        if current.as_ref().is_some_and(|(_, on)| *on == name) {
            let _ = unsafe { CloseDesktop(desktop) };
            return None;
        }
        // Fails while the thread owns a window or a hook: rtx-control's main thread owns none
        // (the clipboard's window has a thread of its own).
        if unsafe { SetThreadDesktop(desktop) }.is_err() {
            let _ = unsafe { CloseDesktop(desktop) };
            return None;
        }
        if let Some((old, _)) = current.replace((desktop, name.clone())) {
            let _ = unsafe { CloseDesktop(old) };
        }
        Some(name)
    })
}

/// "Default", "Winlogon", or a UAC prompt's secure desktop.
fn desktop_name(desktop: HDESK) -> Option<String> {
    let mut buffer = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: the buffer and its size in bytes.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .ok()?;
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}
