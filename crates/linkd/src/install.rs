//! `rtx-linkd install DESTINATION`: install the RealTimeX Link this executable belongs to as Link's
//! own app. RealTimeX carries Link and runs this from its installers (Windows, Linux) or at start
//! (macOS): /Applications/RealTimeX Link.app, Program Files\RealTimeX Link, /opt/RealTimeX Link.
//!
//! Never a downgrade: when the installed Link is this version or newer, nothing changes. An older
//! one is stopped (window, daemon and helper) and replaced; the next start (RealTimeX's Remote
//! Control, or login) runs the new one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const EXECUTABLES: [&str; 3] = ["rtx-linkd", "rtx-link", "rtx-control"];
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(destination: &Path) -> Result<Value> {
    let ours = env!("CARGO_PKG_VERSION");
    let source = source()?;
    let installed = installed_version(destination);
    if let Some(installed) = &installed {
        if version_key(installed) >= version_key(ours) {
            return Ok(json!({ "installed": false, "version": installed, "reason": "current" }));
        }
    }
    let stopped = destination.exists() && stop(destination)?;
    replace(&source, destination)?;
    Ok(json!({
        "installed": true,
        "version": ours,
        "previous": installed,
        "stopped": stopped,
    }))
}

/// What to copy: the .app this executable is in (macOS), else its folder.
fn source() -> Result<PathBuf> {
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

fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Where the executables are under an installed Link.
fn executables_dir(install: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        install.join("Contents/MacOS")
    } else {
        install.to_path_buf()
    }
}

/// The installed rtx-linkd's version, from its --version line.
fn installed_version(destination: &Path) -> Option<String> {
    let output = Command::new(executables_dir(destination).join(executable("rtx-linkd")))
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line: Value = serde_json::from_slice(&output.stdout).ok()?;
    line.get("version")?.as_str().map(str::to_string)
}

/// "1.2.3" (anything after a "-" or "+" ignored) as numbers, for comparing.
fn version_key(version: &str) -> Vec<u64> {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
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
