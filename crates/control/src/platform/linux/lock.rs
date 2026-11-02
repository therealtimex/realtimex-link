//! Whether this desktop's screen is locked (realtimex-ai-app#3055, §2.2). agent-desktop reads
//! AT-SPI straight from the apps, which works behind a lock screen too, so the accessibility
//! calls and the window list are refused while it is locked, and when Link can't tell.
//!
//! Asked over D-Bus, each with a short timeout:
//! - GNOME (Wayland and X11): `org.gnome.ScreenSaver.GetActive` on the session bus;
//! - every desktop that tells logind (GNOME, KDE and most lock screens): the `LockedHint` of the
//!   user's graphical session (`User.Display`). rtx-control may run in another session (an SSH
//!   or service start), so its own session's hint is asked only when the user has no graphical
//!   one;
//! - others: `org.freedesktop.ScreenSaver.GetActive`, when GNOME's isn't there (GNOME answers
//!   it with NotSupported).
//!
//! Locked when any of them says so; unknown when none answers.
//!
//! Seen on T30 (GNOME 50, headless Wayland): unlocked, `GetActive` false and `LockedHint` false;
//! locked, both true. Right after `org.gnome.ScreenSaver.Lock`, `LockedHint` was already true
//! while `GetActive` still said false, so neither is enough alone.

use std::sync::Mutex;
use std::time::Duration;

use zbus::blocking::{connection, Connection};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// How long one D-Bus question may take: a refusal is better than a stalled call.
const ASK_TIMEOUT: Duration = Duration::from_millis(1500);
const LOGIND: &str = "org.freedesktop.login1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

static SESSION_BUS: Mutex<Option<Connection>> = Mutex::new(None);
static SYSTEM_BUS: Mutex<Option<Connection>> = Mutex::new(None);

/// Locked (`Some(true)`), not locked (`Some(false)`), or unknown (`None`): no answer at all.
pub fn lock_state() -> Option<bool> {
    let gnome = ask(&SESSION_BUS, true, gnome_active);
    let logind = ask(&SYSTEM_BUS, false, logind_locked);
    combine(gnome, logind, || ask(&SESSION_BUS, true, freedesktop_active))
}

/// Known to be locked (the screenshot's `locked`, which only says what the picture shows).
pub fn screen_locked() -> bool {
    lock_state() == Some(true)
}

/// Locked if any says so; else not locked if any answered; else unknown. The freedesktop
/// screensaver is asked only when GNOME's didn't answer.
fn combine(gnome: Option<bool>, logind: Option<bool>, freedesktop: impl FnOnce() -> Option<bool>) -> Option<bool> {
    let other = if gnome.is_none() { freedesktop() } else { None };
    let answers = [gnome, logind, other];
    if answers.contains(&Some(true)) {
        return Some(true);
    }
    answers.into_iter().flatten().next()
}

/// One question on a kept connection; the connection is dropped and made again after a failure.
fn ask(bus: &Mutex<Option<Connection>>, session: bool, question: fn(&Connection) -> zbus::Result<bool>) -> Option<bool> {
    let mut slot = bus.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        let builder = if session { connection::Builder::session() } else { connection::Builder::system() };
        *slot = builder.ok()?.method_timeout(ASK_TIMEOUT).build().ok();
    }
    let answer = question(slot.as_ref()?);
    if answer.is_err() {
        // A missing service is an answer to keep the connection for; anything else, start over.
        if !matches!(answer, Err(zbus::Error::MethodError(..)) | Err(zbus::Error::FDO(..))) {
            *slot = None;
        }
    }
    answer.ok()
}

fn gnome_active(bus: &Connection) -> zbus::Result<bool> {
    let reply = bus.call_method(
        Some("org.gnome.ScreenSaver"),
        "/org/gnome/ScreenSaver",
        Some("org.gnome.ScreenSaver"),
        "GetActive",
        &(),
    )?;
    reply.body().deserialize::<bool>()
}

fn freedesktop_active(bus: &Connection) -> zbus::Result<bool> {
    let reply = bus.call_method(
        Some("org.freedesktop.ScreenSaver"),
        "/org/freedesktop/ScreenSaver",
        Some("org.freedesktop.ScreenSaver"),
        "GetActive",
        &(),
    )?;
    reply.body().deserialize::<bool>()
}

/// `LockedHint` of the user's graphical session, else of rtx-control's own.
fn logind_locked(bus: &Connection) -> zbus::Result<bool> {
    let uid = unsafe { libc::getuid() };
    let user = bus
        .call_method(Some(LOGIND), "/org/freedesktop/login1", Some("org.freedesktop.login1.Manager"), "GetUser", &(uid,))?
        .body()
        .deserialize::<OwnedObjectPath>()?;
    let display = property(bus, user.as_str(), "org.freedesktop.login1.User", "Display")?;
    let session = graphical_session(&display).unwrap_or_else(|| "/org/freedesktop/login1/session/auto".to_string());
    let hint = property(bus, &session, "org.freedesktop.login1.Session", "LockedHint")?;
    match &*hint {
        Value::Bool(locked) => Ok(*locked),
        _ => Err(zbus::Error::Failure("LockedHint is not a boolean".into())),
    }
}

/// The session path in `User.Display` (`(so)`); none when it is `/` (no graphical session).
fn graphical_session(display: &Value<'_>) -> Option<String> {
    let Value::Structure(display) = display else {
        return None;
    };
    match display.fields().get(1) {
        Some(Value::ObjectPath(path)) if path.as_str() != "/" => Some(path.as_str().to_string()),
        _ => None,
    }
}

fn property(bus: &Connection, path: &str, interface: &str, name: &str) -> zbus::Result<OwnedValue> {
    bus.call_method(Some(LOGIND), path, Some(PROPERTIES), "Get", &(interface, name))?
        .body()
        .deserialize::<OwnedValue>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, StructureBuilder};

    #[test]
    fn any_locked_answer_wins_and_no_answer_is_unknown() {
        fn never() -> Option<bool> {
            panic!("asked the freedesktop screensaver although GNOME answered")
        }
        assert_eq!(combine(Some(false), Some(false), never), Some(false));
        // T30 right after org.gnome.ScreenSaver.Lock: logind first.
        assert_eq!(combine(Some(false), Some(true), never), Some(true));
        assert_eq!(combine(Some(true), Some(false), never), Some(true));
        assert_eq!(combine(Some(true), None, never), Some(true));
        // Not GNOME: logind, then the freedesktop screensaver.
        assert_eq!(combine(None, Some(false), || None), Some(false));
        assert_eq!(combine(None, Some(false), || Some(true)), Some(true));
        assert_eq!(combine(None, None, || Some(false)), Some(false));
        // Nobody answered: unknown, which the accessibility calls refuse.
        assert_eq!(combine(None, None, || None), None);
    }

    #[test]
    fn the_graphical_session_comes_from_user_display() {
        let display = |id: &str, path: &'static str| {
            Value::Structure(
                StructureBuilder::new()
                    .add_field(id.to_string())
                    .add_field(ObjectPath::try_from(path).unwrap())
                    .build()
                    .unwrap(),
            )
        };
        assert_eq!(
            graphical_session(&display("8", "/org/freedesktop/login1/session/_38")).as_deref(),
            Some("/org/freedesktop/login1/session/_38")
        );
        assert_eq!(graphical_session(&display("", "/")), None);
        assert_eq!(graphical_session(&Value::Bool(true)), None);
    }
}
