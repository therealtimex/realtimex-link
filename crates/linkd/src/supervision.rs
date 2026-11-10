//! "Reopen RealTimeX if it crashes" (realtimex-ai-app#3241 Q5): the setting, kept in the store,
//! and what the tray's watcher (link/src/supervise.rs) last decided, read from the data folder.
//! The watcher runs in rtx-link, never here: under unattended mode this daemon is outside the
//! person's GUI session. This daemon only keeps the setting and reports the state. No route here
//! names a program or arguments.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::store::Store;

/// The store key: a bool; absent means on, for every machine, new or upgraded.
pub const KEY: &str = "reopen-realtimex";
/// The watcher's state in the data folder: `{schema, state, exits, last}`.
pub const STATE_FILE: &str = "app-supervision.json";
/// Its size cap when read: the watcher keeps it far smaller.
const MAX_STATE_BYTES: u64 = 64 * 1024;

/// Whether the tray reopens RealTimeX after an unannounced exit.
pub fn enabled(store: &Store) -> bool {
    store.get(KEY) != Some(Value::Bool(false))
}

pub fn set_enabled(store: &Store, on: bool) -> Result<()> {
    store.set(KEY, &json!(on))
}

/// POST /v1/supervision's body: `{enabled: bool}` and nothing else (unknown fields are 400).
pub fn parse_body(body: &Value) -> Result<bool, ApiError> {
    let object = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be {enabled: true|false}"))?;
    if let Some(unknown) = object.keys().find(|key| key.as_str() != "enabled") {
        return Err(ApiError::bad_request(format!("unknown field: {unknown}")));
    }
    object
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("enabled must be true or false"))
}

/// The watcher's `{state, last}`: "watching" or "stopped", and its last decision (null before
/// the first). A missing or unreadable file is "watching" with no decision.
pub fn state(dir: Option<&Path>) -> (String, Value) {
    let read = dir
        .map(|dir| dir.join(STATE_FILE))
        .filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_STATE_BYTES))
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    state_of(read.as_ref())
}

fn state_of(file: Option<&Value>) -> (String, Value) {
    let Some(file) = file.filter(|file| file.get("schema").and_then(Value::as_u64) == Some(1)) else {
        return ("watching".to_string(), Value::Null);
    };
    let state = match file.get("state").and_then(Value::as_str) {
        Some("stopped") => "stopped",
        _ => "watching",
    };
    let last = file.get("last").filter(|last| last.is_object()).cloned().unwrap_or(Value::Null);
    (state.to_string(), last)
}

/// GET /v1/supervision: `{enabled, state, last}`.
pub fn body(store: &Store, dir: Option<&Path>) -> Value {
    let (state, last) = state(dir);
    json!({ "enabled": enabled(store), "state": state, "last": last })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_means_on_and_a_change_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(enabled(&store));
        set_enabled(&store, false).unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert!(!enabled(&store));
        set_enabled(&store, true).unwrap();
        assert!(enabled(&store));
    }

    #[test]
    fn the_body_is_enabled_only() {
        assert!(parse_body(&json!({ "enabled": true })).unwrap());
        assert!(!parse_body(&json!({ "enabled": false })).unwrap());
        for refused in [
            json!({}),
            json!({ "enabled": "true" }),
            json!({ "enabled": 1 }),
            json!({ "enabled": true, "program": "/bin/sh" }),
            json!({ "enabled": true, "args": [] }),
            json!([true]),
        ] {
            assert_eq!(parse_body(&refused).unwrap_err().status, 400, "{refused}");
        }
    }

    #[test]
    fn the_watchers_state_is_read_from_the_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(state(Some(dir.path())), ("watching".to_string(), Value::Null));
        assert_eq!(state(None), ("watching".to_string(), Value::Null));
        let last = json!({ "at": "2026-11-10T03:00:00Z", "pid": 4242, "decision": "stopped", "reason": "4 unannounced exits in 10 min" });
        std::fs::write(
            dir.path().join(STATE_FILE),
            json!({ "schema": 1, "state": "stopped", "exits": [], "last": last }).to_string(),
        )
        .unwrap();
        assert_eq!(state(Some(dir.path())), ("stopped".to_string(), last));
        // Another schema, or not JSON: as if there were none.
        std::fs::write(dir.path().join(STATE_FILE), r#"{"schema":2,"state":"stopped"}"#).unwrap();
        assert_eq!(state(Some(dir.path())).0, "watching");
        std::fs::write(dir.path().join(STATE_FILE), "not json").unwrap();
        assert_eq!(state(Some(dir.path())).0, "watching");
    }
}
