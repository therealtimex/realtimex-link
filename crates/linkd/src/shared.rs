//! "Shared from this computer" (realtimex-ai-app#2836): what RealTimeX shares through Link, as it
//! pushes it (PUT /v1/shared), for the window's read-only card. Kept, so the card still shows
//! while RealTimeX is closed; RealTimeX sends it again at each start. Link writes the rows' text
//! and badges itself, from these counts and whether RealTimeX is running (relay::origin).

use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::store::Store;

pub const KEY: &str = "shared";
const MAX_COUNT: u64 = 1_000_000;
const MAX_NAMES: usize = 20;
const MAX_NAME: usize = 253;

/// A count field: a whole number, or absent.
fn count(section: &Value, name: &str) -> Result<Option<u64>, ApiError> {
    match section.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|n| *n <= MAX_COUNT)
            .map(Some)
            .ok_or_else(|| ApiError::bad_request(format!("{name} must be a count"))),
    }
}

/// A list of names (sites, desktops): short strings, a few of them.
fn names(section: &Value, name: &str) -> Result<Option<Vec<String>>, ApiError> {
    match section.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(list)) if list.len() <= MAX_NAMES => list
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|s| {
                        !s.is_empty() && s.len() <= MAX_NAME && !s.chars().any(char::is_control)
                    })
                    .map(str::to_string)
                    .ok_or_else(|| ApiError::bad_request(format!("{name} must be a list of names")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ApiError::bad_request(format!(
            "{name} must be a list of at most {MAX_NAMES} names"
        ))),
    }
}

/// PUT /v1/shared's body, checked and reduced to what the card shows: { account?, artifacts?:
/// { published }, webhooks?: { endpoints }, embed?: { widgets, sites }, mobile?: { phones },
/// peers?: { desktops } }. A section left out hides its row.
pub fn parse(body: &Value) -> Result<Value, ApiError> {
    let mut shared = Map::new();
    match body.get("account") {
        None | Some(Value::Null) => {}
        Some(Value::String(account))
            if !account.trim().is_empty()
                && account.len() <= 320
                && !account.chars().any(char::is_control) =>
        {
            shared.insert("account".into(), json!(account.trim()));
        }
        Some(_) => return Err(ApiError::bad_request("account must be an email address")),
    }
    let sections: [(&str, &[&str], &[&str]); 5] = [
        ("artifacts", &["published"], &[]),
        ("webhooks", &["endpoints"], &[]),
        ("embed", &["widgets"], &["sites"]),
        ("mobile", &["phones"], &[]),
        ("peers", &[], &["desktops"]),
    ];
    for (name, counts, lists) in sections {
        let section = match body.get(name) {
            None | Some(Value::Null) => continue,
            Some(section @ Value::Object(_)) => section,
            Some(_) => return Err(ApiError::bad_request(format!("{name} must be an object"))),
        };
        let mut kept = Map::new();
        for field in counts {
            kept.insert((*field).into(), json!(count(section, field)?.unwrap_or(0)));
        }
        for field in lists {
            kept.insert(
                (*field).into(),
                json!(names(section, field)?.unwrap_or_default()),
            );
        }
        shared.insert(name.into(), Value::Object(kept));
    }
    Ok(Value::Object(shared))
}

pub fn get(store: &Store) -> Value {
    store.get(KEY).unwrap_or_else(|| json!({}))
}

pub fn set(store: &Store, shared: &Value) -> Result<(), ApiError> {
    store.set(KEY, shared)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_cards_fields_are_kept() {
        let shared = parse(&json!({
            "account": " phuong@realtimex.ai ",
            "artifacts": { "published": 3, "secret": "x" },
            "embed": { "widgets": 1, "sites": ["support.example.com"] },
            "peers": { "desktops": ["Office Mac", "Studio PC"] },
            "other": { "n": 1 },
        }))
        .unwrap();
        assert_eq!(
            shared,
            json!({
                "account": "phuong@realtimex.ai",
                "artifacts": { "published": 3 },
                "embed": { "widgets": 1, "sites": ["support.example.com"] },
                "peers": { "desktops": ["Office Mac", "Studio PC"] },
            })
        );
        assert_eq!(parse(&json!({})).unwrap(), json!({}));
    }

    #[test]
    fn bad_fields_are_refused() {
        for body in [
            json!({ "account": 3 }),
            json!({ "artifacts": [] }),
            json!({ "artifacts": { "published": -1 } }),
            json!({ "webhooks": { "endpoints": "2" } }),
            json!({ "peers": { "desktops": [""] } }),
            json!({ "peers": { "desktops": ["a\nb"] } }),
            json!({ "embed": { "sites": vec!["x"; 21] } }),
        ] {
            assert!(parse(&body).is_err(), "{body}");
        }
    }
}
