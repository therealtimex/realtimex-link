//! The two launchd jobs' property lists (#11 M7). Both run the root copy's main executable
//! (M1), name RealTimeX.AI as their app in Login Items, and are rewritten whole at every turn-on.

/// The supervisor (root) and the per-session bridge.
pub const SERVICE_LABEL: &str = "ai.realtimex.link.service";
pub const SESSION_LABEL: &str = "ai.realtimex.link.session";
pub const SERVICE_PLIST: &str = "/Library/LaunchDaemons/ai.realtimex.link.service.plist";
pub const SESSION_PLIST: &str = "/Library/LaunchAgents/ai.realtimex.link.session.plist";
/// The root copy of Link (M2): what both jobs run.
pub const ROOT_APP: &str = "/Library/Application Support/RealTimeX Link/RealTimeX Link.app";
/// The only PATH root's jobs get (M2: tools by absolute path anyway).
pub const FIXED_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// The socket's name in the supervisor's `Sockets` (launch_activate_socket).
pub const SOCKET_NAME: &str = "Listeners";

/// The root copy's main executable.
pub fn root_program() -> String {
    format!("{ROOT_APP}/Contents/MacOS/rtx-link")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
"#;

/// The supervisor: root, at boot, with the socket launchd makes (0666; only root may create
/// anything in /var/run). Exiting 0 after turning off doesn't start it again.
pub fn service(program: &str) -> String {
    format!(
        r#"{HEADER}  <key>Label</key>
  <string>{SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--launchd</string>
    <string>service</string>
  </array>
  <key>AssociatedBundleIdentifiers</key>
  <string>{app}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{FIXED_PATH}</string>
  </dict>
  <key>Sockets</key>
  <dict>
    <key>{SOCKET_NAME}</key>
    <dict>
      <key>SockPathName</key>
      <string>{socket}</string>
      <key>SockPathMode</key>
      <integer>438</integer>
    </dict>
  </dict>
</dict>
</plist>
"#,
        program = escape(program),
        app = crate::autostart::ASSOCIATED_APP,
        socket = crate::unattended::MACOS_SOCKET,
    )
}

/// The session bridge: in each GUI session, as root at the login window and as the session's
/// user in Aqua; Interactive, so its timers aren't stretched.
pub fn session(program: &str) -> String {
    format!(
        r#"{HEADER}  <key>Label</key>
  <string>{SESSION_LABEL}</string>
  <key>LimitLoadToSessionType</key>
  <array>
    <string>LoginWindow</string>
    <string>Aqua</string>
  </array>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--launchd</string>
    <string>session</string>
  </array>
  <key>AssociatedBundleIdentifiers</key>
  <string>{app}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#,
        program = escape(program),
        app = crate::autostart::ASSOCIATED_APP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value after `<key>{key}</key>`, up to its closing tag.
    fn value_after<'a>(text: &'a str, key: &str) -> &'a str {
        let at = text.find(&format!("<key>{key}</key>")).expect(key) + key.len() + 11;
        let rest = text[at..].trim_start();
        let end = rest.find("</key>").map_or(rest.len(), |end| end);
        &rest[..end]
    }

    #[test]
    fn both_jobs_run_the_root_copys_main_executable_and_name_realtimex() {
        let program = root_program();
        assert_eq!(
            program,
            "/Library/Application Support/RealTimeX Link/RealTimeX Link.app/Contents/MacOS/rtx-link"
        );
        for (text, label, role) in [
            (service(&program), SERVICE_LABEL, "service"),
            (session(&program), SESSION_LABEL, "session"),
        ] {
            assert!(value_after(&text, "Label").starts_with(&format!("<string>{label}</string>")));
            let arguments = value_after(&text, "ProgramArguments");
            assert!(arguments.starts_with(&format!(
                "<array>\n    <string>{program}</string>\n    <string>--launchd</string>\n    <string>{role}</string>"
            )));
            // After ProgramArguments, like the per-user login item (autostart.rs).
            let associated = text.find("<key>AssociatedBundleIdentifiers</key>").unwrap();
            assert!(associated > text.find("<key>ProgramArguments</key>").unwrap());
            assert!(value_after(&text, "AssociatedBundleIdentifiers").starts_with("<string>co.realtimex.ai</string>"));
            assert!(text.contains("<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>"));
        }
    }

    #[test]
    fn the_supervisor_has_its_socket_and_a_fixed_path() {
        let text = service(&root_program());
        assert!(text.contains("<key>Sockets</key>\n  <dict>\n    <key>Listeners</key>"));
        assert!(text.contains("<string>/var/run/ai.realtimex.link.sock</string>"));
        // 0666 in decimal.
        assert!(text.contains("<key>SockPathMode</key>\n      <integer>438</integer>"));
        assert!(text.contains(&format!("<key>PATH</key>\n    <string>{FIXED_PATH}</string>")));
        assert!(!text.contains("LimitLoadToSessionType"));
    }

    #[test]
    fn the_session_job_loads_at_the_login_window_and_in_aqua() {
        let text = session(&root_program());
        assert!(value_after(&text, "LimitLoadToSessionType")
            .starts_with("<array>\n    <string>LoginWindow</string>\n    <string>Aqua</string>\n  </array>"));
        assert!(value_after(&text, "ProcessType").starts_with("<string>Interactive</string>"));
        assert!(!text.contains("Sockets"));
    }

    #[test]
    fn a_path_with_markup_is_escaped() {
        assert!(service("/a&b<c>").contains("<string>/a&amp;b&lt;c&gt;</string>"));
    }

    /// macOS CI: launchd's own parser takes both (plutil -lint).
    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_accepts_both() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [("service", service(&root_program())), ("session", session(&root_program()))] {
            let path = dir.path().join(format!("{name}.plist"));
            std::fs::write(&path, text).unwrap();
            let status = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "{name}.plist");
        }
    }
}
