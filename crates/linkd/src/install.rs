//! `rtx-linkd install DESTINATION`: install the RealTimeX Link this executable belongs to as Link's
//! own app. RealTimeX carries Link and runs this from its installers (Windows, Linux) or at start
//! (macOS): /Applications/RealTimeX Link.app, Program Files\RealTimeX Link, /opt/RealTimeX Link.
//!
//! Never a downgrade: when the installed Link is this version or newer, nothing changes; one
//! whose version can't be told is left alone too. An older one is stopped (window, daemon and
//! helper) and replaced; the next start (RealTimeX's Remote Control, or login) runs the new one.
//! What can be checked before stopping it is (the folder can be written, no other user's Link
//! runs there), so a refused install leaves a remote host running.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const EXECUTABLES: [&str; 3] = ["rtx-linkd", "rtx-link", "rtx-control"];
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// The installed version, beside the executables (Windows, Linux; the .app has its Info.plist),
/// for when the installed rtx-linkd can't say.
const VERSION_FILE: &str = "VERSION";

pub fn run(destination: &Path) -> Result<Value> {
    let ours = env!("CARGO_PKG_VERSION");
    let source = source()?;
    let installed = installed_version(destination)?;
    if let Some(installed) = &installed {
        if version_key(installed) >= version_key(ours) {
            return Ok(json!({ "installed": false, "version": installed, "reason": "current" }));
        }
    }
    check_writable(destination)?;
    if destination.exists() && others_running(destination) {
        bail!(
            "another user's RealTimeX Link is running from {}; it can be updated by an \
             administrator, or once that user quits it",
            destination.display()
        );
    }
    let stopped = destination.exists() && stop(destination)?;
    replace(&source, destination)?;
    menu_entry(destination)?;
    Ok(json!({
        "installed": true,
        "version": ours,
        "previous": installed,
        "stopped": stopped,
    }))
}

/// What to copy: the .app this executable is in (macOS), else its folder.
pub(crate) fn source() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("where is rtx-linkd?")?;
    let dir = exe.parent().context("rtx-linkd has no folder")?;
    if cfg!(target_os = "macos") {
        let app = dir
            .ancestors()
            .nth(2)
            .context("rtx-linkd is not in an .app")?;
        if app.extension().is_none_or(|ext| ext != "app") {
            bail!("rtx-linkd is not in an .app: {}", exe.display());
        }
        return Ok(app.to_path_buf());
    }
    Ok(dir.to_path_buf())
}

pub(crate) fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Where the executables are under an installed Link.
pub(crate) fn executables_dir(install: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        install.join("Contents/MacOS")
    } else {
        install.to_path_buf()
    }
}

/// The installed Link's version: its rtx-linkd's --version line, else its Info.plist (macOS) or
/// VERSION file. None when there is no rtx-linkd to replace.
fn installed_version(destination: &Path) -> Result<Option<String>> {
    let linkd = executables_dir(destination).join(executable("rtx-linkd"));
    let reported = || -> Option<String> {
        let output = Command::new(&linkd)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let line: Value = serde_json::from_slice(&output.stdout).ok()?;
        line.get("version")?.as_str().map(str::to_string)
    };
    detect_version(linkd.is_file(), reported, || recorded_version(destination))
}

/// An rtx-linkd that doesn't answer --version (damaged, blocked, a format this one doesn't read)
/// may still be newer: its recorded version decides, and without one nothing is replaced.
fn detect_version(
    exists: bool,
    reported: impl FnOnce() -> Option<String>,
    recorded: impl FnOnce() -> Option<String>,
) -> Result<Option<String>> {
    if !exists {
        return Ok(None);
    }
    match reported().or_else(recorded) {
        Some(version) => Ok(Some(version)),
        None => bail!(
            "the installed RealTimeX Link doesn't say which version it is, so it isn't replaced \
             (never a downgrade)"
        ),
    }
}

/// The version written at install: CFBundleShortVersionString in the .app's Info.plist (macOS),
/// the VERSION file elsewhere.
fn recorded_version(destination: &Path) -> Option<String> {
    if cfg!(target_os = "macos") {
        let text = std::fs::read_to_string(destination.join("Contents/Info.plist")).ok()?;
        plist_version(&text)
    } else {
        let text = std::fs::read_to_string(destination.join(VERSION_FILE)).ok()?;
        Some(text.trim().to_string()).filter(|version| looks_like_version(version))
    }
}

fn plist_version(text: &str) -> Option<String> {
    let key = "<key>CFBundleShortVersionString</key>";
    let rest = &text[text.find(key)? + key.len()..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    // Only whitespace between the key and its value.
    if !rest[..start - "<string>".len()].trim().is_empty() {
        return None;
    }
    Some(rest[start..end].trim().to_string()).filter(|version| looks_like_version(version))
}

fn looks_like_version(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
}

/// Installing moves the installed Link aside and puts the new one in its place: that needs the
/// folder it is in, and on Linux and Windows the installed folder itself, to be writable.
/// Checked with a file made and removed again, before anything is stopped. Not inside the .app:
/// writing into another app's bundle is what macOS asks the person about.
fn check_writable(destination: &Path) -> Result<()> {
    let parent = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let mut folders = vec![parent];
    if !cfg!(target_os = "macos") && destination.is_dir() {
        folders.push(destination);
    }
    for folder in folders {
        let probe = folder.join(format!(".rtx-linkd-install-{}", std::process::id()));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .with_context(|| {
                format!(
                    "can't install RealTimeX Link: {} is not writable",
                    folder.display()
                )
            })?;
        let _ = std::fs::remove_file(&probe);
    }
    Ok(())
}

/// Whether a Link this process can't stop runs from the destination: another user's, without
/// root or administrator. Stopping this user's and then failing would leave a host offline.
fn others_running(destination: &Path) -> bool {
    let dir = executables_dir(destination);
    let paths: Vec<PathBuf> = EXECUTABLES
        .iter()
        .map(|name| dir.join(executable(name)))
        .collect();
    platform_others_running(&paths)
}

#[cfg(unix)]
fn platform_others_running(paths: &[PathBuf]) -> bool {
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if me == 0 {
        return false;
    }
    let pids = |args: &[&str]| -> Vec<u32> {
        Command::new("pgrep")
            .args(args)
            .stderr(Stdio::null())
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| line.trim().parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let me = me.to_string();
    paths.iter().any(|path| {
        let pattern = pattern(path);
        let mine = pids(&["-u", &me, "-f", &pattern]);
        pids(&["-f", &pattern])
            .iter()
            .any(|pid| !mine.contains(pid))
    })
}

/// PowerShell: exit 0 when a Link runs in another session (another user's) and this process
/// isn't an administrator, who can stop it. Without administrator another session's path can't
/// be read, so any Link there counts.
#[cfg(any(windows, test))]
fn others_running_script(paths: &[PathBuf]) -> String {
    let ours = paths
        .iter()
        .map(|path| format!("'{}'", path.display().to_string().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator); \
         if ($admin) {{ exit 1 }}; \
         $me = (Get-Process -Id $PID).SessionId; $ours = @({ours}); \
         if (Get-Process -Name 'rtx-linkd','rtx-link','rtx-control' -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.SessionId -ne $me -and ($null -eq $_.Path -or $ours -contains $_.Path) }}) \
         {{ exit 0 }} else {{ exit 1 }}"
    )
}

#[cfg(windows)]
fn platform_others_running(paths: &[PathBuf]) -> bool {
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &others_running_script(paths),
        ])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// "1.2.3" (anything after a "-" or "+" ignored) as numbers, for comparing.
pub(crate) fn version_key(version: &str) -> Vec<u64> {
    version
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Stop every process running from the installed Link (any user's, when run as root or
/// administrator). True when one was running.
fn stop(destination: &Path) -> Result<bool> {
    let dir = executables_dir(destination);
    let paths: Vec<PathBuf> = EXECUTABLES
        .iter()
        .map(|name| dir.join(executable(name)))
        .collect();
    if !paths.iter().any(|path| running(path)) {
        return Ok(false);
    }
    for path in &paths {
        signal(path);
    }
    let until = Instant::now() + STOP_TIMEOUT;
    while paths.iter().any(|path| running(path)) {
        if Instant::now() > until {
            bail!("RealTimeX Link did not stop: {}", dir.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(true)
}

#[cfg(unix)]
fn pattern(path: &Path) -> String {
    // The command line starts with the executable's path: an exact, anchored match.
    let mut pattern = String::from("^");
    for c in path.display().to_string().chars() {
        if r"\^$.|?*+()[]{}".contains(c) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern + "( |$)"
}

#[cfg(unix)]
fn running(path: &Path) -> bool {
    Command::new("pgrep")
        .args(["-f", &pattern(path)])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// SIGTERM: the daemon stops as it does on any SIGTERM, the window exits.
#[cfg(unix)]
fn signal(path: &Path) {
    let _ = Command::new("pkill").args(["-f", &pattern(path)]).status();
}

#[cfg(windows)]
fn powershell_path(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}

#[cfg(windows)]
fn running(path: &Path) -> bool {
    let script = format!(
        "if (Get-Process | Where-Object {{ $_.Path -eq '{}' }}) {{ exit 0 }} else {{ exit 1 }}",
        powershell_path(path)
    );
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn signal(path: &Path) {
    let script = format!(
        "Get-Process | Where-Object {{ $_.Path -eq '{}' }} | Stop-Process -Force",
        powershell_path(path)
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .status();
}

fn sibling(destination: &Path, suffix: &str) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    destination.with_file_name(name)
}

/// Copy beside the destination first, then swap, so a failed copy leaves the installed Link as
/// it was.
fn replace(source: &Path, destination: &Path) -> Result<()> {
    let staging = sibling(destination, ".new");
    let old = sibling(destination, ".old");
    for leftover in [&staging, &old] {
        if leftover.exists() {
            std::fs::remove_dir_all(leftover)
                .with_context(|| format!("could not remove {}", leftover.display()))?;
        }
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    copy(source, &staging)?;
    if destination.exists() {
        std::fs::rename(destination, &old)
            .with_context(|| format!("could not move {} aside", destination.display()))?;
    }
    if let Err(error) = std::fs::rename(&staging, destination) {
        if old.exists() {
            let _ = std::fs::rename(&old, destination);
        }
        return Err(error).with_context(|| format!("could not install {}", destination.display()));
    }
    if old.exists() {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// Linux, installed for everyone (root): RealTimeX Link in the applications menu, so its window
/// opens without the tray. macOS and Windows have theirs (/Applications, the Start menu shortcut
/// RealTimeX's installer adds).
#[cfg(target_os = "linux")]
fn menu_entry(destination: &Path) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let path = Path::new("/usr/share/applications/realtimex-link.desktop");
    let app = destination.join("rtx-link");
    let icon = destination.join("icon.png");
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=RealTimeX Link\nComment=Lets your other computers \
         connect to this one\nExec={}\nIcon={}\nTerminal=false\nCategories=Network;RemoteAccess;\n",
        crate::autostart::desktop_exec(&app, ""),
        icon.display()
    );
    std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))
}

#[cfg(not(target_os = "linux"))]
fn menu_entry(_destination: &Path) -> Result<()> {
    Ok(())
}

/// The whole .app with ditto (signatures, extended attributes and links kept).
#[cfg(target_os = "macos")]
fn copy(source: &Path, staging: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/ditto")
        .arg(source)
        .arg(staging)
        .status()
        .context("could not run ditto")?;
    if !status.success() {
        bail!("ditto {} failed ({status})", source.display());
    }
    Ok(())
}

/// The executables and the icon, readable and runnable by every user.
#[cfg(not(target_os = "macos"))]
fn copy(source: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging)?;
    let mut names: Vec<String> = EXECUTABLES.iter().map(|name| executable(name)).collect();
    names.push("icon.png".to_string());
    for name in names {
        let from = source.join(&name);
        if name == "icon.png" && !from.exists() {
            continue;
        }
        let to = staging.join(&name);
        std::fs::copy(&from, &to).with_context(|| format!("could not copy {}", from.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if name == "icon.png" { 0o644 } else { 0o755 };
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))?;
        }
    }
    let version = staging.join(VERSION_FILE);
    std::fs::write(&version, env!("CARGO_PKG_VERSION"))
        .with_context(|| format!("could not write {}", version.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&version, std::fs::Permissions::from_mode(0o644))?;
        std::fs::set_permissions(staging, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(version_key("0.10.0") > version_key("0.9.9"));
        assert!(version_key("1.0.0") > version_key("0.99.0"));
        assert_eq!(version_key("0.2.0-dev+abc"), version_key("0.2.0"));
        assert!(version_key("0.1.0") >= version_key("0.1.0"));
    }

    #[cfg(unix)]
    #[test]
    fn process_patterns_match_the_path_exactly() {
        let pattern = pattern(Path::new(
            "/Applications/RealTimeX Link.app/Contents/MacOS/rtx-link",
        ));
        assert_eq!(
            pattern,
            r"^/Applications/RealTimeX Link\.app/Contents/MacOS/rtx-link( |$)"
        );
    }

    #[test]
    fn an_installed_link_that_cant_say_its_version_is_not_replaced() {
        let none = || None;
        // Nothing installed: install.
        assert_eq!(detect_version(false, none, none).unwrap(), None);
        // --version answers.
        assert_eq!(
            detect_version(true, || Some("0.1.9".into()), none).unwrap(),
            Some("0.1.9".to_string())
        );
        // It doesn't: the recorded version decides.
        assert_eq!(
            detect_version(true, none, || Some("9.0.0".into())).unwrap(),
            Some("9.0.0".to_string())
        );
        // Neither: refused, never a downgrade.
        assert!(detect_version(true, none, none).is_err());
    }

    #[test]
    fn versions_are_read_from_info_plist() {
        let plist = "<dict>\n  <key>CFBundleVersion</key>\n  <string>1</string>\n  \
                     <key>CFBundleShortVersionString</key>\n  <string>0.1.10</string>\n</dict>";
        assert_eq!(plist_version(plist).as_deref(), Some("0.1.10"));
        assert_eq!(
            plist_version("<key>CFBundleShortVersionString</key><true/><string>1.0</string>"),
            None
        );
        assert_eq!(plist_version("<key>CFBundleName</key><string>x</string>"), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn versions_are_read_from_the_version_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(recorded_version(dir.path()), None);
        std::fs::write(dir.path().join(VERSION_FILE), "0.1.11\n").unwrap();
        assert_eq!(recorded_version(dir.path()).as_deref(), Some("0.1.11"));
        std::fs::write(dir.path().join(VERSION_FILE), "garbage").unwrap();
        assert_eq!(recorded_version(dir.path()), None);
    }

    #[test]
    fn an_unwritable_folder_is_found_before_anything_stops() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("RealTimeX Link");
        check_writable(&destination).unwrap();
        std::fs::create_dir(&destination).unwrap();
        check_writable(&destination).unwrap();
        // The probe doesn't stay.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // SAFETY: geteuid has no preconditions.
            if unsafe { libc::geteuid() } != 0 {
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
                    .unwrap();
                let refused = check_writable(&destination);
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                assert!(refused.is_err());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn nobody_elses_link_runs_from_a_new_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!others_running(dir.path()));
    }

    #[test]
    fn the_windows_check_quotes_paths() {
        let script =
            others_running_script(&[PathBuf::from(r"C:\Program Files\it's\rtx-linkd.exe")]);
        assert!(script.contains(r"$ours = @('C:\Program Files\it''s\rtx-linkd.exe');"));
        assert!(script.contains("if ($admin) { exit 1 };"));
    }

    #[test]
    fn staging_and_old_copies_sit_beside_the_destination() {
        let destination = Path::new("/opt/RealTimeX Link");
        assert_eq!(
            sibling(destination, ".new"),
            Path::new("/opt/RealTimeX Link.new")
        );
        assert_eq!(
            sibling(destination, ".old"),
            Path::new("/opt/RealTimeX Link.old")
        );
    }
}
