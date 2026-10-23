//! Terminals on Linux (#2747): typed text is pasted, and a terminal takes Ctrl+V as a control
//! key (the shell shows ^V). X11 tells the focused window's WM_CLASS; on GNOME Wayland only the
//! accessibility bus (AT-SPI) says which app is active, and the paste key is that terminal's own
//! shortcut, which a person may have changed (Ptyxis with Ctrl+V).

use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use zbus::blocking::{connection, Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

const KEYSYM_CTRL: u32 = 0xffe3;
const KEYSYM_SHIFT: u32 = 0xffe1;
const KEYSYM_ALT: u32 = 0xffe9;
const KEYSYM_SUPER: u32 = 0xffeb;
const KEYSYM_INSERT: u32 = 0xff63;
const KEYSYM_V: u32 = 0x76;
/// Asking the accessibility bus must not hold up typing: past this, it is no terminal.
const ATSPI_TIMEOUT: Duration = Duration::from_millis(400);
/// AT-SPI's STATE_ACTIVE: the window that has the focus.
const STATE_ACTIVE: u32 = 1;

/// Terminal names: X11 WM_CLASS names and executable names, lowercased.
const TERMINALS: &[&str] = &[
    "ptyxis",
    "gnome-terminal",
    "gnome-terminal-server",
    "org.gnome.console",
    "kgx",
    "konsole",
    "xterm",
    "uxterm",
    "urxvt",
    "rxvt",
    "kitty",
    "alacritty",
    "tilix",
    "terminator",
    "xfce4-terminal",
    "mate-terminal",
    "lxterminal",
    "qterminal",
    "terminology",
    "wezterm",
    "wezterm-gui",
    "org.wezfurlong.wezterm",
    "foot",
    "st",
    "st-256color",
    "sakura",
    "guake",
    "tilda",
    "yakuake",
    "com.mitchellh.ghostty",
    "ghostty",
    "blackbox",
    "com.raggesilver.blackbox",
    "warp",
];

pub fn is_terminal_name(name: &str) -> bool {
    TERMINALS.contains(&name.to_lowercase().as_str())
}

/// Whether a WM_CLASS value ("instance\0class\0") names a terminal.
pub fn is_terminal_class(wm_class: &[u8]) -> bool {
    wm_class
        .split(|&byte| byte == 0)
        .filter(|name| !name.is_empty())
        .any(|name| is_terminal_name(&String::from_utf8_lossy(name)))
}

/// The executable of the terminal whose window is active, by AT-SPI; None for any other app,
/// or when the accessibility bus does not answer in time.
pub fn focused_terminal_by_atspi() -> Option<String> {
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(active_terminal().ok().flatten());
    });
    wait.recv_timeout(ATSPI_TIMEOUT).ok().flatten()
}

fn active_terminal() -> Result<Option<String>> {
    let address: String = Proxy::new(
        &Connection::session()?,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
    )?
    .call("GetAddress", &())?;
    let a11y = connection::Builder::address(address.as_str())?.build()?;
    let dbus = Proxy::new(
        &a11y,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    let root = Proxy::new(
        &a11y,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
        "org.a11y.atspi.Accessible",
    )?;
    let apps: Vec<(String, OwnedObjectPath)> = root.call("GetChildren", &())?;
    for (bus, path) in apps {
        // Only terminals' windows are asked about: one cheap question per app.
        let Ok(pid) = dbus.call::<_, _, u32>("GetConnectionUnixProcessID", &(bus.as_str(),)) else {
            continue;
        };
        let Some(exe) = executable(pid) else { continue };
        if !is_terminal_name(&exe) {
            continue;
        }
        let app = Proxy::new(
            &a11y,
            bus.as_str(),
            path.as_str(),
            "org.a11y.atspi.Accessible",
        )?;
        let windows: Vec<(String, OwnedObjectPath)> = app.call("GetChildren", &())?;
        for (window_bus, window_path) in windows {
            let window = Proxy::new(
                &a11y,
                window_bus.as_str(),
                window_path.as_str(),
                "org.a11y.atspi.Accessible",
            )?;
            let states: Vec<u32> = window.call("GetState", &())?;
            if states
                .first()
                .is_some_and(|bits| bits & (1 << STATE_ACTIVE) != 0)
            {
                return Ok(Some(exe));
            }
        }
    }
    Ok(None)
}

fn executable(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(exe.file_name()?.to_string_lossy().into_owned())
}

/// The keys that paste in this terminal: its own shortcut when it has a setting for one (Ptyxis,
/// GNOME Terminal), else Ctrl+Shift+V.
pub fn paste_keys(terminal: &str) -> Vec<u32> {
    let setting = match terminal {
        "ptyxis" => gsettings(&["get", "org.gnome.Ptyxis.Shortcuts", "paste-clipboard"]),
        "gnome-terminal-server" | "gnome-terminal" => gsettings(&[
            "get",
            "org.gnome.Terminal.Legacy.Keybindings:/org/gnome/terminal/legacy/keybindings/",
            "paste",
        ]),
        _ => None,
    };
    setting
        .as_deref()
        .and_then(parse_accelerator)
        .unwrap_or_else(|| vec![KEYSYM_CTRL, KEYSYM_SHIFT, KEYSYM_V])
}

fn gsettings(args: &[&str]) -> Option<String> {
    let output = Command::new("gsettings").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A GTK accelerator as gsettings prints it ('<Control><Shift>v', quoted) as keysyms, modifiers
/// first. None for anything else (disabled, a key this does not know).
fn parse_accelerator(accelerator: &str) -> Option<Vec<u32>> {
    let mut rest = accelerator.trim().trim_matches('\'');
    let mut keys = Vec::new();
    while let Some(after) = rest.strip_prefix('<') {
        let (modifier, tail) = after.split_once('>')?;
        keys.push(match modifier.to_lowercase().as_str() {
            "control" | "ctrl" | "primary" => KEYSYM_CTRL,
            "shift" => KEYSYM_SHIFT,
            "alt" | "mod1" => KEYSYM_ALT,
            "super" => KEYSYM_SUPER,
            _ => return None,
        });
        rest = tail;
    }
    let key = match rest {
        "Insert" => KEYSYM_INSERT,
        key if key.len() == 1 && key.is_ascii() => {
            u32::from(key.to_ascii_lowercase().as_bytes()[0])
        }
        _ => return None,
    };
    if keys.is_empty() {
        return None;
    }
    keys.push(key);
    Some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_terminals_by_either_wm_class_name() {
        assert!(is_terminal_class(b"ptyxis\0ptyxis\0"));
        assert!(is_terminal_class(
            b"gnome-terminal-server\0Gnome-terminal\0"
        ));
        assert!(is_terminal_class(b"xterm\0UXTerm\0"));
        assert!(is_terminal_class(b"kitty\0kitty\0"));
        assert!(!is_terminal_class(b"Navigator\0firefox_firefox\0"));
        assert!(!is_terminal_class(b"electron\0Electron\0"));
        assert!(!is_terminal_class(b"Thunar\0Thunar\0"));
        assert!(!is_terminal_class(b""));
        assert!(is_terminal_name("kgx"));
        assert!(!is_terminal_name("chrome"));
    }

    #[test]
    fn reads_a_terminal_paste_shortcut() {
        assert_eq!(
            parse_accelerator("'<Control>v'"),
            Some(vec![KEYSYM_CTRL, KEYSYM_V])
        );
        assert_eq!(
            parse_accelerator("'<Control><Shift>v'"),
            Some(vec![KEYSYM_CTRL, KEYSYM_SHIFT, KEYSYM_V])
        );
        assert_eq!(
            parse_accelerator("'<Shift><Primary>V'"),
            Some(vec![KEYSYM_SHIFT, KEYSYM_CTRL, KEYSYM_V])
        );
        assert_eq!(
            parse_accelerator("'<Shift>Insert'"),
            Some(vec![KEYSYM_SHIFT, KEYSYM_INSERT])
        );
        // Disabled, or nothing this knows: the default is used instead.
        assert_eq!(parse_accelerator("''"), None);
        assert_eq!(parse_accelerator("'disabled'"), None);
        assert_eq!(parse_accelerator("'<Hyper>v'"), None);
        assert_eq!(parse_accelerator("'v'"), None);
    }
}
