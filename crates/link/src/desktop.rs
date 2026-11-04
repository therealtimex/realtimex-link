//! RealTimeX itself on this computer: open it, or its download page when it is not installed.

use std::path::PathBuf;
use std::process::{Command, Stdio};

const DOWNLOAD_URL: &str = "https://realtimex.ai";

/// The installed RealTimeX, where its installers put it.
pub fn realtimex_app() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_default();
        vec![
            "/Applications/RealTimeX.AI.app".into(),
            PathBuf::from(home).join("Applications/RealTimeX.AI.app"),
        ]
    } else if cfg!(windows) {
        let mut list = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            list.push(PathBuf::from(local).join("Programs\\RealTimeX.AI\\RealTimeX.AI.exe"));
        }
        list.push("C:\\Program Files\\RealTimeX.AI\\RealTimeX.AI.exe".into());
        list
    } else {
        vec!["/opt/RealTimeX.AI/RealTimeX.AI".into()]
    };
    candidates.into_iter().find(|path| path.exists())
}

fn spawn(program: &str, args: &[&str]) {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: this window has no console, so `cmd /C start` would open one.
        command.creation_flags(0x0800_0000);
    }
    let _ = command.spawn();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Os {
    MacOs,
    Windows,
    Linux,
}

impl Os {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// How a URL is opened on an OS. Pure, so tests check every OS on every OS.
#[derive(Debug, PartialEq, Eq)]
enum Opener<'a> {
    /// A program with the URL as exactly one argument (macOS `open`, Linux `xdg-open`).
    Program {
        program: &'static str,
        args: Vec<&'a str>,
    },
    /// Windows: ShellExecuteW("open", file). No command line, so nothing splits the URL; `cmd
    /// /C start` cut it at the first `&` and the sign-in reached Keycloak without response_type
    /// (realtimex-link#16).
    ShellExecute { file: &'a str },
    /// Windows, not http(s): ShellExecute would launch a file or program, so it is not opened.
    Refused,
}

fn opener(os: Os, url: &str) -> Opener<'_> {
    match os {
        Os::MacOs => Opener::Program {
            program: "open",
            args: vec![url],
        },
        Os::Linux => Opener::Program {
            program: "xdg-open",
            args: vec![url],
        },
        Os::Windows if is_web_url(url) => Opener::ShellExecute { file: url },
        Os::Windows => Opener::Refused,
    }
}

/// http:// or https://, any case.
fn is_web_url(url: &str) -> bool {
    let has = |prefix: &str| {
        url.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    has("http://") || has("https://")
}

pub fn open_url(url: &str) {
    match opener(Os::current(), url) {
        Opener::Program { program, args } => spawn(program, &args),
        Opener::ShellExecute { file } => shell_open(file),
        Opener::Refused => eprintln!("open_url: not an http(s) URL, not opened: {url}"),
    }
}

/// A NUL-terminated UTF-16 string for the W APIs.
#[cfg_attr(not(windows), allow(dead_code))]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// ShellExecuteW on its own short-lived thread: it may block while the browser starts, and
/// a caller can be the UI thread. COM is initialised there as ShellExecute requires.
#[cfg(windows)]
fn shell_open(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        use windows_sys::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        };
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let file = wide(&url);
        let verb = wide("open");
        // SAFETY: both strings are NUL-terminated and outlive the calls; the rest are null.
        unsafe {
            let com = CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            );
            let done = ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            ) as isize;
            if done <= 32 {
                eprintln!("open_url: ShellExecuteW failed with {done}: {url}");
            }
            if com >= 0 {
                CoUninitialize();
            }
        }
    });
}

#[cfg(not(windows))]
fn shell_open(url: &str) {
    eprintln!("open_url: ShellExecute is Windows only, not opened: {url}");
}

/// Open RealTimeX, or its download page when it is not on this computer.
pub fn open_realtimex() {
    match realtimex_app() {
        Some(app) if cfg!(target_os = "macos") => spawn("open", &[&app.to_string_lossy()]),
        Some(app) => spawn(&app.to_string_lossy(), &[]),
        None => open_url(DOWNLOAD_URL),
    }
}

/// macOS: the Privacy & Security pane for a permission.
pub fn open_privacy_settings(name: &str) {
    let pane = if name == "accessibility" {
        "Privacy_Accessibility"
    } else {
        "Privacy_ScreenCapture"
    };
    open_url(&format!(
        "x-apple.systempreferences:com.apple.preference.security?{pane}"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like signin.rs's sign-in URL: seven parameters, six `&`, encoded `:` and `/`.
    const SIGN_IN: &str = "https://accounts.realtimex.ai/auth/realms/realtimex/protocol/openid-connect/auth?client_id=realtimex-link&response_type=code&scope=openid%20profile%20email&redirect_uri=http%3A%2F%2F127.0.0.1%3A53123%2Fcallback&state=Zm9v&code_challenge=YmFy-_baz&code_challenge_method=S256";

    #[test]
    fn windows_hands_the_whole_sign_in_url_to_shell_execute() {
        assert_eq!(SIGN_IN.matches('&').count(), 6);
        let chosen = opener(Os::Windows, SIGN_IN);
        assert_eq!(chosen, Opener::ShellExecute { file: SIGN_IN });
        match chosen {
            Opener::ShellExecute { file } => {
                assert!(file.contains("&response_type=code&"));
                assert!(file.ends_with("&code_challenge_method=S256"));
            }
            other => panic!("not ShellExecute: {other:?}"),
        }
    }

    #[test]
    fn windows_never_goes_through_cmd() {
        for url in [SIGN_IN, DOWNLOAD_URL, "HTTP://example.com/?a=1&b=2"] {
            assert!(
                !matches!(opener(Os::Windows, url), Opener::Program { .. }),
                "{url}"
            );
        }
    }

    #[test]
    fn mac_and_linux_pass_the_url_as_one_argument() {
        assert_eq!(
            opener(Os::MacOs, SIGN_IN),
            Opener::Program {
                program: "open",
                args: vec![SIGN_IN]
            }
        );
        assert_eq!(
            opener(Os::Linux, SIGN_IN),
            Opener::Program {
                program: "xdg-open",
                args: vec![SIGN_IN]
            }
        );
    }

    #[test]
    fn windows_opens_only_web_urls() {
        for refused in [
            "file:///C:/x.exe",
            "C:\\x.exe",
            "cmd",
            "",
            "http:/",
            "javascript:alert(1)",
            "ms-settings:privacy",
        ] {
            assert_eq!(opener(Os::Windows, refused), Opener::Refused, "{refused}");
        }
        for accepted in ["HTTPS://realtimex.ai", "http://127.0.0.1:1/", DOWNLOAD_URL] {
            assert_eq!(
                opener(Os::Windows, accepted),
                Opener::ShellExecute { file: accepted },
                "{accepted}"
            );
        }
    }

    #[test]
    fn wide_round_trips_with_one_trailing_nul() {
        let text = format!("{SIGN_IN}&x=ü€😀");
        let w = wide(&text);
        assert_eq!(w.last(), Some(&0));
        assert_eq!(w.iter().filter(|&&unit| unit == 0).count(), 1);
        assert_eq!(String::from_utf16(&w[..w.len() - 1]).unwrap(), text);
    }
}
