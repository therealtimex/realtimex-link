//! A controller's actions as rtx-control's primitive ops: every action of a batch is checked before any runs, so a bad batch has no effect.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::backend::Backend;
use crate::codes::physical_key;
use crate::error::ApiError;

pub const MAX_ACTIONS: usize = 50;
const MAX_WAIT_MS: i64 = 10_000;
const MAX_TEXT_LENGTH: usize = 2000;

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::bad_request(message)
}

/// X11 keysyms: the key vocabulary rtx-control accepts on every desktop.
fn named_keysym(name: &str) -> Option<u32> {
    Some(match name {
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "space" => 0x20,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "ctrl" | "control" => 0xffe3,
        "shift" => 0xffe1,
        "alt" => 0xffe9,
        "super" | "meta" | "cmd" => 0xffeb,
        "menu" => 0xff67,
        "printscreen" => 0xff61,
        _ => {
            let n: u32 = name.strip_prefix('f')?.parse().ok()?;
            if (1..=12).contains(&n) {
                0xffbe + n - 1
            } else {
                return None;
            }
        }
    })
}

fn char_keysym(c: char) -> Option<u32> {
    let code = c as u32;
    match c {
        '\n' => Some(0xff0d),
        '\t' => Some(0xff09),
        _ if (0x20..=0x7e).contains(&code) || (0xa0..=0xff).contains(&code) => Some(code),
        _ if code < 0x20 || code == 0x7f => None,
        _ => Some(0x0100_0000 + code),
    }
}

/// Keysyms for each character; refuses control characters. Length as JavaScript counts it.
pub fn text_keysyms(text: &str) -> Result<Vec<u32>, String> {
    if text.is_empty() {
        return Err("text must be a non-empty string".into());
    }
    if text.encode_utf16().count() > MAX_TEXT_LENGTH {
        return Err(format!("text is longer than {MAX_TEXT_LENGTH} characters"));
    }
    text.chars()
        .map(|c| {
            char_keysym(c)
                .ok_or_else(|| "text contains an unsupported control character".to_string())
        })
        .collect()
}

/// "ctrl+shift+t" or "Enter" as keysyms, modifiers first.
pub fn combo_keysyms(combo: &str) -> Result<Vec<u32>, String> {
    if combo.trim().is_empty() {
        return Err("keys must be a non-empty string such as ctrl+l".into());
    }
    combo
        .split('+')
        .map(|part| {
            let name = part.trim();
            if let Some(keysym) = named_keysym(&name.to_lowercase()) {
                return Ok(keysym);
            }
            let mut chars = name.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if let Some(keysym) = c.to_lowercase().next().and_then(char_keysym) {
                    return Ok(keysym);
                }
            }
            Err(format!("Unknown key: {name}"))
        })
        .collect()
}

/// JavaScript's Number(value): numbers, numeric strings, booleans.
fn number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) if s.trim().is_empty() => 0.0,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(f64::NAN),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Null) => 0.0,
        Some(_) => f64::NAN,
    }
}

fn point(action: &Value, kind: &str) -> Result<Map<String, Value>, ApiError> {
    let x = number(action.get("x"));
    let y = number(action.get("y"));
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return Err(bad(format!(
            "{kind} needs non-negative x and y in screenshot pixels"
        )));
    }
    let mut fields = Map::new();
    fields.insert("x".into(), json!(x));
    fields.insert("y".into(), json!(y));
    if action.get("width").is_none() && action.get("height").is_none() {
        return Ok(fields);
    }
    let width = number(action.get("width"));
    let height = number(action.get("height"));
    let whole = |n: f64| n.is_finite() && n.fract() == 0.0 && n > 0.0;
    if !whole(width) || !whole(height) {
        return Err(bad(format!(
            "{kind} width and height must be positive integers"
        )));
    }
    fields.insert("spaceWidth".into(), json!(width as i64));
    fields.insert("spaceHeight".into(), json!(height as i64));
    Ok(fields)
}

fn integer(value: Option<&Value>, name: &str, min: i64, max: i64) -> Result<i64, ApiError> {
    let n = if value.is_none() { 0.0 } else { number(value) };
    if !n.is_finite() || n.fract() != 0.0 || n < min as f64 || n > max as f64 {
        return Err(bad(format!(
            "{name} must be an integer between {min} and {max}"
        )));
    }
    Ok(n as i64)
}

fn op(name: &str, mut fields: Map<String, Value>) -> Value {
    fields.insert("op".into(), json!(name));
    Value::Object(fields)
}

fn has_point(action: &Value) -> bool {
    action.get("x").is_some() || action.get("y").is_some()
}

fn button_of(action: &Value) -> Result<String, ApiError> {
    let button = action
        .get("button")
        .and_then(Value::as_str)
        .filter(|b| !b.is_empty())
        .unwrap_or("left");
    if !["left", "right", "middle"].contains(&button) {
        return Err(bad(format!("Unknown button: {button}")));
    }
    Ok(button.to_string())
}

fn button(button: &str, pressed: bool) -> Value {
    json!({ "op": "button", "button": button, "pressed": pressed })
}

/// One action as primitive ops.
pub fn plan_action(action: &Value) -> Result<Vec<Value>, ApiError> {
    if !action.is_object() {
        return Err(bad("Each action must be an object"));
    }
    let kind = action.get("type").and_then(Value::as_str).unwrap_or("");
    let mut ops = Vec::new();
    match kind {
        "move" => ops.push(op("move", point(action, kind)?)),
        "click" => {
            let name = button_of(action)?;
            if has_point(action) {
                ops.push(op("move", point(action, kind)?));
            }
            let clicks = if action.get("double").is_some_and(truthy) {
                2
            } else {
                1
            };
            for _ in 0..clicks {
                ops.push(button(&name, true));
                ops.push(button(&name, false));
            }
        }
        // A drag (#2736): press at one point, moves, release at another.
        "down" | "up" => {
            let name = button_of(action)?;
            if has_point(action) {
                ops.push(op("move", point(action, kind)?));
            }
            ops.push(button(&name, kind == "down"));
        }
        "type" => {
            let text = action.get("text").and_then(Value::as_str).unwrap_or("");
            let keysyms = text_keysyms(text).map_err(bad)?;
            match action.get("method") {
                Some(Value::String(method)) if method == "keys" => {
                    for keysym in keysyms {
                        ops.push(json!({ "op": "keysym", "keysym": keysym, "pressed": true }));
                        ops.push(json!({ "op": "keysym", "keysym": keysym, "pressed": false }));
                    }
                }
                // Keystrokes pass through the input method (Telex turns "test" into "tét"): text
                // is pasted by default.
                None => ops.push(paste_text(action, text)),
                Some(Value::String(method)) if method == "paste" => {
                    ops.push(paste_text(action, text))
                }
                _ => return Err(bad("type method must be \"paste\" or \"keys\"")),
            }
        }
        "key" => match (action.get("code"), action.get("keys")) {
            (Some(code), None) => ops.extend(physical_key_op(action, code)?),
            (Some(_), Some(_)) => return Err(bad("A key action has code or keys, not both")),
            (None, _) => {
                let combo = action.get("keys").and_then(Value::as_str).unwrap_or("");
                let keysyms = combo_keysyms(combo).map_err(bad)?;
                for keysym in &keysyms {
                    ops.push(json!({ "op": "keysym", "keysym": keysym, "pressed": true }));
                }
                for keysym in keysyms.iter().rev() {
                    ops.push(json!({ "op": "keysym", "keysym": keysym, "pressed": false }));
                }
            }
        },
        "scroll" => {
            let dy = integer(action.get("dy"), "dy", -50, 50)?;
            let dx = integer(action.get("dx"), "dx", -50, 50)?;
            if dy == 0 && dx == 0 {
                return Err(bad("scroll needs dy or dx"));
            }
            if has_point(action) {
                ops.push(op("move", point(action, kind)?));
            }
            ops.push(json!({ "op": "scroll", "dy": dy, "dx": dx }));
        }
        "wait" => ops
            .push(json!({ "op": "wait", "ms": integer(action.get("ms"), "ms", 0, MAX_WAIT_MS)? })),
        _ => {
            let shown = action.get("type").map_or("undefined".to_string(), |t| {
                t.as_str().map_or(t.to_string(), String::from)
            });
            return Err(bad(format!("Unknown action type: {shown}")));
        }
    }
    Ok(ops)
}

/// A physical key (realtimex-link#7): `{type:"key", code, down, repeat?}`, a KeyboardEvent.code
/// pressed by position. A code with no key here (`Lang1`, `Fn`) is skipped, not refused: a
/// refusal would clear the viewer's queue and end its Control (#2909). rtx-control treats a down
/// for a held key as its repeat, so `repeat` is only checked.
fn physical_key_op(action: &Value, code: &Value) -> Result<Option<Value>, ApiError> {
    let code = code
        .as_str()
        .ok_or_else(|| bad("code must be a string such as KeyA"))?;
    let down = action
        .get("down")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad("down must be true or false"))?;
    if action
        .get("repeat")
        .is_some_and(|repeat| !repeat.is_boolean())
    {
        return Err(bad("repeat must be true or false"));
    }
    Ok(physical_key(code).map(|key| json!({ "op": "key", "code": key.code, "pressed": down })))
}

/// `{type:"key", code}`: a physical key, as opposed to a `keys` combo.
pub fn is_physical_key(action: &Value) -> bool {
    action.get("type").and_then(Value::as_str) == Some("key") && action.get("code").is_some()
}

/// A pointer move names the display it lands on; nothing else does, and without one nothing
/// changes from before (the primary).
fn on_display(op: &str, params: &mut Map<String, Value>, display: Option<&str>) {
    if let (Some(display), "move") = (display, op) {
        params.insert("display".into(), json!(display));
    }
}

fn paste_text(action: &Value, text: &str) -> Value {
    let terminal = action.get("terminal") == Some(&Value::Bool(true));
    json!({ "op": "paste_text", "text": text, "terminal": terminal })
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// A batch's ops. Physical keys come only from a live view (`live_view`): over HTTP nothing
/// would release a key left held (#2909), so the agent skill keeps `keys` and `type`.
pub fn plan_actions(actions: Option<&Value>, live_view: bool) -> Result<Vec<Vec<Value>>, ApiError> {
    let Some(list) = actions
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
    else {
        return Err(bad("actions must be a non-empty array"));
    };
    if list.len() > MAX_ACTIONS {
        return Err(bad(format!("At most {MAX_ACTIONS} actions per request")));
    }
    list.iter()
        .map(|action| {
            if !live_view && is_physical_key(action) {
                return Err(bad(
                    "Physical keys (code) are for a live view only; send keys such as ctrl+l",
                ));
            }
            plan_action(action)
        })
        .collect()
}

/// Run a batch on `display` (None: the primary; realtimex-link#12): its pointer moves land
/// there. Resolves with how many actions ran.
pub async fn run_actions(
    backend: &Arc<Backend>,
    actions: Option<&Value>,
    live_view: bool,
    display: Option<&str>,
) -> Result<usize, ApiError> {
    let plans = plan_actions(actions, live_view)?;
    for ops in &plans {
        for step in ops {
            let mut params = step.as_object().cloned().unwrap_or_default();
            let name = params
                .remove("op")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            on_display(&name, &mut params, display);
            if name == "wait" {
                let ms = params.get("ms").and_then(Value::as_u64).unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
            } else {
                backend.call(&name, Value::Object(params)).await?;
            }
        }
    }
    Ok(plans.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_like_the_javascript_server() {
        let click =
            plan_action(&json!({ "type": "click", "x": 10, "y": 20, "double": true })).unwrap();
        assert_eq!(click.len(), 5);
        assert_eq!(click[0], json!({ "op": "move", "x": 10.0, "y": 20.0 }));
        let key = plan_action(&json!({ "type": "key", "keys": "ctrl+shift+T" })).unwrap();
        let syms: Vec<_> = key
            .iter()
            .map(|o| {
                (
                    o["keysym"].as_u64().unwrap(),
                    o["pressed"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            syms,
            vec![
                (0xffe3, true),
                (0xffe1, true),
                (0x74, true),
                (0x74, false),
                (0xffe1, false),
                (0xffe3, false)
            ]
        );
        assert_eq!(
            plan_action(&json!({ "type": "type", "text": "tét" })).unwrap(),
            vec![json!({ "op": "paste_text", "text": "tét", "terminal": false })]
        );
        assert_eq!(
            plan_action(&json!({ "type": "type", "text": "a", "method": "keys" }))
                .unwrap()
                .len(),
            2
        );
        let scaled =
            plan_action(&json!({ "type": "move", "x": 1, "y": 2, "width": 640, "height": 400 }))
                .unwrap();
        assert_eq!(scaled[0]["spaceWidth"], 640);
        assert_eq!(
            plan_action(&json!({ "type": "type", "text": "a", "method": "x" }))
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(
            plan_action(&json!({ "type": "scroll" }))
                .unwrap_err()
                .message,
            "scroll needs dy or dx"
        );
        assert_eq!(
            plan_action(&json!({ "type": "zap" })).unwrap_err().message,
            "Unknown action type: zap"
        );
        assert!(plan_actions(Some(&json!([])), false).is_err());
        assert!(plan_actions(Some(&json!([{ "type": "move", "x": -1, "y": 0 }])), false).is_err());
        assert_eq!(combo_keysyms("f5").unwrap(), vec![0xffc2]);
        assert!(text_keysyms("a\u{7}").is_err());
    }

    fn error(action: Value) -> String {
        plan_action(&action).unwrap_err().message
    }

    #[test]
    fn a_physical_key_is_one_key_op_by_code() {
        assert_eq!(
            plan_action(&json!({ "type": "key", "code": "ShiftLeft", "down": true })).unwrap(),
            vec![json!({ "op": "key", "code": "ShiftLeft", "pressed": true })]
        );
        assert_eq!(
            plan_action(&json!({ "type": "key", "code": "KeyA", "down": true, "repeat": true }))
                .unwrap(),
            vec![json!({ "op": "key", "code": "KeyA", "pressed": true })]
        );
        assert_eq!(
            plan_action(&json!({ "type": "key", "code": "KeyA", "down": false })).unwrap(),
            vec![json!({ "op": "key", "code": "KeyA", "pressed": false })]
        );
    }

    /// #2909: a refusal would clear the viewer's queue and turn its Control off.
    #[test]
    fn a_well_formed_code_link_has_no_key_for_is_skipped() {
        for code in ["Lang1", "Fn", "MediaPlayPause", "Unidentified", ""] {
            let ops = plan_action(&json!({ "type": "key", "code": code, "down": true })).unwrap();
            assert!(ops.is_empty(), "{code}");
        }
    }

    #[test]
    fn a_malformed_physical_key_is_refused() {
        assert_eq!(
            error(json!({ "type": "key", "code": 65, "down": true })),
            "code must be a string such as KeyA"
        );
        assert_eq!(
            error(json!({ "type": "key", "code": "KeyA" })),
            "down must be true or false"
        );
        assert_eq!(
            error(json!({ "type": "key", "code": "KeyA", "down": 1 })),
            "down must be true or false"
        );
        assert_eq!(
            error(json!({ "type": "key", "code": "KeyA", "down": true, "repeat": "yes" })),
            "repeat must be true or false"
        );
        assert_eq!(
            error(json!({ "type": "key", "code": "KeyA", "keys": "ctrl+l", "down": true })),
            "A key action has code or keys, not both"
        );
        // Neither: the combo's own message, as before.
        assert_eq!(
            error(json!({ "type": "key" })),
            "keys must be a non-empty string such as ctrl+l"
        );
    }

    #[test]
    fn the_old_combo_is_unchanged() {
        assert_eq!(
            plan_action(&json!({ "type": "key", "keys": "ctrl+l" })).unwrap(),
            vec![
                json!({ "op": "keysym", "keysym": 0xffe3, "pressed": true }),
                json!({ "op": "keysym", "keysym": 0x6c, "pressed": true }),
                json!({ "op": "keysym", "keysym": 0x6c, "pressed": false }),
                json!({ "op": "keysym", "keysym": 0xffe3, "pressed": false }),
            ]
        );
    }

    /// Downs and ups keep their order in a batch, between moves and clicks.
    #[test]
    fn a_batch_keeps_its_order() {
        let batch = json!([
            { "type": "key", "code": "ShiftLeft", "down": true },
            { "type": "move", "x": 5, "y": 6 },
            { "type": "key", "code": "KeyA", "down": true },
            { "type": "key", "code": "KeyA", "down": false },
            { "type": "click", "x": 5, "y": 6 },
            { "type": "key", "code": "ShiftLeft", "down": false },
        ]);
        let ops: Vec<String> = plan_actions(Some(&batch), true)
            .unwrap()
            .into_iter()
            .flatten()
            .map(|op| match op["op"].as_str().unwrap() {
                "key" => format!(
                    "{}{}",
                    op["code"].as_str().unwrap(),
                    if op["pressed"] == true { "↓" } else { "↑" }
                ),
                other => other.to_string(),
            })
            .collect();
        assert_eq!(
            ops,
            vec![
                "ShiftLeft↓",
                "move",
                "KeyA↓",
                "KeyA↑",
                "move",
                "button",
                "button",
                "ShiftLeft↑"
            ]
        );
    }

    #[test]
    fn only_moves_name_the_display_and_none_is_as_before() {
        let mut ops: Vec<(String, Map<String, Value>)> =
            plan_action(&json!({ "type": "click", "x": 1, "y": 2 }))
                .unwrap()
                .into_iter()
                .map(|op| {
                    let mut fields = op.as_object().cloned().unwrap();
                    let name = fields.remove("op").unwrap().as_str().unwrap().to_string();
                    (name, fields)
                })
                .collect();
        let before = ops.clone();
        for (name, params) in &mut ops {
            on_display(name, params, None);
        }
        assert_eq!(ops, before, "no display: the ops are what they were");
        for (name, params) in &mut ops {
            on_display(name, params, Some("2"));
        }
        assert_eq!(ops[0].1["display"], "2");
        assert!(ops[1..].iter().all(|(_, params)| params.get("display").is_none()));
    }

    #[test]
    fn physical_keys_are_for_a_live_view_only() {
        let batch = json!([{ "type": "key", "code": "KeyA", "down": true }]);
        let refused = plan_actions(Some(&batch), false).unwrap_err();
        assert_eq!(refused.status, 400);
        assert!(plan_actions(Some(&batch), true).is_ok());
        // The combo still works over HTTP.
        assert!(plan_actions(Some(&json!([{ "type": "key", "keys": "ctrl+l" }])), false).is_ok());
    }
}
