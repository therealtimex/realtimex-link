//! Controllers admitted to this computer. Each gets its own token per actor (kept only as a
//! SHA-256 hash) and its own permissions; revoking one blocks that client's ID until it is
//! allowed again. Clients are kept as the JSON objects RealTimeX saved, so its list moves over
//! unchanged.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::store::Store;
use crate::util::{base64url, iso, now_ms, parse_iso, random_bytes, sha256_hex, uuid};

pub const MAX_CLIENTS: usize = 50;
const MAX_TEXT_LENGTH: usize = 80;
const USAGE_SAVE_INTERVAL_MS: i64 = 60 * 1000;
/// Access tokens last this long on both routes (ADR-2611 §1.3).
pub const TOKEN_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// A client key: the refresh credential a controller keeps instead of the password.
pub const CLIENT_KEY_PREFIX: &str = "rtx_ck_";
const ACTORS: [&str; 2] = ["person", "agent"];
pub const STORE_KEY: &str = "clients";

type Client = Map<String, Value>;

pub fn hash_token(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

fn same_hash(a: &str, b: &str) -> bool {
    let decode = |text: &str| -> Option<Vec<u8>> {
        text.len()
            .is_multiple_of(2)
            .then(|| {
                (0..text.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
                    .collect()
            })
            .flatten()
    };
    match (decode(a), decode(b)) {
        (Some(left), Some(right)) => crate::util::same_bytes(&left, &right),
        _ => false,
    }
}

/// JavaScript's String(value || "").trim().slice(0, 80), or the fallback when empty.
fn clean_text(value: Option<&Value>, fallback: &str) -> String {
    let text = match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(true)) => "true".into(),
        _ => String::new(),
    };
    let units: Vec<u16> = text.trim().encode_utf16().take(MAX_TEXT_LENGTH).collect();
    let trimmed = String::from_utf16_lossy(&units);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}

/// Control without view makes no sense; full access (Control) includes the clipboard and files.
pub fn normalize_permissions(permissions: &Value) -> Value {
    let control = permissions.get("control") == Some(&Value::Bool(true));
    let view = control || permissions.get("view") != Some(&Value::Bool(false));
    json!({ "view": view, "control": control, "clipboard": control, "files": control })
}

fn actor_of(actor: &str) -> &'static str {
    if actor == "agent" {
        "agent"
    } else {
        "person"
    }
}

fn str_of<'a>(client: &'a Client, key: &str) -> &'a str {
    client.get(key).and_then(Value::as_str).unwrap_or("")
}

fn public(client: &Client) -> Value {
    let mut shown = client.clone();
    for key in ["tokens", "keyHash", "tokenHash"] {
        shown.remove(key);
    }
    Value::Object(shown)
}

/// Tokens saved before tokens were per actor belong to the client's last actor.
fn upgrade_tokens(mut client: Client) -> Client {
    if !client.get("tokens").is_some_and(Value::is_object) {
        client.insert("tokens".into(), json!({}));
    }
    if let Some(hash) = client
        .remove("tokenHash")
        .filter(|hash| hash.is_string() && hash != "")
    {
        let via = if str_of(&client, "tokenIssuedVia") == "public" {
            "public"
        } else {
            "tailnet"
        };
        let expires = client
            .get("tokenExpiresAt")
            .cloned()
            .filter(|v| v.is_string())
            .unwrap_or(Value::Null);
        let actor = actor_of(str_of(&client, "lastActor"));
        client["tokens"][actor] = json!({ "hash": hash, "via": via, "expiresAt": expires });
    }
    client
}

/// A token entry past its expiry. One without a readable `expiresAt` (saved before tokens
/// expired) counts as expired too: it would otherwise work forever. Its client signs in again.
fn expired(entry: &Value, now: i64) -> bool {
    entry
        .get("expiresAt")
        .and_then(Value::as_str)
        .and_then(parse_iso)
        .is_none_or(|at| at.timestamp_millis() <= now)
}

pub struct Connected {
    pub client: Value,
    pub token: String,
    pub token_expires_at: String,
    pub client_key: Option<String>,
}

pub struct ConnectRequest<'a> {
    pub client_id: &'a str,
    pub name: Option<&'a Value>,
    pub machine: Option<&'a Value>,
    pub address: Option<&'a str>,
    pub default_permissions: &'a Value,
    pub permissions: Option<&'a Value>,
    pub via: &'a str,
    pub actor: &'a str,
    pub admitted_by: &'a str,
    pub new_key: bool,
}

pub enum KeyState {
    Ok(Value),
    Revoked,
    Reset,
}

#[derive(Clone)]
pub struct Authenticated {
    pub client: Value,
    pub actor: &'static str,
}

impl Authenticated {
    pub fn id(&self) -> &str {
        self.client.get("id").and_then(Value::as_str).unwrap_or("")
    }

    pub fn name(&self) -> &str {
        self.client
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn may(&self, permission: &str) -> bool {
        self.client
            .get("permissions")
            .and_then(|p| p.get(permission))
            == Some(&Value::Bool(true))
    }

    /// The client as a request sees it, with the token's actor.
    pub fn with_actor(&self) -> Value {
        let mut shown = self.client.clone();
        shown["actor"] = json!(self.actor);
        shown
    }
}

pub struct ClientStore {
    store: Arc<Store>,
    clients: Mutex<Option<Vec<Client>>>,
    last_usage_save: AtomicI64,
    /// The host's current password generation: a password change makes older client keys stop working.
    pub key_generation: AtomicI64,
    token_ttl_ms: i64,
}

fn not_found() -> ApiError {
    ApiError::new(404, "Client not found")
}

impl ClientStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            clients: Mutex::new(None),
            last_usage_save: AtomicI64::new(0),
            key_generation: AtomicI64::new(0),
            token_ttl_ms: TOKEN_TTL_MS,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Vec<Client>) -> T) -> T {
        let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        let clients = guard.get_or_insert_with(|| match self.store.get(STORE_KEY) {
            Some(Value::Array(list)) => list
                .into_iter()
                .filter_map(|v| match v {
                    Value::Object(client) => Some(upgrade_tokens(client)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        });
        f(clients)
    }

    fn persist(&self, clients: &[Client]) -> Result<(), ApiError> {
        let list = Value::Array(clients.iter().cloned().map(Value::Object).collect());
        self.store.set(STORE_KEY, &list).map_err(ApiError::from)
    }

    fn generation(&self) -> i64 {
        self.key_generation.load(Ordering::SeqCst)
    }

    pub fn list(&self) -> Vec<Value> {
        self.with(|clients| clients.iter().map(public).collect())
    }

    /// Called once the client is admitted. A fresh token, and a client key on the first
    /// admission, after a password change, or when `new_key` asks for one. None: revoked.
    pub fn connect(&self, request: ConnectRequest<'_>) -> Result<Option<Connected>, ApiError> {
        let generation = self.generation();
        self.with(|clients| {
            let id = clean_text(Some(&json!(request.client_id)), "");
            let mut index = if id.is_empty() {
                None
            } else {
                clients.iter().position(|c| str_of(c, "clientId") == id)
            };
            if let Some(i) = index {
                if clients[i].get("revoked") == Some(&Value::Bool(true)) {
                    return Ok(None);
                }
            }
            let now = now_ms();
            let now_iso = iso(chrono::DateTime::from_timestamp_millis(now).unwrap_or_default());
            if index.is_none() {
                if clients.len() >= MAX_CLIENTS {
                    return Err(ApiError::new(409, format!("At most {MAX_CLIENTS} clients")));
                }
                let permissions = normalize_permissions(
                    request.permissions.unwrap_or(request.default_permissions),
                );
                let client = json!({
                    "id": uuid(),
                    "clientId": if id.is_empty() { uuid() } else { id.clone() },
                    "permissions": permissions,
                    "revoked": false,
                    "firstSeenAt": now_iso,
                    "admittedBy": request.admitted_by,
                    "tokens": {},
                });
                let Value::Object(client) = client else {
                    unreachable!()
                };
                clients.push(client);
                index = Some(clients.len() - 1);
            }
            let client = &mut clients[index.unwrap_or_default()];
            // An explicit grant (the owner's approval) is the current decision.
            if let Some(permissions) = request.permissions {
                client.insert("permissions".into(), normalize_permissions(permissions));
            }
            let mut client_key = None;
            let key_generation = client.get("keyGeneration").and_then(Value::as_i64);
            if request.new_key
                || str_of(client, "keyHash").is_empty()
                || key_generation != Some(generation)
            {
                let key = format!("{CLIENT_KEY_PREFIX}{}", base64url(&random_bytes::<32>()));
                client.insert("keyHash".into(), json!(hash_token(&key)));
                client.insert("keyGeneration".into(), json!(generation));
                client_key = Some(key);
            }
            let token = base64url(&random_bytes::<32>());
            client.insert(
                "name".into(),
                json!(clean_text(request.name, "Unnamed client")),
            );
            client.insert("machine".into(), json!(clean_text(request.machine, "")));
            let via = if request.via == "public" {
                "public"
            } else {
                "tailnet"
            };
            let expires = iso(
                chrono::DateTime::from_timestamp_millis(now + self.token_ttl_ms)
                    .unwrap_or_default(),
            );
            let actor = actor_of(request.actor);
            if !client.get("tokens").is_some_and(Value::is_object) {
                client.insert("tokens".into(), json!({}));
            }
            client["tokens"][actor] =
                json!({ "hash": hash_token(&token), "via": via, "expiresAt": expires });
            client.insert("tokenIssuedVia".into(), json!(via));
            client.insert("tokenExpiresAt".into(), json!(expires));
            client.insert("lastActor".into(), json!(actor));
            client.insert("lastUsedAt".into(), json!(now_iso));
            client.insert(
                "lastAddress".into(),
                request.address.map_or(Value::Null, |a| json!(a)),
            );
            let shown = public(client);
            self.persist(clients)?;
            Ok(Some(Connected {
                client: shown,
                token,
                token_expires_at: expires,
                client_key,
            }))
        })
    }

    /// The client a client key belongs to, or None for an unknown key.
    pub fn by_client_key(&self, key: &str) -> Option<KeyState> {
        if !key.starts_with(CLIENT_KEY_PREFIX) {
            return None;
        }
        let hash = hash_token(key);
        let generation = self.generation();
        self.with(|clients| {
            let client = clients
                .iter()
                .find(|c| same_hash(str_of(c, "keyHash"), &hash))?;
            if client.get("revoked") == Some(&Value::Bool(true)) {
                return Some(KeyState::Revoked);
            }
            if client.get("keyGeneration").and_then(Value::as_i64) != Some(generation) {
                return Some(KeyState::Reset);
            }
            Some(KeyState::Ok(public(client)))
        })
    }

    /// A saved client with this client id holds a client key that still works (not revoked, from
    /// the current password generation): only that key proves the client is who it says.
    pub fn has_working_key(&self, client_id: &str) -> bool {
        let id = clean_text(Some(&json!(client_id)), "");
        if id.is_empty() {
            return false;
        }
        let generation = self.generation();
        self.with(|clients| {
            clients.iter().any(|c| {
                str_of(c, "clientId") == id
                    && c.get("revoked") != Some(&Value::Bool(true))
                    && !str_of(c, "keyHash").is_empty()
                    && c.get("keyGeneration").and_then(Value::as_i64) == Some(generation)
            })
        })
    }

    /// The saved client with this self-reported client id.
    pub fn by_client_id(&self, client_id: &str) -> Option<Value> {
        let id = clean_text(Some(&json!(client_id)), "");
        if id.is_empty() {
            return None;
        }
        self.with(|clients| {
            clients
                .iter()
                .find(|c| str_of(c, "clientId") == id)
                .map(public)
        })
    }

    fn find_token(clients: &[Client], hash: &str) -> Option<(usize, &'static str)> {
        for (index, client) in clients.iter().enumerate() {
            if client.get("revoked") == Some(&Value::Bool(true)) {
                continue;
            }
            for actor in ACTORS {
                let entry_hash = client
                    .get("tokens")
                    .and_then(|t| t.get(actor))
                    .and_then(|e| e.get("hash"))
                    .and_then(Value::as_str);
                if entry_hash.is_some_and(|h| same_hash(h, hash)) {
                    return Some((index, actor));
                }
            }
        }
        None
    }

    /// The client for this token, or None. Records when and from where it was used.
    pub fn authenticate(&self, token: &str, address: Option<&str>) -> Option<Authenticated> {
        if token.is_empty() {
            return None;
        }
        let hash = hash_token(token);
        self.with(|clients| {
            let (index, actor) = Self::find_token(clients, &hash)?;
            let now = now_ms();
            if expired(&clients[index]["tokens"][actor], now) {
                return None;
            }
            let client = &mut clients[index];
            client.insert("lastUsedAt".into(), json!(crate::util::iso_now()));
            client.insert(
                "lastAddress".into(),
                address.map_or(Value::Null, |a| json!(a)),
            );
            let shown = public(client);
            if now - self.last_usage_save.load(Ordering::SeqCst) >= USAGE_SAVE_INTERVAL_MS {
                self.last_usage_save.store(now, Ordering::SeqCst);
                let _ = self.persist(clients);
            }
            Some(Authenticated {
                client: shown,
                actor,
            })
        })
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut Client)) -> Result<Value, ApiError> {
        self.with(|clients| {
            let client = clients
                .iter_mut()
                .find(|c| str_of(c, "id") == id)
                .ok_or_else(not_found)?;
            change(client);
            let shown = public(client);
            self.persist(clients)?;
            Ok(shown)
        })
    }

    pub fn set_permissions(&self, id: &str, permissions: &Value) -> Result<Value, ApiError> {
        self.update(id, |client| {
            client.insert("permissions".into(), normalize_permissions(permissions));
        })
    }

    /// Revoke ends the client's token now and refuses its sign-ins until allowed again.
    pub fn set_revoked(&self, id: &str, revoked: bool) -> Result<Value, ApiError> {
        self.update(id, |client| {
            client.insert("revoked".into(), json!(revoked));
            if revoked {
                client.insert("tokens".into(), json!({}));
            }
        })
    }

    /// Disconnect: end this client's sign-ins (person and agent). Not revoked.
    pub fn sign_out(&self, id: &str) -> Result<Value, ApiError> {
        self.update(id, |client| {
            client.insert("tokens".into(), json!({}));
        })
    }

    pub fn remove(&self, id: &str) -> Result<(), ApiError> {
        self.with(|clients| {
            let before = clients.len();
            clients.retain(|c| str_of(c, "id") != id);
            if clients.len() == before {
                return Err(not_found());
            }
            self.persist(clients)
        })
    }

    /// After a password change: every client must sign in again with the new one.
    pub fn sign_out_all(&self) -> Result<(), ApiError> {
        self.with(|clients| {
            for client in clients.iter_mut() {
                client.insert("tokens".into(), json!({}));
            }
            self.persist(clients)
        })
    }

    /// True for a known, not revoked client whose token has expired (it may sign in again).
    pub fn token_expired(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let hash = hash_token(token);
        self.with(|clients| {
            Self::find_token(clients, &hash)
                .is_some_and(|(index, actor)| expired(&clients[index]["tokens"][actor], now_ms()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ClientStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (dir, ClientStore::new(store))
    }

    fn connect<'a>(
        clients: &ClientStore,
        id: &'a str,
        actor: &'a str,
        defaults: &'a Value,
    ) -> Connected {
        clients
            .connect(ConnectRequest {
                client_id: id,
                name: Some(&Value::Null),
                machine: None,
                address: Some("100.64.0.2"),
                default_permissions: defaults,
                permissions: None,
                via: "tailnet",
                actor,
                admitted_by: "password",
                new_key: false,
            })
            .unwrap()
            .unwrap()
    }

    #[test]
    fn tokens_per_actor_keys_and_revoke() {
        let (_dir, clients) = store();
        let defaults = json!({ "view": true, "control": false });
        let person = connect(&clients, "ctl-1", "person", &defaults);
        assert!(person
            .client_key
            .as_deref()
            .unwrap()
            .starts_with(CLIENT_KEY_PREFIX));
        assert_eq!(person.client["name"], "Unnamed client");
        assert_eq!(
            person.client["permissions"],
            json!({ "view": true, "control": false, "clipboard": false, "files": false })
        );
        assert!(person.client.get("tokens").is_none());
        // The agent signing in keeps the person's token working.
        let agent = connect(&clients, "ctl-1", "agent", &defaults);
        assert!(agent.client_key.is_none());
        assert_eq!(
            clients.authenticate(&person.token, None).unwrap().actor,
            "person"
        );
        assert_eq!(
            clients.authenticate(&agent.token, None).unwrap().actor,
            "agent"
        );
        let key = person.client_key.unwrap();
        assert!(matches!(clients.by_client_key(&key), Some(KeyState::Ok(_))));
        // A password change resets keys.
        clients.key_generation.store(1, Ordering::SeqCst);
        assert!(matches!(clients.by_client_key(&key), Some(KeyState::Reset)));
        let id = person.client["id"].as_str().unwrap().to_string();
        clients.set_revoked(&id, true).unwrap();
        assert!(clients.authenticate(&person.token, None).is_none());
        assert!(connect_revoked(&clients));
        clients.set_revoked(&id, false).unwrap();
        clients.remove(&id).unwrap();
        assert!(clients.list().is_empty());
        assert_eq!(clients.remove(&id).unwrap_err().status, 404);
    }

    fn connect_revoked(clients: &ClientStore) -> bool {
        clients
            .connect(ConnectRequest {
                client_id: "ctl-1",
                name: None,
                machine: None,
                address: None,
                default_permissions: &json!({}),
                permissions: None,
                via: "tailnet",
                actor: "person",
                admitted_by: "password",
                new_key: false,
            })
            .unwrap()
            .is_none()
    }

    #[test]
    fn reads_what_the_javascript_store_saved() {
        let (dir, _) = store();
        let saved = Arc::new(Store::open(dir.path()).unwrap());
        // Before per-actor tokens: one tokenHash for the last actor.
        let token = "legacy-token";
        saved
            .set(
                STORE_KEY,
                &json!([{
                    "id": "a1", "clientId": "c1", "name": "Mac", "permissions": { "view": true, "control": true },
                    "revoked": false, "tokenHash": hash_token(token), "lastActor": "agent", "custom": 7,
                    "tokenExpiresAt": "2999-01-01T00:00:00.000Z"
                }]),
            )
            .unwrap();
        let clients = ClientStore::new(saved);
        let found = clients.authenticate(token, Some("100.64.0.9")).unwrap();
        assert_eq!(found.actor, "agent");
        assert!(found.may("control"));
        assert_eq!(clients.list()[0]["custom"], 7);
        clients.sign_out("a1").unwrap();
        assert!(clients.authenticate(token, None).is_none());
    }

    #[test]
    fn tokens_without_an_expiry_are_expired() {
        let (dir, _) = store();
        let saved = Arc::new(Store::open(dir.path()).unwrap());
        saved
            .set(
                STORE_KEY,
                &json!([{
                    "id": "a1", "clientId": "c1", "name": "Mac", "permissions": { "view": true },
                    "revoked": false, "tokenHash": hash_token("legacy-token"), "lastActor": "person"
                }, {
                    "id": "a2", "clientId": "c2", "name": "PC", "permissions": { "view": true },
                    "revoked": false,
                    "tokens": { "person": { "hash": hash_token("odd-token"), "expiresAt": "soon" } }
                }]),
            )
            .unwrap();
        let clients = ClientStore::new(saved);
        for token in ["legacy-token", "odd-token"] {
            assert!(clients.authenticate(token, None).is_none(), "{token}");
            // Known and expired: the controller is told to sign in again.
            assert!(clients.token_expired(token), "{token}");
        }
    }

    #[test]
    fn only_a_current_client_key_makes_a_client_id_taken() {
        let (_dir, clients) = store();
        let defaults = json!({ "view": true });
        connect(&clients, "ctl-1", "person", &defaults);
        assert!(clients.has_working_key("ctl-1"));
        assert!(!clients.has_working_key("ctl-2"));
        assert!(!clients.has_working_key(""));
        // A password change resets keys: the saved client may come back through a request.
        clients.key_generation.store(1, Ordering::SeqCst);
        assert!(!clients.has_working_key("ctl-1"));
    }
}
