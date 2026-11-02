//! Windows on Windows (realtimex-ai-app#3055): visible, unowned top-level windows, front to
//! back (rtx-screens' list), and focusing one.
//!
//! Windows lets a process set the foreground window only in some cases; attaching to the
//! foreground thread's input first (AttachThreadInput) is the usual way through. Whether it
//! worked is checked: the window must be in front within FOCUS_CHECK. A window of an elevated
//! process can't be focused from Link at medium integrity (UIPI): that is `input-blocked`.
//!
//! A UWP app's window (Calculator, Settings) belongs to ApplicationFrameHost; the app itself
//! draws in a `Windows.UI.Core.CoreWindow` child of another process. The list names that process,
//! so the window's `appKey` is the app's package (`pkg:Microsoft.WindowsCalculator_8wekyb3d8bbwe`),
//! not the frame host every UWP app shares.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation,
    TokenIntegrityLevel, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx,
    WTSINFOEXW, WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
    OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, FindWindowExW, GetAncestor, GetCursorPos, GetForegroundWindow, GetWindow,
    GetWindowThreadProcessId, IsIconic, IsWindow, SetForegroundWindow, ShowWindow, WindowFromPoint,
    GA_ROOT, GW_OWNER, SW_RESTORE,
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
        .map(|mut info| {
            if is_frame_host(&info.app) {
                if let Some((pid, exe)) = hosted_app(hwnd(info.id as u64)) {
                    let (app, name) = app_names(&exe);
                    info.pid = pid as i32;
                    info.app = app;
                    info.app_name = name;
                }
            }
            Listed {
                id: WindowId::Win(info.id as u64),
                focused: hwnd(info.id as u64) == foreground,
                info,
            }
        })
        .collect())
}

/// ApplicationFrameHost, the process that frames every UWP app's window (rtx-screens names a
/// window's app by its executable, lowercased; a full path too, should it ever give one).
fn is_frame_host(app: &str) -> bool {
    let app = app.to_ascii_lowercase();
    let name = app.rsplit(['\\', '/']).next().unwrap_or(&app);
    name == "applicationframehost.exe"
}

/// A program's `app` and `appName` as rtx-screens gives them: the executable's file name
/// lowercased, and its name without `.exe` ("CalculatorApp").
fn app_names(exe: &str) -> (String, String) {
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    let stem = file
        .strip_suffix(".exe")
        .or_else(|| file.strip_suffix(".EXE"))
        .unwrap_or(file);
    (file.to_ascii_lowercase(), stem.to_string())
}

/// The UWP app in an ApplicationFrameHost window: its CoreWindow child's process and its
/// executable's path. None when the frame hosts nothing now (a suspended or closing app).
fn hosted_app(frame: HWND) -> Option<(u32, String)> {
    // SAFETY: plain Win32 queries on a window handle; a closed one fails harmlessly.
    let core = unsafe {
        FindWindowExW(Some(frame), None, w!("Windows.UI.Core.CoreWindow"), PCWSTR::null())
    }
    .ok()
    .filter(|core| !core.is_invalid())?;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(core, Some(&mut pid)) };
    let mut frame_pid = 0u32;
    unsafe { GetWindowThreadProcessId(frame, Some(&mut frame_pid)) };
    if pid == 0 || pid == frame_pid {
        return None;
    }
    Some((pid, exe_path(pid)?))
}

/// A process's executable, its full path.
fn exe_path(pid: u32) -> Option<String> {
    // SAFETY: a handle this function opens and closes; the buffer and its length.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let read = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(process);
        read.ok()?;
        Some(String::from_utf16_lossy(&buffer[..size as usize]))
    }
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

/// Locked (`Some(true)`), not locked (`Some(false)`), or unknown (`None`), for refusing window
/// lists and accessibility (which fail closed when it is unknown): locked while the input desktop
/// isn't the user's own (the sign-in screen, a UAC prompt), and while this session is locked.
/// The Windows 11 lock screen's curtain (LockApp) is on the Default desktop, so only the session's
/// own lock state tells (QA of realtimex-ai-app#3055).
pub fn lock_state() -> Option<bool> {
    if !super::desktop::input_desktop_is_default() {
        return Some(true);
    }
    session_locked()
}

/// Known to be locked (a screenshot's `locked`, which only says what the picture shows).
pub fn screen_locked() -> bool {
    lock_state() == Some(true)
}

/// Whether the session this process runs in (the signed-in user's, also under Link's service)
/// is locked, as the session manager says: from the moment the lock screen comes up until it is
/// unlocked. None when it can't be read.
fn session_locked() -> Option<bool> {
    let mut session = 0u32;
    // SAFETY: an out value of the right type.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.ok()?;
    let mut buffer = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: the buffer is Windows' own, read only within the size it reports, freed once.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
    }
    .ok()?;
    if buffer.is_null() {
        return None;
    }
    let flags = if bytes as usize >= std::mem::size_of::<WTSINFOEXW>() {
        // SAFETY: a WTSINFOEXW, as WTSSessionInfoEx answers; level 1 is its only member.
        let info = unsafe { &*(buffer.0 as *const WTSINFOEXW) };
        (info.Level == 1).then(|| unsafe { info.Data.WTSInfoExLevel1.SessionFlags })
    } else {
        None
    };
    // SAFETY: memory WTSQuerySessionInformationW allocated.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    session_flags(flags?)
}

/// WTSINFOEX_LEVEL1's SessionFlags: WTS_SESSIONSTATE_LOCK (0) is locked on Windows 10 and 11,
/// WTS_SESSIONSTATE_UNLOCK (1) unlocked, anything else (WTS_SESSIONSTATE_UNKNOWN) unknown.
fn session_flags(flags: i32) -> Option<bool> {
    match flags as u32 {
        WTS_SESSIONSTATE_LOCK => Some(true),
        WTS_SESSIONSTATE_UNLOCK => Some(false),
        _ => None,
    }
}

/// Mandatory integrity levels (their RIDs).
const INTEGRITY_HIGH: u32 = 0x3000;
const INTEGRITY_SYSTEM: u32 = 0x4000;

/// A process's mandatory integrity level: 0x2000 medium, 0x3000 high (run as administrator),
/// 0x4000 system.
fn integrity(process: HANDLE) -> Option<u32> {
    // SAFETY: a token this function opens and closes; the buffer holds TOKEN_MANDATORY_LABEL and
    // the SID it points into; GetSidSubAuthority* read that SID.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut buffer = [0u64; 16];
        let mut size = 0u32;
        let read = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            std::mem::size_of_val(&buffer) as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        read.ok()?;
        let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        let count = GetSidSubAuthorityCount(sid);
        if count.is_null() || *count == 0 {
            return None;
        }
        let rid = GetSidSubAuthority(sid, u32::from(*count) - 1);
        (!rid.is_null()).then(|| *rid)
    }
}

/// This process's integrity level, read once.
fn own_integrity() -> Option<u32> {
    static OWN: OnceLock<Option<u32>> = OnceLock::new();
    // SAFETY: the pseudo handle of this process needs no closing.
    *OWN.get_or_init(|| integrity(unsafe { GetCurrentProcess() }))
}

/// The integrity level of the process behind `window`; None when it can't be read.
fn window_integrity(window: HWND) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: plain queries; the handle is closed.
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let level = integrity(process);
        let _ = CloseHandle(process);
        level
    }
}

/// Whether input from a process at integrity `own` reaches a window at `target` (UIPI: never a
/// higher level). A window whose level can't be read counts as out of reach, unless this process
/// runs as SYSTEM (Link's service), which reaches every window.
fn reaches(own: Option<u32>, target: Option<u32>) -> bool {
    let own = own.unwrap_or(0);
    if own >= INTEGRITY_SYSTEM {
        return true;
    }
    target.is_some_and(|target| target <= own)
}

/// Windows drops input aimed at a window of a process with a higher integrity level (an app run
/// as administrator) and SendInput still says it sent it (QA of realtimex-ai-app#3055): before a
/// press goes out, `input-blocked` when the window that would get it is such one. `pointer`: the
/// window under the pointer (a button or the wheel), else the one in front (keys).
pub fn input_reaches(pointer: bool) -> Result<()> {
    // SAFETY: plain getters.
    let window = unsafe {
        if pointer {
            let mut at = POINT::default();
            if GetCursorPos(&mut at).is_err() {
                return Ok(());
            }
            let under = WindowFromPoint(at);
            if under.is_invalid() {
                return Ok(());
            }
            let root = GetAncestor(under, GA_ROOT);
            if root.is_invalid() {
                under
            } else {
                root
            }
        } else {
            GetForegroundWindow()
        }
    };
    if window.is_invalid() {
        return Ok(());
    }
    let own = own_integrity();
    if reaches(own, None) {
        return Ok(());
    }
    let target = window_integrity(window);
    if reaches(own, target) {
        return Ok(());
    }
    let elevated = target.is_some_and(|level| level >= INTEGRITY_HIGH);
    Err(refused(
        "input-blocked",
        if elevated {
            "The window there belongs to an app running as administrator: Windows doesn't let \
             Link's input reach it. Ask the person at the computer to close it or switch to \
             another window, or turn on \"Start with this computer\" (service mode)."
        } else {
            "Windows doesn't let Link's input reach the window there (it belongs to a protected \
             or higher-privileged process). Ask the person at the computer."
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_flags_say_locked_unlocked_or_unknown() {
        assert_eq!(session_flags(0), Some(true));
        assert_eq!(session_flags(1), Some(false));
        assert_eq!(session_flags(-1), None);
    }

    #[test]
    fn input_reaches_only_windows_at_or_below_this_level() {
        let (medium, high) = (Some(0x2000), Some(0x3000));
        assert!(reaches(medium, medium));
        assert!(reaches(medium, Some(0x1000)));
        assert!(!reaches(medium, high));
        assert!(!reaches(medium, None), "unknown counts as out of reach");
        assert!(reaches(high, high));
        assert!(reaches(Some(0x4000), None), "SYSTEM reaches everything");
    }

    #[test]
    fn a_uwp_frame_is_named_by_the_app_it_hosts() {
        assert!(is_frame_host("applicationframehost.exe"));
        assert!(is_frame_host(r"C:\Windows\System32\ApplicationFrameHost.exe"));
        assert!(!is_frame_host("notepad.exe"));
        assert_eq!(
            app_names(r"C:\Program Files\WindowsApps\Microsoft.WindowsCalculator_11.2405.2.0_x64__8wekyb3d8bbwe\CalculatorApp.exe"),
            ("calculatorapp.exe".to_string(), "CalculatorApp".to_string())
        );
    }
}
