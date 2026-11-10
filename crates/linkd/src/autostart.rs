//! Start at login: RealTimeX Link's menu bar / tray app (`rtx-link --background`, no window), which
//! starts this daemon. Each platform's own per-user login item: a LaunchAgent on macOS, the Run
//! key on Windows, an XDG autostart entry on Linux.
//!
//! Link owns the login item, and it is there while "Keep RealTimeX Link running" or "Reopen
//! RealTimeX if it crashes" is on (host.rs; realtimex-ai-app#3241 Q5): with the defaults, always.
//! It no longer follows Remote Control: the old opt-out (POST /v1/autostart { enabled: false },
//! store key `autostart-off`) is cleared once at start ([`retire_opt_out`]) and ignored after.
//! Only the installed Link writes it, so a build or a copy run from elsewhere can't point it at
//! itself. On macOS the LaunchAgent keeps the tray alive (`KeepAlive {SuccessfulExit=false}`):
//! launchd starts it again after a crash or a kill, and leaves a Quit (exit 0) alone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::store::Store;

/// The store key of the retired "don't start at login", which app builds from 2026-10-23 set
/// through POST /v1/autostart when Remote Control turned off. Removed once at start (#3241).
pub const OPT_OUT_KEY: &str = "autostart-off";
/// Set once the old opt-out was cleared, so the clearing (and its log line) happens once.
pub const OPT_OUT_RETIRED_KEY: &str = "autostart-off-retired";

/// The login item's name: the Run value (Windows), the entry's Name (Linux).
#[cfg(not(target_os = "macos"))]
const NAME: &str = "RealTimeX Link";
/// macOS keeps a login item's parent from its first registration, so the label from before
/// #11 stays under the team's name: a new label registers under RealTimeX.AI (M7).
#[cfg(target_os = "macos")]
const LABEL: &str = "ai.realtimex.link.agent";
#[cfg(target_os = "macos")]
const OLD_LABEL: &str = "ai.realtimex.link";

/// rtx-link installed next to this daemon (the same folder, or the .app's Contents/MacOS).
pub fn app_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "rtx-link.exe"
    } else {
        "rtx-link"
    };
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.join(name);
    app.is_file().then_some(app)
}

/// Whether the login item is there, starts this installation's rtx-link and is in today's form
/// (macOS: it names RealTimeX.AI, #11 M7; an older one is rewritten once).
pub fn enabled() -> bool {
    match (app_binary(), platform::current()) {
        (Some(app), Some(current)) => current == command_line(&app) && platform::current_form(),
        _ => false,
    }
}

/// There is something to start at login: rtx-link next to this daemon, in the installed Link.
pub fn available() -> bool {
    app_binary().is_some() && crate::paths::running_installed()
}

/// Add or remove the login item. Adding needs rtx-link next to this daemon, in the installed
/// Link: the item follows no copy but that one.
pub fn set(on: bool) -> Result<()> {
    if !on {
        return platform::remove();
    }
    let app = app_binary().context("rtx-link is not installed next to rtx-linkd")?;
    anyhow::ensure!(
        crate::paths::running_installed(),
        "only the installed RealTimeX Link starts at login, not {}",
        app.display()
    );
    platform::write(&command_line(&app), &app)
}

/// The one-time migration (#3241 Q5): when the store still has the old opt-out, remove it and
/// remember that it was retired. True when it cleared one now (the caller logs it), false when
/// there was none or it was cleared before.
pub fn retire_opt_out(store: &Store) -> Result<bool> {
    if store.get(OPT_OUT_RETIRED_KEY) == Some(Value::Bool(true)) && store.get(OPT_OUT_KEY).is_none() {
        return Ok(false);
    }
    let had = store.get(OPT_OUT_KEY).is_some();
    if had {
        store.remove(OPT_OUT_KEY)?;
    }
    store.set(OPT_OUT_RETIRED_KEY, &json!(true))?;
    Ok(had)
}

/// POST /v1/autostart { enabled }, which older RealTimeX builds send from Remote Control: it
/// changes nothing (#3241 Q5), so the store isn't even read. The line to log for false.
pub fn requested(on: bool) -> Option<&'static str> {
    (!on).then_some(
        "[link] POST /v1/autostart {enabled:false} ignored: start at login no longer follows Remote Control (#3241)",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add,
    Remove,
    Keep,
}

/// What the login item needs: `wanted` is Keep running or Reopen RealTimeX being on (#3241),
/// `installed` this copy being the installed Link (it may write the item), `present` any item
/// there, `ours` one that starts this installation's rtx-link in today's form.
pub fn change(wanted: bool, installed: bool, present: bool, ours: bool) -> Change {
    if wanted {
        if ours || !installed {
            Change::Keep
        } else {
            Change::Add
        }
    } else if present {
        Change::Remove
    } else {
        Change::Keep
    }
}

/// Make the login item match `wanted` (Keep running or Reopen RealTimeX); returns what was done.
pub fn follow(wanted: bool) -> Result<Change> {
    let wanted = change(
        wanted,
        available(),
        platform::current().is_some(),
        enabled(),
    );
    match wanted {
        Change::Add => set(true)?,
        Change::Remove => platform::remove()?,
        Change::Keep => {}
    }
    Ok(wanted)
}

/// How the login item starts rtx-link, as the platform stores it.
fn command_line(app: &Path) -> String {
    let path = app.display().to_string();
    if cfg!(windows) {
        format!("\"{path}\" --background")
    } else if cfg!(target_os = "macos") {
        // ProgramArguments: the path alone; --background is the second argument.
        path
    } else {
        desktop_exec(app, " --background")
    }
}

/// A Desktop Entry's Exec value: the program quoted, with ", `, $ and \ escaped, then `args`.
pub fn desktop_exec(app: &Path, args: &str) -> String {
    let mut quoted = String::from("\"");
    for c in app.display().to_string().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted + "\"" + args
}

/// macOS shows a login item by the app its `AssociatedBundleIdentifiers` names: RealTimeX.AI
/// when it is installed, else the program's own name (the Human's decision, #11 M7). The key
/// goes after `ProgramArguments`, so [`launch_agent_program`] still reads the program first.
#[cfg(any(target_os = "macos", test))]
pub const ASSOCIATED_APP: &str = "co.realtimex.ai";

/// The program a LaunchAgent starts: the first `<string>` after the first `<array>`.
#[cfg(any(target_os = "macos", test))]
fn launch_agent_program(text: &str) -> Option<String> {
    let start = text.find("<array>")? + "<array>".len();
    let rest = &text[start..];
    let first = rest.find("<string>")? + "<string>".len();
    let end = rest[first..].find("</string>")? + first;
    Some(
        rest[first..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// The LaunchAgent for `program` (already escaped for XML). `KeepAlive {SuccessfulExit=false}`
/// (#3241 Q5): launchd starts the tray again after a crash or a signal, within ThrottleInterval,
/// and leaves its Quit (exit 0) and a logout alone. `AssociatedBundleIdentifiers` comes after
/// `ProgramArguments`, so [`launch_agent_program`] still reads the program first.
#[cfg(any(target_os = "macos", test))]
fn launch_agent_text(label: &str, program: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--background</string>
  </array>
  <key>AssociatedBundleIdentifiers</key>
  <string>{ASSOCIATED_APP}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
</dict>
</plist>
"#
    )
}

/// Whether a LaunchAgent is in today's form: it names RealTimeX.AI (#11 M7) and keeps the tray
/// alive (#3241). An older one is rewritten once.
#[cfg(any(target_os = "macos", test))]
fn in_current_form(text: &str) -> bool {
    names_realtimex(text) && text.contains("<key>KeepAlive</key>")
}

/// Whether a LaunchAgent names RealTimeX.AI as its app.
#[cfg(any(target_os = "macos", test))]
fn names_realtimex(text: &str) -> bool {
    text.find("<key>AssociatedBundleIdentifiers</key>").is_some_and(|at| {
        text[at..]
            .split_once("<string>")
            .is_some_and(|(_, rest)| rest.starts_with(&format!("{ASSOCIATED_APP}</string>")))
    })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist_named(label: &str) -> PathBuf {
        PathBuf::from(crate::paths::home_dir())
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"))
    }

    fn plist() -> PathBuf {
        plist_named(LABEL)
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// The program path in the LaunchAgent (or the one from before #11), when there is one.
    pub fn current() -> Option<String> {
        [plist(), plist_named(OLD_LABEL)]
            .iter()
            .find_map(|path| super::launch_agent_program(&std::fs::read_to_string(path).ok()?))
    }

    /// Whether the LaunchAgent is today's: naming RealTimeX.AI as its app (#11 M7) and keeping
    /// the tray alive (#3241).
    pub fn current_form() -> bool {
        std::fs::read_to_string(plist()).is_ok_and(|text| super::in_current_form(&text))
            && !plist_named(OLD_LABEL).exists()
    }

    pub fn write(program: &str, _app: &Path) -> Result<()> {
        let path = plist();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = super::launch_agent_text(LABEL, &escape(program));
        std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
        // The tray this session started keeps running; the next login starts it from the new item
        // (or rtx-link's launchd hand-off bootstraps it, link/src/main.rs).
        super::remove_file(&plist_named(OLD_LABEL))
    }

    pub fn remove() -> Result<()> {
        super::remove_file(&plist_named(OLD_LABEL))?;
        super::remove_file(&plist())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// The Run value has one form.
    pub fn current_form() -> bool {
        true
    }

    /// The Run value's command line, when there is one.
    pub fn current() -> Option<String> {
        let (key, name) = (wide(RUN), wide(NAME));
        let mut bytes = 0u32;
        // SAFETY: the key and value names are NUL-terminated; a null buffer asks for the size.
        let size = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if size != ERROR_SUCCESS || bytes < 2 {
            return None;
        }
        let mut data = vec![0u16; bytes as usize / 2];
        // SAFETY: data holds `bytes` bytes, as RegGetValueW reported.
        let read = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if read != ERROR_SUCCESS {
            return None;
        }
        let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
        Some(String::from_utf16_lossy(&data[..len]))
    }

    pub fn write(command: &str, _app: &Path) -> Result<()> {
        let (key, name, value) = (wide(RUN), wide(NAME), wide(command));
        // SAFETY: NUL-terminated UTF-16 strings; the size includes the terminator.
        let result = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        anyhow::ensure!(
            result == ERROR_SUCCESS,
            "could not write the Run value (error {result})"
        );
        Ok(())
    }

    pub fn remove() -> Result<()> {
        let (key, name) = (wide(RUN), wide(NAME));
        // SAFETY: NUL-terminated UTF-16 strings.
        let result = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        anyhow::ensure!(
            result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND,
            "could not remove the Run value (error {result})"
        );
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    fn desktop_file() -> PathBuf {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(crate::paths::home_dir()).join(".config"));
        config.join("autostart/realtimex-link.desktop")
    }

    /// The entry has one form.
    pub fn current_form() -> bool {
        true
    }

    /// The entry's Exec line, when there is one.
    pub fn current() -> Option<String> {
        let text = std::fs::read_to_string(desktop_file()).ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("Exec="))
            .map(str::to_string)
    }

    pub fn write(exec: &str, app: &Path) -> Result<()> {
        let path = desktop_file();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let icon = app.with_file_name("icon.png");
        let icon = if icon.is_file() {
            format!("Icon={}\n", icon.display())
        } else {
            String::new()
        };
        let text = format!(
            "[Desktop Entry]\nType=Application\nName={NAME}\nComment=Lets your other computers \
             connect to this one\nExec={exec}\n{icon}StartupWMClass=realtimex-link\n\
             Terminal=false\nNoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))
    }

    pub fn remove() -> Result<()> {
        super::remove_file(&desktop_file())
    }
}

#[cfg(not(windows))]
fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("could not remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_item_follows_keep_running_or_reopen_realtimex() {
        // #3241 Q5: wanted is keep_running || reopen_realtimex.
        let wanted = |keep_running: bool, reopen: bool| keep_running || reopen;
        for (keep_running, reopen) in [(true, true), (true, false), (false, true)] {
            let on = wanted(keep_running, reopen);
            // Added by the installed Link, kept when it is already there in today's form.
            assert_eq!(change(on, true, false, false), Change::Add);
            assert_eq!(change(on, true, true, false), Change::Add);
            assert_eq!(change(on, true, true, true), Change::Keep);
            // A copy run from elsewhere doesn't point it at itself.
            assert_eq!(change(on, false, false, false), Change::Keep);
            assert_eq!(change(on, false, true, false), Change::Keep);
        }
        // Both off: removed, whichever copy wrote it.
        let off = wanted(false, false);
        assert_eq!(change(off, true, true, true), Change::Remove);
        assert_eq!(change(off, false, true, false), Change::Remove);
        assert_eq!(change(off, true, false, false), Change::Keep);
    }

    #[test]
    fn the_old_opt_out_is_retired_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // An install the 2026-10-23 app builds turned off.
        store.set(OPT_OUT_KEY, &json!(true)).unwrap();
        assert!(retire_opt_out(&store).unwrap(), "cleared now: the caller logs it");
        assert_eq!(store.get(OPT_OUT_KEY), None);
        assert_eq!(store.get(OPT_OUT_RETIRED_KEY), Some(json!(true)));
        // Kept across restarts, and never logged again.
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert!(!retire_opt_out(&store).unwrap());
        assert!(!retire_opt_out(&store).unwrap());
        // An install that never had it: nothing to log, still marked.
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = Store::open(fresh_dir.path()).unwrap();
        assert!(!retire_opt_out(&fresh).unwrap());
        assert_eq!(fresh.get(OPT_OUT_RETIRED_KEY), Some(json!(true)));
    }

    #[test]
    fn turning_start_at_login_off_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let off = requested(false).unwrap();
        assert!(off.contains("ignored"));
        assert_eq!(requested(true), None);
        // No opt-out is written: the login item still follows Keep running or Reopen RealTimeX.
        assert_eq!(store.get(OPT_OUT_KEY), None);
        assert_eq!(change(true, true, false, false), Change::Add);
    }

    #[test]
    fn the_launch_agent_keeps_the_tray_alive() {
        let text = launch_agent_text(
            "ai.realtimex.link.agent",
            "/Applications/RealTimeX Link.app/Contents/MacOS/rtx-link",
        );
        assert!(text.contains(
            "<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>"
        ));
        assert!(text.contains("<key>ThrottleInterval</key>\n  <integer>10</integer>"));
        assert!(text.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(in_current_form(&text));
        // The program is still read first.
        assert_eq!(
            launch_agent_program(&text).as_deref(),
            Some("/Applications/RealTimeX Link.app/Contents/MacOS/rtx-link")
        );
        // An item from before #3241 (no KeepAlive) isn't in today's form: rewritten once.
        let before = text.replace(
            "  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n",
            "",
        );
        assert!(names_realtimex(&before));
        assert!(!in_current_form(&before));
        assert_eq!(launch_agent_program(&before), launch_agent_program(&text));
    }

    #[test]
    fn a_launch_agent_names_realtimex_after_its_program() {
        let today = r#"<dict>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/RealTimeX Link.app/Contents/MacOS/rtx-link</string>
    <string>--background</string>
  </array>
  <key>AssociatedBundleIdentifiers</key>
  <string>co.realtimex.ai</string>
</dict>"#;
        assert_eq!(
            launch_agent_program(today).as_deref(),
            Some("/Applications/RealTimeX Link.app/Contents/MacOS/rtx-link")
        );
        assert!(names_realtimex(today));
        // An item from before #11: not in today's form, so follow() rewrites it once.
        let older = today.replace("<key>AssociatedBundleIdentifiers</key>\n  <string>co.realtimex.ai</string>\n", "");
        assert!(!names_realtimex(&older));
        assert_eq!(launch_agent_program(&older), launch_agent_program(today));
        assert!(!names_realtimex(&today.replace("co.realtimex.ai", "com.example.other")));
        // The present-but-not-ours case: rewritten (Change::Add), never removed.
        assert_eq!(change(true, true, true, false), Change::Add);
    }

    #[test]
    fn command_lines_quote_the_path() {
        let app = Path::new("/opt/RealTimeX Link/rtx-link");
        let line = command_line(app);
        if cfg!(windows) {
            assert_eq!(line, "\"/opt/RealTimeX Link/rtx-link\" --background");
        } else if cfg!(target_os = "macos") {
            assert_eq!(line, "/opt/RealTimeX Link/rtx-link");
        } else {
            assert_eq!(line, "\"/opt/RealTimeX Link/rtx-link\" --background");
            assert_eq!(command_line(Path::new("/a$b")), "\"/a\\$b\" --background");
        }
    }
}
