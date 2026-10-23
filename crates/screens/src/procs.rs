//! Process table: which app a process belongs to.
//!
//! Meeting apps render audio from helper processes (Chrome renderers, Teams' WebView2), so the
//! app is the top of the parent chain below the shell. Process loopback on that root with
//! "include process tree" then captures every helper, like the macOS `.app` grouping.

use std::collections::HashMap;
use std::mem::size_of;

use anyhow::Result;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// Shells, terminals and system hosts: an app's parent chain stops below these.
const STOP_AT: &[&str] = &[
    "explorer.exe",
    "svchost.exe",
    "services.exe",
    "wininit.exe",
    "winlogon.exe",
    "userinit.exe",
    "sihost.exe",
    "runtimebroker.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "conhost.exe",
    "openconsole.exe",
    "windowsterminal.exe",
    "taskhostw.exe",
];

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub exe: String,
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

pub fn snapshot() -> Result<HashMap<u32, Proc>> {
    let mut out = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                out.insert(
                    entry.th32ProcessID,
                    Proc {
                        pid: entry.th32ProcessID,
                        parent: entry.th32ParentProcessID,
                        exe: wide(&entry.szExeFile),
                    },
                );
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    Ok(out)
}

/// The app root of `pid`: climb parents until the next one is a shell or system host.
pub fn root_of(pid: u32, procs: &HashMap<u32, Proc>) -> u32 {
    let mut current = pid;
    for _ in 0..64 {
        let Some(p) = procs.get(&current) else { break };
        let Some(parent) = procs.get(&p.parent) else {
            break;
        };
        if parent.pid == current
            || parent.pid <= 4
            || STOP_AT.contains(&parent.exe.to_ascii_lowercase().as_str())
        {
            break;
        }
        current = parent.pid;
    }
    current
}

/// `chrome.exe` matches `chrome.exe`, `chrome` and `Chrome`.
pub fn exe_matches(exe: &str, query: &str) -> bool {
    let exe = exe.to_ascii_lowercase();
    let q = query.to_ascii_lowercase();
    exe == q || exe.strip_suffix(".exe") == Some(q.as_str())
}

pub fn display_name(exe: &str) -> String {
    exe.strip_suffix(".exe")
        .or_else(|| exe.strip_suffix(".EXE"))
        .unwrap_or(exe)
        .to_string()
}
