//! The activity log (realtimex-ai-app#3055 §1.4): what other computers, agents on this one, and
//! the owner did here, kept in remote-host.db's `audit` table on this computer only. The Link
//! window reads it through the local API (GET/DELETE /v1/host/audit); no remote route reads it.
//!
//! Best effort, never in the way: a row goes into a bounded channel to one writer thread, which
//! saves rows in batches. A full channel or a failed write drops the row and counts it
//! (`dropped`): remote control never waits on, or fails because of, the log.
//!
//! What a row holds is whitelisted per kind ([`clean_detail`]): never typed or pasted text, its
//! length, clipboard or file contents, element names or values, window titles, tree content,
//! pictures or credentials. A `key` is kept only with a modifier or as a named non-printing key;
//! a bare printable key only counts as `printable` (a run of them is typed text).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::clients::Authenticated;
use crate::error::ApiError;
use crate::store::{AuditRow, Store};
use crate::util::now_ms;

/// Rows are kept 30 days and at most MAX_ROWS of them, whichever comes first (fixed: no setting).
pub const RETENTION_DAYS: i64 = 30;
pub const RETENTION_MS: i64 = RETENTION_DAYS * 24 * 60 * 60 * 1000;
pub const MAX_ROWS: i64 = 50_000;
/// Rows waiting for the writer; more are dropped (and counted).
pub const CHANNEL: usize = 1024;
/// A row's `detail`, as JSON, at most this long: file names go first when it is longer.
pub const MAX_DETAIL: usize = 2048;
const MAX_FILES: usize = 20;
const MAX_FILE_NAME: usize = 120;
const MAX_KEYS: usize = 20;
const MAX_NAME: usize = 80;
const MAX_SHORT: usize = 64;
const MAX_APP: usize = 120;
/// Accessibility reads: one row per client and window per minute, with their count.
pub const AX_READ_WINDOW_MS: i64 = 60 * 1000;
/// Rows written together at most.
const BATCH: usize = 256;
/// How often the writer looks for accessibility reads whose minute is over.
const TICK: Duration = Duration::from_secs(10);

pub const KINDS: &[&str] = &[
    "connect",
    "access",
    "view",
    "screenshot",
    "actions",
    "windows",
    "focus",
    "wait",
    "clipboard",
    "transfer",
    "ax-read",
    "ax-act",
    "audit-cleared",
];

/// The owner's changes (`access.detail.change`, design Addendum 1 D1).
pub const ACCESS_CHANGES: &[&str] = &[
    "allowed",
    "denied",
    "permission",
    "blocked",
    "allowed-again",
    "removed",
    "disconnected",
    "new-password",
];

/// Action kinds counted in `actions.detail.kinds` (and a view's `input`).
const ACTION_KINDS: &[&str] = &[
    "click",
    "double_click",
    "right_click",
    "move",
    "scroll",
    "type",
    "key",
    "printable",
    "wait",
    "down",
    "up",
    "drag",
    "paste",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Refused,
    Error,
    Pending,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Refused => "refused",
            Outcome::Error => "error",
            Outcome::Pending => "pending",
        }
    }

    /// A refusal (the computer, the owner or the system said no) or a failure.
    pub fn of_status(status: u16) -> Self {
        match status {
            200..=299 => Outcome::Ok,
            401 | 403 | 409 | 423 | 429 => Outcome::Refused,
            _ => Outcome::Error,
        }
    }
}

/// Who a row is about: the client (as it calls itself), the actor and the route.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Who {
    /// Link's own id of the client (the overview's `id`), or `local:<caller>` for an agent on this
    /// computer; None for a computer that was never admitted, and for the owner's new password.
    pub client_id: Option<String>,
    pub client_name: Option<String>,
    pub machine: Option<String>,
    /// `person`, `agent` or `owner`.
    pub actor: String,
    /// `tailnet`, `public` or `local`.
    pub via: String,
}

impl Who {
    /// A signed-in client, over `via`.
    pub fn client(client: &Authenticated, via: &str) -> Self {
        let text = |name: &str| {
            client
                .client
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| clip(s, MAX_NAME))
        };
        Self {
            client_id: text("id"),
            client_name: text("name"),
            machine: text("machine"),
            actor: client.actor.to_string(),
            via: via.to_string(),
        }
    }

    /// The owner, through the window or RealTimeX on this computer, about `subject` (a client as
    /// the overview lists it), or about no client.
    pub fn owner(subject: Option<&Value>) -> Self {
        let text = |name: &str| {
            subject
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| clip(s, MAX_NAME))
        };
        Self {
            client_id: text("id"),
            client_name: text("name"),
            machine: text("machine"),
            actor: "owner".into(),
            via: "local".into(),
        }
    }

    /// An agent on this computer through the local API (X-RTX-Caller): `local:<caller id>`, the
    /// thread's name only (the window adds its own "RealTimeX here" wording; Addendum 3 G5).
    pub fn local(caller_id: &str, name: &str) -> Self {
        Self {
            client_id: Some(clip(&format!("local:{caller_id}"), 200)),
            client_name: Some(clip(name, MAX_NAME)).filter(|n| !n.is_empty()),
            machine: None,
            actor: "agent".into(),
            via: "local".into(),
        }
    }
}

/// One row, as recorded (`detail` is cleaned before it is kept).
#[derive(Debug, Clone)]
pub struct Entry {
    pub at: i64,
    pub who: Who,
    pub kind: &'static str,
    pub outcome: Outcome,
    pub code: Option<String>,
    pub detail: Value,
}

enum Msg {
    Row(AuditRow),
    /// Everything sent before is written: answered once it is.
    Flush(SyncSender<()>),
}

struct PendingReads {
    started: i64,
    who: Who,
    app: Option<String>,
    count: u64,
}

struct Inner {
    store: Arc<Store>,
    dropped: AtomicU64,
    /// Accessibility reads still adding up, by (client id, window).
    reads: Mutex<HashMap<(String, String), PendingReads>>,
}

/// The log's writing side; cheap to clone.
#[derive(Clone)]
pub struct Audit {
    inner: Arc<Inner>,
    sender: SyncSender<Msg>,
}

impl Audit {
    /// The log of `store`, with its writer thread.
    pub fn start(store: Arc<Store>) -> Self {
        let (audit, receiver) = Self::detached(store, CHANNEL);
        let inner = audit.inner.clone();
        let spawned = std::thread::Builder::new()
            .name("audit-writer".into())
            .spawn(move || write_rows(&inner, &receiver));
        if spawned.is_err() {
            // No writer: every row is counted as dropped (the channel fills and stays full).
            audit.inner.dropped.fetch_add(1, Ordering::SeqCst);
        }
        audit
    }

    /// Without a writer (tests drive `receiver` themselves): `capacity` rows fit.
    fn detached(store: Arc<Store>, capacity: usize) -> (Self, Receiver<Msg>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let audit = Self {
            inner: Arc::new(Inner {
                store,
                dropped: AtomicU64::new(0),
                reads: Mutex::new(HashMap::new()),
            }),
            sender,
        };
        (audit, receiver)
    }

    /// Rows lost since start: a full channel, or a write that failed.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::SeqCst)
    }

    /// Record `entry` (never blocks; dropped and counted when the writer is behind).
    pub fn record(&self, entry: Entry) {
        let row = to_row(entry);
        match self.sender.try_send(Msg::Row(row)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.inner.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Record a row of `kind` for `who`, now.
    pub fn note(&self, who: Who, kind: &'static str, outcome: Outcome, code: Option<&str>, detail: Value) {
        self.record(Entry {
            at: now_ms(),
            who,
            kind,
            outcome,
            code: code.map(str::to_string),
            detail,
        });
    }

    /// Record the outcome of a request: ok, or the error's code and whether it was refused.
    pub fn outcome<T>(&self, who: Who, kind: &'static str, result: &Result<T, ApiError>, detail: Value) {
        match result {
            Ok(_) => self.note(who, kind, Outcome::Ok, None, detail),
            Err(error) => self.note(
                who,
                kind,
                Outcome::of_status(error.status),
                Some(&error.code_or_status()),
                detail,
            ),
        }
    }

    /// An accessibility read (apps, snapshot, find, get, a reading batch) of `window` in `app`:
    /// one row per client and window per minute, with how many reads it covers.
    pub fn ax_read(&self, who: Who, window: &str, app: Option<&str>) {
        let key = (who.client_id.clone().unwrap_or_default(), window.to_string());
        let now = now_ms();
        let finished = {
            let mut reads = lock(&self.inner.reads);
            let current = reads
                .get(&key)
                .map(|pending| now - pending.started < AX_READ_WINDOW_MS);
            if current == Some(true) {
                if let Some(pending) = reads.get_mut(&key) {
                    pending.count += 1;
                    if pending.app.is_none() {
                        pending.app = app.map(str::to_string);
                    }
                }
                return;
            }
            let finished = reads.remove(&key);
            reads.insert(
                key,
                PendingReads {
                    started: now,
                    who,
                    app: app.map(str::to_string),
                    count: 1,
                },
            );
            finished
        };
        if let Some(done) = finished {
            self.record(read_entry(done));
        }
    }

    /// Accessibility reads whose minute is over (all of them with `all`) become rows.
    pub fn flush_reads(&self, all: bool) {
        for entry in due_reads(&self.inner, all) {
            self.record(entry);
        }
    }

    /// Wait until every row sent so far is written (or the writer is gone).
    pub fn flush(&self) {
        let (done, wait) = mpsc::sync_channel(1);
        if self.sender.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }

    /// A page of entries, newest first.
    pub fn page(&self, query: &Query) -> anyhow::Result<Value> {
        let (rows, next) = self.inner.store.audit_page(
            query.before,
            query.limit,
            query.client.as_deref(),
            query.kind.as_deref(),
        )?;
        let entries: Vec<Value> = rows.iter().map(entry_json).collect();
        Ok(json!({
            "entries": entries,
            "next": next,
            "retentionDays": RETENTION_DAYS,
            "dropped": self.dropped(),
        }))
    }

    /// The owner clears the log: everything goes, then one `audit-cleared` row says how much.
    pub fn clear(&self) -> anyhow::Result<u64> {
        self.flush_reads(true);
        self.flush();
        let cleared = self.inner.store.audit_clear()?;
        let row = to_row(Entry {
            at: now_ms(),
            who: Who::owner(None),
            kind: "audit-cleared",
            outcome: Outcome::Ok,
            code: None,
            detail: json!({ "rows": cleared }),
        });
        self.inner.store.audit_insert(&[row])?;
        Ok(cleared)
    }

    /// Rows past 30 days, or beyond the newest MAX_ROWS, go (hourly, with the webhook inbox).
    pub fn prune(&self) -> anyhow::Result<usize> {
        self.inner
            .store
            .audit_prune(now_ms() - RETENTION_MS, MAX_ROWS)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn read_entry(reads: PendingReads) -> Entry {
    let mut detail = json!({ "count": reads.count });
    if let Some(app) = reads.app {
        detail["app"] = json!(app);
    }
    Entry {
        at: reads.started,
        who: reads.who,
        kind: "ax-read",
        outcome: Outcome::Ok,
        code: None,
        detail,
    }
}

fn due_reads(inner: &Inner, all: bool) -> Vec<Entry> {
    let now = now_ms();
    let mut reads = lock(&inner.reads);
    let due: Vec<(String, String)> = reads
        .iter()
        .filter(|(_, pending)| all || now - pending.started >= AX_READ_WINDOW_MS)
        .map(|(key, _)| key.clone())
        .collect();
    due.into_iter()
        .filter_map(|key| reads.remove(&key))
        .map(read_entry)
        .collect()
}

/// The writer: rows in batches, each batch one transaction; a batch that fails is counted as
/// dropped. Accessibility reads whose minute is over are written as they come due.
fn write_rows(inner: &Arc<Inner>, receiver: &Receiver<Msg>) {
    loop {
        let first = match receiver.recv_timeout(TICK) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut rows: Vec<AuditRow> = due_reads(inner, false).into_iter().map(to_row).collect();
        let mut flushes = Vec::new();
        let mut next = first;
        while let Some(msg) = next {
            match msg {
                Msg::Row(row) => rows.push(row),
                Msg::Flush(done) => flushes.push(done),
            }
            if rows.len() >= BATCH {
                break;
            }
            next = receiver.try_recv().ok();
        }
        save(inner, &rows);
        for done in flushes {
            let _ = done.send(());
        }
    }
}

fn save(inner: &Inner, rows: &[AuditRow]) {
    if rows.is_empty() {
        return;
    }
    if inner.store.audit_insert(rows).is_err() {
        inner
            .dropped
            .fetch_add(rows.len() as u64, Ordering::SeqCst);
    }
}

fn to_row(entry: Entry) -> AuditRow {
    let detail = clean_detail(entry.kind, &entry.detail);
    let code = entry
        .code
        .filter(|_| entry.outcome != Outcome::Ok)
        .map(|code| clip(&code, MAX_SHORT));
    let actor = match entry.who.actor.as_str() {
        "agent" => "agent",
        "owner" => "owner",
        _ => "person",
    };
    let via = match entry.who.via.as_str() {
        "public" => "public",
        "local" => "local",
        _ => "tailnet",
    };
    AuditRow {
        id: 0,
        at: entry.at,
        client_id: entry.who.client_id,
        client_name: entry.who.client_name,
        machine: entry.who.machine,
        actor: actor.into(),
        via: via.into(),
        kind: entry.kind.into(),
        outcome: entry.outcome.as_str().into(),
        code,
        detail: if detail.as_object().is_some_and(Map::is_empty) {
            None
        } else {
            Some(detail.to_string())
        },
    }
}

fn entry_json(row: &AuditRow) -> Value {
    json!({
        "id": row.id,
        "at": row.at,
        "clientId": row.client_id,
        "clientName": row.client_name,
        "machine": row.machine,
        "actor": row.actor,
        "via": row.via,
        "kind": row.kind,
        "outcome": row.outcome,
        "code": row.code,
        "detail": row
            .detail
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or(json!({})),
    })
}

/// GET /v1/host/audit's query.
#[derive(Debug, Default)]
pub struct Query {
    pub before: Option<i64>,
    pub limit: usize,
    pub client: Option<String>,
    pub kind: Option<String>,
}

impl Query {
    pub fn parse(params: &HashMap<String, String>) -> Result<Self, ApiError> {
        let number = |name: &str| -> Result<Option<i64>, ApiError> {
            match params.get(name).filter(|v| !v.is_empty()) {
                None => Ok(None),
                Some(text) => text
                    .parse::<i64>()
                    .map(Some)
                    .map_err(|_| ApiError::bad_request(format!("{name} must be a number"))),
            }
        };
        let limit = number("limit")?.unwrap_or(100);
        if !(1..=200).contains(&limit) {
            return Err(ApiError::bad_request("limit must be from 1 to 200"));
        }
        let kind = params.get("kind").filter(|k| !k.is_empty()).cloned();
        if let Some(kind) = &kind {
            if !KINDS.contains(&kind.as_str()) {
                return Err(ApiError::bad_request("Unknown kind"));
            }
        }
        Ok(Self {
            before: number("before")?,
            limit: limit as usize,
            client: params
                .get("client")
                .filter(|c| !c.is_empty())
                .map(|c| clip(c, 256)),
            kind,
        })
    }
}

// --- what a row may hold ------------------------------------------------------------------

/// At most `max` characters.
fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn one_of(value: Option<&Value>, allowed: &[&str]) -> Option<Value> {
    let text = value?.as_str()?;
    allowed.contains(&text).then(|| json!(text))
}

fn short(value: Option<&Value>, max: usize) -> Option<Value> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| json!(clip(text, max)))
}

fn count(value: Option<&Value>) -> Option<Value> {
    value?.as_u64().map(|n| json!(n))
}

fn counts(value: Option<&Value>) -> Option<Value> {
    let map = value?.as_object()?;
    let kept: Map<String, Value> = map
        .iter()
        .filter(|(name, _)| ACTION_KINDS.contains(&name.as_str()))
        .filter_map(|(name, n)| n.as_u64().map(|n| (name.clone(), json!(n))))
        .collect();
    (!kept.is_empty()).then_some(Value::Object(kept))
}

fn file_names(value: Option<&Value>) -> Option<Value> {
    let names: Vec<Value> = value?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        // Only a name: anything after the last separator.
        .map(|name| name.rsplit(['/', '\\']).next().unwrap_or(""))
        .filter(|name| !name.is_empty())
        .take(MAX_FILES)
        .map(|name| json!(clip(name, MAX_FILE_NAME)))
        .collect();
    (!names.is_empty()).then_some(Value::Array(names))
}

fn key_list(value: Option<&Value>) -> Option<Value> {
    let keys: Vec<Value> = value?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter_map(loggable_key)
        .take(MAX_KEYS)
        .map(Value::String)
        .collect();
    (!keys.is_empty()).then_some(Value::Array(keys))
}

/// Only the keys `kind` may hold, each checked; nothing else is kept.
pub fn clean_detail(kind: &str, detail: &Value) -> Value {
    let get = |name: &str| detail.get(name);
    let mut kept = Map::new();
    let mut put = |name: &str, value: Option<Value>| {
        if let Some(value) = value {
            kept.insert(name.into(), value);
        }
    };
    let permission = |v: Option<&Value>| one_of(v, &["view", "control"]);
    match kind {
        "connect" => {
            put(
                "admittedBy",
                one_of(get("admittedBy"), &["password", "request", "account", "client-key"]),
            );
            put("permission", permission(get("permission")));
        }
        "access" => {
            put("change", one_of(get("change"), ACCESS_CHANGES));
            put("permission", permission(get("permission")));
        }
        "view" => {
            put("display", short(get("display"), MAX_SHORT));
            put("durationMs", count(get("durationMs")));
            put("input", counts(get("input")));
        }
        "screenshot" | "windows" => put("display", short(get("display"), MAX_SHORT)),
        "actions" => {
            put("display", short(get("display"), MAX_SHORT));
            put("kinds", counts(get("kinds")));
            put("keys", key_list(get("keys")));
        }
        "focus" => put("app", short(get("app"), MAX_APP)),
        "wait" => {
            put("display", short(get("display"), MAX_SHORT));
            put("changed", get("changed").and_then(Value::as_bool).map(Value::Bool));
        }
        "clipboard" => {
            put("direction", one_of(get("direction"), &["read", "write"]));
            put("kind", one_of(get("kind"), &["text", "image", "files"]));
        }
        "transfer" => {
            let op = one_of(
                get("op"),
                &["send", "get", "list", "paste-image", "paste-files"],
            );
            let listing = op.as_ref().and_then(Value::as_str) == Some("list");
            put("op", op);
            put(
                "direction",
                one_of(get("direction"), &["to-this-computer", "from-this-computer"]),
            );
            // A listing keeps a count only (Addendum 1 D2).
            if !listing {
                put("files", file_names(get("files")));
                put("bytes", count(get("bytes")));
            }
            put("count", count(get("count")));
            put("folder", short(get("folder"), MAX_SHORT));
        }
        "ax-read" => {
            put("app", short(get("app"), MAX_APP));
            put("count", count(get("count")));
        }
        "ax-act" => {
            put("app", short(get("app"), MAX_APP));
            put(
                "action",
                one_of(
                    get("action"),
                    &[
                        "click",
                        "set-value",
                        "type",
                        "toggle",
                        "select",
                        "expand",
                        "collapse",
                        "scroll-to",
                    ],
                ),
            );
            put("role", short(get("role"), 40));
            put("via", one_of(get("via"), &["type", "set-value", "paste"]));
        }
        "audit-cleared" => put("rows", count(get("rows"))),
        _ => {}
    }
    let mut kept = Value::Object(kept);
    // At most MAX_DETAIL: the file names give way first, from the end.
    while kept.to_string().len() > MAX_DETAIL {
        let Some(files) = kept.get_mut("files").and_then(Value::as_array_mut) else {
            break;
        };
        if files.pop().is_none() {
            break;
        }
        if files.is_empty() {
            if let Some(fields) = kept.as_object_mut() {
                fields.remove("files");
            }
        }
    }
    if kept.to_string().len() > MAX_DETAIL {
        return json!({});
    }
    kept
}

const MODIFIERS: &[&str] = &[
    "ctrl", "control", "alt", "option", "cmd", "command", "meta", "super", "win", "windows",
];
const NAMED_KEYS: &[&str] = &[
    "enter",
    "return",
    "tab",
    "esc",
    "escape",
    "backspace",
    "delete",
    "del",
    "up",
    "down",
    "left",
    "right",
    "arrowup",
    "arrowdown",
    "arrowleft",
    "arrowright",
    "home",
    "end",
    "pageup",
    "pagedown",
    "page_up",
    "page_down",
];

fn named_key(key: &str) -> bool {
    if NAMED_KEYS.contains(&key) {
        return true;
    }
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| (1..=24).contains(&n))
}

/// A key combo as the log may keep it ("ctrl+s", "enter"), or None for a printable key
/// (typing, with or without Shift).
pub fn loggable_key(combo: &str) -> Option<String> {
    let parts: Vec<String> = combo
        .split('+')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    let main = parts.last()?;
    let modified = parts[..parts.len() - 1]
        .iter()
        .any(|part| MODIFIERS.contains(&part.as_str()));
    if !(modified || named_key(main)) {
        return None;
    }
    // A modifier alone ("ctrl") is a key of its own.
    if MODIFIERS.contains(&main.as_str()) && parts.len() == 1 {
        return Some(main.clone());
    }
    // Never longer than a combo: nothing typed rides along.
    if parts.len() > 5 || parts.iter().any(|part| part.chars().count() > 12) {
        return None;
    }
    Some(parts.join("+"))
}

/// A physical key (KeyboardEvent.code) as a key name: "enter", "f5", "s"; with whether it is a
/// modifier.
fn physical_name(code: &str) -> (String, bool) {
    let lower = code.to_ascii_lowercase();
    for (prefix, name) in [
        ("control", "ctrl"),
        ("alt", "alt"),
        ("meta", "cmd"),
        ("os", "win"),
    ] {
        if lower.starts_with(prefix) && lower.len() <= prefix.len() + 5 {
            return (name.into(), true);
        }
    }
    let name = lower
        .strip_prefix("key")
        .or_else(|| lower.strip_prefix("digit"))
        .or_else(|| lower.strip_prefix("arrow"))
        .unwrap_or(&lower)
        .to_string();
    (name, false)
}

/// What a batch of actions did, for an `actions` row or a view's `input`: counts per kind, and
/// the keys the log may keep.
pub fn tally(actions: &[Value], into: &mut Map<String, Value>, keys: &mut Vec<String>) {
    let add = |into: &mut Map<String, Value>, kind: &str| {
        let n = into.get(kind).and_then(Value::as_u64).unwrap_or(0) + 1;
        into.insert(kind.into(), json!(n));
    };
    let mut held: Vec<String> = Vec::new();
    for action in actions {
        let kind = action.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "key" {
            if ACTION_KINDS.contains(&kind) {
                add(into, kind);
            }
            continue;
        }
        // A physical key: its downs count; modifiers held make a combo.
        if let Some(code) = action.get("code").and_then(Value::as_str) {
            let down = action.get("down").and_then(Value::as_bool).unwrap_or(false);
            let (name, modifier) = physical_name(code);
            if modifier {
                if down {
                    if !held.contains(&name) {
                        held.push(name);
                    }
                } else {
                    held.retain(|h| *h != name);
                }
                continue;
            }
            if !down {
                continue;
            }
            let combo = held
                .iter()
                .cloned()
                .chain(std::iter::once(name))
                .collect::<Vec<_>>()
                .join("+");
            match loggable_key(&combo) {
                Some(combo) => {
                    add(into, "key");
                    if !keys.contains(&combo) && keys.len() < MAX_KEYS {
                        keys.push(combo);
                    }
                }
                None => add(into, "printable"),
            }
            continue;
        }
        let combo = action.get("keys").and_then(Value::as_str).unwrap_or("");
        match loggable_key(combo) {
            Some(combo) => {
                add(into, "key");
                if !keys.contains(&combo) && keys.len() < MAX_KEYS {
                    keys.push(combo);
                }
            }
            None => add(into, "printable"),
        }
    }
}

/// An `actions` row's detail for this batch on `display`.
pub fn actions_detail(actions: Option<&Value>, display: Option<&str>) -> Value {
    let mut kinds = Map::new();
    let mut keys = Vec::new();
    if let Some(list) = actions.and_then(Value::as_array) {
        tally(list, &mut kinds, &mut keys);
    }
    let mut detail = json!({ "kinds": kinds });
    if !keys.is_empty() {
        detail["keys"] = json!(keys);
    }
    if let Some(display) = display {
        detail["display"] = json!(display);
    }
    detail
}

/// What a person's live view did, summed for its `view` row: clicks, scrolls and keys (typing
/// counts as `key`: no count reads as text length). Pointer moves aren't counted.
pub fn view_input(actions: &[Value], into: &mut Map<String, Value>) {
    for action in actions {
        let kind = match action.get("type").and_then(Value::as_str).unwrap_or("") {
            "click" | "double_click" | "right_click" | "down" => "click",
            "scroll" => "scroll",
            "key" => {
                if action.get("code").is_some() && action.get("down") != Some(&Value::Bool(true)) {
                    continue;
                }
                "key"
            }
            "type" | "paste" => "key",
            _ => continue,
        };
        let n = into.get(kind).and_then(Value::as_u64).unwrap_or(0) + 1;
        into.insert(kind.into(), json!(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRETS: &[&str] = &[
        "hunter2-typed-text",
        "pasted-secret-text",
        "clipboard-secret",
        "file-content-secret",
        "Element Name Secret",
        "element-value-secret",
        "Window Title Secret",
        "tree-content-secret",
        "screenshot-bytes-secret",
        "bearer-token-secret",
        "rtx_ck_client-key-secret",
        "password-secret",
    ];

    fn store() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (dir, store)
    }

    fn who() -> Who {
        Who {
            client_id: Some("c1".into()),
            client_name: Some("Office PC".into()),
            machine: Some("DESKTOP-4KQ7".into()),
            actor: "agent".into(),
            via: "public".into(),
        }
    }

    fn all(audit: &Audit) -> Vec<Value> {
        audit.flush();
        audit.page(&Query { limit: 200, ..Query::default() }).unwrap()["entries"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Every kind, with secrets everywhere a careless caller might put them.
    fn noisy(kind: &str) -> Value {
        let mut detail = json!({
            "text": SECRETS[0], "paste": SECRETS[1], "clipboard": SECRETS[2],
            "content": SECRETS[3], "name": SECRETS[4], "value": SECRETS[5], "title": SECRETS[6],
            "tree": SECRETS[7], "data": SECRETS[8], "token": SECRETS[9], "clientKey": SECRETS[10],
            "password": SECRETS[11],
        });
        let fields = match kind {
            "connect" => json!({ "admittedBy": "password", "permission": "control" }),
            "access" => json!({ "change": "permission", "permission": "view" }),
            "view" => json!({ "display": "1", "durationMs": 1260000, "input": { "click": 3, "text": SECRETS[0] } }),
            "screenshot" | "windows" => json!({ "display": "1" }),
            "actions" => actions_detail(
                Some(&json!([
                    { "type": "click", "x": 1, "y": 2 },
                    { "type": "type", "text": SECRETS[0] },
                    { "type": "key", "keys": "ctrl+s" },
                    { "type": "key", "keys": "h" },
                    { "type": "key", "keys": "shift+H" },
                ])),
                Some("1"),
            ),
            "focus" => json!({ "app": "notepad.exe" }),
            "wait" => json!({ "display": "1", "changed": true }),
            "clipboard" => json!({ "direction": "read", "kind": "text" }),
            "transfer" => json!({ "op": "get", "direction": "from-this-computer", "folder": "Downloads",
                "files": ["/Users/me/Downloads/Q3.xlsx", "C:\\Users\\me\\a.pdf"], "count": 2, "bytes": 1480000 }),
            "ax-read" => json!({ "app": "com.apple.calculator", "count": 3 }),
            "ax-act" => json!({ "app": "EXCEL.EXE", "action": "set-value", "role": "text field", "via": "set-value" }),
            "audit-cleared" => json!({ "rows": 12 }),
            _ => json!({}),
        };
        for (name, value) in fields.as_object().unwrap() {
            detail[name] = value.clone();
        }
        detail
    }

    #[test]
    fn each_kind_keeps_its_whitelisted_detail_only() {
        let (_dir, store) = store();
        let audit = Audit::start(store.clone());
        for kind in KINDS {
            audit.note(who(), *kind, Outcome::Ok, None, noisy(kind));
        }
        let entries = all(&audit);
        assert_eq!(entries.len(), KINDS.len());
        let detail = |kind: &str| {
            entries
                .iter()
                .find(|e| e["kind"] == kind)
                .unwrap()["detail"]
                .clone()
        };
        assert_eq!(detail("connect"), json!({ "admittedBy": "password", "permission": "control" }));
        assert_eq!(detail("access"), json!({ "change": "permission", "permission": "view" }));
        assert_eq!(detail("view"), json!({ "display": "1", "durationMs": 1260000, "input": { "click": 3 } }));
        assert_eq!(detail("screenshot"), json!({ "display": "1" }));
        assert_eq!(
            detail("actions"),
            json!({ "display": "1", "kinds": { "click": 1, "type": 1, "key": 1, "printable": 2 }, "keys": ["ctrl+s"] })
        );
        assert_eq!(detail("focus"), json!({ "app": "notepad.exe" }));
        assert_eq!(detail("wait"), json!({ "display": "1", "changed": true }));
        assert_eq!(detail("clipboard"), json!({ "direction": "read", "kind": "text" }));
        assert_eq!(
            detail("transfer"),
            json!({ "op": "get", "direction": "from-this-computer", "folder": "Downloads",
                "files": ["Q3.xlsx", "a.pdf"], "count": 2, "bytes": 1480000 })
        );
        assert_eq!(detail("ax-read"), json!({ "app": "com.apple.calculator", "count": 3 }));
        assert_eq!(
            detail("ax-act"),
            json!({ "app": "EXCEL.EXE", "action": "set-value", "role": "text field", "via": "set-value" })
        );
        assert_eq!(detail("audit-cleared"), json!({ "rows": 12 }));
        // The row itself: who, as it calls itself, and how.
        let first = &entries[entries.len() - 1];
        assert_eq!(first["kind"], "connect");
        assert_eq!(first["clientId"], "c1");
        assert_eq!(first["clientName"], "Office PC");
        assert_eq!(first["machine"], "DESKTOP-4KQ7");
        assert_eq!(first["actor"], "agent");
        assert_eq!(first["via"], "public");
        assert_eq!(first["outcome"], "ok");
        assert_eq!(first["code"], Value::Null);
    }

    #[test]
    fn no_secret_reaches_the_table() {
        let (dir, store) = store();
        let audit = Audit::start(store.clone());
        for kind in KINDS {
            audit.note(who(), *kind, Outcome::Ok, None, noisy(kind));
            audit.note(who(), *kind, Outcome::Refused, Some("view-only"), noisy(kind));
        }
        audit.ax_read(who(), "w:1", Some("EXCEL.EXE"));
        audit.flush_reads(true);
        audit.flush();
        drop(audit);
        drop(store);
        // The whole database file, its write-ahead log included, holds none of them.
        let mut bytes = Vec::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            bytes.extend(std::fs::read(entry.unwrap().path()).unwrap());
        }
        let text = String::from_utf8_lossy(&bytes);
        for secret in SECRETS {
            assert!(!text.contains(secret), "{secret} was stored");
        }
    }

    #[test]
    fn keys_are_kept_only_with_a_modifier_or_named() {
        assert_eq!(loggable_key("ctrl+s").as_deref(), Some("ctrl+s"));
        assert_eq!(loggable_key("Cmd+Shift+T").as_deref(), Some("cmd+shift+t"));
        assert_eq!(loggable_key("enter").as_deref(), Some("enter"));
        assert_eq!(loggable_key("F12").as_deref(), Some("f12"));
        assert_eq!(loggable_key("shift+tab").as_deref(), Some("shift+tab"));
        assert_eq!(loggable_key("a"), None);
        assert_eq!(loggable_key("shift+a"), None);
        assert_eq!(loggable_key("space"), None);
        assert_eq!(loggable_key("f25"), None);
        // Physical keys: Ctrl held, then S, is ctrl+s; a letter alone is typing.
        let mut kinds = Map::new();
        let mut keys = Vec::new();
        let batch = json!([
            { "type": "key", "code": "ControlLeft", "down": true },
            { "type": "key", "code": "KeyS", "down": true },
            { "type": "key", "code": "KeyS", "down": false },
            { "type": "key", "code": "ControlLeft", "down": false },
            { "type": "key", "code": "KeyH", "down": true },
            { "type": "key", "code": "Enter", "down": true },
        ]);
        tally(batch.as_array().unwrap(), &mut kinds, &mut keys);
        assert_eq!(keys, ["ctrl+s", "enter"]);
        assert_eq!(Value::Object(kinds), json!({ "key": 2, "printable": 1 }));
    }

    #[test]
    fn a_view_sums_its_input() {
        let mut input = Map::new();
        view_input(
            json!([
                { "type": "move", "x": 1, "y": 1 },
                { "type": "click", "x": 1, "y": 1 },
                { "type": "scroll", "dy": 3 },
                { "type": "key", "code": "KeyA", "down": true },
                { "type": "key", "code": "KeyA", "down": false },
                { "type": "type", "text": "secret" },
            ])
            .as_array()
            .unwrap(),
            &mut input,
        );
        assert_eq!(Value::Object(input), json!({ "click": 1, "scroll": 1, "key": 2 }));
    }

    #[test]
    fn a_long_detail_gives_up_file_names_first() {
        let names: Vec<String> = (0..40).map(|i| format!("{i:03}-{}", "x".repeat(150))).collect();
        let detail = clean_detail(
            "transfer",
            &json!({ "op": "get", "files": names, "count": 40, "bytes": 5, "folder": "Downloads" }),
        );
        assert!(detail.to_string().len() <= MAX_DETAIL);
        let files = detail["files"].as_array().unwrap();
        assert!(!files.is_empty() && files.len() < MAX_FILES);
        assert!(files.iter().all(|f| f.as_str().unwrap().chars().count() <= MAX_FILE_NAME));
        assert_eq!(detail["count"], 40);
        // A listing keeps a count, no names.
        let listed = clean_detail(
            "transfer",
            &json!({ "op": "list", "files": ["a.txt"], "count": 1, "folder": "Documents" }),
        );
        assert_eq!(listed, json!({ "op": "list", "count": 1, "folder": "Documents" }));
    }

    #[test]
    fn pages_newest_first_by_cursor_and_filters() {
        let (_dir, store) = store();
        let audit = Audit::start(store);
        for i in 0..5 {
            let mut who = who();
            if i % 2 == 1 {
                who.client_id = Some("c2".into());
            }
            audit.note(who, "screenshot", Outcome::Ok, None, json!({}));
        }
        audit.note(Who::owner(Some(&json!({ "id": "c2", "name": "Lan" }))), "access", Outcome::Ok, None, json!({ "change": "blocked" }));
        audit.flush();
        let page = audit.page(&Query { limit: 2, ..Query::default() }).unwrap();
        let ids: Vec<i64> = page["entries"].as_array().unwrap().iter().map(|e| e["id"].as_i64().unwrap()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids[0] > ids[1]);
        assert_eq!(page["next"], json!(ids[1]));
        assert_eq!(page["retentionDays"], 30);
        assert_eq!(page["dropped"], 0);
        let rest = audit
            .page(&Query { limit: 200, before: Some(ids[1]), ..Query::default() })
            .unwrap();
        assert_eq!(rest["entries"].as_array().unwrap().len(), 4);
        assert_eq!(rest["next"], Value::Null);
        // A client's rows include the owner's changes about it.
        let theirs = audit
            .page(&Query { limit: 200, client: Some("c2".into()), ..Query::default() })
            .unwrap();
        let kinds: Vec<&str> = theirs["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["access", "screenshot", "screenshot"]);
        let access = &theirs["entries"][0];
        assert_eq!((access["actor"].as_str(), access["via"].as_str()), (Some("owner"), Some("local")));
        let by_kind = audit
            .page(&Query { limit: 200, kind: Some("access".into()), ..Query::default() })
            .unwrap();
        assert_eq!(by_kind["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn clearing_leaves_one_note() {
        let (_dir, store) = store();
        let audit = Audit::start(store);
        for _ in 0..3 {
            audit.note(who(), "screenshot", Outcome::Ok, None, json!({}));
        }
        assert_eq!(audit.clear().unwrap(), 3);
        let entries = all(&audit);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["kind"], "audit-cleared");
        assert_eq!(entries[0]["actor"], "owner");
        assert_eq!(entries[0]["detail"], json!({ "rows": 3 }));
    }

    #[test]
    fn prunes_by_age_and_by_count() {
        let (_dir, store) = store();
        let row = |at: i64| to_row(Entry {
            at,
            who: who(),
            kind: "screenshot",
            outcome: Outcome::Ok,
            code: None,
            detail: json!({}),
        });
        let now = now_ms();
        let old: Vec<AuditRow> = (0..3).map(|i| row(now - RETENTION_MS - 1000 - i)).collect();
        let fresh: Vec<AuditRow> = (0..10).map(|i| row(now - i)).collect();
        store.audit_insert(&old).unwrap();
        store.audit_insert(&fresh).unwrap();
        assert_eq!(store.audit_prune(now - RETENTION_MS, 50_000).unwrap(), 3);
        // At most 4 rows: the newest 4 stay.
        assert_eq!(store.audit_prune(now - RETENTION_MS, 4).unwrap(), 6);
        let (rows, _) = store.audit_page(None, 200, None, None).unwrap();
        assert_eq!(rows.len(), 4);
        let newest: i64 = rows.iter().map(|r| r.id).max().unwrap();
        assert!(rows.iter().all(|r| r.id > newest - 4));
    }

    #[test]
    fn a_full_channel_drops_and_counts_never_blocks() {
        let (_dir, store) = store();
        let (audit, receiver) = Audit::detached(store.clone(), 4);
        for _ in 0..10 {
            audit.note(who(), "screenshot", Outcome::Ok, None, json!({}));
        }
        assert_eq!(audit.dropped(), 6);
        // The writer catches up with what it holds; nothing more was lost.
        let inner = audit.inner.clone();
        drop(audit);
        write_rows(&inner, &receiver);
        assert_eq!(store.audit_page(None, 200, None, None).unwrap().0.len(), 4);
        assert_eq!(inner.dropped.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn a_failed_write_is_counted() {
        let (_dir, store) = store();
        let (audit, receiver) = Audit::detached(store.clone(), 16);
        store.audit_break_for_test();
        audit.note(who(), "screenshot", Outcome::Ok, None, json!({}));
        audit.note(who(), "wait", Outcome::Ok, None, json!({}));
        let inner = audit.inner.clone();
        drop(audit);
        write_rows(&inner, &receiver);
        assert_eq!(inner.dropped.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn accessibility_reads_add_up_per_client_and_window() {
        let (_dir, store) = store();
        let audit = Audit::start(store);
        for _ in 0..4 {
            audit.ax_read(who(), "m:42", Some("com.apple.calculator"));
        }
        audit.ax_read(who(), "m:43", Some("com.apple.TextEdit"));
        let mut other = who();
        other.client_id = Some("c2".into());
        audit.ax_read(other, "m:42", Some("com.apple.calculator"));
        audit.flush_reads(true);
        let entries = all(&audit);
        assert_eq!(entries.len(), 3);
        let mut counts: Vec<u64> = entries.iter().map(|e| e["detail"]["count"].as_u64().unwrap()).collect();
        counts.sort();
        assert_eq!(counts, [1, 1, 4]);
        assert!(entries.iter().all(|e| e["kind"] == "ax-read"));
    }

    #[test]
    fn outcomes_follow_the_status() {
        assert_eq!(Outcome::of_status(200), Outcome::Ok);
        assert_eq!(Outcome::of_status(403), Outcome::Refused);
        assert_eq!(Outcome::of_status(409), Outcome::Refused);
        assert_eq!(Outcome::of_status(423), Outcome::Refused);
        assert_eq!(Outcome::of_status(502), Outcome::Error);
        assert_eq!(Outcome::of_status(404), Outcome::Error);
    }

    #[test]
    fn the_query_is_checked() {
        let params = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        assert_eq!(Query::parse(&params(&[])).unwrap().limit, 100);
        assert!(Query::parse(&params(&[("limit", "0")])).is_err());
        assert!(Query::parse(&params(&[("limit", "201")])).is_err());
        assert!(Query::parse(&params(&[("kind", "nope")])).is_err());
        assert!(Query::parse(&params(&[("before", "x")])).is_err());
        let query = Query::parse(&params(&[("before", "9"), ("client", "c1"), ("kind", "view")])).unwrap();
        assert_eq!((query.before, query.client.as_deref(), query.kind.as_deref()), (Some(9), Some("c1"), Some("view")));
    }
}
