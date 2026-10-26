//! "Shared from this computer" (realtimex-ai-app#2836): what RealTimeX shares through Link, as it
//! pushes it (PUT /v1/shared), for the window's read-only card. Kept, so the card still shows
//! while RealTimeX is closed; RealTimeX sends it again at each start. Link writes the rows' text
//! and badges itself, from these counts and names and whether RealTimeX is running.

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::store::Store;

pub const KEY: &str = "shared";
/// The rows, in the window's order.
pub const KINDS: [&str; 5] = ["artifacts", "webhooks", "widget", "mobile", "messaging"];
const MAX_COUNT: u64 = 1_000_000;
const MAX_NAMES: usize = 20;
const MAX_NAME: usize = 253;

/// PUT /v1/shared's items, checked and reduced to what the card shows: [{ kind, count, names }],
/// one per kind, in KINDS' order. names: the widget's sites, the messaging desktops. A kind left
/// out hides its row.
pub fn parse(body: &Value) -> Result<Value, ApiError> {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("items must be a list"))?;
    let mut kept: Vec<(usize, Value)> = Vec::new();
    for item in items {
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
        let order = KINDS.iter().position(|k| *k == kind).ok_or_else(|| {
            ApiError::bad_request(format!("kind must be one of {}", KINDS.join(", ")))
        })?;
        if kept.iter().any(|(o, _)| *o == order) {
            return Err(ApiError::bad_request(format!("{kind} is listed twice")));
        }
        let count = match item.get("count") {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_u64()
                .filter(|n| *n <= MAX_COUNT)
                .ok_or_else(|| ApiError::bad_request("count must be a whole number"))?,
        };
        let names = match item.get("names") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) if list.len() <= MAX_NAMES => list
                .iter()
                .map(|name| {
                    name.as_str()
                        .map(str::trim)
                        .filter(|s| {
                            !s.is_empty() && s.len() <= MAX_NAME && !s.chars().any(char::is_control)
                        })
                        .map(str::to_string)
                        .ok_or_else(|| ApiError::bad_request("names must be short names"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(ApiError::bad_request(format!(
                    "names must be a list of at most {MAX_NAMES}"
                )))
            }
        };
        kept.push((
            order,
            json!({ "kind": kind, "count": count, "names": names }),
        ));
    }
    kept.sort_by_key(|(order, _)| *order);
    Ok(Value::Array(
        kept.into_iter().map(|(_, item)| item).collect(),
    ))
}

pub fn get(store: &Store) -> Value {
    store
        .get(KEY)
        .filter(Value::is_array)
        .unwrap_or_else(|| json!([]))
}

pub fn set(store: &Store, items: &Value) -> Result<(), ApiError> {
    store.set(KEY, items)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_kept_in_the_cards_order() {
        let items = parse(&json!({
            "exposureId": "x",
            "items": [
                { "kind": "messaging", "count": 2, "names": ["Office Mac", " Studio PC "] },
                { "kind": "artifacts", "count": 3, "detail": "ignored" },
                { "kind": "widget", "count": 1, "names": ["support.example.com"] },
            ],
        }))
        .unwrap();
        assert_eq!(
            items,
            json!([
                { "kind": "artifacts", "count": 3, "names": [] },
                { "kind": "widget", "count": 1, "names": ["support.example.com"] },
                { "kind": "messaging", "count": 2, "names": ["Office Mac", "Studio PC"] },
            ])
        );
        assert_eq!(parse(&json!({ "items": [] })).unwrap(), json!([]));
    }

    #[test]
    fn bad_items_are_refused() {
        for body in [
            json!({}),
            json!({ "items": {} }),
            json!({ "items": [{ "kind": "shares" }] }),
            json!({ "items": [{ "kind": "mobile" }, { "kind": "mobile" }] }),
            json!({ "items": [{ "kind": "mobile", "count": -1 }] }),
            json!({ "items": [{ "kind": "mobile", "count": "2" }] }),
            json!({ "items": [{ "kind": "messaging", "names": [""] }] }),
            json!({ "items": [{ "kind": "messaging", "names": ["a\nb"] }] }),
            json!({ "items": [{ "kind": "widget", "names": vec!["x"; 21] }] }),
        ] {
            assert!(parse(&body).is_err(), "{body}");
        }
    }
}
