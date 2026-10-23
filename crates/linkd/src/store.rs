//! The daemon's own state: one SQLite file in its data folder, the same remote-host.db the
//! daemon keeps: a `meta` table and a `state` table of JSON values under
//! the keys the plugin used for its state files (clients, settings…).

use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::files;
use crate::util::iso_now;

pub const SCHEMA_VERSION: u32 = 1;
pub const DB_FILE: &str = "remote-host.db";

#[derive(Debug)]
pub struct StoreTooNew(pub u32);

impl std::fmt::Display for StoreTooNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "remote-host.db is from a newer version (schema {})",
            self.0
        )
    }
}

impl std::error::Error for StoreTooNew {}

pub struct Store {
    db: Mutex<Connection>,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self> {
        files::ensure_private_dir(data_dir)?;
        let file = data_dir.join(DB_FILE);
        let db = Connection::open(&file).with_context(|| format!("open {}", file.display()))?;
        files::make_private(&file)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.busy_timeout(std::time::Duration::from_millis(2000))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )?;
        let version: u32 = db
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|text| text.parse().ok())
            .unwrap_or(0);
        if version > SCHEMA_VERSION {
            return Err(StoreTooNew(version).into());
        }
        if version < 1 {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS state (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL)",
            )?;
        }
        set_meta(&db, "schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The saved value, or None (also for a value that is not JSON).
    pub fn get(&self, key: &str) -> Option<Value> {
        let text: Option<String> = self
            .db()
            .query_row("SELECT value FROM state WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten();
        text.and_then(|text| serde_json::from_str(&text).ok())
            .filter(|value: &Value| !value.is_null())
    }

    pub fn set(&self, key: &str, value: &Value) -> Result<()> {
        set_state(&self.db(), key, value)
    }

    pub fn meta_get(&self, key: &str) -> Option<String> {
        self.db()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    #[cfg(test)]
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        set_meta(&self.db(), key, value)
    }

    /// Several writes that land together or not at all.
    pub fn transaction(&self, values: &[(&str, Value)], meta: &[(&str, String)]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (key, value) in values {
            set_state(&tx, key, value)?;
        }
        for (key, value) in meta {
            set_meta(&tx, key, value)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn set_state(db: &Connection, key: &str, value: &Value) -> Result<()> {
    if value.is_null() {
        bail!("No value to save for {key}");
    }
    db.execute(
        "INSERT INTO state (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value.to_string(), iso_now()],
    )?;
    Ok(())
}

fn set_meta(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_json_values_and_meta_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.get("clients"), None);
        store.set("clients", &json!([{ "id": "c1" }])).unwrap();
        store.meta_set("legacy_import", "{}").unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.get("clients"), Some(json!([{ "id": "c1" }])));
        assert_eq!(store.meta_get("schema_version").as_deref(), Some("1"));
        assert_eq!(store.meta_get("legacy_import").as_deref(), Some("{}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = dir.path().join(DB_FILE);
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path())
            .unwrap()
            .meta_set("schema_version", "9")
            .unwrap();
        let error = Store::open(dir.path()).err().unwrap();
        assert_eq!(error.downcast_ref::<StoreTooNew>().map(|e| e.0), Some(9));
    }

    #[test]
    fn a_transaction_lands_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let failed = store.transaction(&[("a", json!(1)), ("b", Value::Null)], &[]);
        assert!(failed.is_err());
        assert_eq!(store.get("a"), None);
        store
            .transaction(&[("a", json!(1))], &[("mark", "x".into())])
            .unwrap();
        assert_eq!(store.get("a"), Some(json!(1)));
        assert_eq!(store.meta_get("mark").as_deref(), Some("x"));
    }
}
