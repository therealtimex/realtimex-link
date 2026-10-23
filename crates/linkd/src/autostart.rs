//! Start at login: RealTimeX Link's menu bar / tray app (`rtx-link --background`, no window), which
//! starts this daemon. RealTimeX turns it on with its Remote Control plugin and off without it
//! (POST /v1/autostart). Each platform's own per-user login item: a LaunchAgent on macOS, the Run
//! key on Windows, an XDG autostart entry on Linux.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The login item's name: the Run value (Windows), the entry's Name (Linux).
#[cfg(not(target_os = "macos"))]
const NAME: &str = "RealTimeX Link";
#[cfg(target_os = "macos")]
const LABEL: &str = "ai.realtimex.link";

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

/// Whether the login item is there and starts this installation's rtx-link.
pub fn enabled() -> bool {
    match (app_binary(), platform::current()) {
        (Some(app), Some(current)) => current == command_line(&app),
        _ => false,
    }
}

/// Add or remove the login item. Adding needs rtx-link next to this daemon.
pub fn set(on: bool) -> Result<()> {
    if !on {
        return platform::remove();
    }
    let app = app_binary().context("rtx-link is not installed next to rtx-linkd")?;
    platform::write(&command_line(&app), &app)
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

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist() -> PathBuf {
        PathBuf::from(crate::paths::home_dir())
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// The program path in the LaunchAgent, when there is one.
    pub fn current() -> Option<String> {
        let text = std::fs::read_to_string(plist()).ok()?;
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

    pub fn write(program: &str, _app: &Path) -> Result<()> {
        let path = plist();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--background</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
</dict>
</plist>
"#,
            escape(program)
        );
        std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))
    }

    pub fn remove() -> Result<()> {
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
             connect to this one\nExec={exec}\n{icon}Terminal=false\nNoDisplay=true\n\
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
