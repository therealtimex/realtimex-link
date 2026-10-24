//! Link updates itself from its signed releases (github.com/therealtimex/realtimex-link): it looks
//! for a newer release at start and every few hours, and the window offers Update. RealTimeX also
//! updates Link when it updates; this is for a Link installed on its own.
//!
//! Only a signed, installed Link updates itself, and only to a build signed by the same signer:
//! - macOS: the release's signed archive (checked against SHA256SUMS), its app checked with
//!   codesign (strict) and for this app's own Team ID, then installed with that build's
//!   `rtx-linkd install` into this app's place and opened again (a detached helper: installing
//!   stops this daemon).
//! - Windows: the release's signed installer (SHA256SUMS, then Authenticode: valid and the same
//!   signer as this rtx-linkd), started for the person to confirm (it asks for administrator).
//! - Linux: /opt needs root, so the window opens the release page (the .deb / .rpm are there).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::host::Host;

const DEFAULT_REPOSITORY: &str = "therealtimex/realtimex-link";
const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// RTX_LINK_UPDATE_REPOSITORY points at another repository (testing).
fn repository() -> String {
    std::env::var("RTX_LINK_UPDATE_REPOSITORY")
        .ok()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| DEFAULT_REPOSITORY.to_string())
}

#[derive(Clone, Default)]
struct Release {
    version: String,
    page: String,
    /// name -> download URL
    assets: Vec<(String, String)>,
}

#[derive(Default)]
struct State {
    latest: Option<Release>,
    busy: Option<&'static str>,
    error: Option<String>,
    checked_at: Option<String>,
}

pub struct Updater {
    host: Weak<Host>,
    state: Mutex<State>,
    started: std::sync::atomic::AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// Newer than this build.
fn newer(version: &str) -> bool {
    crate::install::version_key(version) > crate::install::version_key(env!("CARGO_PKG_VERSION"))
}

/// The release asset this platform installs from.
fn asset_name(version: &str) -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    match std::env::consts::OS {
        "macos" => Some(format!("realtimex-link-macos-{arch}.tar.gz")),
        "windows" => Some(format!("RealTimeX-Link-Setup-{version}.exe")),
        _ => None,
    }
}

impl Updater {
    pub fn new(host: Weak<Host>) -> Arc<Self> {
        Arc::new(Self {
            host,
            state: Mutex::new(State::default()),
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Checks at start and every few hours (once per daemon, however often hosting restarts).
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_CHECK).await;
            loop {
                let _ = me.check().await;
                tokio::time::sleep(CHECK_EVERY).await;
            }
        });
    }

    fn log(&self, line: &str) {
        if let Some(host) = self.host.upgrade() {
            (host.log)(&format!("[update] {line}"));
        }
    }

    /// For the window: { current, latest, available, page, busy, error, checkedAt }.
    pub fn status(&self) -> Value {
        let state = lock(&self.state);
        let latest = state.latest.as_ref();
        json!({
            "current": env!("CARGO_PKG_VERSION"),
            "latest": latest.map(|r| r.version.clone()),
            "available": latest.is_some_and(|r| newer(&r.version)),
            "page": latest.map(|r| r.page.clone()),
            "busy": state.busy,
            "error": state.error,
            "checkedAt": state.checked_at,
        })
    }

    /// Look for the latest release now.
    pub async fn check(&self) -> Result<Value> {
        let release = tokio::task::spawn_blocking(latest_release).await?;
        let mut state = lock(&self.state);
        state.checked_at = Some(crate::util::iso_now());
        match release {
            Ok(release) => {
                if newer(&release.version)
                    && state.latest.as_ref().map(|r| &r.version) != Some(&release.version)
                {
                    drop(state);
                    self.log(&format!("RealTimeX Link {} is available", release.version));
                    state = lock(&self.state);
                }
                state.latest = Some(release);
                state.error = None;
            }
            Err(error) => {
                state.error = Some(format!("Couldn't check for updates: {error:#}"));
            }
        }
        drop(state);
        Ok(self.status())
    }

    /// Update to the latest release. Linux (and an unsigned or not-installed Link): { open: page }
    /// for the window to open instead.
    pub async fn install(self: &Arc<Self>) -> Result<Value> {
        let release = lock(&self.state)
            .latest
            .clone()
            .filter(|r| newer(&r.version))
            .ok_or_else(|| anyhow!("RealTimeX Link is up to date"))?;
        let Some(name) = asset_name(&release.version) else {
            return Ok(json!({ "open": release.page }));
        };
        {
            let mut state = lock(&self.state);
            if state.busy.is_some() {
                bail!("An update is already under way");
            }
            state.busy = Some("downloading");
            state.error = None;
        }
        let me = self.clone();
        let outcome =
            tokio::task::spawn_blocking(move || me.download_and_start(&release, &name)).await?;
        let mut state = lock(&self.state);
        state.busy = None;
        match outcome {
            Ok(answer) => Ok(answer),
            Err(error) => {
                state.error = Some(format!("{error:#}"));
                drop(state);
                self.log(&format!("failed: {error:#}"));
                Err(error)
            }
        }
    }

    fn download_and_start(&self, release: &Release, name: &str) -> Result<Value> {
        let url = |file: &str| {
            release
                .assets
                .iter()
                .find(|(n, _)| n == file)
                .map(|(_, u)| u.clone())
                .ok_or_else(|| anyhow!("the release has no {file}"))
        };
        let sums = String::from_utf8(get(&url("SHA256SUMS")?)?)?;
        let expected = sums
            .lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let sha = parts.next()?;
                (parts.next()?.trim_start_matches('*') == name).then(|| sha.to_lowercase())
            })
            .ok_or_else(|| anyhow!("SHA256SUMS doesn't list {name}"))?;
        let bytes = get(&url(name)?)?;
        let actual = crate::util::sha256_hex(&bytes);
        if actual != expected {
            bail!("{name}: SHA-256 {actual}, but the release lists {expected}");
        }
        let dir = std::env::temp_dir().join(format!("realtimex-link-update-{}", release.version));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(name);
        std::fs::write(&file, &bytes)?;
        lock(&self.state).busy = Some("installing");
        self.log(&format!(
            "installing {} from {}",
            release.version,
            file.display()
        ));
        platform::start(&dir, &file)
    }
}

/// GET with Link's User-Agent (GitHub requires one).
fn get(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header(
            "User-Agent",
            &format!("RealTimeX-Link/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/octet-stream")
        .call()
        .with_context(|| url.to_string())?;
    Ok(response
        .body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .read_to_vec()?)
}

fn latest_release() -> Result<Release> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        repository()
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let answer: Value = agent
        .get(&url)
        .header(
            "User-Agent",
            &format!("RealTimeX-Link/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| url.clone())?
        .body_mut()
        .read_json()?;
    parse_release(&answer).ok_or_else(|| anyhow!("{url}: not a release"))
}

fn parse_release(answer: &Value) -> Option<Release> {
    let version = answer
        .get("tag_name")?
        .as_str()?
        .trim_start_matches('v')
        .to_string();
    Some(Release {
        version,
        page: answer.get("html_url")?.as_str()?.to_string(),
        assets: answer
            .get("assets")?
            .as_array()?
            .iter()
            .filter_map(|a| {
                Some((
                    a.get("name")?.as_str()?.to_string(),
                    a.get("browser_download_url")?.as_str()?.to_string(),
                ))
            })
            .collect(),
    })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// The Team ID a signed app or executable carries ("not set" for ad-hoc).
    fn team_id(path: &Path) -> Option<String> {
        let output = Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=2"])
            .arg(path)
            .stdout(Stdio::null())
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find_map(|line| line.strip_prefix("TeamIdentifier=").map(str::to_string))
            .filter(|team| team != "not set")
    }

    pub fn start(dir: &Path, archive: &Path) -> Result<Value> {
        let installed = crate::install::source()?;
        if !installed.starts_with("/Applications") {
            bail!("Only RealTimeX Link installed in Applications updates itself");
        }
        let ours = team_id(&installed).ok_or_else(|| {
            anyhow!("This RealTimeX Link isn't signed, so it can't update itself")
        })?;
        let status = Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dir)
            .status()?;
        if !status.success() {
            bail!("couldn't unpack {}", archive.display());
        }
        let app = dir.join("RealTimeX Link.app");
        let verified = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app)
            .status()?;
        if !verified.success() || team_id(&app).as_deref() != Some(ours.as_str()) {
            bail!("the downloaded RealTimeX Link isn't signed by the same developer");
        }
        // Installing stops this daemon (and the window): a helper of its own installs and opens
        // the new Link, in the background like at login.
        let linkd = crate::install::executables_dir(&app).join("rtx-linkd");
        let script = r#""$1" install "$2" && /usr/bin/open -g -a "$2" --args --background"#;
        detached(
            Command::new("/bin/sh")
                .args(["-c", script, "update"])
                .arg(&linkd)
                .arg(&installed),
        )?;
        Ok(json!({ "started": true }))
    }

    fn detached(command: &mut Command) -> Result<()> {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid has no preconditions; it runs in the child before exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    /// "Valid|<signer subject>" for a file's Authenticode signature.
    fn signature(path: &Path) -> Option<(bool, String)> {
        let script = format!(
            "$s = Get-AuthenticodeSignature -LiteralPath '{}'; \"$($s.Status)|$($s.SignerCertificate.Subject)\"",
            path.display().to_string().replace('\'', "''")
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let (status, subject) = text.split_once('|')?;
        Some((status == "Valid", subject.to_string()))
    }

    pub fn start(_dir: &Path, setup: &Path) -> Result<Value> {
        let ours = std::env::current_exe()?;
        let (valid, signer) = signature(&ours).unwrap_or((false, String::new()));
        if !valid || signer.is_empty() {
            bail!("This RealTimeX Link isn't signed, so it can't update itself");
        }
        match signature(setup) {
            Some((true, subject)) if subject == signer => {}
            _ => bail!("the downloaded installer isn't signed by the same publisher"),
        }
        // The installer asks for administrator and stops this Link itself.
        Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(setup)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(json!({ "started": true }))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn start(_dir: &Path, _file: &Path) -> Result<Value> {
        bail!("Linux updates come from the release page")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_parse() {
        let answer = json!({
            "tag_name": "v9.8.7",
            "html_url": "https://github.com/x/y/releases/tag/v9.8.7",
            "assets": [{ "name": "SHA256SUMS", "browser_download_url": "https://d/SHA256SUMS" }],
        });
        let release = parse_release(&answer).unwrap();
        assert_eq!(release.version, "9.8.7");
        assert!(newer(&release.version));
        assert!(!newer("0.0.1"));
        assert_eq!(release.assets[0].0, "SHA256SUMS");
        assert!(parse_release(&json!({ "message": "Not Found" })).is_none());
    }

    #[test]
    fn each_platform_has_its_asset() {
        let name = asset_name("1.2.3");
        if cfg!(target_os = "macos") {
            assert!(name.unwrap().starts_with("realtimex-link-macos-"));
        } else if cfg!(windows) {
            assert_eq!(name.unwrap(), "RealTimeX-Link-Setup-1.2.3.exe");
        } else {
            assert!(name.is_none());
        }
    }
}
