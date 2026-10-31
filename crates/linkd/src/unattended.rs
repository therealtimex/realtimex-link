//! Start with this computer (realtimex-link#10, #11, realtimex-ai-app#2967): Link's own service
//! runs its owner's daemon from boot, so computers the owner allowed can connect at the sign-in
//! and lock screens.
//! - Windows: the service (LocalSystem, installed by the installer); the choice is
//!   `unattended.json` in Link's ProgramData folder, which only SYSTEM and administrators may
//!   write, changed through the service's pipe (the service checks the caller is an
//!   administrator; no UAC prompt).
//! - macOS: a root supervisor (launchd.rs) that runs the owner's daemon as the owner; the choice
//!   is `unattended.json` in `/Library/Application Support/RealTimeX Link`, written only by the
//!   root step an administrator approves at a prompt. The supervisor's socket answers status and
//!   turns it off (no prompt).
//!
//! `GET`/`POST /v1/unattended` answer the contract's body:
//! `{supported, serviceInstalled, enabled, needsPermission, canChange, reason?, pending}`.

// The service sides (service.rs, launchd.rs) use most of this on Windows and macOS only.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ApiError;

/// The service's name in the Service Control Manager, and its pipe.
pub const SERVICE_NAME: &str = "RealTimeX Link Service";
pub const PIPE_NAME: &str = r"\\.\pipe\RealTimeX-Link-Service";
/// macOS: the root folder, and the supervisor's socket (launchd makes it; only root can create
/// anything in /var/run, so whatever answers there is the supervisor).
pub const MACOS_MACHINE_DIR: &str = "/Library/Application Support/RealTimeX Link";
pub const MACOS_SOCKET: &str = "/var/run/ai.realtimex.link.sock";
/// The machine's choice, in [`machine_dir`].
const CONFIG_FILE: &str = "unattended.json";

/// What the service runs, when on: the owner's daemon, on the owner's data folder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    /// The Windows SID of the user who turned it on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner_sid: String,
    /// macOS: that user's uid and short name (the supervisor runs the daemon as them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_uid: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner_name: String,
    /// That user's Link data folder.
    #[serde(default)]
    pub data_dir: PathBuf,
}

impl Config {
    /// Off when there is no file, or one that doesn't parse: it is never on by accident.
    pub fn load(dir: &Path) -> Config {
        std::fs::read(dir.join(CONFIG_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Written whole to a temporary file and renamed, so a reader never sees half of it.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let temporary = dir.join(format!("{CONFIG_FILE}.new"));
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temporary, dir.join(CONFIG_FILE))
    }

    /// Whether the service runs this data folder's daemon.
    pub fn runs(&self, data_dir: &Path) -> bool {
        self.enabled && crate::paths::same_folder(&self.data_dir, data_dir, std::env::consts::OS)
    }
}

/// `%ProgramData%\RealTimeX Link`: the installer creates it, writable by SYSTEM and
/// administrators only. macOS: [`MACOS_MACHINE_DIR`], root's.
pub fn machine_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return PathBuf::from(MACOS_MACHINE_DIR);
    }
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("RealTimeX Link")
}

/// Whether this data folder's owner turned it on (its login item stays while it is: SD §1).
/// macOS reads the choice only from a root-owned folder and file that aren't links and that
/// nobody else may write (#11 M8): anything else reads as off.
pub fn enabled_for(data_dir: &Path) -> bool {
    if cfg!(windows) {
        return Config::load(&machine_dir()).runs(data_dir);
    }
    #[cfg(target_os = "macos")]
    {
        let dir = machine_dir();
        if root_only(&dir) && root_only(&dir.join(CONFIG_FILE)) {
            return Config::load(&dir).runs(data_dir);
        }
    }
    false
}

/// A path root may trust: no link, owned by root, and no group or other write.
#[cfg(unix)]
pub fn root_only(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        trusted_by_root(meta.file_type().is_symlink(), meta.uid(), meta.mode())
    })
}

/// [`root_only`]'s rule on its own, for the tests.
pub fn trusted_by_root(is_link: bool, uid: u32, mode: u32) -> bool {
    !is_link && uid == 0 && mode & 0o022 == 0
}

/// One request to the service's pipe, a JSON line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Request {
    /// The machine's state, and whether the caller may change it.
    Status { data_dir: PathBuf },
    Enable { data_dir: PathBuf },
    Disable { data_dir: PathBuf },
    /// macOS: a session bridge's hello (launchd.rs), its bundle version and whether its session
    /// has the console. The supervisor hands the connection to the owner's daemon.
    Bridge { version: String, on_console: bool },
}

/// The service's answer, a JSON line. `code` is set when it refused.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    /// The caller's data folder is the one the service runs.
    pub enabled: bool,
    /// The caller is an administrator of this computer.
    pub can_change: bool,
    /// Why the service doesn't run the daemon although it is on (`profile-unavailable`, or
    /// the owner is no longer an administrator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Refusal codes, shared with the contract (§6).
pub const OS_ADMIN_REQUIRED: &str = "os-admin-required";
pub const SERVICE_MISSING: &str = "service-missing";
/// The folder isn't exactly the caller's standard one (SD C5): another instance can't.
pub const STANDARD_ONLY: &str = "unattended-standard-only";
/// On, but the owner's folder couldn't be reached at boot (a roaming profile, SD C6).
pub const PROFILE_UNAVAILABLE: &str = "profile-unavailable";
/// On, and the service is waiting for the owner's own daemon to let go of the folder (it was
/// started while the service was stopped): that daemon steps aside (QA of !12).
pub const OWNER_DAEMON_RUNNING: &str = "owner-daemon-running";
/// macOS (#11 M6): the administrator cancelled the prompt; the prompt can't be shown here; the
/// jobs were turned off in System Settings › Login Items, so launchd won't load them.
pub const ADMIN_CANCELLED: &str = "admin-cancelled";
pub const ADMIN_PROMPT_UNAVAILABLE: &str = "admin-prompt-unavailable";
pub const LOGIN_ITEMS_DISABLED: &str = "login-items-disabled";
/// macOS (#11 M5): the root copy and the owner's daemon are different versions for a moment
/// (an update); the supervisor refreshes the copy.
pub const UNATTENDED_UPDATING: &str = "unattended-updating";
/// macOS (#11 M5): pasted files sit in the owner's own temporary folder, so only the owner's
/// own session can take them.
pub const FILES_NEED_OWNER_SESSION: &str = "files-need-owner-session";

impl Reply {
    pub fn refused(code: &str, error: impl Into<String>) -> Reply {
        Reply {
            code: Some(code.to_string()),
            error: Some(error.into()),
            ..Reply::default()
        }
    }

    /// The refusal as the local API's error: 403 for permission, 503 for no service.
    pub fn into_result(self) -> Result<Reply, ApiError> {
        match self.code.as_deref() {
            None => Ok(self),
            Some(code) => {
                let message = self.error.unwrap_or_else(|| code.to_string());
                let status = match code {
                    OS_ADMIN_REQUIRED => 403,
                    STANDARD_ONLY | ADMIN_CANCELLED | LOGIN_ITEMS_DISABLED => 409,
                    SERVICE_MISSING | ADMIN_PROMPT_UNAVAILABLE | UNATTENDED_UPDATING => 503,
                    _ => 500,
                };
                Err(ApiError::coded(status, code, message))
            }
        }
    }
}

/// Whether a per-user daemon must leave this folder to the service (SD C4): the service runs
/// it, so taking the lock would shut the service out. When the service is idle for this folder
/// (its owner's profile was unavailable at boot, or the owner is no longer an administrator),
/// or isn't there, the owner's own daemon runs as before.
pub fn yields(reply: Option<&Reply>) -> bool {
    reply.is_some_and(|reply| {
        reply.enabled
            && reply.code.is_none()
            && reply.idle.as_deref().is_none_or(|idle| idle == OWNER_DAEMON_RUNNING)
    })
}

/// macOS (#11 M8): this daemon's administrator prompt is open.
pub static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the per-user daemon tells the body when no supervisor answers: on macOS that only means
/// off (nothing is installed until it is turned on; consult §2: anyone may ask an admin);
/// on Windows, that the service is missing.
pub fn no_service() -> Option<Reply> {
    cfg!(target_os = "macos").then(|| Reply { can_change: true, ..Reply::default() })
}

/// GET /v1/unattended's body (contract §6). Off where it isn't supported.
pub fn body(supported: bool, service: Option<&Reply>) -> Value {
    let installed = supported && service.is_some();
    let reply = service.cloned().unwrap_or_default();
    let mut body = json!({
        "supported": supported,
        "serviceInstalled": installed,
        "enabled": installed && reply.enabled,
        "needsPermission": false,
        "canChange": installed && reply.can_change,
        "pending": supported && PENDING.load(std::sync::atomic::Ordering::Relaxed),
    });
    if installed && !reply.can_change {
        body["reason"] = json!(OS_ADMIN_REQUIRED);
    } else if let Some(idle) = reply.idle.filter(|_| installed && reply.enabled) {
        body["reason"] = json!(idle);
    }
    body
}

/// Ask the service. `Ok(None)`: no service is listening (not installed, or stopped).
#[cfg(windows)]
pub fn ask(request: &Request) -> anyhow::Result<Option<Reply>> {
    use std::io::{BufRead, BufReader, Write};
    let pipe = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)
    {
        Ok(pipe) => pipe,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    (&pipe).write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(&pipe).read_line(&mut answer)?;
    Ok(Some(serde_json::from_str(answer.trim())?))
}

/// macOS: ask the supervisor on its socket. `Ok(None)`: it isn't there (off).
#[cfg(target_os = "macos")]
pub fn ask(request: &Request) -> anyhow::Result<Option<Reply>> {
    use std::io::{BufRead, BufReader, ErrorKind, Write};
    let stream = match std::os::unix::net::UnixStream::connect(MACOS_SOCKET) {
        Ok(stream) => stream,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer)?;
    if answer.trim().is_empty() {
        // Closed with no answer: the supervisor refused this caller (M4).
        anyhow::bail!("RealTimeX Link's service closed the connection");
    }
    Ok(Some(serde_json::from_str(answer.trim())?))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn ask(_request: &Request) -> anyhow::Result<Option<Reply>> {
    Ok(None)
}

/// Whether this build can start with the computer: Windows, and macOS when this bundle carries
/// a Developer ID team signature (#11 M4, M8: an unsigned or ad-hoc build has no row).
pub fn supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::launchd::sign::own_team().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        cfg!(windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_is_off_without_a_file_and_kept_whole() {
        let dir = std::env::temp_dir().join(format!("rtx-unattended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Config::load(&dir), Config::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), b"{not json").unwrap();
        assert!(!Config::load(&dir).enabled);
        let on = Config {
            enabled: true,
            owner_sid: "S-1-5-21-1-2-3-1001".into(),
            data_dir: PathBuf::from("/home/a/.config/realtimex-link"),
            ..Config::default()
        };
        on.save(&dir).unwrap();
        assert_eq!(Config::load(&dir), on);
        assert!(on.runs(Path::new("/home/a/.config/realtimex-link")));
        assert!(!on.runs(Path::new("/home/b/.config/realtimex-link")));
        assert!(!Config { enabled: false, ..on }.runs(Path::new("/home/a/.config/realtimex-link")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_macos_choice_names_its_owner_and_a_windows_one_doesnt_change() {
        let mac = Config {
            enabled: true,
            owner_uid: Some(501),
            owner_name: "rta".into(),
            data_dir: PathBuf::from("/Users/rta/Library/Application Support/RealTimeX Link"),
            ..Config::default()
        };
        let text = serde_json::to_string(&mac).unwrap();
        assert_eq!(
            text,
            r#"{"enabled":true,"ownerUid":501,"ownerName":"rta","dataDir":"/Users/rta/Library/Application Support/RealTimeX Link"}"#
        );
        assert_eq!(serde_json::from_str::<Config>(&text).unwrap(), mac);
        let windows: Config =
            serde_json::from_str(r#"{"enabled":true,"ownerSid":"S-1-5-21-1","dataDir":"C:/x"}"#).unwrap();
        assert_eq!(windows.owner_uid, None);
        assert_eq!(
            serde_json::to_string(&windows).unwrap(),
            r#"{"enabled":true,"ownerSid":"S-1-5-21-1","dataDir":"C:/x"}"#
        );
    }

    #[test]
    fn root_trusts_only_its_own_unwritable_files() {
        assert!(trusted_by_root(false, 0, 0o100644));
        assert!(trusted_by_root(false, 0, 0o40755));
        // Owned by someone else, writable by a group or others, or a link: off.
        assert!(!trusted_by_root(false, 501, 0o100644));
        assert!(!trusted_by_root(false, 0, 0o100664));
        assert!(!trusted_by_root(false, 0, 0o100646));
        assert!(!trusted_by_root(true, 0, 0o120755));
    }

    #[test]
    fn a_bridge_says_hello_with_its_version() {
        let hello = Request::Bridge { version: "0.3.1".into(), on_console: true };
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"op":"bridge","version":"0.3.1","onConsole":true}"#
        );
    }

    #[test]
    fn pipe_lines_name_their_op_and_folder() {
        let request = Request::Enable { data_dir: PathBuf::from("C:/Users/a/AppData/Roaming/RealTimeX Link") };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(
            line,
            r#"{"op":"enable","dataDir":"C:/Users/a/AppData/Roaming/RealTimeX Link"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        assert!(serde_json::from_str::<Request>(r#"{"op":"format-disk"}"#).is_err());
        let reply: Reply = serde_json::from_str(r#"{"enabled":true,"canChange":true}"#).unwrap();
        assert_eq!(serde_json::to_string(&reply).unwrap(), r#"{"enabled":true,"canChange":true}"#);
    }

    #[test]
    fn refusals_become_the_contracts_errors() {
        let status = |code: &str| Reply::refused(code, "no").into_result().unwrap_err().status;
        assert_eq!(status(OS_ADMIN_REQUIRED), 403);
        assert_eq!(status(STANDARD_ONLY), 409);
        assert_eq!(status(SERVICE_MISSING), 503);
        assert_eq!(status(ADMIN_CANCELLED), 409);
        assert_eq!(status(ADMIN_PROMPT_UNAVAILABLE), 503);
        assert_eq!(status(UNATTENDED_UPDATING), 503);
        assert_eq!(status("io"), 500);
        let error = Reply::refused(OS_ADMIN_REQUIRED, "an administrator of this computer")
            .into_result()
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some(OS_ADMIN_REQUIRED));
        assert!(Reply::default().into_result().is_ok());
    }

    #[test]
    fn a_per_user_daemon_yields_only_to_a_service_running_its_folder() {
        let running = Reply { enabled: true, can_change: true, ..Reply::default() };
        assert!(yields(Some(&running)));
        assert!(!yields(None));
        assert!(!yields(Some(&Reply::default())));
        let idle = Reply { idle: Some(PROFILE_UNAVAILABLE.into()), ..running.clone() };
        assert!(!yields(Some(&idle)));
        // The service waits for this very daemon's folder: it steps aside (QA of !12).
        let waiting = Reply { idle: Some(OWNER_DAEMON_RUNNING.into()), ..running.clone() };
        assert!(yields(Some(&waiting)));
        assert!(!yields(Some(&Reply::refused("io", "broken"))));
    }

    #[test]
    fn the_body_follows_the_contract() {
        assert_eq!(
            body(false, None),
            json!({"supported": false, "serviceInstalled": false, "enabled": false,
                   "needsPermission": false, "canChange": false, "pending": false})
        );
        // Windows without the service: the reinstall alert.
        let missing = body(true, None);
        assert_eq!(missing["serviceInstalled"], false);
        assert!(missing.get("reason").is_none());
        let admin = Reply { enabled: true, can_change: true, ..Reply::default() };
        let on = body(true, Some(&admin));
        assert_eq!((on["enabled"].clone(), on["canChange"].clone()), (json!(true), json!(true)));
        assert!(on.get("reason").is_none());
        let standard = Reply { enabled: false, can_change: false, ..Reply::default() };
        let badge = body(true, Some(&standard));
        assert_eq!(badge["canChange"], false);
        assert_eq!(badge["reason"], OS_ADMIN_REQUIRED);
        // On, with the owner's folder unreachable at boot: still On, and why (SD C6).
        let roaming = Reply {
            enabled: true,
            can_change: true,
            idle: Some(PROFILE_UNAVAILABLE.into()),
            ..Reply::default()
        };
        let idle = body(true, Some(&roaming));
        assert_eq!((idle["enabled"].clone(), idle["reason"].clone()), (json!(true), json!(PROFILE_UNAVAILABLE)));
    }
}
