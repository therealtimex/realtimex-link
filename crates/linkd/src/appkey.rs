//! `appKey` (realtimex-ai-app#3055 Addendum 2): a stable identity of the program behind a window,
//! computed from its pid, on every window and app Link lists (`/remote-control/windows`,
//! `ax/apps`, `/v1/ax/*`). Never the window title, and never a bare executable name (any program
//! can be called notepad.exe).
//!
//! | Platform | appKey, in order of preference |
//! |---|---|
//! | macOS | `bundle:<CFBundleIdentifier>` (rtx-control's `app`); else `exe:<real path>` |
//! | Windows | `pkg:<PackageFamilyName>`; else `exe:<full path, lowercased>` |
//! | Linux | `flatpak:<app id>`; `snap:<name>`; `desktop:<id>` (GIO_LAUNCHED_DESKTOP_FILE); else `exe:<path>` |
//!
//! A process Link can't open (protected, another user's without rights) has no key: null.

use serde_json::Value;

/// Add `appKey` to each window of rtx-control's `{windows:[…]}` answer.
pub fn add_keys(answer: &mut Value) {
    if let Some(list) = answer.get_mut("windows").and_then(Value::as_array_mut) {
        for window in list {
            let key = of_window(window);
            window["appKey"] = key.map_or(Value::Null, Value::String);
        }
    }
}

/// A window's appKey from its pid (and on macOS its bundle id).
pub fn of_window(window: &Value) -> Option<String> {
    if cfg!(target_os = "macos") {
        if let Some(bundle) = window
            .get("app")
            .and_then(Value::as_str)
            .filter(|a| a.contains('.'))
        {
            return Some(format!("bundle:{bundle}"));
        }
    }
    let pid = window.get("pid").and_then(Value::as_u64)?;
    of_pid(u32::try_from(pid).ok()?)
}

#[cfg(target_os = "linux")]
pub fn of_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
    if let Ok(info) = std::fs::read_to_string(proc.join("root/.flatpak-info")) {
        if let Some(name) = flatpak_name(&info) {
            return Some(format!("flatpak:{name}"));
        }
    }
    if let Ok(cgroup) = std::fs::read_to_string(proc.join("cgroup")) {
        if let Some(name) = snap_name(&cgroup) {
            return Some(format!("snap:{name}"));
        }
    }
    if let Ok(environ) = std::fs::read(proc.join("environ")) {
        if let Some(id) = desktop_id(&environ) {
            return Some(format!("desktop:{id}"));
        }
    }
    std::fs::read_link(proc.join("exe"))
        .ok()
        .map(|exe| format!("exe:{}", exe.display()))
}

/// `[Application] name=…` of a .flatpak-info.
#[cfg(any(target_os = "linux", test))]
fn flatpak_name(info: &str) -> Option<String> {
    let mut application = false;
    for line in info.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            application = line == "[Application]";
        } else if application {
            if let Some(name) = line.strip_prefix("name=") {
                return Some(name.trim().to_string()).filter(|n| !n.is_empty());
            }
        }
    }
    None
}

/// `snap.<name>.` in a cgroup path.
#[cfg(any(target_os = "linux", test))]
fn snap_name(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let start = line.find("snap.")? + 5;
        let rest = &line[start..];
        let name = &rest[..rest.find('.')?];
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// The desktop entry's id from GIO_LAUNCHED_DESKTOP_FILE (its file name without .desktop).
#[cfg(any(target_os = "linux", test))]
fn desktop_id(environ: &[u8]) -> Option<String> {
    environ.split(|b| *b == 0).find_map(|entry| {
        let text = std::str::from_utf8(entry).ok()?;
        let path = text.strip_prefix("GIO_LAUNCHED_DESKTOP_FILE=")?;
        let file = path.rsplit('/').next()?;
        let id = file.strip_suffix(".desktop").unwrap_or(file);
        (!id.is_empty()).then(|| id.to_string())
    })
}

#[cfg(target_os = "macos")]
pub fn of_pid(pid: u32) -> Option<String> {
    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer is valid for its length; proc_pidpath writes at most that much.
    let written = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if written <= 0 {
        return None;
    }
    buffer.truncate(written as usize);
    let path = String::from_utf8(buffer).ok()?;
    let real = std::fs::canonicalize(&path).map_or(path, |p| p.display().to_string());
    Some(format!("exe:{real}"))
}

#[cfg(windows)]
pub fn of_pid(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
    use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls with buffers valid for the lengths given; the handle is closed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut family = [0u16; 256];
        let mut length = family.len() as u32;
        let packaged = GetPackageFamilyName(process, &mut length, family.as_mut_ptr()) == ERROR_SUCCESS;
        let key = if packaged && length > 1 {
            let name = String::from_utf16_lossy(&family[..length as usize - 1]);
            Some(format!("pkg:{name}"))
        } else {
            let mut path = [0u16; 1024];
            let mut size = path.len() as u32;
            if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut size) != 0 {
                let path = String::from_utf16_lossy(&path[..size as usize]);
                Some(format!("exe:{}", path.to_lowercase()))
            } else {
                None
            }
        };
        CloseHandle(process);
        key
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn of_pid(_pid: u32) -> Option<String> {
    None
}

/// Whether a process runs elevated (Windows; `no-tree: elevated`, §2.4.4). None elsewhere, or
/// when it can't be told.
#[cfg(windows)]
pub fn elevated(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls; the handles are closed; the buffer is TOKEN_ELEVATION's size.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut token = std::ptr::null_mut();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token) != 0;
        CloseHandle(process);
        if !opened {
            return None;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) != 0;
        CloseHandle(token);
        ok.then_some(elevation.TokenIsElevated != 0)
    }
}

#[cfg(not(windows))]
pub fn elevated(_pid: u32) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn linux_keys_prefer_flatpak_snap_desktop_then_exe() {
        assert_eq!(
            flatpak_name("[Application]\nname=org.gnome.Calculator\nruntime=x\n[Instance]\nname=no").as_deref(),
            Some("org.gnome.Calculator")
        );
        assert_eq!(flatpak_name("[Instance]\nname=x"), None);
        assert_eq!(
            snap_name("0::/user.slice/user-1000.slice/user@1000.service/app.slice/snap.firefox.firefox-1234.scope\n").as_deref(),
            Some("firefox")
        );
        assert_eq!(snap_name("0::/user.slice/session-2.scope"), None);
        assert_eq!(
            desktop_id(b"HOME=/home/u\0GIO_LAUNCHED_DESKTOP_FILE=/usr/share/applications/org.gnome.TextEditor.desktop\0").as_deref(),
            Some("org.gnome.TextEditor")
        );
        assert_eq!(desktop_id(b"HOME=/home/u\0"), None);
    }

    #[test]
    fn every_window_gains_a_key_never_its_title() {
        let mut answer = json!({ "windows": [{ "id": "x:1", "pid": std::process::id(), "app": "rtx-linkd", "title": "Secret title" }] });
        add_keys(&mut answer);
        let key = &answer["windows"][0]["appKey"];
        assert!(key.is_null() || !key.as_str().unwrap().contains("Secret title"));
        if cfg!(target_os = "linux") {
            assert!(key.as_str().unwrap().starts_with("exe:/"), "{key}");
        }
        // A bare name is never a key.
        assert_eq!(of_window(&json!({ "app": "notepad.exe" })), None);
    }
}
