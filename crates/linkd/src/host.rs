//! Hosting: being the computer others control. This
//! machine has an App ID and a password the user can copy or change; controllers sign in with
//! the password or a client key and are listed as clients, each with its own permissions; a new
//! computer may ask first. It listens on Tailscale.
//!
//! It is also reached through its own Public Exposure relay (relay.rs) once RealTimeX has signed
//! it in; its App ID is then the relay's exposure ID.
//!
//! "Allow my own computers" admits a controller that proves the same RealTimeX account
//! (identity.rs) without asking; without this computer's account it behaves like "Ask me".

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::Backend;
use crate::clients::{normalize_permissions, Authenticated, ClientStore, ConnectRequest, KeyState};
use crate::error::ApiError;
use crate::identity::IdentityVerifier;
use crate::password;
use crate::relay::Relay;
use crate::requests::{NewRequest, Poll, RequestStore};
use crate::router::{self, Listener};
use crate::secret_box::SecretBox;
use crate::store::Store;
use crate::tailscale::{self, Tailnet};
use crate::util::{iso_now, now_ms};

pub const DEFAULT_PORT: u16 = 7420;
/// Several hosts can share a machine (the installed app and a dev build): each takes the first
/// free port; controllers scan the same range.
pub const PORT_RANGE: u16 = 10;
const RECHECK_INTERVAL: Duration = Duration::from_secs(60);
pub const NEW_COMPUTER_POLICIES: [&str; 3] = ["ask", "own-account", "password-only"];
const DEFAULT_NEW_COMPUTER_POLICY: &str = "ask";
/// A saved client's first request after this long without one is a new session (ADR-2611 §3).
const SESSION_GAP_MS: i64 = 10 * 60 * 1000;
/// macOS permissions are read in a fresh helper process (a grant reaches only processes started
/// after it); the window asks for the overview every few seconds, so the answer is kept this long.
const PERMISSIONS_FOR: Duration = Duration::from_secs(10);
/// The window's light/dark theme as RealTimeX set it (absent: follow the OS).
const APPEARANCE_KEY: &str = "appearance";

pub mod keys {
    pub const PASSWORD: &str = "password";
    pub const FALLBACK_APP_ID: &str = "fallback-app-id";
    pub const SETTINGS: &str = "settings";
}

pub struct Settings {
    pub default_permissions: Value,
    pub allow_incoming: bool,
    pub new_computer_policy: String,
}

struct State {
    password: String,
    password_changed_at: Option<String>,
    fallback_app_id: String,
    default_permissions: Value,
    allow_incoming: bool,
    new_computer_policy: String,
    last_error: Option<String>,
    helper_error: Option<String>,
    tailscale: Option<Tailnet>,
    listening: Option<String>,
    port: u16,
    session: Option<Value>,
    stopped: bool,
}

/// A sign-in's answer: the HTTP status and body POST /connect sends.
pub struct Admitted {
    pub status: u16,
    pub body: Value,
    pub wrong_password: bool,
    pub retry_after: Option<u64>,
}

impl Admitted {
    fn refuse(status: u16, code: &str, error: &str) -> Self {
        Self {
            status,
            body: json!({ "error": error, "code": code }),
            wrong_password: false,
            retry_after: None,
        }
    }
}

pub struct Host {
    pub store: Arc<Store>,
    secrets: SecretBox,
    pub clients: ClientStore,
    pub requests: RequestStore,
    pub backend: Arc<Backend>,
    state: Mutex<State>,
    listener: tokio::sync::Mutex<Option<Listener>>,
    /// This computer's own Public Exposure (relay.rs).
    pub relay: Arc<Relay>,
    /// Link's own sign-in to the account (signin.rs).
    pub signin: Arc<crate::signin::SignIn>,
    /// Link's own updates (update.rs).
    pub updater: Arc<crate::update::Updater>,
    identity: IdentityVerifier,
    reconciling: tokio::sync::Mutex<()>,
    last_active: Mutex<HashMap<String, i64>>,
    permissions: Mutex<Option<(std::time::Instant, Value)>>,
    base_port: u16,
    me: Weak<Host>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Host {
    pub fn new(
        store: Arc<Store>,
        secrets: SecretBox,
        backend: Arc<Backend>,
        base_port: u16,
        log: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me: &Weak<Host>| {
            let created = log.clone();
            let settled = log.clone();
            Host {
                clients: ClientStore::new(store.clone()),
                requests: RequestStore::new(
                    Box::new(move |request| {
                        let kind = if request.same_account {
                            "same account"
                        } else {
                            "other"
                        };
                        created(&format!(
                            "[remote-control] {} asks to connect ({kind})",
                            request.client_name
                        ));
                    }),
                    Box::new(move |request| {
                        settled(&format!(
                            "[remote-control] request from {}: {}",
                            request.client_name, request.status
                        ));
                    }),
                ),
                store,
                secrets,
                backend,
                state: Mutex::new(State {
                    password: String::new(),
                    password_changed_at: None,
                    fallback_app_id: String::new(),
                    default_permissions: json!({ "view": true, "control": true }),
                    allow_incoming: false,
                    new_computer_policy: DEFAULT_NEW_COMPUTER_POLICY.into(),
                    last_error: None,
                    helper_error: None,
                    tailscale: None,
                    listening: None,
                    port: base_port,
                    session: None,
                    stopped: true,
                }),
                listener: tokio::sync::Mutex::new(None),
                relay: Relay::new(me.clone()),
                signin: Arc::new(crate::signin::SignIn::new(me.clone())),
                updater: crate::update::Updater::new(me.clone()),
                identity: IdentityVerifier::new(),
                reconciling: tokio::sync::Mutex::new(()),
                last_active: Mutex::new(HashMap::new()),
                permissions: Mutex::new(None),
                base_port,
                me: me.clone(),
                log,
            }
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    // --- identity -----------------------------------------------------------

    /// This computer's App ID, Link's own: its relay exposure ID once it has one (relay.rs), else
    /// the one it made for itself.
    pub fn app_id(&self) -> String {
        self.relay
            .exposure_id()
            .unwrap_or_else(|| self.state().fallback_app_id.clone())
    }

    /// The App ID Link had before its relay exposure (the one it made for itself): controllers
    /// that saved it find Link by it and move to the current one.
    fn former_app_ids(&self) -> Vec<String> {
        let current = self.app_id();
        let fallback = self.state().fallback_app_id.clone();
        if fallback.is_empty() || fallback == current {
            Vec::new()
        } else {
            vec![fallback]
        }
    }

    pub fn identity(&self) -> Value {
        // Controllers mark only "dev" (a development build); the daemon is an installed app.
        let mut identity =
            json!({ "appId": self.app_id(), "machine": machine_name(), "channel": "app" });
        let former = self.former_app_ids();
        if !former.is_empty() {
            identity["formerAppIds"] = json!(former);
        }
        identity
    }

    /// A secret sealed with this computer's key (for the store).
    pub fn seal_secret(&self, text: &str) -> anyhow::Result<Value> {
        self.secrets.seal(text)
    }

    pub fn open_secret(&self, sealed: &Value) -> Option<String> {
        self.secrets.open(sealed)
    }

    pub fn password(&self) -> String {
        self.state().password.clone()
    }

    pub fn settings(&self) -> Settings {
        let state = self.state();
        Settings {
            default_permissions: state.default_permissions.clone(),
            allow_incoming: state.allow_incoming,
            new_computer_policy: state.new_computer_policy.clone(),
        }
    }

    /// This computer's own Public Exposure (relay.rs): { state, url, detail }.
    pub fn public_state(&self) -> Value {
        self.relay.state()
    }

    pub fn load_identity(&self) -> Result<(), ApiError> {
        let settings = self.store.get(keys::SETTINGS).unwrap_or(json!({}));
        {
            let mut state = self.state();
            if let Some(permissions) = settings.get("defaultPermissions").filter(|v| v.is_object())
            {
                state.default_permissions = normalize_permissions(permissions);
            }
            state.allow_incoming = settings.get("allowIncoming") == Some(&Value::Bool(true));
            state.new_computer_policy = settings
                .get("newComputerPolicy")
                .and_then(Value::as_str)
                .filter(|policy| NEW_COMPUTER_POLICIES.contains(policy))
                .unwrap_or(DEFAULT_NEW_COMPUTER_POLICY)
                .to_string();
        }
        let mut fallback = self
            .store
            .get(keys::FALLBACK_APP_ID)
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        if fallback.is_empty() {
            let number =
                10_000_000 + u32::from_le_bytes(crate::util::random_bytes::<4>()) % 90_000_000;
            fallback = format!("rc{number}");
            self.store.set(keys::FALLBACK_APP_ID, &json!(fallback))?;
        }
        self.state().fallback_app_id = fallback;
        let stored = self.store.get(keys::PASSWORD);
        let saved = stored
            .as_ref()
            .and_then(|s| s.get("ciphertext"))
            .and_then(Value::as_str)
            .and_then(|ciphertext| self.secrets.decrypt(ciphertext))
            .filter(|password| !password.is_empty());
        match (saved, stored) {
            (Some(password), Some(stored)) => {
                let generation = stored
                    .get("generation")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                self.clients
                    .key_generation
                    .store(generation, std::sync::atomic::Ordering::SeqCst);
                let mut state = self.state();
                state.password = password;
                state.password_changed_at = stored
                    .get("changedAt")
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            _ => self.save_password(&password::generate(), false)?,
        }
        Ok(())
    }

    fn save_password(&self, password: &str, bump_generation: bool) -> Result<(), ApiError> {
        let ciphertext = self
            .secrets
            .encrypt(password)
            .map_err(|_| ApiError::internal("Could not encrypt the Remote Control password"))?;
        let current = self
            .clients
            .key_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        let generation = current + i64::from(bump_generation);
        let changed_at = iso_now();
        self.store.set(
            keys::PASSWORD,
            &json!({ "ciphertext": ciphertext, "changedAt": changed_at, "generation": generation }),
        )?;
        self.clients
            .key_generation
            .store(generation, std::sync::atomic::Ordering::SeqCst);
        let mut state = self.state();
        state.password = password.to_string();
        state.password_changed_at = Some(changed_at);
        Ok(())
    }

    /// New password: every client must sign in again with it; the running session ends.
    pub async fn change_password(&self, password: Option<&str>) -> Result<(), ApiError> {
        let next = match password {
            Some(password) => password::validate(password)?,
            None => password::generate(),
        };
        self.save_password(&next, true)?;
        self.clients.sign_out_all()?;
        self.end_streams(|_| true).await;
        let _ = self.backend.stop().await;
        self.state().session = None;
        Ok(())
    }

    // --- sign-in ------------------------------------------------------------

    /// POST /connect (ADR-2611 §1.3), first match wins: client key, password, password-only
    /// refusal, else a pending request. `password_blocked`: too many wrong passwords lately
    /// (router.rs), which refuses the password branch only.
    pub async fn admit(
        &self,
        body: &Value,
        address: &str,
        via: &str,
        password_blocked: bool,
    ) -> Result<Admitted, ApiError> {
        if !self.state().allow_incoming {
            return Ok(Admitted::refuse(
                403,
                "not-accepting",
                "This computer does not accept remote control",
            ));
        }
        let text = |key: &str| {
            body.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let client = body
            .get("client")
            .filter(|c| c.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let client_id = match client.get("id") {
            Some(Value::String(id)) => id.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let actor = if client.get("actor").and_then(Value::as_str) == Some("agent") {
            "agent"
        } else {
            "person"
        };
        let who = Who {
            client_id,
            name: client.get("name").cloned(),
            machine: client.get("machine").cloned(),
            actor,
            address: address.to_string(),
            via: via.to_string(),
        };
        let client_key = text("clientKey");
        if !client_key.is_empty() {
            return Ok(match self.clients.by_client_key(&client_key) {
                Some(KeyState::Ok(found)) => {
                    let id = found
                        .get("clientId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.admit_client(
                        &Who {
                            client_id: id,
                            ..who
                        },
                        "client-key",
                        None,
                        false,
                    )?
                }
                Some(KeyState::Revoked) => Admitted::refuse(
                    403,
                    "access-revoked",
                    "This computer no longer accepts this client",
                ),
                // Reset by a password change, or unknown (removed): the controller recovers.
                _ => Admitted::refuse(
                    401,
                    "credential-reset",
                    "Sign in again: this computer's password changed",
                ),
            });
        }
        let supplied = text("password");
        if !supplied.is_empty() {
            if password_blocked {
                // As the router's other 429s say it.
                return Ok(Admitted {
                    status: 429,
                    body: json!({ "error": "Too many failed attempts" }),
                    wrong_password: false,
                    retry_after: None,
                });
            }
            if !password::same(&self.password(), &supplied) {
                let mut refused =
                    Admitted::refuse(401, "wrong-password", "Wrong App ID or password");
                refused.wrong_password = true;
                return Ok(refused);
            }
            self.requests.withdraw_client(&who.client_id);
            let request_id = text("requestId");
            if !request_id.is_empty() {
                self.requests.withdraw(&request_id);
            }
            return self.admit_client(&who, "password", None, false);
        }
        // Same account (identity.rs): in at once with "Allow my own computers", else marked so.
        let identity = text("identity");
        let account = if identity.is_empty() {
            None
        } else {
            self.identity
                .verify(
                    &self.store,
                    &identity,
                    &self.app_id(),
                    &self.relay.api_base(),
                )
                .await
        };
        if account.is_some() && self.state().new_computer_policy == "own-account" {
            return self.admit_client(&who, "account", None, false);
        }
        if self.state().new_computer_policy == "password-only" {
            return Ok(Admitted::refuse(
                401,
                "password-required",
                "This computer needs its password",
            ));
        }
        if self
            .clients
            .by_client_id(&who.client_id)
            .is_some_and(|saved| saved.get("revoked") == Some(&Value::Bool(true)))
        {
            return Ok(Admitted::refuse(
                403,
                "access-revoked",
                "This computer no longer accepts this client",
            ));
        }
        // Over the tailnet the address names its owner; over the relay nobody is named.
        let login = if via == "tailnet" {
            tailscale::login_of(address).await
        } else {
            None
        };
        let created = self.requests.create(NewRequest {
            client_id: if who.client_id.is_empty() {
                crate::util::uuid()
            } else {
                who.client_id.clone()
            },
            client_name: who.name.clone(),
            machine: who.machine.clone(),
            same_account: account.is_some(),
            verified_machine_name: account.and_then(|a| a.verified_machine_name),
            via: via.to_string(),
            remote_address: Some(address.to_string()),
            tailnet_login: login,
            // Asking again for an open request (RealTimeX sends the requestId it was given).
            request_id: Some(text("requestId")).filter(|id| !id.is_empty()),
        });
        match created {
            Ok((request, _)) => Ok(Admitted {
                status: 202,
                body: json!({
                    "code": "pending",
                    "requestId": request.id,
                    "expiresAt": request.visible()["expiresAt"],
                    "pollAfterMs": 0,
                    "sameAccount": request.same_account,
                }),
                wrong_password: false,
                retry_after: None,
            }),
            Err(error) if error.code.as_deref() == Some("rate-limited") => {
                let mut refused = Admitted::refuse(429, "rate-limited", &error.message);
                refused.retry_after = error.retry_after;
                Ok(refused)
            }
            // A request is open for this client ID and this caller didn't show its requestId.
            Err(error) if error.code.as_deref() == Some("request-conflict") => Ok(
                Admitted::refuse(409, "request-conflict", &error.message),
            ),
            Err(error) => Err(error),
        }
    }

    /// A client is in: a fresh 24 h token, and a client key when it has none for this password.
    fn admit_client(
        &self,
        who: &Who,
        admitted_by: &str,
        permissions: Option<&Value>,
        new_key: bool,
    ) -> Result<Admitted, ApiError> {
        let defaults = self.state().default_permissions.clone();
        let connected = self.clients.connect(ConnectRequest {
            client_id: &who.client_id,
            name: who.name.as_ref(),
            machine: who.machine.as_ref(),
            address: Some(&who.address),
            default_permissions: &defaults,
            permissions,
            via: &who.via,
            actor: who.actor,
            admitted_by,
            new_key,
        })?;
        let Some(connected) = connected else {
            return Ok(Admitted::refuse(
                403,
                "access-revoked",
                "This computer no longer accepts this client",
            ));
        };
        let mut body = json!({
            "token": connected.token,
            "tokenExpiresAt": connected.token_expires_at,
            "client": connected.client,
            "admittedBy": admitted_by,
        });
        if let Some(key) = connected.client_key {
            body["clientKey"] = json!(key);
        }
        if let (Some(target), Some(identity)) = (body.as_object_mut(), self.identity().as_object())
        {
            target.extend(identity.clone());
        }
        Ok(Admitted {
            status: 200,
            body,
            wrong_password: false,
            retry_after: None,
        })
    }

    pub async fn poll_request(&self, id: &str, wait_ms: u64) -> Option<Poll> {
        self.requests.poll(id, wait_ms).await
    }

    pub fn cancel_request(&self, id: &str) -> Option<String> {
        self.requests.withdraw(id)
    }

    /// Open requests for this computer's own UI.
    pub fn connection_requests(&self) -> Value {
        json!({ "requests": self.requests.pending(), "hostEmail": null })
    }

    /// Allow (control or view only) or deny; the requester's next poll claims the result.
    ///
    /// Allowing marks the request "deciding" first, so a second allow can't admit the client
    /// again and replace the credentials just approved. A request names its client ID itself: when
    /// that ID is a saved client whose key still works, the request never proved that key, so
    /// allowing it admits a new client instead of changing (and taking over) the saved one. A
    /// saved client whose key a password change reset has no working credential to protect: it
    /// comes back through a request as itself.
    pub async fn decide_connection_request(
        &self,
        reference: &str,
        decision: &str,
    ) -> Result<Value, ApiError> {
        let settled =
            || ApiError::coded(409, "request-settled", "This request was already settled");
        if decision == "deny" {
            if !self.requests.deny(reference) {
                return Err(settled());
            }
            return Ok(json!({ "status": "denied" }));
        }
        if decision != "allow-control" && decision != "allow-view" {
            return Err(ApiError::bad_request(
                "decision must be allow-control, allow-view or deny",
            ));
        }
        let request = self
            .requests
            .begin_decision(reference)
            .ok_or_else(settled)?;
        let grant = json!({ "view": true, "control": decision == "allow-control" });
        let client_id = if self.clients.has_working_key(&request.client_id) {
            crate::util::uuid()
        } else {
            request.client_id.clone()
        };
        let who = Who {
            client_id,
            name: Some(json!(request.client_name)),
            machine: Some(json!(request.machine)),
            actor: "person",
            address: request.remote_address.clone().unwrap_or_default(),
            via: request.via.clone(),
        };
        let admitted = match self.admit_client(&who, "request", Some(&grant), true) {
            Ok(admitted) => admitted,
            Err(error) => {
                self.requests.undecide(&request.reference);
                return Err(error);
            }
        };
        if admitted.status != 200 {
            self.requests.deny_decided(&request.reference);
            return Ok(json!({ "status": "denied" }));
        }
        if !self
            .requests
            .allow(&request.reference, grant, admitted.body)
        {
            return Err(settled());
        }
        Ok(json!({ "status": "allowed" }))
    }

    /// "Connected" once per session: after SESSION_GAP_MS without requests.
    fn note_active(&self, client: &Authenticated) {
        let key = format!("{}:{}", client.id(), client.actor);
        let now = now_ms();
        let last = self
            .last_active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, now)
            .unwrap_or(0);
        if now - last >= SESSION_GAP_MS {
            (self.log)(&format!(
                "[remote-control] {} ({}) connected",
                client.name(),
                client.actor
            ));
        }
    }

    /// A viewer sent a file here, pasted something or fetched files: told to the log for now.
    pub fn note_transfer(&self, client: &Value, transfer: &Value) {
        let name = client
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("A client");
        (self.log)(&format!("[remote-control] transfer by {name}: {transfer}"));
    }

    /// Every request of a signed-in client; also tracks who holds the current session.
    pub fn authenticate(&self, token: &str, address: Option<&str>) -> Option<Authenticated> {
        let client = self.clients.authenticate(token, address)?;
        self.note_active(&client);
        let started = self.backend.started();
        let mut state = self.state();
        let holder = state
            .session
            .as_ref()
            .and_then(|s| s.get("clientId"))
            .and_then(Value::as_str)
            .map(String::from);
        if !started || holder.as_deref() != Some(client.id()) {
            state.session = Some(
                json!({ "clientId": client.id(), "clientName": client.name(), "since": iso_now() }),
            );
        }
        Some(client)
    }

    pub fn token_expired(&self, token: &str) -> bool {
        self.clients.token_expired(token)
    }

    // --- settings -----------------------------------------------------------

    fn save_settings(&self) -> Result<(), ApiError> {
        let settings = self.settings();
        self.store.set(
            keys::SETTINGS,
            &json!({
                "defaultPermissions": settings.default_permissions,
                "allowIncoming": settings.allow_incoming,
                "newComputerPolicy": settings.new_computer_policy,
            }),
        )?;
        Ok(())
    }

    pub async fn set_new_computer_policy(&self, policy: &str) -> Result<(), ApiError> {
        if !NEW_COMPUTER_POLICIES.contains(&policy) {
            return Err(ApiError::bad_request(format!(
                "newComputerPolicy must be one of {}",
                NEW_COMPUTER_POLICIES.join(", ")
            )));
        }
        self.state().new_computer_policy = policy.to_string();
        self.save_settings()
    }

    pub async fn set_default_permissions(&self, permissions: &Value) -> Result<(), ApiError> {
        self.state().default_permissions = normalize_permissions(permissions);
        self.save_settings()
    }

    /// Turning incoming control off closes the listener and ends any session at once.
    pub async fn set_allow_incoming(&self, allow: bool) -> Result<(), ApiError> {
        self.state().allow_incoming = allow;
        self.save_settings()?;
        self.follow_hosting_at_login();
        if !allow {
            // Viewers are cut off first, then capture stops.
            self.end_streams(|_| true).await;
            self.close_listener().await;
            let _ = self.backend.stop().await;
            self.state().session = None;
        }
        self.reconcile().await;
        self.relay.wake();
        Ok(())
    }

    /// Hosting on means this computer should be reachable after a restart too: Link starts at
    /// login. Hosting off, nothing needs Link at login, so the item goes (autostart.rs). Turned
    /// off through POST /v1/autostart, it stays off. A copy with its own data folder
    /// (development, tests) leaves the login item alone.
    fn sync_login_item(&self) -> anyhow::Result<()> {
        if !crate::paths::is_standard() {
            return Ok(());
        }
        let hosting = self.state().allow_incoming;
        let opted_out = crate::autostart::opted_out(&self.store);
        match crate::autostart::follow(hosting, opted_out)? {
            crate::autostart::Change::Add => {
                (self.log)("[remote-control] RealTimeX Link starts at login");
            }
            crate::autostart::Change::Remove => {
                (self.log)("[remote-control] RealTimeX Link no longer starts at login");
            }
            crate::autostart::Change::Keep => {}
        }
        Ok(())
    }

    fn follow_hosting_at_login(&self) {
        if let Err(error) = self.sync_login_item() {
            (self.log)(&format!(
                "[remote-control] couldn't update starting at login: {error:#}"
            ));
        }
    }

    /// POST /v1/autostart: off is kept (the login item stays away whatever hosting does); on
    /// lets it follow hosting again.
    pub fn set_start_at_login(&self, on: bool) -> Result<(), ApiError> {
        crate::autostart::set_opted_out(&self.store, !on)?;
        self.sync_login_item()?;
        Ok(())
    }

    pub fn start_at_login_opted_out(&self) -> bool {
        crate::autostart::opted_out(&self.store)
    }

    /// Light, dark or system (the OS's, the default): RealTimeX's theme, which it posts on start
    /// and when it changes (POST /v1/appearance), so Link's window looks like RealTimeX's.
    pub fn appearance(&self) -> &'static str {
        appearance(&self.store)
    }

    pub fn set_appearance(&self, theme: &str) -> Result<(), ApiError> {
        set_appearance(&self.store, theme)
    }

    // --- lifecycle ----------------------------------------------------------

    pub async fn start(self: &Arc<Self>) {
        self.state().stopped = false;
        if let Err(error) = self.load_identity() {
            self.state().last_error = Some(error.message);
        }
        self.reconcile().await;
        self.follow_hosting_at_login();
        self.relay.start();
        self.updater.start();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RECHECK_INTERVAL).await;
                let Some(host) = weak.upgrade() else { return };
                if host.state().stopped {
                    return;
                }
                host.reconcile().await;
            }
        });
    }

    /// Follow Tailscale: listen on its address, move if it changes, wait if it is down.
    pub async fn reconcile(&self) {
        let _one = self.reconciling.lock().await;
        if self.state().stopped {
            return;
        }
        // Development and tests: listen on this address instead of Tailscale's.
        let tailnet = match std::env::var("RTX_LINKD_LISTEN_ADDRESS")
            .ok()
            .filter(|a| !a.is_empty())
        {
            Some(address) => Some(Tailnet {
                address,
                dns_name: None,
            }),
            None => tailscale::detect().await,
        };
        let address = {
            let mut state = self.state();
            state.tailscale = tailnet.clone();
            if state.allow_incoming {
                tailnet.map(|t| t.address)
            } else {
                None
            }
        };
        if self.state().listening == address {
            return;
        }
        self.close_listener().await;
        let Some(address) = address else { return };
        if !self.backend.available() {
            return;
        }
        self.state().last_error = None;
        for offset in 0..PORT_RANGE {
            let port = self.base_port + offset;
            match router::start_listener(&address, port, self.me.clone(), self.log.clone()).await {
                Ok(listener) => {
                    *self.listener.lock().await = Some(listener);
                    let mut state = self.state();
                    state.port = port;
                    state.listening = Some(address.clone());
                    state.last_error = None;
                    drop(state);
                    (self.log)(&format!("[remote-control] listening on {address}:{port}"));
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    self.state().last_error = Some(format!(
                        "Ports {}–{} are all in use on {address}",
                        self.base_port,
                        self.base_port + PORT_RANGE - 1
                    ));
                }
                Err(error) => {
                    self.state().last_error = Some(error.to_string());
                    return;
                }
            }
        }
    }

    async fn close_listener(&self) {
        if let Some(listener) = self.listener.lock().await.take() {
            listener.shutdown().await;
        }
        self.state().listening = None;
    }

    pub async fn stop(&self) {
        self.state().stopped = true;
        self.relay.stop();
        self.close_listener().await;
        self.backend.dispose();
    }

    /// End the open live views of clients `matches` accepts.
    async fn end_streams(&self, matches: impl Fn(&str) -> bool) {
        if let Some(listener) = self.listener.lock().await.as_ref() {
            listener.end_streams(&matches);
        }
        if let Some(routes) = self.relay.routes() {
            routes.end_streams(&matches);
        }
    }

    async fn revalidate(&self, id: &str) {
        let listener = self.listener.lock().await.as_ref().map(Listener::shared);
        let relayed = self.relay.routes().map(|routes| routes.shared());
        for shared in listener.into_iter().chain(relayed) {
            router::revalidate_peers(&shared, |client| client == id).await;
        }
    }

    /// Ask macOS for a permission (its prompt and settings pane); the next overview reads them
    /// again instead of the kept answer.
    pub async fn request_permission(&self, name: &str) -> Result<(), ApiError> {
        *self.permissions.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.backend.request_permission(name).await
    }

    pub async fn stop_session(&self) {
        let _ = self.backend.stop().await;
        self.state().session = None;
    }

    // --- clients ------------------------------------------------------------

    pub async fn set_client_permissions(
        &self,
        id: &str,
        permissions: &Value,
    ) -> Result<Value, ApiError> {
        let client = self.clients.set_permissions(id, permissions)?;
        if client.pointer("/permissions/view") != Some(&Value::Bool(true)) {
            self.end_streams(|client| client == id).await;
        } else {
            // Still viewing, maybe without control now: its clipboard and transfers stop at once.
            self.revalidate(id).await;
        }
        Ok(client)
    }

    /// Revoking also ends the running session, which may belong to that client.
    pub async fn set_client_revoked(&self, id: &str, revoked: bool) -> Result<Value, ApiError> {
        let client = self.clients.set_revoked(id, revoked)?;
        if revoked {
            self.end_streams(|client| client == id).await;
            let _ = self.backend.stop().await;
            self.state().session = None;
        }
        Ok(client)
    }

    /// Disconnect: its session ends now; it may connect again.
    pub async fn disconnect_client(&self, id: &str) -> Result<Value, ApiError> {
        let client = self.clients.sign_out(id)?;
        self.end_streams(|client| client == id).await;
        let holder = self
            .state()
            .session
            .as_ref()
            .and_then(|s| s.get("clientId"))
            .and_then(Value::as_str)
            .map(String::from);
        if holder.as_deref() == Some(id) {
            let _ = self.backend.stop().await;
            self.state().session = None;
        }
        Ok(client)
    }

    pub async fn remove_client(&self, id: &str) -> Result<(), ApiError> {
        self.clients.remove(id)?;
        self.end_streams(|client| client == id).await;
        let _ = self.backend.stop().await;
        self.state().session = None;
        Ok(())
    }

    // --- status -------------------------------------------------------------

    async fn read_permissions(&self) -> Value {
        if !self.backend.available() {
            return Value::Null;
        }
        let cached = self
            .permissions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some((_, permissions)) = cached.filter(|(at, _)| at.elapsed() < PERMISSIONS_FOR) {
            return permissions;
        }
        match self.backend.permissions().await {
            Ok(permissions) => {
                self.state().helper_error = None;
                let permissions = permissions.unwrap_or(Value::Null);
                *self.permissions.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some((std::time::Instant::now(), permissions.clone()));
                permissions
            }
            Err(error) => {
                self.state().helper_error = Some(error.message);
                Value::Null
            }
        }
    }

    fn state_code(&self) -> &'static str {
        let available = self.backend.available();
        let state = self.state();
        if !state.allow_incoming {
            "off"
        } else if !available {
            "unsupported"
        } else if state.last_error.is_some() || state.helper_error.is_some() {
            "error"
        } else if state.tailscale.is_none() {
            "waiting-tailscale"
        } else if state.listening.is_none() {
            "starting"
        } else {
            "ready"
        }
    }

    /// Everything the settings page shows, except the password.
    pub async fn overview(&self) -> Value {
        // First: a failing helper must already count in the state below.
        let permissions = self.read_permissions().await;
        let code = self.state_code();
        let detected = self.backend.detected();
        let session_active = self.state().session.is_some() && self.backend.in_use();
        let app_id = self.app_id();
        let state = self.state();
        json!({
            "appId": app_id,
            "appIdSource": "plugin",
            "allowIncoming": state.allow_incoming,
            "publicExposure": self.public_state(),
            "state": code,
            "stateDetail": match code {
                "unsupported" => detected.reason.clone().unwrap_or_default(),
                "error" => state.last_error.clone().or_else(|| state.helper_error.clone()).unwrap_or_default(),
                _ => String::new(),
            },
            "stateDetailCode": if code == "unsupported" { json!(detected.code) } else { Value::Null },
            "machine": machine_name(),
            "tailscale": state.listening.as_ref().map(|address| json!({
                "address": address,
                "port": state.port,
                "dnsName": state.tailscale.as_ref().and_then(|t| t.dns_name.clone()),
            })),
            "session": match (&state.session, session_active) {
                (Some(session), true) => { let mut s = session.clone(); s["active"] = json!(true); s }
                _ => json!({ "active": false }),
            },
            "permissions": permissions,
            "clients": self.clients.list(),
            "defaultPermissions": state.default_permissions,
            "newComputerPolicy": state.new_computer_policy,
            "passwordChangedAt": state.password_changed_at,
        })
    }

    fn state_label(&self) -> String {
        let detected = self.backend.detected();
        let state = self.state();
        if !state.allow_incoming {
            return "Off: turn on Allow remote control".into();
        }
        if detected.binary.is_none() {
            return detected
                .reason
                .unwrap_or_else(|| "Not supported on this desktop".into());
        }
        if let Some(error) = &state.last_error {
            return format!("Error: {error}");
        }
        if state.tailscale.is_none() {
            return "Waiting for Tailscale: install it and sign in on this machine".into();
        }
        if state.listening.is_none() {
            return "Starting".into();
        }
        "Ready".into()
    }

    pub async fn status(&self) -> Value {
        let describe = |client: &Value| {
            let access = if client.get("revoked") == Some(&Value::Bool(true)) {
                "revoked"
            } else if client.pointer("/permissions/control") == Some(&Value::Bool(true)) {
                "view + control"
            } else {
                "view only"
            };
            let name = client.get("name").and_then(Value::as_str).unwrap_or("");
            let machine = client
                .get("machine")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            let used = client
                .get("lastUsedAt")
                .and_then(Value::as_str)
                .unwrap_or("never");
            format!("{name}{machine}: {access}, last used {used}")
        };
        let clients = self.clients.list();
        let label = self.state_label();
        let in_use = self.backend.in_use();
        let app_id = self.app_id();
        let state = self.state();
        json!({
            "appId": app_id,
            "appIdSource": "This computer",
            "state": label,
            "machine": machine_name(),
            "tailscale": state.listening.as_ref().map_or(String::new(), |address| {
                let dns = state.tailscale.as_ref().and_then(|t| t.dns_name.clone()).map(|d| format!(" ({d})")).unwrap_or_default();
                format!("{address}:{}{dns}", state.port)
            }),
            "publicExposure": "Not available yet",
            "desktop": desktop_label(self.backend.available()),
            "session": if in_use { "Active: an agent is connected" } else { "Idle" },
            "passwordChangedAt": state.password_changed_at.clone().unwrap_or_default(),
            "clients": if clients.is_empty() { "No clients yet".to_string() } else { clients.iter().map(describe).collect::<Vec<_>>().join("; ") },
            "updatedAt": iso_now(),
        })
    }
}

struct Who {
    client_id: String,
    name: Option<Value>,
    machine: Option<Value>,
    actor: &'static str,
    address: String,
    via: String,
}

pub fn machine_name() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is valid for its length; the name is NUL-terminated within it.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

fn desktop_label(available: bool) -> String {
    if !available {
        return "Not supported".into();
    }
    if cfg!(target_os = "linux") {
        let wayland = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
            || std::env::var_os("WAYLAND_DISPLAY").is_some();
        return if wayland {
            "GNOME on Wayland".into()
        } else {
            "X11".into()
        };
    }
    match std::env::consts::OS {
        "macos" => "macOS".into(),
        "windows" => "Windows".into(),
        other => other.into(),
    }
}

fn appearance(store: &Store) -> &'static str {
    match store.get(APPEARANCE_KEY).as_ref().and_then(Value::as_str) {
        Some("light") => "light",
        Some("dark") => "dark",
        _ => "system",
    }
}

fn set_appearance(store: &Store, theme: &str) -> Result<(), ApiError> {
    match theme {
        "light" | "dark" => store.set(APPEARANCE_KEY, &json!(theme))?,
        "system" => store.remove(APPEARANCE_KEY)?,
        _ => return Err(ApiError::bad_request("theme must be light, dark or system")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_is_what_realtimex_set_and_system_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(appearance(&store), "system");
        set_appearance(&store, "dark").unwrap();
        assert_eq!(appearance(&store), "dark");
        set_appearance(&store, "light").unwrap();
        assert_eq!(appearance(&store), "light");
        assert!(set_appearance(&store, "blue").is_err());
        assert_eq!(appearance(&store), "light");
        set_appearance(&store, "system").unwrap();
        assert_eq!(appearance(&store), "system");
        assert_eq!(store.get(APPEARANCE_KEY), None);
    }
}
