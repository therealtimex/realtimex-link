//! The daemon's own state: one SQLite file in its data folder, the same remote-host.db the
//! daemon keeps: a `meta` table and a `state` table of JSON values under
//! the keys the plugin used for its state files (clients, settings…).
//!
//! Schema 2 (realtimex-ai-app#2836 M3) adds `inbox`: webhook deliveries Link kept while RealTimeX
//! wasn't running, until RealTimeX acknowledges them (inbox.rs).

use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::files;
use crate::util::iso_now;

pub const SCHEMA_VERSION: u32 = 2;
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
        if version < 2 {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS inbox (
                    id TEXT PRIMARY KEY,
                    account TEXT NOT NULL,
                    endpoint TEXT NOT NULL,
                    received_at INTEGER NOT NULL,
                    method TEXT NOT NULL,
                    path TEXT NOT NULL,
                    headers TEXT NOT NULL,
                    body BLOB NOT NULL,
                    size INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS inbox_endpoint ON inbox (account, endpoint, received_at);
                 CREATE INDEX IF NOT EXISTS inbox_received ON inbox (received_at);",
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

    /// Forget a value (get answers None again).
    pub fn remove(&self, key: &str) -> Result<()> {
        self.db()
            .execute("DELETE FROM state WHERE key = ?1", [key])?;
        Ok(())
    }

    #[cfg(test)]
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

/// A webhook delivery as the inbox keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    pub id: String,
    /// The account whose RealTimeX it is for (its Keycloak subject): only that account reads or
    /// acknowledges it (realtimex-ai-app#2836, an account switch must not hand it over).
    pub account: String,
    /// The endpoint's URL token.
    pub endpoint: String,
    /// Milliseconds since the epoch.
    pub received_at: i64,
    pub method: String,
    /// The local path and query, as RealTimeX's route sees it (/wh/<token>?…).
    pub path: String,
    /// [[name, value], …], in the order received.
    pub headers: Value,
    pub body: Vec<u8>,
}

/// Why a delivery wasn't kept.
#[derive(Debug, PartialEq)]
pub enum InboxRefusal {
    /// The endpoint already holds `deliveries` deliveries or `bytes` bytes.
    Full { deliveries: u64, bytes: u64 },
}

impl Store {
    /// Keep a delivery, unless its endpoint is at its quota (checked and written together).
    pub fn inbox_add(
        &self,
        delivery: &Delivery,
        max_deliveries: u64,
        max_bytes: u64,
    ) -> Result<std::result::Result<(), InboxRefusal>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (count, bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM inbox WHERE account = ?1 AND endpoint = ?2",
            [&delivery.account, &delivery.endpoint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let size = delivery.body.len() as u64;
        if count as u64 >= max_deliveries || bytes as u64 + size > max_bytes {
            return Ok(Err(InboxRefusal::Full {
                deliveries: count as u64,
                bytes: bytes as u64,
            }));
        }
        tx.execute(
            "INSERT INTO inbox (id, account, endpoint, received_at, method, path, headers, body, size) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                delivery.id,
                delivery.account,
                delivery.endpoint,
                delivery.received_at,
                delivery.method,
                delivery.path,
                delivery.headers.to_string(),
                delivery.body,
                size as i64
            ],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Up to `limit` of `account`'s deliveries, oldest first, after `after` (a delivery's id,
    /// from an earlier page), and how many it has kept in all.
    pub fn inbox_list(
        &self,
        account: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Delivery>, u64)> {
        let db = self.db();
        let start: Option<(i64, String)> = match after {
            Some(id) => db
                .query_row(
                    "SELECT received_at, id FROM inbox WHERE id = ?1 AND account = ?2",
                    [id, account],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?,
            None => None,
        };
        let (at, id) = start.unwrap_or((i64::MIN, String::new()));
        let mut statement = db.prepare(
            "SELECT id, endpoint, received_at, method, path, headers, body FROM inbox \
             WHERE account = ?4 AND (received_at > ?1 OR (received_at = ?1 AND id > ?2)) \
             ORDER BY received_at, id LIMIT ?3",
        )?;
        let rows = statement.query_map(params![at, id, limit as i64, account], |row| {
            let headers: String = row.get(5)?;
            Ok(Delivery {
                id: row.get(0)?,
                account: account.to_string(),
                endpoint: row.get(1)?,
                received_at: row.get(2)?,
                method: row.get(3)?,
                path: row.get(4)?,
                headers: serde_json::from_str(&headers).unwrap_or(Value::Array(Vec::new())),
                body: row.get(6)?,
            })
        })?;
        let deliveries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let total: i64 = db.query_row(
            "SELECT COUNT(*) FROM inbox WHERE account = ?1",
            [account],
            |row| row.get(0),
        )?;
        Ok((deliveries, total as u64))
    }

    /// How many deliveries each of `account`'s endpoints holds: { token: count }.
    pub fn inbox_counts(&self, account: &str) -> Result<std::collections::HashMap<String, u64>> {
        let db = self.db();
        let mut statement = db
            .prepare("SELECT endpoint, COUNT(*) FROM inbox WHERE account = ?1 GROUP BY endpoint")?;
        let rows = statement.query_map([account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `account`'s RealTimeX has these: they go (only its own). Answers how many went.
    pub fn inbox_ack(&self, account: &str, ids: &[String]) -> Result<usize> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let mut removed = 0;
        for id in ids {
            removed += tx.execute(
                "DELETE FROM inbox WHERE id = ?1 AND account = ?2",
                [id, account],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Deliveries received before `before` (ms) go: the inbox keeps them for a while only.
    pub fn inbox_prune(&self, before: i64) -> Result<usize> {
        Ok(self
            .db()
            .execute("DELETE FROM inbox WHERE received_at < ?1", [before])?)
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
        assert_eq!(store.meta_get("schema_version").as_deref(), Some("2"));
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

    fn delivery(id: &str, endpoint: &str, at: i64, body: &[u8]) -> Delivery {
        Delivery {
            id: id.into(),
            account: "a1".into(),
            endpoint: endpoint.into(),
            received_at: at,
            method: "POST".into(),
            path: format!("/wh/{endpoint}"),
            headers: json!([["content-type", "application/json"]]),
            body: body.to_vec(),
        }
    }

    #[test]
    fn a_version_1_store_gains_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .set("settings", &json!({ "allowIncoming": true }))
                .unwrap();
            store.db().execute_batch("DROP TABLE inbox").unwrap();
            store.meta_set("schema_version", "1").unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.meta_get("schema_version").as_deref(), Some("2"));
        assert_eq!(
            store.get("settings"),
            Some(json!({ "allowIncoming": true }))
        );
        assert_eq!(store.inbox_list("a1", None, 10).unwrap().1, 0);
    }

    #[test]
    fn the_inbox_keeps_pages_quotas_acks_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (i, at) in [(1, 100), (2, 100), (3, 200)] {
            assert_eq!(
                store
                    .inbox_add(&delivery(&format!("d{i}"), "tok", at, b"{}"), 10, 1000)
                    .unwrap(),
                Ok(())
            );
        }
        store
            .inbox_add(&delivery("e1", "other", 150, b"x"), 10, 1000)
            .unwrap()
            .unwrap();
        // Oldest first, a page at a time.
        let (page, total) = store.inbox_list("a1", None, 2).unwrap();
        assert_eq!(total, 4);
        assert_eq!(
            page.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["d1", "d2"]
        );
        assert_eq!(page[0], delivery("d1", "tok", 100, b"{}"));
        let (page, _) = store.inbox_list("a1", Some("d2"), 10).unwrap();
        assert_eq!(
            page.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["e1", "d3"]
        );
        // The quota is per endpoint, by count and by bytes.
        assert!(matches!(
            store
                .inbox_add(&delivery("d4", "tok", 300, b"{}"), 3, 1000)
                .unwrap(),
            Err(InboxRefusal::Full { deliveries: 3, .. })
        ));
        assert!(matches!(
            store
                .inbox_add(&delivery("d4", "tok", 300, b"0123456789"), 10, 15)
                .unwrap(),
            Err(InboxRefusal::Full { bytes: 6, .. })
        ));
        assert_eq!(store.inbox_counts("a1").unwrap()["tok"], 3);
        // Another account sees and acknowledges none of them.
        assert_eq!(store.inbox_list("a2", None, 10).unwrap().1, 0);
        assert_eq!(store.inbox_ack("a2", &["d1".into()]).unwrap(), 0);
        assert_eq!(
            store
                .inbox_ack("a1", &["d1".into(), "nope".into()])
                .unwrap(),
            1
        );
        assert_eq!(store.inbox_prune(150).unwrap(), 1);
        let (left, total) = store.inbox_list("a1", None, 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            left.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["e1", "d3"]
        );
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
