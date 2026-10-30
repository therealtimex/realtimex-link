//! The root steps (#11 M2, M6): `rtx-linkd unattended-apply`, run by the administrator's prompt,
//! turns it on; the off path (the supervisor's `disable`, an uninstall, or the support command
//! `unattended-remove`) takes everything out again; and the root copy's refresh.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::os;
use super::plist::{self, ROOT_APP, SERVICE_LABEL, SERVICE_PLIST, SESSION_LABEL, SESSION_PLIST};
use crate::unattended::{Config, MACOS_MACHINE_DIR};

/// The installed Link the root copy comes from.
pub const APPLICATIONS_APP: &str = "/Applications/RealTimeX Link.app";
const CONFIG_FILE: &str = "unattended.json";

/// A bundle's CFBundleShortVersionString.
pub fn bundle_version(app: &Path) -> Option<String> {
    let text = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    crate::install::plist_version(&text)
}

/// Copy `/Applications`' bundle into a fresh staging folder, verify **the copy** (never the
/// source, then a copy of it: I3) and rename it into place. The old copy is removed after.
pub fn install_root_copy(team: &str) -> Result<()> {
    let root = Path::new(MACOS_MACHINE_DIR);
    os::root_dir(root, 0o755)?;
    let staging_base = root.join("staging");
    os::root_dir(&staging_base, 0o700)?;
    let staging = staging_base.join(crate::util::uuid());
    os::root_dir(&staging, 0o700)?;
    let result = (|| {
        let staged = staging.join("RealTimeX Link.app");
        let copied = std::process::Command::new("/usr/bin/ditto")
            .arg("--noqtn")
            .arg(APPLICATIONS_APP)
            .arg(&staged)
            .status()?;
        if !copied.success() {
            bail!("couldn't copy {APPLICATIONS_APP}");
        }
        let owned = std::process::Command::new("/usr/sbin/chown")
            .args(["-R", "-h", "root:wheel"])
            .arg(&staged)
            .status()?;
        // chmod refuses -R with -h; without -H or -L it doesn't follow links anyway.
        let locked = std::process::Command::new("/bin/chmod")
            .args(["-R", "go-w"])
            .arg(&staged)
            .status()?;
        if !owned.success() || !locked.success() {
            bail!("couldn't make the copy root's");
        }
        if !super::sign::bundle_is_ours(&staged, team) {
            bail!("the copied RealTimeX Link isn't signed by this one's developer");
        }
        let target = PathBuf::from(ROOT_APP);
        let old = staging.join("old.app");
        if std::fs::symlink_metadata(&target).is_ok() {
            std::fs::rename(&target, &old).context("moving the old copy aside")?;
        }
        std::fs::rename(&staged, &target).context("putting the copy in place")?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// `rtx-linkd unattended-apply --owner-uid <uid> --data-dir <dir>` (M6 step 3), as root.
pub fn apply(args: &[String]) -> Result<()> {
    // SAFETY: plain getters.
    if unsafe { libc::geteuid() } != 0 {
        bail!("unattended-apply runs as root, from the administrator's prompt");
    }
    // AuthorizationExecuteWithPrivileges leaves the real uid the caller's: all root, for launchctl.
    // SAFETY: plain calls; as root they can't fail.
    unsafe {
        libc::setgid(0);
        libc::setuid(0);
    }
    let value = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let uid: u32 = value("--owner-uid")
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| anyhow!("--owner-uid is required"))?;
    let data_dir = PathBuf::from(value("--data-dir").ok_or_else(|| anyhow!("--data-dir is required"))?);
    let owner = os::user(uid).ok_or_else(|| anyhow!("no user {uid}"))?;
    if uid == 0 {
        bail!("root can't own this");
    }
    // 1. The folder again, for that uid: its home's standard folder, theirs, with no link in the
    //    path (C5).
    let standard = super::policy::standard_folder(&owner.home);
    if data_dir != standard || !owned_without_links(&standard, uid) {
        bail!("{} isn't {}'s own RealTimeX Link folder", data_dir.display(), owner.name);
    }
    let team = super::sign::own_team().ok_or_else(|| anyhow!("this RealTimeX Link isn't signed"))?;
    // Another owner's supervisor first (another admin turning it on for themselves replaces it).
    let _ = os::launchctl(&["bootout", &format!("system/{SERVICE_LABEL}")]);
    // 2. Copy and verify.
    install_root_copy(&team)?;
    // 3. The choice.
    let config = Config {
        enabled: true,
        owner_uid: Some(uid),
        owner_name: owner.name.clone(),
        data_dir: standard,
        ..Config::default()
    };
    os::write_root_file(&Path::new(MACOS_MACHINE_DIR).join(CONFIG_FILE), &serde_json::to_vec_pretty(&config)?)?;
    // 4. The plists.
    let program = plist::root_program();
    os::write_root_file(Path::new(SERVICE_PLIST), plist::service(&program).as_bytes())?;
    os::write_root_file(Path::new(SESSION_PLIST), plist::session(&program).as_bytes())?;
    // 5–6. Load them: the supervisor now, the session job in every GUI session there is (the
    //      login window loads it the next time it shows).
    if !os::launchctl(&["bootstrap", "system", SERVICE_PLIST]) {
        bail!(crate::unattended::LOGIN_ITEMS_DISABLED);
    }
    for user in os::gui_users() {
        let domain = format!("gui/{user}");
        let _ = os::launchctl(&["bootout", &format!("{domain}/{SESSION_LABEL}")]);
        let _ = os::launchctl(&["bootstrap", &domain, SESSION_PLIST]);
    }
    Ok(())
}

/// Every folder from the home down to `folder` belongs to `uid` (or root, for /Users) and none
/// is a link.
fn owned_without_links(folder: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut path = PathBuf::new();
    for part in folder.components() {
        path.push(part);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
        // Down to the home, root's folders; from it on, the owner's.
        if meta.uid() != 0 && meta.uid() != uid {
            return false;
        }
    }
    std::fs::symlink_metadata(folder).is_ok_and(|meta| meta.uid() == uid && meta.is_dir())
}

/// The off path's steps after the supervisor answered (M6 *Off* 4–7): the session jobs, the
/// plists, the root copy, and the supervisor itself (last: it may be this process's job).
pub fn remove_everything() {
    for user in os::gui_users() {
        let _ = os::launchctl(&["bootout", &format!("gui/{user}/{SESSION_LABEL}")]);
    }
    let _ = os::launchctl(&["bootout", &format!("loginwindow/{SESSION_LABEL}")]);
    let _ = os::remove_root_file(Path::new(SESSION_PLIST));
    let _ = os::remove_root_file(Path::new(SERVICE_PLIST));
    let _ = std::fs::remove_dir_all(ROOT_APP);
    let _ = std::fs::remove_dir_all(Path::new(MACOS_MACHINE_DIR).join("staging"));
    let _ = os::launchctl(&["bootout", &format!("system/{SERVICE_LABEL}")]);
}

/// Write the choice as off (M6 *Off* 1).
pub fn write_off() -> Result<()> {
    let dir = Path::new(MACOS_MACHINE_DIR);
    os::root_dir(dir, 0o755)?;
    os::write_root_file(&dir.join(CONFIG_FILE), &serde_json::to_vec_pretty(&Config::default())?)?;
    Ok(())
}

/// `sudo …/rtx-linkd unattended-remove` (support): the same path as turning it off.
pub fn remove() -> Result<()> {
    // SAFETY: a plain getter.
    if unsafe { libc::geteuid() } != 0 {
        bail!("run it with sudo");
    }
    write_off()?;
    remove_everything();
    Ok(())
}
