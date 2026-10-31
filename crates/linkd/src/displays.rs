//! Which display each live view shows (realtimex-link#12). One rtx-control serves every viewer,
//! so the choice is never the helper's: each WebRTC view keeps its own (`router.rs` Peer), an
//! MJPEG stream its own, and an HTTP call names one per request. None is the primary display,
//! exactly as before displays could be chosen.
//!
//! The decisions live here, apart from the router, so they can be tested without a desktop: the
//! display a view shows, what a fresh list means for it (a fallback when its display went away,
//! a `displays` push when the list changed), and whether a batch of actions still aims at it.

use std::time::Duration;

use serde_json::{json, Value};

/// While a live view is open, the display list is read this often to see a display come or go.
pub const POLL: Duration = Duration::from_secs(3);
/// A batch of actions aimed at a display the view no longer shows (409).
pub const DISPLAY_CHANGED: &str = "display-changed";
/// Why a view moved to the primary display: its own was unplugged or turned off.
pub const DISPLAY_REMOVED: &str = "display_removed";
/// Why a view moved to the primary display: its own is still attached but can't be captured now
/// (e.g. a macOS lock screen or login window, design risk 2).
pub const DISPLAY_UNAVAILABLE: &str = "display_unavailable";

/// The displays of a `displays` answer (rtx-control's list, in the picker's order).
pub fn list_of(answer: &Value) -> Vec<Value> {
    answer
        .get("displays")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn id_of(display: &Value) -> Option<&str> {
    display.get("id").and_then(Value::as_str)
}

/// Whether `id` is in the list.
pub fn has(list: &[Value], id: &str) -> bool {
    list.iter().any(|display| id_of(display) == Some(id))
}

/// The primary display's id, else the first one's.
pub fn primary(list: &[Value]) -> Option<String> {
    list.iter()
        .find(|display| display.get("primary") == Some(&Value::Bool(true)))
        .or(list.first())
        .and_then(id_of)
        .map(str::to_string)
}

/// The id of the display a view that asked for `chosen` (None: the primary) shows.
pub fn shown(chosen: Option<&str>, list: &[Value]) -> Option<String> {
    match chosen {
        Some(id) => Some(id.to_string()),
        None => primary(list),
    }
}

/// A view's display after a fresh list.
#[derive(Debug, PartialEq)]
pub struct Checked {
    /// The display the view must show from now on (rtx-control gets `peer_display` with it,
    /// after the view's held keys and buttons are let go): its own went away, or it follows the
    /// primary and the primary changed.
    pub switch_to: Option<String>,
    /// What to push to the viewer, when anything changed for it.
    pub push: Option<Value>,
}

/// What a fresh `list` means for a view that shows `shown` (a concrete id; None only while no
/// list was ever read) and `follows` the primary (it never chose, or fell back), last told it
/// shows `told` among `told_list`. A display that is gone falls back to the primary, and the
/// push says from which display. A view that follows the primary moves when the primary does
/// (a lid closed while an external display took over), the same way: a handover, never a
/// silent change of picture under a held button (review of !15, finding 2).
pub fn check(
    shown: Option<&str>,
    follows: bool,
    told: Option<&str>,
    told_list: &[Value],
    list: &[Value],
) -> Checked {
    let gone = shown.filter(|id| !has(list, id));
    let primary = primary(list);
    let switch_to = if gone.is_some() || follows {
        primary.filter(|primary| Some(primary.as_str()) != shown)
    } else {
        None
    };
    let now = switch_to.as_deref().or(shown);
    if now == told && list == told_list && switch_to.is_none() {
        return Checked {
            switch_to,
            push: None,
        };
    }
    let fallback = gone.or(told.filter(|id| !has(list, id)));
    let push = push(list, now, fallback.map(|from| (from, DISPLAY_REMOVED)));
    Checked {
        switch_to,
        push: Some(push),
    }
}

/// What it means that rtx-control lost the display a view shows (`peer_display_lost`, with
/// its `reason`: `display_removed`, or `display_unavailable` when it is attached but can't be
/// captured now). The view moves to the primary of the fresh `list`, never to the lost display
/// itself; nothing moves when there is no other display.
pub fn lost(shown: Option<&str>, reason: &str, list: &[Value]) -> Checked {
    let Some(from) = shown else {
        return Checked {
            switch_to: None,
            push: None,
        };
    };
    let reason = if reason == DISPLAY_UNAVAILABLE {
        DISPLAY_UNAVAILABLE
    } else {
        DISPLAY_REMOVED
    };
    let others: Vec<Value> = list
        .iter()
        .filter(|display| id_of(display) != Some(from))
        .cloned()
        .collect();
    let Some(target) = primary(&others) else {
        return Checked {
            switch_to: None,
            push: None,
        };
    };
    Checked {
        push: Some(push(&others, Some(&target), Some((from, reason)))),
        switch_to: Some(target),
    }
}

/// Whether an MJPEG stream on a named display moves to the primary after a capture failed with
/// `code` (the display went away, or can't be captured now).
pub fn stream_falls_back(code: Option<&str>) -> bool {
    matches!(code, Some(DISPLAY_NOT_FOUND | DISPLAY_UNAVAILABLE_CODE))
}

/// rtx-control's codes for a display that went away, or can't be captured now.
pub const DISPLAY_NOT_FOUND: &str = "display-not-found";
pub const DISPLAY_UNAVAILABLE_CODE: &str = "display-unavailable";

/// `{type:"displays", displays, display, fallback?}`: the list and what this view shows, with
/// `fallback` only when it moved because the display it showed (`from`) went away, and why.
pub fn push(list: &[Value], shown: Option<&str>, fallback: Option<(&str, &str)>) -> Value {
    let mut message = json!({ "type": "displays", "displays": list, "display": shown });
    if let Some((from, reason)) = fallback {
        message["fallback"] = json!({ "from": from, "reason": reason });
    }
    message
}

/// Whether a batch that says it aims at `aimed` (none: old apps, whatever is shown) may run on
/// a view that shows `shown`: else 409 `display-changed` with the display shown, so a late click
/// meant for another screen never lands on this one.
pub fn batch_allowed(aimed: Option<&str>, shown: Option<&str>) -> Result<(), Value> {
    match aimed {
        Some(aimed) if Some(aimed) != shown => Err(json!({
            "ok": false,
            "status": 409,
            "code": DISPLAY_CHANGED,
            "error": "The screen shown changed; look again before acting",
            "display": shown,
        })),
        _ => Ok(()),
    }
}

/// The display a request body or query names (`display`), if any.
pub fn named(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, primary: bool, x: i32) -> Value {
        json!({ "id": id, "name": "", "primary": primary, "x": x, "y": 0,
            "width": 1920, "height": 1080, "scale": 1.0 })
    }

    fn two() -> Vec<Value> {
        vec![display("1", true, 0), display("2", false, 1920)]
    }

    /// Regression against 0.4.1: a view that never chose shows the primary, and an unchanged
    /// list pushes nothing.
    #[test]
    fn a_view_without_a_choice_shows_the_primary_and_hears_nothing_new() {
        let list = two();
        assert_eq!(shown(None, &list).as_deref(), Some("1"));
        assert_eq!(
            check(Some("1"), true, Some("1"), &list, &list),
            Checked { switch_to: None, push: None }
        );
        // A view that never had a list (enumeration failed at its start) is given the primary.
        assert_eq!(check(None, true, None, &[], &list).switch_to.as_deref(), Some("1"));
    }

    #[test]
    fn a_chosen_display_that_is_unplugged_falls_back_to_the_primary() {
        let before = two();
        let after = vec![display("1", true, 0)];
        let checked = check(Some("2"), false, Some("2"), &before, &after);
        assert_eq!(checked.switch_to.as_deref(), Some("1"));
        let push = checked.push.unwrap();
        assert_eq!(push["type"], "displays");
        assert_eq!(push["display"], "1");
        assert_eq!(push["displays"], json!(after));
        assert_eq!(push["fallback"], json!({ "from": "2", "reason": "display_removed" }));
    }

    #[test]
    fn a_list_that_changed_around_the_view_is_pushed_without_a_fallback() {
        let before = two();
        let after = vec![display("1", true, 0), display("2", false, 1920), display("3", false, 3840)];
        let checked = check(Some("2"), false, Some("2"), &before, &after);
        assert_eq!(checked.switch_to, None);
        let push = checked.push.unwrap();
        assert_eq!(push["display"], "2");
        assert!(push.get("fallback").is_none());
    }

    /// Design §3.4 and review of !15, finding 2: a lid closed while an external display became
    /// the primary. A view that follows the primary is handed over (switch_to: its held input
    /// is let go first), with the reason.
    #[test]
    fn a_view_on_the_primary_is_handed_to_a_new_primary_and_says_why() {
        let before = two();
        let after = vec![display("2", true, 0)];
        let checked = check(Some("1"), true, Some("1"), &before, &after);
        assert_eq!(checked.switch_to.as_deref(), Some("2"));
        let push = checked.push.unwrap();
        assert_eq!(push["display"], "2");
        assert_eq!(push["fallback"], json!({ "from": "1", "reason": "display_removed" }));
        // The primary moved with both attached: a handover without a fallback.
        let swapped = vec![display("1", false, 0), display("2", true, 1920)];
        let checked = check(Some("1"), true, Some("1"), &before, &swapped);
        assert_eq!(checked.switch_to.as_deref(), Some("2"));
        assert!(checked.push.unwrap().get("fallback").is_none());
        // A view that chose display 1 stays on it.
        assert_eq!(check(Some("1"), false, Some("1"), &before, &swapped).switch_to, None);
    }

    #[test]
    fn a_display_that_cannot_be_captured_falls_back_with_its_reason() {
        let list = two();
        // Still listed, but rtx-control can't capture it now: never moved onto itself, and the
        // list pushed leaves it out.
        let checked = lost(Some("2"), DISPLAY_UNAVAILABLE, &list);
        assert_eq!(checked.switch_to.as_deref(), Some("1"));
        let push = checked.push.unwrap();
        assert_eq!(push["display"], "1");
        assert_eq!(push["displays"], json!([display("1", true, 0)]));
        assert_eq!(push["fallback"], json!({ "from": "2", "reason": "display_unavailable" }));
        // Gone.
        let push = lost(Some("3"), DISPLAY_REMOVED, &list).push.unwrap();
        assert_eq!(push["fallback"]["reason"], "display_removed");
        // The primary itself, with nothing else: nowhere to go.
        assert_eq!(
            lost(Some("1"), DISPLAY_UNAVAILABLE, &[display("1", true, 0)]),
            Checked { switch_to: None, push: None }
        );
        assert_eq!(lost(None, DISPLAY_REMOVED, &list), Checked { switch_to: None, push: None });
        // An MJPEG stream moves on for either code, and only those.
        assert!(stream_falls_back(Some("display-not-found")));
        assert!(stream_falls_back(Some("display-unavailable")));
        assert!(!stream_falls_back(Some("input-blocked")));
        assert!(!stream_falls_back(None));
    }

    #[test]
    fn a_batch_aimed_at_another_display_is_refused() {
        assert_eq!(batch_allowed(None, Some("1")), Ok(()));
        assert_eq!(batch_allowed(Some("1"), Some("1")), Ok(()));
        let refused = batch_allowed(Some("2"), Some("1")).unwrap_err();
        assert_eq!(refused["status"], 409);
        assert_eq!(refused["code"], "display-changed");
        assert_eq!(refused["display"], "1");
        assert_eq!(refused["ok"], false);
    }

    #[test]
    fn primary_and_names() {
        assert_eq!(primary(&[display("a", false, 0), display("b", false, 5)]).as_deref(), Some("a"));
        assert_eq!(primary(&[]), None);
        assert_eq!(named(Some(&json!("2"))).as_deref(), Some("2"));
        assert_eq!(named(Some(&json!(""))), None);
        assert_eq!(named(Some(&json!(2))), None);
        assert_eq!(named(None), None);
        assert_eq!(list_of(&json!({ "displays": two() })), two());
        assert!(list_of(&json!({})).is_empty());
    }
}
