//! A controller's actions as rtx-control's primitive ops: every action of a batch is checked before any runs, so a bad batch has no effect.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::backend::Backend;
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
        "key" => {
            let combo = action.get("keys").and_then(Value::as_str).unwrap_or("");
            let keysyms = combo_keysyms(combo).map_err(bad)?;
            for keysym in &keysyms {
                ops.push(json!({ "op": "keysym", "keysym": keysym, "pressed": true }));
            }
            for keysym in keysyms.iter().rev() {
                ops.push(json!({ "op": "keysym", "keysym": keysym, "pressed": false }));
            }
        }
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

pub fn plan_actions(actions: Option<&Value>) -> Result<Vec<Vec<Value>>, ApiError> {
    let Some(list) = actions
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
    else {
        return Err(bad("actions must be a non-empty array"));
    };
    if list.len() > MAX_ACTIONS {
        return Err(bad(format!("At most {MAX_ACTIONS} actions per request")));
    }
    list.iter().map(plan_action).collect()
}

/// Run a batch; resolves with how many actions ran.
pub async fn run_actions(
    backend: &Arc<Backend>,
    actions: Option<&Value>,
) -> Result<usize, ApiError> {
    let plans = plan_actions(actions)?;
    for ops in &plans {
        for step in ops {
            let mut params = step.as_object().cloned().unwrap_or_default();
            let name = params
                .remove("op")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
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
        assert!(plan_actions(Some(&json!([]))).is_err());
        assert!(plan_actions(Some(&json!([{ "type": "move", "x": -1, "y": 0 }]))).is_err());
        assert_eq!(combo_keysyms("f5").unwrap(), vec![0xffc2]);
        assert!(text_keysyms("a\u{7}").is_err());
    }
}
