//! The RealTimeX that owns this Link (realtimex-ai-app#2836 §6.2, §6.3 A1/A3). When RealTimeX is
//! installed, Link always uses RealTimeX's account: the first RealTimeX install to hand its
//! account over (with its installId) owns Link, its handovers replace any sign-in Link made
//! itself, and Link offers no sign-out or other account while it is present. Another install is
//! refused (adopted-other); a handover without an installId (an older RealTimeX) is ignored once
//! there is an owner.
//!
//! "Present" ends with POST /v1/account/release (RealTimeX's uninstallers send it), or when
//! RealTimeX's executable is gone from where its installers put it (macOS has no uninstaller),
//! or after 30 days without a handover. A handover in the last 24 h counts as present whatever
//! the executable check says (dev:all runs RealTimeX from source).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::store::Store;

pub const KEY: &str = "realtimex-owner";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Without a handover for this long, RealTimeX no longer counts as present.
const LAPSE_MS: i64 = 30 * DAY_MS;
/// A handover this recent counts as present even with no installed executable.
const RECENT_MS: i64 = DAY_MS;

#[derive(Clone, Debug, PartialEq)]
pub struct Owner {
    pub install_id: String,
    /// Milliseconds since the epoch.
    pub last_handover_at: i64,
}

pub fn get(store: &Store) -> Option<Owner> {
    let value = store.get(KEY)?;
    Some(Owner {
        install_id: value.get("installId")?.as_str()?.to_string(),
        last_handover_at: value.get("lastHandoverAt")?.as_i64()?,
    })
}

/// Whether a RealTimeX owns this Link now.
pub fn present(store: &Store) -> bool {
    get(store).is_some()
}

pub fn valid_install_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn adopted_other(owner: &Owner) -> ApiError {
    ApiError::coded(
        409,
        "adopted-other",
        "Another RealTimeX already uses RealTimeX Link on this computer",
    )
    .with_details(json!({ "ownerInstallId": owner.install_id }))
}

/// A handover from install `install_id`: it becomes the owner when there is none. Ok(true): this
/// install owns Link (and the handover counts); 409 adopted-other for another install.
pub fn claim(store: &Store, install_id: &str, now: i64) -> Result<bool, ApiError> {
    if !valid_install_id(install_id) {
        return Err(ApiError::bad_request(
            "installId must be RealTimeX's install ID",
        ));
    }
    match get(store) {
        Some(owner) if owner.install_id != install_id => Err(adopted_other(&owner)),
        previous => {
            store.set(
                KEY,
                &json!({ "installId": install_id, "lastHandoverAt": now }),
            )?;
            Ok(previous.is_none())
        }
    }
}

/// Calls that only the owner may make (sign-out, release, adopt): Ok when `install_id` is the
/// owner's; 409 adopted-other for another install; 409 signed-out when nobody owns Link yet.
pub fn check(store: &Store, install_id: &str) -> Result<Owner, ApiError> {
    match get(store) {
        Some(owner) if owner.install_id == install_id => Ok(owner),
        Some(owner) => Err(adopted_other(&owner)),
        None => Err(ApiError::coded(
            409,
            "signed-out",
            "RealTimeX hasn't handed its account over to RealTimeX Link",
        )),
    }
}

/// RealTimeX is gone (its uninstaller said so, or it lapsed): Link is its own again.
pub fn release(store: &Store) -> Result<(), ApiError> {
    store.remove(KEY)?;
    Ok(())
}

/// Whether presence lapsed now (and so was released): 30 days without a handover, or no
/// RealTimeX installed and no handover in the last 24 h.
pub fn lapsed(owner: &Owner, now: i64, installed: bool) -> bool {
    let since = now - owner.last_handover_at;
    since > LAPSE_MS || (since > RECENT_MS && !installed)
}

/// Check (at start and every 6 h) whether RealTimeX is still present; release it when not.
/// Answers whether it was released.
pub fn check_presence(store: &Store) -> bool {
    let Some(owner) = get(store) else {
        return false;
    };
    if lapsed(&owner, crate::util::now_ms(), realtimex_installed()) {
        return release(store).is_ok();
    }
    false
}

/// RealTimeX's executable where its installers put it (as Link's window finds it).
pub fn realtimex_installed() -> bool {
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
    candidates.iter().any(|path| path.exists())
}

/// For GET /v1/account: { managedByRealtimex, ownerInstallId }.
pub fn view(store: &Store) -> Value {
    match get(store) {
        Some(owner) => json!({ "managedByRealtimex": true, "ownerInstallId": owner.install_id }),
        None => json!({ "managedByRealtimex": false }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_install_owns_link_and_others_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(!present(&store));
        assert_eq!(
            check(&store, "a").unwrap_err().code.as_deref(),
            Some("signed-out")
        );
        assert_eq!(claim(&store, "install-a", 1).unwrap(), true);
        assert_eq!(claim(&store, "install-a", 2).unwrap(), false);
        assert_eq!(get(&store).unwrap().last_handover_at, 2);
        let other = claim(&store, "install-b", 3).unwrap_err();
        assert_eq!(
            (other.status, other.code.as_deref()),
            (409, Some("adopted-other"))
        );
        assert_eq!(get(&store).unwrap().install_id, "install-a");
        assert!(check(&store, "install-a").is_ok());
        assert_eq!(
            check(&store, "install-b").unwrap_err().code.as_deref(),
            Some("adopted-other")
        );
        release(&store).unwrap();
        assert!(!present(&store));
        assert!(claim(&store, "bad id!", 4).is_err());
    }

    #[test]
    fn presence_lapses_after_30_days_or_without_realtimex_after_a_day() {
        let owner = Owner {
            install_id: "a".into(),
            last_handover_at: 0,
        };
        let hour = 60 * 60 * 1000;
        // Installed: only 30 days without a handover end it.
        assert!(!lapsed(&owner, 29 * DAY_MS, true));
        assert!(lapsed(&owner, 30 * DAY_MS + 1, true));
        // Not installed (dragged to the Trash, or dev:all from source): a recent handover
        // still counts.
        assert!(!lapsed(&owner, 23 * hour, false));
        assert!(lapsed(&owner, 25 * hour, false));
    }
}
