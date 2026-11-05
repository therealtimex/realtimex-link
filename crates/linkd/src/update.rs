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
//! - Linux: the release's archive (checked against SHA256SUMS), unpacked into a folder only this
//!   user can write, then installed with that build's `rtx-linkd install` into /opt/RealTimeX
//!   Link through pkexec, which asks for an administrator's password the way RealTimeX's own
//!   update does. A detached helper does it (installing stops this daemon) and starts the
//!   installed Link again afterwards; a cancelled password prompt leaves the old Link running.

use std::path::Path;
#[cfg(unix)]
use std::process::Command;
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
    /// The last release check's error (cleared by the next check that works).
    error: Option<String>,
    /// Why the last update wasn't installed (Linux: a cancelled password prompt, a failed
    /// install, also one the restarted Link found at start). Kept apart from `error`, so a release
    /// check doesn't clear it before it was seen; the next Update clears it.
    install_error: Option<String>,
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
        "linux" => Some(format!("realtimex-link-linux-{arch}.tar.gz")),
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
        #[cfg(target_os = "linux")]
        if let Some(base) = crate::paths::data_dir().map(|dir| dir.join(LINUX_ATTEMPTS)) {
            self.recover_linux_attempts(&base);
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

    /// For the window: { current, latest, available, page, busy, error, installError, checkedAt }.
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
            "installError": state.install_error,
            "checkedAt": state.checked_at,
        })
    }

    /// Look for the latest release now.
    pub async fn check(&self) -> Result<Value> {
        let release = tokio::task::spawn_blocking(latest_release).await?;
        self.record_check(release);
        Ok(self.status())
    }

    /// A release check's answer. Leaves `install_error` alone: an unseen failed install stays.
    fn record_check(&self, release: Result<Release>) {
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
    }

    /// Update to the latest release. A platform without a release archive: { open: page } for
    /// the window to open instead.
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
            state.install_error = None;
        }
        let me = self.clone();
        let outcome =
            tokio::task::spawn_blocking(move || me.download_and_start(&release, &name)).await?;
        let mut state = lock(&self.state);
        match outcome {
            // Linux: the helper is still asking for the password and installing; it stays busy
            // (no second Update) until the helper's outcome is in (`settle`).
            Ok(answer) if answer.get("pending") == Some(&Value::Bool(true)) => Ok(answer),
            Ok(answer) => {
                state.busy = None;
                Ok(answer)
            }
            Err(error) => {
                state.busy = None;
                state.error = Some(format!("{error:#}"));
                drop(state);
                self.log(&format!("failed: {error:#}"));
                Err(error)
            }
        }
    }

    /// The Linux helper's outcome: the update is no longer busy, and a failure or a cancelled
    /// password prompt shows in the status. (Once installed, the installer has stopped this
    /// daemon and the new Link runs, so this mostly reports what didn't happen.)
    #[cfg(any(target_os = "linux", test))]
    fn settle(&self, outcome: LinuxOutcome) {
        let mut state = lock(&self.state);
        state.busy = None;
        state.install_error = match &outcome {
            LinuxOutcome::Installed => None,
            other => Some(other.message()),
        };
        drop(state);
        self.log(&format!("Linux install: {}", outcome.message()));
    }

    /// At start: what a Linux update helper left after its install stopped the daemon that
    /// started it (the installer stops Link before it copies, so a failure there has no other
    /// reader). Reported once, and the attempt's folder removed; an attempt without a result yet
    /// is left to its helper, unless it is a day old.
    #[cfg(any(target_os = "linux", test))]
    fn recover_linux_attempts(&self, base: &Path) {
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Some(outcome) = LinuxOutcome::take(&dir) {
                self.settle(outcome);
                let _ = std::fs::remove_dir_all(&dir);
            } else if entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60))
            {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    fn download_and_start(self: &Arc<Self>, release: &Release, name: &str) -> Result<Value> {
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
        #[cfg(target_os = "linux")]
        {
            let mut helper = platform::start(&dir, &file)?;
            let me = self.clone();
            std::thread::spawn(move || {
                let _ = helper.wait();
                // Only reached when the install didn't stop this daemon; otherwise the restarted
                // Link's daemon finds the result (`recover_linux_attempts`).
                me.settle(LinuxOutcome::take(&dir).unwrap_or(LinuxOutcome::Unknown));
                let _ = std::fs::remove_dir_all(&dir);
            });
            Ok(json!({ "started": true, "pending": true }))
        }
        #[cfg(not(target_os = "linux"))]
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

/// Start a helper in its own session, so it outlives this daemon when an install stops it.
#[cfg(unix)]
fn detached(command: &mut Command) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid has no preconditions; it runs in the child before exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

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
    // Linux: a folder of this attempt's own, which no other attempt removes or refills while its
    // helper (root, after a password prompt that may wait) still runs what is in it.
    #[cfg(target_os = "linux")]
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        // In Link's own folder, where the restarted Link looks for the result at start.
        let base = crate::paths::data_dir()
            .map(|dir| dir.join(LINUX_ATTEMPTS))
            .unwrap_or_else(|| std::env::temp_dir().join("realtimex-link-updates"));
        std::fs::create_dir_all(&base)?;
        let dir = base.join(format!("{version}-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir)?;
        Ok(dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let dir = std::env::temp_dir().join(format!("realtimex-link-update-{version}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
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

/// Where Link installs itself on Linux (RealTimeX's installer and the .deb / .rpm put it there too).
#[cfg(any(target_os = "linux", all(test, unix)))]
const LINUX_INSTALL_DIR: &str = "/opt/RealTimeX Link";

/// What the Linux helper runs: install the new Link as root through pkexec ($1 pkexec, $2 the new
/// rtx-linkd, $3 the installed folder; pkexec asks for an administrator's password in the
/// session), write pkexec's exit status and the install's output (its error is a JSON line on
/// stdout) and errors into $4 (this attempt's folder) for whichever daemon runs afterwards to
/// report; then start the installed Link in the background
/// whether that worked or not (`;`, not `&&`). The install stops the running Link only once it
/// can replace it, and a second rtx-link leaves when one already runs, so a cancelled prompt
/// keeps the old Link.
#[cfg(any(target_os = "linux", all(test, unix)))]
const LINUX_HELPER: &str = r#""$1" --disable-internal-agent "$2" install "$3" >"$4/install.out" 2>"$4/install.log"; echo $? >"$4/result"; "$3/rtx-link" --background &"#;

/// The folder (in Link's data folder) that holds each Linux update attempt's own folder.
#[cfg(any(target_os = "linux", test))]
const LINUX_ATTEMPTS: &str = "updates";

/// How the Linux helper's pkexec ended (pkexec: 126 the person dismissed the password prompt,
/// 127 not authorized or no way to ask; else the installer's own status).
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum LinuxOutcome {
    Installed,
    Cancelled,
    NotAuthorized(String),
    Failed(String),
    Unknown,
}

#[cfg(any(target_os = "linux", test))]
impl LinuxOutcome {
    /// `out`: the installer's stdout (a JSON line, with `error` when it failed); `log`: stderr
    /// (pkexec's own messages).
    fn from(result: Option<&str>, out: &str, log: &str) -> Self {
        let installer_error = out.lines().rev().find_map(|line| {
            serde_json::from_str::<Value>(line.trim())
                .ok()?
                .get("error")?
                .as_str()
                .map(str::to_string)
        });
        let detail = installer_error.unwrap_or_else(|| {
            log.lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default()
                .to_string()
        });
        match result.map(str::trim).and_then(|code| code.parse::<i32>().ok()) {
            Some(0) => Self::Installed,
            Some(126) => Self::Cancelled,
            Some(127) => Self::NotAuthorized(detail),
            Some(code) => Self::Failed(if detail.is_empty() {
                format!("exit status {code}")
            } else {
                detail
            }),
            None => Self::Unknown,
        }
    }

    /// What the helper left in this attempt's folder, claimed once: renaming the result is what
    /// decides whether the old daemon's waiter or the restarted daemon reports it. None while
    /// the helper hasn't finished (or another reader took it).
    fn take(dir: &Path) -> Option<Self> {
        let taken = dir.join("result.taken");
        std::fs::rename(dir.join("result"), &taken).ok()?;
        let result = std::fs::read_to_string(&taken).ok();
        let out = std::fs::read_to_string(dir.join("install.out")).unwrap_or_default();
        let log = std::fs::read_to_string(dir.join("install.log")).unwrap_or_default();
        Some(Self::from(result.as_deref(), &out, &log))
    }

    fn message(&self) -> String {
        match self {
            Self::Installed => "installed".to_string(),
            Self::Cancelled => {
                "Update cancelled: no administrator's password was given. Click Update to try again"
                    .to_string()
            }
            Self::NotAuthorized(detail) => {
                // Product/UX (realtimex-link!20): what to do next, with What's new (the release
                // page, in the same card) as the fallback.
                let mut message = "Couldn't get an administrator's permission to update. If no \
                                   password prompt appeared, this computer can't show one here: \
                                   ask an administrator to update RealTimeX Link, or install the \
                                   .deb or .rpm from What's new"
                    .to_string();
                if !detail.is_empty() {
                    message.push_str(&format!(" (Details: {detail})"));
                }
                message
            }
            Self::Failed(detail) => format!("The update couldn't be installed: {detail}"),
            Self::Unknown => "The update didn't finish; click Update to try again".to_string(),
        }
    }
}

/// Whether /proc/self/status says this process runs with no_new_privs (it can't gain privileges).
#[cfg(any(target_os = "linux", test))]
fn no_new_privs(status: &str) -> bool {
    status.lines().any(|line| {
        line.strip_prefix("NoNewPrivs:")
            .is_some_and(|value| value.trim() == "1")
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    const PKEXEC: &str = "/usr/bin/pkexec";

    /// Start the helper; the caller waits for it and reports its outcome.
    pub fn start(dir: &Path, archive: &Path) -> Result<std::process::Child> {
        let installed = crate::install::source()?;
        if installed != Path::new(LINUX_INSTALL_DIR) {
            bail!("Only RealTimeX Link installed in {LINUX_INSTALL_DIR} updates itself");
        }
        // A process started with no_new_privs (inherited, e.g. from an app the Electron updater
        // relaunched) can't gain root: pkexec would exit 127 without asking for a password.
        if no_new_privs(&std::fs::read_to_string("/proc/self/status").unwrap_or_default()) {
            bail!(
                "This RealTimeX Link can't ask for an administrator's password because the program \
                 that started it doesn't allow it; quit Link, open it from the applications menu \
                 and update again"
            );
        }
        if !Path::new(PKEXEC).is_file() {
            bail!(
                "Updating needs pkexec (polkit) to ask for an administrator's password; install \
                 the .deb or .rpm from the release page instead"
            );
        }
        // root runs what is unpacked here: only this user may write the folder. A folder of its
        // own, since the install copies the whole folder rtx-linkd is in (not the archive).
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let unpacked = dir.join("RealTimeX Link");
        std::fs::create_dir(&unpacked)?;
        let status = crate::util::command("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(&unpacked)
            .status()?;
        if !status.success() {
            bail!("couldn't unpack {}", archive.display());
        }
        let linkd = unpacked.join("rtx-linkd");
        if !linkd.is_file() {
            bail!("the downloaded archive has no rtx-linkd");
        }
        // Installing stops this daemon (and the window): a helper in a session of its own
        // installs and starts the installed Link, in the background like at login.
        detached(
            crate::util::command("/bin/sh")
                .args(["-c", LINUX_HELPER, "update", PKEXEC])
                .arg(&linkd)
                .arg(&installed)
                .arg(dir),
        )
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use super::*;

    pub fn start(_dir: &Path, _file: &Path) -> Result<Value> {
        bail!("This platform's updates come from the release page")
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
        } else if cfg!(target_os = "linux") {
            assert!(name.unwrap().starts_with("realtimex-link-linux-"));
        } else {
            assert!(name.is_none());
        }
    }

    #[test]
    fn no_new_privs_is_read_from_proc_status() {
        assert!(no_new_privs("Name:\trtx-linkd\nNoNewPrivs:\t1\nSeccomp:\t0\n"));
        assert!(!no_new_privs("Name:\trtx-linkd\nNoNewPrivs:\t0\n"));
        assert!(!no_new_privs(""));
    }

    /// The Linux helper installs through pkexec, records pkexec's status and the install's errors
    /// in the attempt's folder, and starts the installed Link even when the password prompt was
    /// cancelled (a fake pkexec exiting 126 here) without hiding that status.
    #[cfg(unix)]
    #[test]
    fn the_linux_helper_records_the_outcome_then_starts_link_either_way() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rtx-link-helper-{}", std::process::id()));
        let installed = dir.join("RealTimeX Link");
        let attempt = dir.join("attempt");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::create_dir_all(&attempt).unwrap();
        let log = dir.join("log");
        let fake = |path: &Path, body: &str| {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let pkexec = dir.join("pkexec");
        fake(
            &pkexec,
            &format!(
                "echo \"pkexec $*\" >> '{}'; echo 'Request dismissed' >&2; exit 126",
                log.display()
            ),
        );
        let started = dir.join("started");
        fake(
            &installed.join("rtx-link"),
            &format!("echo \"rtx-link $*\" > '{}'", started.display()),
        );
        let status = crate::util::command("/bin/sh")
            .args(["-c", LINUX_HELPER, "update"])
            .arg(&pkexec)
            .arg("/tmp/new/rtx-linkd")
            .arg(&installed)
            .arg(&attempt)
            .status()
            .unwrap();
        // rtx-link is started in the background: give it a moment.
        for _ in 0..50 {
            if started.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ran = std::fs::read_to_string(&log).unwrap();
        let restarted = std::fs::read_to_string(&started).unwrap_or_default();
        let outcome = LinuxOutcome::take(&attempt);
        let again = LinuxOutcome::take(&attempt);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.success());
        assert_eq!(
            ran,
            format!(
                "pkexec --disable-internal-agent /tmp/new/rtx-linkd install {}\n",
                installed.display()
            )
        );
        assert_eq!(restarted, "rtx-link --background\n");
        assert_eq!(outcome, Some(LinuxOutcome::Cancelled));
        assert_eq!(again, None, "a result is reported once");
        assert_eq!(LINUX_INSTALL_DIR, "/opt/RealTimeX Link");
    }

    #[test]
    fn linux_outcomes_tell_cancelled_from_failed() {
        assert_eq!(LinuxOutcome::from(Some("0\n"), "", ""), LinuxOutcome::Installed);
        assert_eq!(
            LinuxOutcome::from(Some("126\n"), "", "Request dismissed"),
            LinuxOutcome::Cancelled
        );
        assert_eq!(
            LinuxOutcome::from(Some("127\n"), "", "Error executing command as another user: No authentication agent found.\n"),
            LinuxOutcome::NotAuthorized(
                "Error executing command as another user: No authentication agent found.".into()
            )
        );
        assert_eq!(
            LinuxOutcome::from(Some("1\n"), "", "warn\nRealTimeX Link did not stop: /opt/RealTimeX Link\n\n"),
            LinuxOutcome::Failed("RealTimeX Link did not stop: /opt/RealTimeX Link".into())
        );
        assert_eq!(
            LinuxOutcome::from(Some("3"), "", ""),
            LinuxOutcome::Failed("exit status 3".into())
        );
        assert_eq!(LinuxOutcome::from(None, "", ""), LinuxOutcome::Unknown);
        // The installer reports its failure as a JSON line on stdout: that is the reason.
        assert_eq!(
            LinuxOutcome::from(
                Some("1\n"),
                "{\"error\":\"could not install /opt/RealTimeX Link: No space left on device\"}\n",
                "some stderr noise\n"
            ),
            LinuxOutcome::Failed(
                "could not install /opt/RealTimeX Link: No space left on device".into()
            )
        );
        assert!(LinuxOutcome::Cancelled.message().contains("cancelled"));
        assert_eq!(
            LinuxOutcome::NotAuthorized("No authentication agent found.".into()).message(),
            "Couldn't get an administrator's permission to update. If no password prompt \
             appeared, this computer can't show one here: ask an administrator to update \
             RealTimeX Link, or install the .deb or .rpm from What's new (Details: No \
             authentication agent found.)"
        );
        assert_eq!(
            LinuxOutcome::NotAuthorized(String::new()).message(),
            "Couldn't get an administrator's permission to update. If no password prompt \
             appeared, this computer can't show one here: ask an administrator to update \
             RealTimeX Link, or install the .deb or .rpm from What's new"
        );
    }

    fn pending_updater() -> Arc<Updater> {
        let updater = Updater::new(Weak::new());
        {
            let mut state = lock(&updater.state);
            state.latest = Some(Release {
                version: "999.0.0".into(),
                page: "https://example.invalid/releases/v999.0.0".into(),
                assets: Vec::new(),
            });
            // What install() leaves while the Linux helper still waits for the password.
            state.busy = Some("installing");
        }
        updater
    }

    /// While a Linux helper waits for the password, a second Update is refused (it would
    /// otherwise start another install over the first), until the helper's outcome settles it.
    #[test]
    fn a_pending_linux_install_refuses_a_second_update_until_it_settles() {
        let updater = pending_updater();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let second = runtime.block_on(updater.install());
        assert!(format!("{:#}", second.unwrap_err()).contains("already under way"));
        assert_eq!(updater.status()["busy"], json!("installing"));

        updater.settle(LinuxOutcome::Cancelled);
        let status = updater.status();
        assert_eq!(status["busy"], Value::Null);
        assert!(status["installError"].as_str().unwrap().contains("cancelled"));
    }

    #[test]
    fn a_failed_linux_install_reports_why() {
        let updater = pending_updater();
        updater.settle(LinuxOutcome::NotAuthorized("No authentication agent found.".into()));
        let status = updater.status();
        assert_eq!(status["busy"], Value::Null);
        assert!(status["installError"]
            .as_str()
            .unwrap()
            .contains("No authentication agent found."));
        updater.settle(LinuxOutcome::Installed);
        assert_eq!(updater.status()["installError"], Value::Null);
    }

    /// The installer stops Link (and the daemon whose waiter would report) before it copies; a
    /// copy that then fails is found by the restarted Link's daemon at start, with the
    /// installer's own reason, once.
    #[cfg(unix)]
    #[test]
    fn a_failure_after_the_installer_stopped_the_daemon_shows_in_the_restarted_link() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("rtx-link-recover-{}", std::process::id()));
        let base = root.join(LINUX_ATTEMPTS);
        let attempt = base.join("9.9.9-1-1");
        let installed = root.join("RealTimeX Link");
        std::fs::create_dir_all(&attempt).unwrap();
        std::fs::create_dir_all(&installed).unwrap();
        let fake = |path: &Path, body: &str| {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // pkexec runs the installer, which (having stopped Link) fails while copying: its error
        // is a JSON line on stdout, its status 1.
        let pkexec = root.join("pkexec");
        fake(
            &pkexec,
            r#"echo '{"error":"could not install /opt/RealTimeX Link: No space left on device"}'; exit 1"#,
        );
        fake(&installed.join("rtx-link"), "exit 0");
        let status = crate::util::command("/bin/sh")
            .args(["-c", LINUX_HELPER, "update"])
            .arg(&pkexec)
            .arg("/tmp/new/rtx-linkd")
            .arg(&installed)
            .arg(&attempt)
            .status()
            .unwrap();
        assert!(status.success());

        // No waiter ran (its daemon was stopped): the restarted Link's Updater starts fresh.
        let restarted = Updater::new(Weak::new());
        restarted.recover_linux_attempts(&base);
        let reported = restarted.status();
        let attempt_left = attempt.exists();
        // Once: a second start finds nothing more to report.
        let again = Updater::new(Weak::new());
        again.recover_linux_attempts(&base);
        let reported_again = again.status();
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(reported["busy"], Value::Null);
        assert_eq!(
            reported["installError"].as_str().unwrap(),
            "The update couldn't be installed: could not install /opt/RealTimeX Link: No space \
             left on device"
        );
        assert!(!attempt_left, "the attempt's folder is removed once reported");
        assert_eq!(reported_again["installError"], Value::Null);

        // The restarted Link knows no release yet (latest None, not available): the window shows
        // installError on its own. The first release check, which works, must not dismiss it.
        assert_eq!(reported["available"], json!(false));
        restarted.record_check(Ok(Release {
            version: "999.0.0".into(),
            page: "https://example.invalid/releases/v999.0.0".into(),
            assets: Vec::new(),
        }));
        let after_check = restarted.status();
        assert_eq!(after_check["available"], json!(true));
        assert_eq!(after_check["error"], Value::Null);
        assert_eq!(after_check["installError"], reported["installError"]);
        // Nor does one that fails: that is the check's own error, beside it.
        restarted.record_check(Err(anyhow!("offline")));
        let after_failed_check = restarted.status();
        assert!(after_failed_check["error"].as_str().unwrap().contains("offline"));
        assert_eq!(after_failed_check["installError"], reported["installError"]);
    }

    /// An attempt still waiting for its password prompt (no result yet) is left to its helper.
    #[test]
    fn an_attempt_without_a_result_is_left_alone() {
        let root = std::env::temp_dir().join(format!("rtx-link-pending-{}", std::process::id()));
        let base = root.join(LINUX_ATTEMPTS);
        let attempt = base.join("9.9.9-2-2");
        std::fs::create_dir_all(&attempt).unwrap();
        let updater = Updater::new(Weak::new());
        updater.recover_linux_attempts(&base);
        let still_there = attempt.exists();
        let status = updater.status();
        let _ = std::fs::remove_dir_all(&root);
        assert!(still_there);
        assert_eq!(status["installError"], Value::Null);
    }
}
