//! Link updates itself from its signed releases (github.com/therealtimex/realtimex-link): it looks
//! for a newer release at start and every few hours, and the window offers Update. RealTimeX also
//! updates Link when it updates; this is for a Link installed on its own.
//!
//! Only a signed, installed Link updates itself, and only to a build signed by the same signer:
//! - macOS: the release's signed archive (checked against SHA256SUMS), its app checked with
//!   codesign (strict) against a requirement that an Apple-issued Developer ID certificate with
//!   this app's own Team ID signed it, and with spctl (notarized), then installed with that
//!   build's `rtx-linkd install` into this app's place. A detached helper does it (installing
//!   stops this daemon) and opens whatever is installed afterwards, so a failed install leaves
//!   the old Link running again rather than none.
//! - Windows: the release's signed installer (SHA256SUMS, then Authenticode: valid, chained to
//!   the same Microsoft root, with the same subject and the same Trusted Signing profile EKU as
//!   this rtx-linkd), started for the person to confirm (it asks for administrator). A detached
//!   helper waits for it and starts the installed Link again if nothing runs it.
//! - Linux: /opt needs root, so the window opens the release page (the .deb / .rpm are there).

use std::path::Path;
// Only the macOS and Windows updates run programs; Linux opens the release page.
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(any(target_os = "macos", windows))]
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::host::Host;

const DEFAULT_REPOSITORY: &str = "therealtimex/realtimex-link";
const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// RTX_LINK_UPDATE_REPOSITORY points at another repository (testing). Only the process's own
/// environment or the constant: never anything from the store, which the owner can write and
/// which Link's service reads as SYSTEM (realtimex-link#10, SD C2).
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
        let dir = download_dir(&release.version)?;
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

/// The codesign requirement the downloaded app must meet: Apple's anchor, a Developer ID
/// intermediate (1.2.840.113635.100.6.2.6), a Developer ID Application leaf
/// (1.2.840.113635.100.6.1.13) and this app's Team ID as the leaf's OU. A self-signed
/// certificate can claim any OU; only Apple issues one under this anchor. None for a Team ID
/// that isn't one (ten letters and digits), so nothing odd lands in the requirement.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn codesign_requirement(team: &str) -> Option<String> {
    let valid = team.len() == 10
        && team
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    valid.then(|| {
        format!(
            "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and \
             certificate leaf[field.1.2.840.113635.100.6.1.13] exists and \
             certificate leaf[subject.OU] = \"{team}\""
        )
    })
}

/// What the macOS helper runs: install the new Link ($1 is its rtx-linkd, $2 the installed app),
/// then open whatever is installed, whether that worked or not (`;`, not `&&`). The install
/// stops the running Link only once it can replace it; either way one runs again.
#[cfg(any(target_os = "macos", test))]
const MACOS_HELPER: &str =
    r#""$1" install "$2"; /usr/bin/open -g -a "$2" --args --background"#;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// The Team ID a signed app or executable carries ("not set" for ad-hoc).
    fn team_id(path: &Path) -> Option<String> {
        let output = crate::util::command("/usr/bin/codesign")
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
        let status = crate::util::command("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dir)
            .status()?;
        if !status.success() {
            bail!("couldn't unpack {}", archive.display());
        }
        let app = dir.join("RealTimeX Link.app");
        let requirement = codesign_requirement(&ours)
            .ok_or_else(|| anyhow!("This RealTimeX Link's Team ID ({ours}) isn't one"))?;
        let verified = crate::util::command("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "-R"])
            .arg(format!("={requirement}"))
            .arg(&app)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !verified.success() {
            bail!("the downloaded RealTimeX Link isn't signed by the same developer");
        }
        // Notarized too, as Gatekeeper would want it opened.
        let notarized = crate::util::command("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(&app)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !notarized.success() {
            bail!("the downloaded RealTimeX Link isn't notarized");
        }
        // Installing stops this daemon (and the window): a helper of its own installs and opens
        // the installed Link, in the background like at login.
        let linkd = crate::install::executables_dir(&app).join("rtx-linkd");
        detached(
            crate::util::command("/bin/sh")
                .args(["-c", MACOS_HELPER, "update"])
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

/// Windows: an Authenticode signature, as far as deciding "the same publisher" needs it.
#[cfg(any(windows, test))]
mod authenticode {
    use std::path::Path;

    use serde_json::Value;

    /// Azure Trusted Signing puts an EKU of the signing profile under this arc on every leaf
    /// certificate; it stays the same when the (short-lived) certificates are renewed, so it
    /// names the publisher where a thumbprint can't.
    const PROFILE_EKU: &str = "1.3.6.1.4.1.311.97.";

    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub struct Signature {
        pub valid: bool,
        pub subject: String,
        /// The chain's root certificate: thumbprint and subject.
        pub root: String,
        pub root_subject: String,
        /// The leaf's EKUs under PROFILE_EKU.
        pub profile: Vec<String>,
    }

    /// One JSON line: { status, subject, root, rootSubject, ekus } (ekus comma-separated). The
    /// chain is built only to name its root (Status already says whether Windows trusts it); the
    /// leaf is usually expired by now, its signature timestamped, so time isn't checked.
    const SCRIPT: &str = r#"$s = Get-AuthenticodeSignature -LiteralPath '{path}'
$c = $s.SignerCertificate
$root = ''; $rootSubject = ''; $ekus = @()
if ($c) {
  $chain = New-Object System.Security.Cryptography.X509Certificates.X509Chain
  $chain.ChainPolicy.RevocationMode = [System.Security.Cryptography.X509Certificates.X509RevocationMode]::NoCheck
  $chain.ChainPolicy.VerificationFlags = [System.Security.Cryptography.X509Certificates.X509VerificationFlags]::IgnoreNotTimeValid
  [void]$chain.Build($c)
  if ($chain.ChainElements.Count -gt 0) {
    $top = $chain.ChainElements[$chain.ChainElements.Count - 1].Certificate
    $root = $top.Thumbprint; $rootSubject = $top.Subject
  }
  foreach ($e in $c.Extensions) {
    if ($e.Oid.Value -eq '2.5.29.37') {
      $u = [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($e, $false)
      foreach ($o in $u.EnhancedKeyUsages) { $ekus += $o.Value }
    }
  }
}
[pscustomobject]@{ status = [string]$s.Status; subject = [string]$c.Subject; root = [string]$root; rootSubject = [string]$rootSubject; ekus = ($ekus -join ',') } | ConvertTo-Json -Compress"#;

    pub fn script(path: &Path) -> String {
        SCRIPT.replace("{path}", &path.display().to_string().replace('\'', "''"))
    }

    pub fn parse(text: &str) -> Option<Signature> {
        let answer: Value = serde_json::from_str(text.trim()).ok()?;
        let field = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Some(Signature {
            valid: field("status") == "Valid",
            subject: field("subject"),
            root: field("root"),
            root_subject: field("rootSubject"),
            profile: field("ekus")
                .split(',')
                .map(str::trim)
                .filter(|eku| eku.starts_with(PROFILE_EKU))
                .map(str::to_string)
                .collect(),
        })
    }

    /// Whether this rtx-linkd's own signature can vouch for others: valid, Trusted Signing.
    pub fn usable(ours: &Signature) -> bool {
        ours.valid && !ours.subject.is_empty() && !ours.root.is_empty() && !ours.profile.is_empty()
    }

    /// The downloaded file is signed as this rtx-linkd is: valid, chained to the same Microsoft
    /// root, the same subject and every profile EKU this one carries.
    pub fn same_publisher(ours: &Signature, theirs: &Signature) -> bool {
        usable(ours)
            && theirs.valid
            && theirs.root.eq_ignore_ascii_case(&ours.root)
            && theirs.root_subject.contains("O=Microsoft Corporation")
            && theirs.subject == ours.subject
            && ours.profile.iter().all(|eku| theirs.profile.contains(eku))
    }

    /// What the Windows helper runs: start the installer (it asks for administrator) and wait
    /// for it, then start the installed Link in the background unless it already runs (the
    /// installer's "Open" may have started it). Cancelled or failed, the Link stopped for it
    /// comes back all the same.
    pub fn helper_script(setup: &Path, app: &Path) -> String {
        let quote = |path: &Path| path.display().to_string().replace('\'', "''");
        format!(
            "try {{ Start-Process -FilePath '{setup}' -Wait -ErrorAction Stop }} catch {{ }}; \
             if (-not (Get-Process -Name 'rtx-link' -ErrorAction SilentlyContinue | \
             Where-Object {{ $_.Path -eq '{app}' }})) {{ \
             Start-Process -FilePath '{app}' -ArgumentList '--background' }}",
            setup = quote(setup),
            app = quote(app),
        )
    }
}

/// A fresh folder for a download. Under Link's service (SYSTEM) never the shared temp folder,
/// whose fixed name another user could claim first: a protected one of its own (SD C2).
fn download_dir(version: &str) -> Result<std::path::PathBuf> {
    #[cfg(windows)]
    if crate::service::active() {
        return Ok(crate::service::update_dir()?);
    }
    let dir = std::env::temp_dir().join(format!("realtimex-link-update-{version}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use super::authenticode::{self, Signature};

    fn signature(path: &Path) -> Option<Signature> {
        // util::command: no console flashing up.
        let output = crate::util::command(crate::util::powershell())
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &authenticode::script(path),
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        authenticode::parse(&String::from_utf8_lossy(&output.stdout))
    }

    pub fn start(_dir: &Path, setup: &Path) -> Result<Value> {
        let ours = signature(&std::env::current_exe()?).unwrap_or_default();
        if !authenticode::usable(&ours) {
            bail!("This RealTimeX Link isn't signed, so it can't update itself");
        }
        let theirs = signature(setup).unwrap_or_default();
        if !authenticode::same_publisher(&ours, &theirs) {
            bail!("the downloaded installer isn't signed by the same publisher");
        }
        // Link's service (SYSTEM) runs the installer itself, silently: no UAC prompt, and the
        // installer stops and starts the service around the update.
        if crate::service::active() {
            use std::os::windows::process::CommandExt;
            crate::util::command(setup)
                .arg("/S")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW: outlives the service it stops.
                .creation_flags(0x0000_0200 | 0x0800_0000)
                .spawn()?;
            return Ok(json!({ "started": true }));
        }
        // The installer asks for administrator and stops this Link itself; it installs where
        // RealTimeX looks for Link, and the helper starts that one again afterwards.
        let installed = crate::paths::install_dirs_for(
            "windows",
            &|name| std::env::var(name).ok(),
            &crate::paths::home_dir(),
        );
        let app = Path::new(&installed[0]).join("rtx-link.exe");
        use std::os::windows::process::CommandExt;
        crate::util::command(crate::util::powershell())
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &authenticode::helper_script(setup, &app),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW: outlives this daemon, which the
            // installer stops, without a console window.
            .creation_flags(0x0000_0200 | 0x0800_0000)
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

    /// The update source takes nothing from the store (SD C2): repository() has no store to
    /// read, and with no override it is Link's own.
    #[test]
    fn the_update_source_is_links_own_unless_the_environment_says_otherwise() {
        if std::env::var_os("RTX_LINK_UPDATE_REPOSITORY").is_none() {
            assert_eq!(repository(), DEFAULT_REPOSITORY);
        }
    }

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
    fn the_macos_requirement_pins_apple_and_the_team() {
        let requirement = codesign_requirement("ABCDE12345").unwrap();
        assert!(requirement.starts_with("anchor apple generic and "));
        assert!(requirement.contains("certificate 1[field.1.2.840.113635.100.6.2.6] exists"));
        assert!(requirement.contains("certificate leaf[field.1.2.840.113635.100.6.1.13] exists"));
        assert!(requirement.ends_with("certificate leaf[subject.OU] = \"ABCDE12345\""));
        assert!(!requirement.contains('\n'));
        for team in ["", "not set", "abcde12345", "ABCDE1234", "ABCDE\"1234", "ABCDE 1234"] {
            assert!(codesign_requirement(team).is_none(), "{team}");
        }
    }

    #[test]
    fn the_macos_helper_opens_link_even_when_installing_fails() {
        assert!(MACOS_HELPER.contains(r#""$1" install "$2"; /usr/bin/open"#));
        assert!(!MACOS_HELPER.contains("&&"));
    }

    fn signed(subject: &str, root: &str, ekus: &str) -> authenticode::Signature {
        authenticode::parse(
            &json!({
                "status": "Valid",
                "subject": subject,
                "root": root,
                "rootSubject": "CN=Microsoft Identity Verification Root Certificate Authority 2020, O=Microsoft Corporation, L=Redmond, S=Washington, C=US",
                "ekus": ekus,
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn windows_publishers_match_by_root_subject_and_profile() {
        let profile = "1.3.6.1.5.5.7.3.3,1.3.6.1.4.1.311.97.1.0,1.3.6.1.4.1.311.97.123.456";
        let ours = signed("CN=RealTimeX, O=RealTimeX", "AB12", profile);
        assert!(authenticode::usable(&ours));
        assert_eq!(
            ours.profile,
            ["1.3.6.1.4.1.311.97.1.0", "1.3.6.1.4.1.311.97.123.456"]
        );
        // A renewed leaf (another thumbprint, same profile) is the same publisher.
        assert!(authenticode::same_publisher(
            &ours,
            &signed("CN=RealTimeX, O=RealTimeX", "ab12", profile)
        ));
        // Same subject, another profile: someone else's Trusted Signing account.
        assert!(!authenticode::same_publisher(
            &ours,
            &signed(
                "CN=RealTimeX, O=RealTimeX",
                "AB12",
                "1.3.6.1.5.5.7.3.3,1.3.6.1.4.1.311.97.1.0,1.3.6.1.4.1.311.97.999.1"
            )
        ));
        // Same subject under another root: any CA can write a subject.
        assert!(!authenticode::same_publisher(
            &ours,
            &signed("CN=RealTimeX, O=RealTimeX", "CD34", profile)
        ));
        assert!(!authenticode::same_publisher(
            &ours,
            &signed("CN=Someone, O=Else", "AB12", profile)
        ));
        let mut invalid = signed("CN=RealTimeX, O=RealTimeX", "AB12", profile);
        invalid.valid = false;
        assert!(!authenticode::same_publisher(&ours, &invalid));
        // Without a profile of its own, this Link can't vouch for anything.
        let plain = signed("CN=RealTimeX, O=RealTimeX", "AB12", "1.3.6.1.5.5.7.3.3");
        assert!(!authenticode::usable(&plain));
        assert!(!authenticode::same_publisher(&plain, &plain));
        assert!(authenticode::parse("not json").is_none());
    }

    #[test]
    fn windows_scripts_quote_paths() {
        let script = authenticode::script(Path::new(r"C:\Temp\it's\setup.exe"));
        assert!(script.starts_with(r"$s = Get-AuthenticodeSignature -LiteralPath 'C:\Temp\it''s\setup.exe'"));
        assert!(!script.contains("{path}"));
        let helper = authenticode::helper_script(
            Path::new(r"C:\Temp\setup.exe"),
            Path::new(r"C:\Program Files\RealTimeX Link\rtx-link.exe"),
        );
        assert!(helper.starts_with(
            r"try { Start-Process -FilePath 'C:\Temp\setup.exe' -Wait -ErrorAction Stop } catch { };"
        ));
        assert!(helper.contains(
            r"Start-Process -FilePath 'C:\Program Files\RealTimeX Link\rtx-link.exe' -ArgumentList '--background'"
        ));
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
