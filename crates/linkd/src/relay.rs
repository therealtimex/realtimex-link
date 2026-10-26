//! This computer's Public Exposure (#2753, realtimex-link#6): the internet reaches it through
//! RealTimeX's relay (https://x.realtimex.ai/<surface>/<exposureId>) without Tailscale, while
//! RealTimeX is closed too. Link is the computer's only connector (realtimex-ai-app#2836): it
//! carries every surface, Remote Control's `d` in-process and RealTimeX's `a e h m p` to the
//! origin RealTimeX registers (origin.rs), and it adopts RealTimeX's exposure when both are signed
//! in to the same account, so there is one App ID.
//!
//! Enrolling (and recovering a lost credential) needs the signed-in account's access token: Link's
//! own sign-in (signin.rs), else the one RealTimeX hands over (POST /v1/account/token); the tunnel
//! itself needs only the connector credential enrollment returns, kept sealed in the store. The
//! wire protocol is rtx-relay/1, as RealTimeX's own connector spoke it
//! (server/utils/publicExposure/relayConnector.cjs there): binary frames over one WebSocket.
//!
//! Flow control: frames to the relay wait in a bounded queue (a slow relay holds the streams
//! back, not memory), each request body and WebSocket waits in a bounded one too, and each
//! surface has a request body limit.

mod origin;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use axum::body::Body;
use axum::extract::ConnectInfo;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::host::Host;
use crate::router::PublicRoutes;

pub use origin::Origin;

pub mod keys {
    /// This Link's relay install ID (a UUID made once, or RealTimeX's after adopting its exposure).
    pub const INSTALL_ID: &str = "relay-install-id";
    /// The enrolled exposure: { exposureId, connector: { url, protocol, websocketSubprotocol },
    /// routes: [{ surface, hostname, pathPrefix, publicBaseUrl, contentBaseUrl }],
    /// allowedPrefixes: { <surface>: [prefix] } }.
    pub const EXPOSURE: &str = "relay-exposure";
    /// The connector credential, sealed.
    pub const CREDENTIAL: &str = "relay-credential";
    /// Exposure IDs this computer had before (its own, before adopting RealTimeX's): controllers
    /// that saved one find this computer by it and move to the current one.
    pub const FORMER_IDS: &str = "relay-former-ids";
    /// { exposureId, from, at, seen }: the exposure adopted from RealTimeX, for the window's
    /// one-time "This computer now has one App ID".
    pub const ADOPTED: &str = "relay-adopted";
}

/// Every surface Link carries: published artifacts, the embed widget, webhooks, the mobile app
/// and desktop messaging go to RealTimeX (the origin); Remote Control is Link's own.
pub const SURFACES: [&str; 6] = ["a", "e", "h", "m", "p", "d"];
const REMOTE_CONTROL: &str = "d";
const PEER: &str = "p";
/// Desktop messaging carries only the peer wire: a policy may narrow it, never widen it.
const PEER_CEILING: &str = "/api/peer/v1";
const DEFAULT_API_BASE: &str = "https://marketplace-api-next.realtimex.ai";
/// The Marketplace API hosts RealTimeX may name (over https, default port). The API base is also
/// the identity tokens' issuer and where their keys come from, so any other host is ignored.
const API_HOSTS: [&str; 2] = [
    "marketplace-api-next.realtimex.ai",
    "marketplace-api.realtimex.ai",
];
/// A local Marketplace (http, any port), in debug builds only.
const DEBUG_API_HOSTS: [&str; 2] = ["127.0.0.1", "localhost"];
const PROTOCOL_NAME: &str = "rtx-relay/1";
const WEBSOCKET_SUBPROTOCOL: &str = "rtx-relay-v1";
const PROTOCOL_VERSION: u8 = 1;
const HEADER_BYTES: usize = 11;
const MAX_DATA_FRAME: usize = 64 * 1024;
const MAX_META_FRAME: usize = 32 * 1024;
const TEXT_FLAG: u8 = 1;
const MORE_FLAG: u8 = 2;
/// Frames waiting for the relay's socket. Streams wait for room, so a slow relay slows them.
const OUT_QUEUE: usize = 256;
/// Request body (or WebSocket) frames waiting for one stream's reader.
const STREAM_QUEUE: usize = 16;
/// A reader that takes nothing for this long has its stream reset (the others go on).
const STREAM_STALL: Duration = Duration::from_secs(30);
/// One WebSocket message, across MORE fragments.
const MAX_WS_MESSAGE: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The relay pings every 10 s; nothing for this long means the connection is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(35);
const RECONNECT_DELAYS: [u64; 6] = [1, 2, 5, 10, 30, 60];
const CLOSE_AUTH_FAILED: u16 = 4401;
/// Recovering by itself (the relay refused the credential, or the service wants it) at most this
/// often: past it something else keeps taking the exposure, and Link shows an error instead.
const MAX_RECOVERIES: usize = 3;
const RECOVERY_WINDOW: Duration = Duration::from_secs(60 * 60);
const CLOSE_ALREADY_ACTIVE: u16 = 4409;
const API_TIMEOUT: Duration = Duration::from_secs(60);
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

mod frame {
    pub const HELLO: u8 = 1;
    pub const OPEN: u8 = 2;
    pub const HEADERS: u8 = 3;
    pub const DATA: u8 = 4;
    pub const END: u8 = 5;
    pub const WINDOW_UPDATE: u8 = 6;
    pub const CANCEL: u8 = 7;
    pub const RESET: u8 = 8;
    pub const WS_OPEN: u8 = 9;
    pub const WS_ACCEPT: u8 = 10;
    pub const WS_DATA: u8 = 11;
    pub const WS_CLOSE: u8 = 12;
    pub const PING: u8 = 13;
    pub const PONG: u8 = 14;
    pub const GOAWAY: u8 = 15;
    pub const TRAILERS: u8 = 16;
}

/// The local paths each surface reaches when the enrollment names no policy for it, as
/// RealTimeX's connector had them (relayConnector.cjs DEFAULT_ALLOWED_PATH_PREFIXES) and the
/// Marketplace sends them (RELAY_PATH_POLICY). /api/ping answers the reachability probe.
fn default_prefixes(surface: &str) -> &'static [&'static str] {
    match surface {
        "a" => &["/ar", "/api/ping"],
        "e" => &["/embed", "/api/embed"],
        "h" => &["/wh", "/api/ping"],
        "m" => &["/mb"],
        "p" => &[PEER_CEILING],
        "d" => &["/remote-control"],
        _ => &[],
    }
}

/// The largest request body each surface takes: artifacts and the widget only post small
/// forms, the mobile app uploads files.
fn body_limit(surface: &str) -> u64 {
    const MIB: u64 = 1024 * 1024;
    match surface {
        "a" | "e" | "d" => MIB,
        "h" => 15 * MIB,
        "p" => 10 * MIB,
        "m" => 50 * MIB,
        _ => 0,
    }
}

/// An access token for the account, and the Marketplace API base.
#[derive(Clone)]
struct Account {
    token: String,
    api_base: String,
    expires_at: Option<i64>,
}

#[derive(Default)]
struct Status {
    state: &'static str,
    detail: Option<String>,
    /// Enrollment must be repeated (no exposure yet, the relay refused the credential, or the
    /// enrolled surfaces aren't the ones Link carries).
    needs_enrollment: bool,
}

pub struct Relay {
    host: Weak<Host>,
    account: Mutex<Option<Account>>,
    /// The Marketplace API base RealTimeX last named.
    api_base: Mutex<Option<String>>,
    status: Mutex<Status>,
    /// Woken when something that decides the relay changed (a token, settings, a stop).
    changed: Notify,
    /// The exposure changed under the open connection (adopted): connect again at once.
    restart: AtomicBool,
    /// Surfaces this Marketplace doesn't support yet (422 RELAY_SURFACE_UNSUPPORTED), until the
    /// daemon restarts.
    unsupported: Mutex<HashSet<String>>,
    /// RealTimeX's servers, while it runs (origin.rs).
    pub origin: origin::Registry,
    /// One adoption at a time.
    adopting: tokio::sync::Mutex<()>,
    /// When Link last recovered its exposure by itself: capped, so it never fights another
    /// connector (an older RealTimeX) for it.
    recoveries: Mutex<Vec<Instant>>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// The routes of the open connection, so revoking a client ends its relayed sessions too.
    routes: Mutex<Option<Arc<PublicRoutes>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Relay {
    pub fn new(host: Weak<Host>) -> Arc<Self> {
        Arc::new(Self {
            host,
            account: Mutex::new(None),
            api_base: Mutex::new(None),
            status: Mutex::new(Status {
                state: "off",
                ..Default::default()
            }),
            changed: Notify::new(),
            restart: AtomicBool::new(false),
            unsupported: Mutex::new(HashSet::new()),
            origin: origin::Registry::default(),
            adopting: tokio::sync::Mutex::new(()),
            recoveries: Mutex::new(Vec::new()),
            task: Mutex::new(None),
            routes: Mutex::new(None),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let relay = self.clone();
        *lock(&self.task) = Some(tokio::spawn(async move { relay.run().await }));
    }

    pub fn stop(&self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
        if let Some(routes) = lock(&self.routes).take() {
            routes.shutdown();
        }
    }

    /// The routes of the open connection, when there is one.
    pub fn routes(&self) -> Option<Arc<PublicRoutes>> {
        lock(&self.routes).clone()
    }

    /// Something that decides the relay changed: look again.
    pub fn wake(&self) {
        self.changed.notify_one();
    }

    /// RealTimeX hands over the signed-in account's access token (and its API base). It signs
    /// Link in only when Link has no sign-in of its own (signin.rs); the API base counts only then,
    /// and only when it names an allowed host (`allowed_api_base`).
    /// Ok(false): kept as an offer only.
    pub fn handover(&self, token: &str, api_base: Option<&str>) -> Result<bool> {
        let token = token.trim();
        if token.is_empty() || token.contains(char::is_whitespace) {
            bail!("token must be the account's access token");
        }
        let takes = self
            .host
            .upgrade()
            .is_some_and(|host| host.signin.takes_handover(token));
        if takes {
            if let Some(base) = api_base.and_then(allowed_api_base) {
                self.set_api_base(base);
            }
            self.set_account(token)?;
        }
        Ok(takes)
    }

    /// Another API base is another issuer: the identity keys kept for the last one go with it.
    fn set_api_base(&self, base: String) {
        let previous = self.api_base();
        *lock(&self.api_base) = Some(base.clone());
        if previous != base {
            if let Some(host) = self.host.upgrade() {
                let _ = host.store.remove(crate::identity::keys::IDENTITY_KEYS);
            }
        }
    }

    /// Sign in with this access token: this computer's account from now on.
    pub fn set_account(&self, token: &str) -> Result<()> {
        // This computer's account, for "Allow my own computers" (identity.rs), kept across restarts.
        if let (Some(host), Some(account)) =
            (self.host.upgrade(), crate::identity::account_of(token))
        {
            if host.store.get(crate::identity::keys::ACCOUNT).as_ref() != Some(&account) {
                host.store.set(crate::identity::keys::ACCOUNT, &account)?;
            }
        }
        *lock(&self.account) = Some(Account {
            token: token.to_string(),
            api_base: self.api_base(),
            expires_at: jwt_expiry(token),
        });
        self.wake();
        Ok(())
    }

    /// Signed out: forget the handed-over token.
    pub fn forget_account(&self) {
        *lock(&self.account) = None;
    }

    /// The Marketplace API base RealTimeX named (also the identity tokens' issuer).
    pub fn api_base(&self) -> String {
        lock(&self.api_base)
            .clone()
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
    }

    /// A current access token: Link's own sign-in first, else RealTimeX's unexpired handover.
    async fn account(&self) -> Option<Account> {
        let signin = self.host.upgrade().map(|host| host.signin.clone());
        if let Some(signin) = signin {
            if let Some(token) = signin.access_token().await {
                return Some(Account {
                    expires_at: jwt_expiry(&token),
                    token,
                    api_base: self.api_base(),
                });
            }
        }
        let account = lock(&self.account).clone()?;
        let expired = account
            .expires_at
            .is_some_and(|at| at <= chrono::Utc::now().timestamp() + 30);
        (!expired).then_some(account)
    }

    /// The enrolled exposure ID, when there is one: this computer's App ID.
    pub fn exposure_id(&self) -> Option<String> {
        let host = self.host.upgrade()?;
        host.store
            .get(keys::EXPOSURE)?
            .get("exposureId")?
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    }

    /// The exposure IDs this computer had before the current one.
    pub fn former_ids(&self) -> Vec<String> {
        let Some(host) = self.host.upgrade() else {
            return Vec::new();
        };
        let current = self.exposure_id();
        strings(host.store.get(keys::FORMER_IDS).as_ref())
            .into_iter()
            .filter(|id| Some(id) != current.as_ref())
            .collect()
    }

    /// For the settings page: { state, url, detail } (url: Remote Control's).
    pub fn state(&self) -> Value {
        let status = lock(&self.status);
        let url = self.host.upgrade().and_then(|host| {
            let exposure = host.store.get(keys::EXPOSURE)?;
            stored_routes(&exposure)
                .into_iter()
                .find(|route| route.get("surface").and_then(Value::as_str) == Some(REMOTE_CONTROL))?
                .get("publicBaseUrl")?
                .as_str()
                .map(str::to_string)
        });
        json!({ "state": status.state, "url": url, "detail": status.detail })
    }

    /// GET /v1/exposure (realtimex-ai-app#2836 §6): { state, appId, installId, account, surfaces:
    /// { <surface>: publicBaseUrl }, formerAppIds, error? }, and for the window: adopted, origin
    /// (RealTimeX registered) and notice (the adopted exposure, until the window showed it once).
    /// state: off (Link turned off), other-account (RealTimeX handed over another account),
    /// signed-out, connecting, online or error.
    pub fn exposure(&self) -> Value {
        let Some(host) = self.host.upgrade() else {
            return json!({ "state": "off" });
        };
        let exposure = host.store.get(keys::EXPOSURE);
        let mut surfaces = serde_json::Map::new();
        for route in exposure.as_ref().map(stored_routes).unwrap_or_default() {
            let Some(surface) = route.get("surface").and_then(Value::as_str) else {
                continue;
            };
            // Artifacts and the widget are linked on their content host, as RealTimeX did.
            let url = route
                .get("contentBaseUrl")
                .and_then(Value::as_str)
                .filter(|_| matches!(surface, "a" | "e"))
                .or_else(|| route.get("publicBaseUrl").and_then(Value::as_str));
            if let Some(url) = url {
                surfaces.insert(surface.to_string(), json!(url));
            }
        }
        let ours = host.store.get(crate::identity::keys::ACCOUNT);
        let subject = |account: Option<&Value>| {
            account
                .and_then(|a| a.get("subject"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let offered = host.signin.offered_account();
        let other_account = offered.is_some()
            && subject(ours.as_ref()).is_some()
            && subject(offered.as_ref()) != subject(ours.as_ref());
        let (state, detail) = {
            let status = lock(&self.status);
            (status.state, status.detail.clone())
        };
        let state = if !host.settings().keep_running {
            "off"
        } else if other_account {
            "other-account"
        } else {
            match state {
                "ready" => "online",
                "error" => "error",
                "not-set-up" => "signed-out",
                "off" => "off",
                _ => "connecting",
            }
        };
        let adopted = host.store.get(keys::ADOPTED);
        let adopted_now = self.adopted_id().is_some();
        let notice = adopted
            .filter(|a| adopted_now && a.get("seen") != Some(&Value::Bool(true)))
            .map(|a| json!({ "appId": a.get("exposureId"), "from": a.get("from") }));
        let mut answer = json!({
            "state": state,
            "appId": host.app_id(),
            "installId": host.store.get(keys::INSTALL_ID),
            "account": ours.as_ref().and_then(|a| a.get("email")),
            "surfaces": surfaces,
            "formerAppIds": host.former_app_ids(),
            "adopted": adopted_now,
            "origin": self.origin.registered(),
            "notice": notice,
        });
        if state == "error" {
            let message = detail.unwrap_or_default();
            let code = if message.starts_with("this exposure was taken back") {
                "recovery-capped"
            } else {
                "relay-error"
            };
            answer["error"] = json!({ "code": code, "message": message });
        }
        answer
    }

    /// The window showed "This computer now has one App ID".
    pub fn notice_seen(&self) -> Result<()> {
        let Some(host) = self.host.upgrade() else {
            return Ok(());
        };
        if let Some(mut adopted) = host.store.get(keys::ADOPTED) {
            adopted["seen"] = Value::Bool(true);
            host.store.set(keys::ADOPTED, &adopted)?;
        }
        Ok(())
    }

    fn set_status(&self, state: &'static str, detail: Option<String>) {
        let mut status = lock(&self.status);
        status.state = state;
        status.detail = detail;
    }

    /// The relay runs while "Keep RealTimeX Link running" is on; Remote Control's own routes
    /// refuse while incoming control is off.
    fn wanted(host: &Host) -> bool {
        host.settings().keep_running
    }

    /// The surfaces to enroll: all of them, but the ones this Marketplace refused.
    fn wanted_surfaces(&self) -> Vec<&'static str> {
        let unsupported = lock(&self.unsupported);
        SURFACES
            .into_iter()
            .filter(|surface| !unsupported.contains(*surface))
            .collect()
    }

    async fn run(self: Arc<Self>) {
        let mut attempt = 0usize;
        loop {
            let Some(host) = self.host.upgrade() else {
                return;
            };
            if !Self::wanted(&host) {
                self.set_status("off", None);
                drop(host);
                self.changed.notified().await;
                continue;
            }
            let refused = lock(&self.status).needs_enrollment;
            let enrolled = self.connection(&host);
            // Enrolled before #6 (only Remote Control), or a surface was added: enrolled again,
            // and until then the exposure keeps serving what it carries.
            let stale = enrolled
                .as_ref()
                .is_some_and(|connection| !connection.carries(&self.wanted_surfaces()));
            let usable = enrolled.is_some() && !refused;
            if refused || enrolled.is_none() || stale {
                match self.account().await {
                    None if usable => {}
                    None => {
                        self.set_status(
                            "not-set-up",
                            Some("Sign in to your RealTimeX account to use the relay".into()),
                        );
                        drop(host);
                        self.changed.notified().await;
                        continue;
                    }
                    Some(account) => match self.enroll(&host, &account).await {
                        Ok(()) => lock(&self.status).needs_enrollment = false,
                        Err(error) if usable => {
                            (host.log)(&format!(
                                "[relay] enrolling every surface failed, serving what is enrolled: {error:#}"
                            ));
                        }
                        Err(error) => {
                            (host.log)(&format!("[relay] enrollment failed: {error:#}"));
                            self.set_status("error", Some(format!("{error:#}")));
                            drop(host);
                            self.wait(Duration::from_secs(backoff(attempt))).await;
                            attempt += 1;
                            continue;
                        }
                    },
                }
            }
            let Some(connection) = self.connection(&host) else {
                continue;
            };
            self.set_status("connecting", None);
            let routes = match PublicRoutes::new(self.host.clone(), host.log.clone()) {
                Ok(routes) => Arc::new(routes),
                Err(error) => {
                    self.set_status("error", Some(error.to_string()));
                    drop(host);
                    self.wait(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let log = host.log.clone();
            drop(host);
            *lock(&self.routes) = Some(routes.clone());
            // This connection already uses the current exposure.
            self.restart.store(false, Ordering::SeqCst);
            let ended = self.serve(&connection, &routes).await;
            *lock(&self.routes) = None;
            routes.shutdown();
            let delay = match ended {
                Ended::Stopped => {
                    self.set_status("off", None);
                    self.changed.notified().await;
                    attempt = 0;
                    continue;
                }
                Ended::Restart => {
                    attempt = 0;
                    continue;
                }
                Ended::AuthFailed => {
                    log("[relay] the relay refused the connector credential: enrolling again");
                    lock(&self.status).needs_enrollment = true;
                    Duration::from_secs(1)
                }
                Ended::AlreadyActive => {
                    self.set_status(
                        "connecting",
                        Some("Another connection is still active".into()),
                    );
                    // A takeover (adopt) replaces the lease at once; otherwise the old lease
                    // runs out within its TTL. Short, growing waits either way.
                    Duration::from_secs(backoff(attempt))
                }
                Ended::Lost { was_ready, reason } => {
                    if was_ready {
                        attempt = 0;
                    }
                    log(&format!("[relay] disconnected: {reason}"));
                    self.set_status("connecting", Some(reason));
                    Duration::from_secs(backoff(attempt))
                }
            };
            attempt += 1;
            self.wait(delay).await;
        }
    }

    /// Sleep, or less when something changed.
    async fn wait(&self, delay: Duration) {
        let _ = tokio::time::timeout(delay, self.changed.notified()).await;
    }

    /// The stored exposure, ready to connect.
    fn connection(&self, host: &Host) -> Option<Connection> {
        let exposure = host.store.get(keys::EXPOSURE)?;
        let credential = host.open_secret(&host.store.get(keys::CREDENTIAL)?)?;
        Connection::from_stored(
            &exposure,
            host.store.get(keys::INSTALL_ID)?.as_str()?,
            credential,
        )
    }

    fn install_id(host: &Host) -> Result<String> {
        if let Some(id) = host
            .store
            .get(keys::INSTALL_ID)
            .and_then(|v| v.as_str().map(String::from))
        {
            return Ok(id);
        }
        let id = crate::util::uuid();
        host.store.set(keys::INSTALL_ID, &json!(id))?;
        Ok(id)
    }

    /// Enroll (or refresh) this computer's exposure with every surface Link carries; a lost
    /// credential is recovered first. A surface the Marketplace doesn't support yet is left out.
    async fn enroll(&self, host: &Host, account: &Account) -> Result<()> {
        let install_id = Self::install_id(host)?;
        let credential = host
            .store
            .get(keys::CREDENTIAL)
            .and_then(|sealed| host.open_secret(&sealed));
        let stored = host.store.get(keys::EXPOSURE);
        let text = |name: &str| {
            stored
                .as_ref()
                .and_then(|e| e.get(name))
                .and_then(Value::as_str)
                .map(String::from)
        };
        let (exposure_id, app_id) = (text("exposureId"), text("appId"));
        loop {
            let surfaces = self.wanted_surfaces();
            match self
                .register(
                    host,
                    account,
                    &install_id,
                    credential.clone(),
                    exposure_id.as_deref(),
                    &surfaces,
                    app_id.as_deref(),
                )
                .await
            {
                Err(EnrollFailure::Unsupported(surface))
                    if surfaces.contains(&surface.as_str()) =>
                {
                    (host.log)(&format!(
                        "[relay] the relay service doesn't support surface {surface} yet"
                    ));
                    lock(&self.unsupported).insert(surface);
                }
                Err(EnrollFailure::Unsupported(surface)) => {
                    bail!("the relay service refused surface {surface}")
                }
                Err(EnrollFailure::Other(error)) => return Err(error),
                Ok(answer) => {
                    self.save(host, &answer)?;
                    (host.log)(&format!(
                        "[relay] enrolled: {} ({})",
                        answer
                            .get("exposureId")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        surfaces.join(" ")
                    ));
                    return Ok(());
                }
            }
        }
    }

    /// POST /v1/relay-exposures; on 409 (the service wants the credential this install lost) the
    /// exposure is recovered first (its credential rotates).
    async fn register(
        &self,
        host: &Host,
        account: &Account,
        install_id: &str,
        credential: Option<String>,
        exposure_id: Option<&str>,
        surfaces: &[&str],
        app_id: Option<&str>,
    ) -> Result<Value, EnrollFailure> {
        let body = registration(install_id, credential.clone(), surfaces, app_id);
        let key = format!(
            "relay-{}",
            hex(&Sha256::digest(
                json!({ "installId": install_id, "surfaces": surfaces, "appName": "RealTimeX Link", "appVersion": env!("CARGO_PKG_VERSION") })
                    .to_string()
                    .as_bytes()
            ))
        );
        let url = format!("{}/v1/relay-exposures", account.api_base);
        match api_post(&url, &account.token, &key, &body).await {
            Ok(answer) => Ok(answer),
            Err(failure) if failure.status == 409 => {
                if !self.may_recover() {
                    return Err(EnrollFailure::Other(anyhow!(
                        "this exposure was taken back {MAX_RECOVERIES} times within an hour; \
                         another app (an older RealTimeX?) uses it"
                    )));
                }
                let exposure_id = exposure_id.ok_or_else(|| {
                    EnrollFailure::Other(anyhow!(
                        "the relay knows this install, but not its exposure"
                    ))
                })?;
                let recover_key = format!(
                    "relay-recovery-{}",
                    hex(&Sha256::digest(
                        json!({ "exposureId": exposure_id, "installId": install_id })
                            .to_string()
                            .as_bytes()
                    ))
                );
                let recovered = recover(account, exposure_id, install_id, &recover_key)
                    .await
                    .map_err(EnrollFailure::from)?;
                self.save(host, &recovered).map_err(EnrollFailure::Other)?;
                let credential = recovered
                    .pointer("/connector/credential")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let body = registration(install_id, credential, surfaces, app_id);
                api_post(&url, &account.token, &format!("{key}-recovered"), &body)
                    .await
                    .map_err(EnrollFailure::from)
            }
            Err(failure) => Err(failure.into()),
        }
    }

    /// Whether Link may recover its exposure by itself now (and counts it).
    fn may_recover(&self) -> bool {
        let mut recoveries = lock(&self.recoveries);
        recoveries.retain(|at| at.elapsed() < RECOVERY_WINDOW);
        if recoveries.len() >= MAX_RECOVERIES {
            return false;
        }
        recoveries.push(Instant::now());
        true
    }

    /// The exposure adopted from RealTimeX, while it is the current one.
    pub fn adopted_id(&self) -> Option<String> {
        let host = self.host.upgrade()?;
        let adopted = host
            .store
            .get(keys::ADOPTED)?
            .get("exposureId")?
            .as_str()?
            .to_string();
        (self.exposure_id().as_deref() == Some(adopted.as_str())).then_some(adopted)
    }

    /// POST /v1/exposure/adopt (realtimex-ai-app#2836 §6): take over RealTimeX's exposure, so
    /// this computer has one App ID and RealTimeX's published, webhook and embed URLs keep working.
    /// `account` is RealTimeX's account (its Keycloak subject) and must be Link's. Link recovers
    /// the exposure's credential with its own token for that account (it rotates: RealTimeX
    /// stopped re-registering first), enrolls it with every surface under RealTimeX's install ID
    /// and app ID, keeps it and connects. Its previous App ID becomes a former one; its own
    /// exposure is retired once the adopted one has been online. Answers { appId, formerAppIds }.
    pub async fn adopt(
        &self,
        exposure_id: &str,
        install_id: &str,
        account: &str,
    ) -> Result<Value, crate::error::ApiError> {
        use crate::error::ApiError;
        let _one = self.adopting.lock().await;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| ApiError::new(503, "RealTimeX Link is stopping"))?;
        let valid = |text: &str, max: usize| {
            !text.is_empty()
                && text.len() <= max
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        if !valid(exposure_id, 128) || !valid(install_id, 128) || !valid(account, 128) {
            return Err(ApiError::bad_request(
                "exposureId, installId and account (the Keycloak subject) are required",
            ));
        }
        // This computer's account: Link's own sign-in, or RealTimeX's handover it took.
        let ours = host.store.get(crate::identity::keys::ACCOUNT);
        let ours_subject = ours
            .as_ref()
            .and_then(|a| a.get("subject"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let token = match (ours_subject, self.account().await) {
            (Some(subject), Some(token)) if subject == account => token,
            (Some(subject), _) if subject != account => {
                let realtimex = host.signin.offered_email();
                return Err(ApiError::coded(
                    409,
                    "other-account",
                    "RealTimeX Link is signed in to another account",
                )
                .with_details(json!({
                    "linkAccount": ours.as_ref().and_then(|a| a.get("email")),
                    "realtimexAccount": realtimex,
                })));
            }
            _ => {
                return Err(ApiError::coded(
                    409,
                    "signed-out",
                    "RealTimeX Link isn't signed in; hand over RealTimeX's account first",
                ))
            }
        };
        // One RealTimeX per Link: adopting again is the same answer, another exposure is refused.
        if let Some(adopted) = self.adopted_id() {
            if adopted == exposure_id {
                return Ok(json!({ "appId": exposure_id, "formerAppIds": host.former_app_ids() }));
            }
            return Err(ApiError::coded(
                409,
                "adopted-other",
                "RealTimeX Link already carries another RealTimeX's exposure",
            )
            .with_details(json!({ "appId": adopted })));
        }
        let failed = |message: String| ApiError::coded(502, "adopt-failed", message);
        // The App ID other computers know this one by until now, and Link's own exposure.
        let previous = host.app_id();
        let own_exposure = self.exposure_id().filter(|id| id != exposure_id);
        let recovered = recover(
            &token,
            exposure_id,
            install_id,
            &format!("relay-adopt-{}", crate::util::uuid()),
        )
        .await
        .map_err(|failure| {
            if failure.status == 404 {
                ApiError::coded(404, "exposure-not-found", failure.message)
            } else {
                failed(failure.message)
            }
        })?;
        let credential = recovered
            .pointer("/connector/credential")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| failed("the relay service answered without a credential".into()))?;
        let app_id = recovered
            .get("appId")
            .and_then(Value::as_str)
            .map(str::to_string);
        // From here the exposure is Link's: its credential rotated, so RealTimeX can no longer
        // connect with its own. It is kept at once; enrolling every surface follows, and if
        // that fails now the relay loop keeps trying (the exposure counts as stale).
        let mut former = strings(host.store.get(keys::FORMER_IDS).as_ref());
        if !former.contains(&previous) {
            former.push(previous.clone());
        }
        former.retain(|id| id != exposure_id);
        let (exposure, sealed) = self.stored(&host, &recovered).map_err(ApiError::from)?;
        let mut values = vec![
            (keys::EXPOSURE, exposure),
            (keys::INSTALL_ID, json!(install_id)),
            (keys::FORMER_IDS, json!(former)),
            (
                keys::ADOPTED,
                json!({
                    "exposureId": exposure_id,
                    "from": previous,
                    "at": crate::util::iso_now(),
                    // Link's own exposure, retired once this one has been online.
                    "retire": own_exposure,
                }),
            ),
        ];
        if let Some(sealed) = sealed {
            values.push((keys::CREDENTIAL, sealed));
        }
        host.store
            .transaction(&values, &[])
            .map_err(ApiError::from)?;
        lock(&self.status).needs_enrollment = false;
        self.set_status("connecting", None);
        (host.log)(&format!(
            "[relay] adopted RealTimeX's exposure {exposure_id} (was {previous})"
        ));
        let enrolled = self
            .enroll_adopted(
                &host,
                &token,
                exposure_id,
                install_id,
                &credential,
                app_id.as_deref(),
            )
            .await;
        self.restart.store(true, Ordering::SeqCst);
        self.wake();
        if let Err(message) = enrolled {
            (host.log)(&format!(
                "[relay] enrolling the adopted exposure failed, trying again: {message}"
            ));
            return Err(ApiError::coded(
                503,
                "adopt-pending",
                format!("The exposure is Link's now; enrolling it is being retried: {message}"),
            )
            .retry_after(5));
        }
        Ok(json!({ "appId": exposure_id, "formerAppIds": host.former_app_ids() }))
    }

    /// Every surface on the adopted exposure, with its app ID (an update without it clears it).
    async fn enroll_adopted(
        &self,
        host: &Host,
        token: &Account,
        exposure_id: &str,
        install_id: &str,
        credential: &str,
        app_id: Option<&str>,
    ) -> Result<(), String> {
        let mut surfaces = self.wanted_surfaces();
        let answer = loop {
            let key = format!("relay-adopt-enroll-{}", crate::util::uuid());
            let url = format!("{}/v1/relay-exposures", token.api_base);
            let body = registration(install_id, Some(credential.to_string()), &surfaces, app_id);
            match api_post(&url, &token.token, &key, &body).await {
                Ok(answer) => break answer,
                Err(failure) => match EnrollFailure::from(failure) {
                    EnrollFailure::Unsupported(surface) if surfaces.contains(&surface.as_str()) => {
                        surfaces.retain(|s| *s != surface);
                        lock(&self.unsupported).insert(surface);
                    }
                    EnrollFailure::Unsupported(surface) => {
                        return Err(format!("the relay service refused surface {surface}"))
                    }
                    EnrollFailure::Other(error) => return Err(format!("{error:#}")),
                },
            }
        };
        if answer.get("exposureId").and_then(Value::as_str) != Some(exposure_id) {
            return Err("the relay service enrolled another exposure".into());
        }
        self.save(host, &answer)
            .map_err(|error| format!("{error:#}"))
    }

    /// The adopted exposure is online: Link's own one goes (tombstoned), once.
    fn retire_own(self: &Arc<Self>) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let Some(id) = host
            .store
            .get(keys::ADOPTED)
            .and_then(|a| a.get("retire").and_then(Value::as_str).map(str::to_string))
        else {
            return;
        };
        let relay = self.clone();
        tokio::spawn(async move {
            let Some(account) = relay.account().await else {
                return;
            };
            let url = format!("{}/v1/relay-exposures/{id}", account.api_base);
            let done = api_delete(&url, &account.token).await;
            let Some(host) = relay.host.upgrade() else {
                return;
            };
            match done {
                Ok(()) => {
                    if let Some(mut adopted) = host.store.get(keys::ADOPTED) {
                        adopted["retire"] = Value::Null;
                        let _ = host.store.set(keys::ADOPTED, &adopted);
                    }
                    (host.log)(&format!("[relay] retired Link's own exposure {id}"));
                }
                Err(error) => (host.log)(&format!(
                    "[relay] couldn't retire Link's own exposure {id} yet: {error}"
                )),
            }
        });
    }

    /// Keep an enrollment answer: the exposure (without the credential) and the sealed credential.
    fn save(&self, host: &Host, answer: &Value) -> Result<()> {
        let (exposure, sealed) = self.stored(host, answer)?;
        let mut values = vec![(keys::EXPOSURE, exposure)];
        if let Some(sealed) = sealed {
            values.push((keys::CREDENTIAL, sealed));
        }
        host.store.transaction(&values, &[])?;
        Ok(())
    }

    /// What the store keeps of an enrollment answer: (exposure, sealed credential).
    fn stored(&self, host: &Host, answer: &Value) -> Result<(Value, Option<Value>)> {
        let exposure = exposure_of(answer)?;
        let sealed = answer
            .pointer("/connector/credential")
            .and_then(Value::as_str)
            .map(|credential| host.seal_secret(credential))
            .transpose()?;
        Ok((exposure, sealed))
    }

    /// One connection, until it ends.
    async fn serve(self: &Arc<Self>, connection: &Connection, routes: &Arc<PublicRoutes>) -> Ended {
        let mut request = match connection.url.as_str().into_client_request() {
            Ok(request) => request,
            Err(error) => return Ended::lost(false, error.to_string()),
        };
        if let Ok(value) = connection.subprotocol.parse() {
            request
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", value);
        }
        let connected =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await;
        let socket = match connected {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(error)) => return Ended::lost(false, error.to_string()),
            Err(_) => return Ended::lost(false, "the relay did not answer".into()),
        };
        let (mut sink, mut source) = socket.split();
        let (data, mut outgoing) = mpsc::channel::<Message>(OUT_QUEUE);
        let (control, mut urgent) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            loop {
                // The frame loop's own answers (PONG, a refused stream) go first.
                let message = tokio::select! {
                    biased;
                    Some(message) = urgent.recv() => message,
                    Some(message) = outgoing.recv() => message,
                    else => break,
                };
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let send = Sender { data, control };
        send.urgent_meta(
            frame::HELLO,
            0,
            &json!({
                "protocol": PROTOCOL_NAME,
                "exposureId": connection.exposure_id,
                "installId": connection.install_id,
                "credential": connection.credential,
                "connectionId": crate::util::uuid(),
            }),
        );
        let mut decoder = Decoder::default();
        let mut streams: HashMap<u32, Stream> = HashMap::new();
        let mut ready = false;
        let started = Instant::now();
        let ended = loop {
            let wait = if ready {
                IDLE_TIMEOUT
            } else {
                CONNECT_TIMEOUT.saturating_sub(started.elapsed())
            };
            let message = tokio::select! {
                message = tokio::time::timeout(wait, source.next()) => message,
                () = self.changed.notified() => {
                    if self.restart.swap(false, Ordering::SeqCst) {
                        break Ended::Restart;
                    }
                    if self.host.upgrade().is_some_and(|host| Self::wanted(&host)) {
                        continue;
                    }
                    break Ended::Stopped;
                }
            };
            let message = match message {
                Err(_) => {
                    break Ended::lost(
                        ready,
                        if ready {
                            "the relay went quiet"
                        } else {
                            "the relay did not accept in time"
                        }
                        .into(),
                    )
                }
                Ok(None) => break Ended::lost(ready, "the relay closed the connection".into()),
                Ok(Some(Err(error))) => break Ended::lost(ready, error.to_string()),
                Ok(Some(Ok(message))) => message,
            };
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                Message::Close(close) => {
                    let code = close.as_ref().map(|c| u16::from(c.code)).unwrap_or(1005);
                    break match code {
                        CLOSE_AUTH_FAILED => Ended::AuthFailed,
                        CLOSE_ALREADY_ACTIVE => Ended::AlreadyActive,
                        _ => Ended::lost(
                            ready,
                            format!(
                                "the relay closed the connection ({code}{})",
                                close
                                    .map(|c| format!(": {}", c.reason))
                                    .filter(|r| r != ": ")
                                    .unwrap_or_default()
                            ),
                        ),
                    };
                }
                _ => continue,
            };
            let frames = match decoder.feed(&bytes) {
                Ok(frames) => frames,
                Err(error) => {
                    send.close(1011, "connector protocol error");
                    break Ended::lost(ready, error.to_string());
                }
            };
            let mut fatal = None;
            let mut goaway = false;
            for frame in frames {
                if !ready {
                    if frame.kind == frame::HELLO && frame.stream == 0 {
                        let hello: Value =
                            serde_json::from_slice(&frame.payload).unwrap_or_default();
                        if hello.get("protocol").and_then(Value::as_str) != Some(PROTOCOL_NAME) {
                            fatal = Some("the relay speaks another protocol".to_string());
                        } else if hello.get("accepted") != Some(&Value::Bool(true)) {
                            fatal = Some("the relay refused the connector".to_string());
                        } else {
                            ready = true;
                            self.set_status("ready", None);
                            self.retire_own();
                            if let Some(host) = self.host.upgrade() {
                                (host.log)(&format!("[relay] ready: {}", connection.exposure_id));
                            }
                        }
                    }
                    continue;
                }
                match self
                    .handle(frame, connection, routes, &send, &mut streams)
                    .await
                {
                    Ok(Handled::Go) => {}
                    Ok(Handled::GoAway) => goaway = true,
                    Err(error) => {
                        fatal = Some(error.to_string());
                        break;
                    }
                }
            }
            if let Some(reason) = fatal {
                send.close(1011, "connector protocol error");
                break Ended::lost(ready, reason);
            }
            if goaway {
                // The relay asked this connector to stop: no reconnecting until something
                // changes.
                break Ended::Stopped;
            }
        };
        for (_, stream) in streams.drain() {
            stream.task.abort();
        }
        send.close(1000, "");
        drop(send);
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        ended
    }

    /// One frame of a ready connection.
    async fn handle(
        self: &Arc<Self>,
        frame: Frame,
        connection: &Connection,
        routes: &Arc<PublicRoutes>,
        send: &Sender,
        streams: &mut HashMap<u32, Stream>,
    ) -> Result<Handled> {
        if frame.stream == 0 {
            match frame.kind {
                frame::PING => send.urgent(frame::PONG, 0, &frame.payload, 0),
                frame::GOAWAY => return Ok(Handled::GoAway),
                _ => {}
            }
            return Ok(Handled::Go);
        }
        // Finished streams answered END themselves; drop their entries now and then.
        streams.retain(|_, stream| !stream.task.is_finished());
        match frame.kind {
            frame::OPEN | frame::WS_OPEN => {
                let websocket = frame.kind == frame::WS_OPEN;
                let value: Value = serde_json::from_slice(&frame.payload)
                    .context("the relay sent a request that is not JSON")?;
                if let Some(old) = streams.remove(&frame.stream) {
                    old.task.abort();
                }
                match connection.request(&value, websocket) {
                    Ok(request) => {
                        let stream = self.open(frame.stream, request, routes.clone(), send.clone());
                        streams.insert(frame.stream, stream);
                    }
                    Err(reason) => send.refuse(frame.stream, &reason),
                }
            }
            frame::DATA | frame::WS_DATA => {
                let Some(stream) = streams.get_mut(&frame.stream) else {
                    send.refuse(frame.stream, "unknown stream");
                    return Ok(Handled::Go);
                };
                let outcome = if frame.kind == frame::DATA {
                    stream.body(frame.payload).await
                } else {
                    stream.message(frame.payload, frame.flags).await
                };
                if let Err(reason) = outcome {
                    if let Some(stream) = streams.remove(&frame.stream) {
                        stream.task.abort();
                    }
                    send.refuse(frame.stream, reason);
                }
            }
            frame::END => match streams.get_mut(&frame.stream) {
                Some(stream) => stream.input = None,
                None => send.refuse(frame.stream, "unknown stream"),
            },
            frame::WS_CLOSE => {
                // The public client closed: the origin is told, and its close answers.
                if let Some(stream) = streams.get_mut(&frame.stream) {
                    let close: Value = serde_json::from_slice(&frame.payload).unwrap_or_default();
                    let code = close
                        .get("code")
                        .and_then(Value::as_u64)
                        .and_then(|c| u16::try_from(c).ok())
                        .unwrap_or(1000);
                    if let Some(Input::Socket(tx)) = stream.input.take() {
                        let _ = tx.try_send(Message::Close(Some(
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: CloseCode::from(code),
                                reason: String::new().into(),
                            },
                        )));
                    }
                }
            }
            frame::CANCEL | frame::RESET => {
                if let Some(stream) = streams.remove(&frame.stream) {
                    stream.task.abort();
                }
            }
            // The relay's credit for what it has passed on: the bounded queue already paces.
            frame::WINDOW_UPDATE => {}
            _ => {}
        }
        Ok(Handled::Go)
    }

    /// Start one stream: Remote Control's routes in-process, everything else through the origin.
    fn open(
        self: &Arc<Self>,
        id: u32,
        request: RelayRequest,
        routes: Arc<PublicRoutes>,
        send: Sender,
    ) -> Stream {
        let limit = body_limit(&request.surface);
        if request.websocket {
            let (tx, rx) = mpsc::channel::<Message>(STREAM_QUEUE);
            let target = self.origin.target(&request.surface);
            let registry = self.origin.clone_handle();
            let task = tokio::spawn(async move {
                origin::websocket(id, request, rx, target, registry, send).await;
            });
            return Stream {
                input: Some(Input::Socket(tx)),
                received: 0,
                limit: MAX_WS_MESSAGE as u64,
                parts: Vec::new(),
                text: false,
                task,
            };
        }
        let (tx, rx) = mpsc::channel::<bytes::Bytes>(STREAM_QUEUE);
        let too_large = request.length.is_some_and(|length| length > limit);
        let task = if too_large {
            tokio::spawn(async move {
                origin::answer(
                    id,
                    &send,
                    413,
                    "application/json",
                    json!({ "error": "Request body too large" }).to_string(),
                )
                .await;
            })
        } else if request.surface == REMOTE_CONTROL {
            tokio::spawn(async move { in_process(id, request, rx, routes, send).await })
        } else {
            let target = self.origin.target(&request.surface);
            let registry = self.origin.clone_handle();
            tokio::spawn(async move {
                origin::http(id, request, rx, target, registry, send).await;
            })
        };
        Stream {
            input: Some(Input::Body(tx)),
            received: 0,
            limit,
            parts: Vec::new(),
            text: false,
            task,
        }
    }
}

/// Run one request through Remote Control's public routes and stream the answer back.
async fn in_process(
    id: u32,
    mut request: RelayRequest,
    body: mpsc::Receiver<bytes::Bytes>,
    routes: Arc<PublicRoutes>,
    send: Sender,
) {
    let stream = tokio_stream::wrappers::ReceiverStream::new(body).map(Ok::<_, std::io::Error>);
    request
        .headers
        .push(("x-forwarded-for".into(), request.client.to_string()));
    request
        .headers
        .push(("x-forwarded-proto".into(), "https".into()));
    let mut builder = axum::http::Request::builder()
        .method(request.method.as_str())
        .uri(&request.path);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    let Ok(mut http) = builder.body(Body::from_stream(stream)) else {
        send.meta(frame::RESET, id, &json!({ "error": "bad request" }))
            .await;
        return;
    };
    http.extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(request.client, 0)));
    let response = match routes.router.clone().oneshot(http).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status().as_u16();
    let headers = response_headers(response.headers());
    if !send
        .meta(
            frame::HEADERS,
            id,
            &json!({ "status": status, "headers": headers }),
        )
        .await
    {
        return;
    }
    let mut data = response.into_body().into_data_stream();
    while let Some(chunk) = data.next().await {
        match chunk {
            Ok(chunk) => {
                if !send.data(frame::DATA, id, &chunk, 0).await {
                    return;
                }
            }
            Err(error) => {
                send.meta(frame::RESET, id, &json!({ "error": error.to_string() }))
                    .await;
                return;
            }
        }
    }
    send.raw(frame::END, id, &[], 0).await;
}

/// A response's headers for HEADERS: lowercase names, repeated ones joined, hop-by-hop and
/// content-length left out.
fn response_headers(map: &axum::http::HeaderMap) -> serde_json::Map<String, Value> {
    let mut headers = serde_json::Map::new();
    for (name, value) in map {
        let name = name.as_str().to_lowercase();
        if HOP_BY_HOP.contains(&name.as_str()) || name == "content-length" {
            continue;
        }
        let Ok(value) = value.to_str() else { continue };
        let joined = match headers.get(&name).and_then(Value::as_str) {
            Some(previous) => format!("{previous}, {value}"),
            None => value.to_string(),
        };
        headers.insert(name, Value::String(joined));
    }
    headers
}

/// The registration body (POST /v1/relay-exposures).
fn registration(
    install_id: &str,
    credential: Option<String>,
    surfaces: &[&str],
    app_id: Option<&str>,
) -> Value {
    json!({
        "installId": install_id,
        // The exposure's app ID as it is: an update without it would clear it.
        "appId": app_id,
        "connectorCredential": credential,
        "runtimeChannel": "app",
        "appName": "RealTimeX Link",
        "appVersion": env!("CARGO_PKG_VERSION"),
        "machineName": crate::host::machine_name(),
        "osPlatform": os_platform(),
        "surfaces": surfaces,
    })
}

/// POST /v1/relay-exposures/{id}/recover: the exposure bound to `install_id` with a new
/// credential (only for its owner's account).
async fn recover(
    account: &Account,
    exposure_id: &str,
    install_id: &str,
    key: &str,
) -> Result<Value, ApiFailure> {
    api_post(
        &format!(
            "{}/v1/relay-exposures/{exposure_id}/recover",
            account.api_base
        ),
        &account.token,
        key,
        &json!({ "installId": install_id }),
    )
    .await
}

/// What the store keeps of an enrollment answer (without the credential): every surface's
/// public routes (the content host of artifacts and the widget too) and its path policy.
fn exposure_of(answer: &Value) -> Result<Value> {
    let exposure_id = answer
        .get("exposureId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("the relay answered without an exposure ID"))?;
    let connector = answer.get("connector").cloned().unwrap_or(json!({}));
    let url = connector
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("wss://"))
        .ok_or_else(|| anyhow!("the relay answered without a wss:// connector"))?;
    let routes: Vec<Value> = answer
        .get("surfaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| {
            s.get("surface")
                .and_then(Value::as_str)
                .is_some_and(|surface| SURFACES.contains(&surface))
        })
        .map(|s| {
            json!({
                "surface": s.get("surface"),
                "hostname": s.get("hostname"),
                "pathPrefix": s.get("pathPrefix"),
                "publicBaseUrl": s.get("publicBaseUrl"),
                "contentBaseUrl": s.get("contentBaseUrl"),
            })
        })
        .collect();
    if routes.is_empty() {
        bail!("the relay did not give this computer any route");
    }
    // An entry replaces a surface's defaults whole; a surface without one keeps them.
    let mut allowed = serde_json::Map::new();
    for entry in answer
        .get("pathAllowlist")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(surface), Some(prefix)) = (
            entry.get("surface").and_then(Value::as_str),
            entry.get("pathPrefix").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !SURFACES.contains(&surface) {
            continue;
        }
        let list = allowed
            .entry(surface.to_string())
            .or_insert_with(|| json!([]));
        if let Some(list) = list.as_array_mut() {
            list.push(json!(prefix));
        }
    }
    Ok(json!({
        "exposureId": exposure_id,
        "appId": answer.get("appId"),
        "connector": {
            "url": url,
            "protocol": connector.get("protocol"),
            "websocketSubprotocol": connector.get("websocketSubprotocol"),
        },
        "routes": routes,
        "allowedPrefixes": allowed,
    }))
}

/// The stored routes, each with its surface (an exposure enrolled before #6 had only `d`, and
/// its routes no surface).
fn stored_routes(exposure: &Value) -> Vec<Value> {
    exposure
        .get("routes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|route| {
            let mut route = route.clone();
            if route.get("surface").and_then(Value::as_str).is_none() {
                route["surface"] = json!(REMOTE_CONTROL);
            }
            route
        })
        .collect()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// "https://host/<surface>/<id>/" as (host, "/<surface>/<id>"), or None.
fn public_route(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if host.is_empty() || host.contains(['@', ':', '?', '#']) || path.contains(['?', '#']) {
        return None;
    }
    let path = format!("/{}", path.trim_end_matches('/'));
    Some((host.to_lowercase(), path))
}

struct Connection {
    exposure_id: String,
    install_id: String,
    url: String,
    subprotocol: String,
    credential: String,
    /// (surface, hostname, pathPrefix) of each public route to this computer.
    routes: Vec<(String, String, String)>,
    /// The path policy of each surface the enrollment named (others keep `default_prefixes`).
    allowed: HashMap<String, Vec<String>>,
}

pub(crate) struct RelayRequest {
    surface: String,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    client: std::net::IpAddr,
    public_host: String,
    public_prefix: String,
    /// The client's Content-Length, for the body limit before anything is read.
    length: Option<u64>,
    websocket: bool,
    /// The WebSocket subprotocols the client offers.
    protocols: Vec<String>,
}

impl Connection {
    fn from_stored(exposure: &Value, install_id: &str, credential: String) -> Option<Self> {
        let mut routes = Vec::new();
        for route in stored_routes(exposure) {
            let surface = route.get("surface")?.as_str()?.to_string();
            let prefix = route.get("pathPrefix")?.as_str()?.to_string();
            if let Some(host) = route.get("hostname").and_then(Value::as_str) {
                routes.push((surface.clone(), host.to_lowercase(), prefix.clone()));
            }
            // Artifacts and the widget are also served on the content host.
            for key in ["publicBaseUrl", "contentBaseUrl"] {
                if let Some((host, path)) = route
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(public_route)
                {
                    if path == prefix {
                        let entry = (surface.clone(), host, path);
                        if !routes.contains(&entry) {
                            routes.push(entry);
                        }
                    }
                }
            }
        }
        let allowed = match exposure.get("allowedPrefixes") {
            // Before #6: Remote Control's list.
            Some(Value::Array(list)) => HashMap::from([(
                REMOTE_CONTROL.to_string(),
                strings(Some(&Value::Array(list.clone()))),
            )]),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(surface, list)| (surface.clone(), strings(Some(list))))
                .collect(),
            _ => HashMap::new(),
        };
        Some(Connection {
            exposure_id: exposure.get("exposureId")?.as_str()?.to_string(),
            install_id: install_id.to_string(),
            url: exposure.pointer("/connector/url")?.as_str()?.to_string(),
            subprotocol: exposure
                .pointer("/connector/websocketSubprotocol")
                .and_then(Value::as_str)
                .unwrap_or(WEBSOCKET_SUBPROTOCOL)
                .to_string(),
            credential,
            routes,
            allowed,
        })
    }

    /// Whether this exposure is enrolled with exactly these surfaces.
    fn carries(&self, surfaces: &[&str]) -> bool {
        let enrolled: HashSet<&str> = self.routes.iter().map(|(s, _, _)| s.as_str()).collect();
        enrolled == surfaces.iter().copied().collect()
    }

    /// Whether `pathname` is in `surface`'s policy. An empty list (an invalid policy) reaches
    /// nothing; desktop messaging never leaves its ceiling.
    fn allows(&self, surface: &str, pathname: &str) -> bool {
        let under = |prefix: &str| {
            let prefix = prefix.trim_end_matches('/');
            !prefix.is_empty()
                && (pathname == prefix || pathname.starts_with(&format!("{prefix}/")))
        };
        let within_ceiling = |prefix: &str| {
            surface != PEER
                || prefix == PEER_CEILING
                || prefix.starts_with(&format!("{PEER_CEILING}/"))
        };
        match self.allowed.get(surface) {
            Some(list) => list
                .iter()
                .any(|prefix| within_ceiling(prefix) && under(prefix)),
            None => default_prefixes(surface).iter().any(|prefix| under(prefix)),
        }
    }

    /// The local request for an OPEN or WS_OPEN, or why the stream is refused (it alone is
    /// reset).
    fn request(&self, value: &Value, websocket: bool) -> Result<RelayRequest, String> {
        let text = |name: &str| value.get(name).and_then(Value::as_str).unwrap_or("");
        let surface = text("surface");
        let route = (
            surface.to_string(),
            text("publicHost").trim().to_lowercase(),
            text("publicPrefix").trim().to_string(),
        );
        if !self.routes.iter().any(|(s, _, _)| s == surface) {
            return Err("unauthorized surface".into());
        }
        if !self.routes.contains(&route) {
            return Err("unauthorized public route".into());
        }
        let path = text("path");
        let pathname = path.split(['?', '#']).next().unwrap_or("");
        let safe = path.starts_with('/')
            && !path.starts_with("//")
            && !pathname.split('/').any(|part| part == ".." || part == ".")
            && !path.contains('\\')
            && !path.chars().any(char::is_control);
        if !safe || !self.allows(surface, pathname) {
            return Err("unauthorized request path".into());
        }
        let method = match text("method").to_uppercase() {
            m if m.is_empty() => "GET".to_string(),
            m => m,
        };
        // Remote Control has no WebSocket routes; desktop messaging is plain GET and POST.
        if websocket && (surface == REMOTE_CONTROL || surface == PEER) {
            return Err("unauthorized request method".into());
        }
        if surface == PEER && !matches!(method.as_str(), "GET" | "POST") {
            return Err("unauthorized request method".into());
        }
        let mut headers = Vec::new();
        let mut protocols = Vec::new();
        let mut length = None;
        if let Some(map) = value.get("headers").and_then(Value::as_object) {
            for (name, raw) in map {
                let name = name.trim().to_lowercase();
                let values: Vec<String> = match raw {
                    Value::Array(list) => list
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                    Value::String(v) => vec![v.clone()],
                    _ => continue,
                };
                if name == "content-length" {
                    length = values.first().and_then(|v| v.trim().parse::<u64>().ok());
                }
                if websocket && name == "sec-websocket-protocol" {
                    protocols.extend(
                        values
                            .iter()
                            .flat_map(|v| v.split(','))
                            .map(|p| p.trim().to_string())
                            .filter(|p| !p.is_empty()),
                    );
                }
                // The forwarding headers, and RealTimeX's relay markers, are Link's to set.
                if name.is_empty()
                    || HOP_BY_HOP.contains(&name.as_str())
                    || name == "host"
                    || name == "content-length"
                    || name.starts_with("x-forwarded-")
                    || name.starts_with("x-realtimex-")
                    || (websocket && name.starts_with("sec-websocket-"))
                    || name.contains(['\r', '\n'])
                {
                    continue;
                }
                for v in values {
                    if !v.contains(['\r', '\n']) {
                        headers.push((name.clone(), v));
                    }
                }
            }
        }
        // The public client: what the relay names, never what the client claims.
        let client = text("remoteAddress")
            .parse::<std::net::IpAddr>()
            .map_err(|_| "the relay named no client address".to_string())?;
        Ok(RelayRequest {
            surface: surface.to_string(),
            method,
            path: path.to_string(),
            headers,
            client,
            public_host: route.1,
            public_prefix: route.2,
            length,
            websocket,
            protocols,
        })
    }
}

/// What a stream reads from the relay: its request body, or its WebSocket's messages.
enum Input {
    Body(mpsc::Sender<bytes::Bytes>),
    Socket(mpsc::Sender<Message>),
}

struct Stream {
    /// None after END (or the client's WS_CLOSE).
    input: Option<Input>,
    /// Request body bytes so far, against `limit`.
    received: u64,
    limit: u64,
    /// The WebSocket message being put together from MORE fragments.
    parts: Vec<u8>,
    text: bool,
    task: JoinHandle<()>,
}

impl Stream {
    /// A DATA frame: the next part of the request body.
    async fn body(&mut self, chunk: bytes::Bytes) -> Result<(), &'static str> {
        self.received += chunk.len() as u64;
        if self.received > self.limit {
            return Err("request body too large");
        }
        let Some(Input::Body(tx)) = &self.input else {
            return Ok(());
        };
        match tokio::time::timeout(STREAM_STALL, tx.send(chunk)).await {
            Ok(_) => Ok(()),
            Err(_) => Err("the request body was not read"),
        }
    }

    /// A WS_DATA frame: a whole message, or a fragment of one (MORE).
    async fn message(&mut self, chunk: bytes::Bytes, flags: u8) -> Result<(), &'static str> {
        if self.parts.is_empty() {
            self.text = flags & TEXT_FLAG != 0;
        }
        if self.parts.len() + chunk.len() > MAX_WS_MESSAGE {
            return Err("WebSocket message too large");
        }
        self.parts.extend_from_slice(&chunk);
        if flags & MORE_FLAG != 0 {
            return Ok(());
        }
        let payload = std::mem::take(&mut self.parts);
        let message = if self.text {
            match String::from_utf8(payload) {
                Ok(text) => Message::Text(text.into()),
                Err(_) => return Err("WebSocket text is not UTF-8"),
            }
        } else {
            Message::Binary(payload.into())
        };
        let Some(Input::Socket(tx)) = &self.input else {
            return Ok(());
        };
        match tokio::time::timeout(STREAM_STALL, tx.send(message)).await {
            Ok(_) => Ok(()),
            Err(_) => Err("the WebSocket was not read"),
        }
    }
}

enum Handled {
    Go,
    GoAway,
}

enum Ended {
    Stopped,
    /// The exposure changed (adopted): connect again with the new one.
    Restart,
    AuthFailed,
    AlreadyActive,
    Lost {
        was_ready: bool,
        reason: String,
    },
}

impl Ended {
    fn lost(was_ready: bool, reason: String) -> Self {
        Ended::Lost { was_ready, reason }
    }
}

/// Frames to the relay: a stream's own frames wait in order for room in the bounded queue; the
/// frame loop's answers (PONG, a refused stream) jump it.
#[derive(Clone)]
pub(crate) struct Sender {
    data: mpsc::Sender<Message>,
    control: mpsc::UnboundedSender<Message>,
}

fn encode(kind: u8, stream: u32, payload: &[u8], flags: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
    bytes.push(PROTOCOL_VERSION);
    bytes.push(kind);
    bytes.push(flags);
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn meta_payload(value: &Value) -> Vec<u8> {
    let text = value.to_string();
    if text.len() > MAX_META_FRAME {
        json!({ "error": "metadata too large" })
            .to_string()
            .into_bytes()
    } else {
        text.into_bytes()
    }
}

impl Sender {
    /// One frame, in its stream's order. False: the connection is gone.
    async fn raw(&self, kind: u8, stream: u32, payload: &[u8], flags: u8) -> bool {
        self.data
            .send(Message::Binary(encode(kind, stream, payload, flags).into()))
            .await
            .is_ok()
    }

    async fn meta(&self, kind: u8, stream: u32, value: &Value) -> bool {
        self.raw(kind, stream, &meta_payload(value), 0).await
    }

    /// DATA or WS_DATA in frames of at most 64 KiB; a WebSocket message keeps its boundary with
    /// MORE on every frame but its last (and its TEXT flag on each).
    async fn data(&self, kind: u8, stream: u32, payload: &[u8], flags: u8) -> bool {
        if payload.is_empty() {
            return self.raw(kind, stream, payload, flags).await;
        }
        let parts = payload.chunks(MAX_DATA_FRAME).count();
        for (index, part) in payload.chunks(MAX_DATA_FRAME).enumerate() {
            let more = if kind == frame::WS_DATA && index + 1 < parts {
                MORE_FLAG
            } else {
                0
            };
            if !self.raw(kind, stream, part, flags | more).await {
                return false;
            }
        }
        true
    }

    fn urgent(&self, kind: u8, stream: u32, payload: &[u8], flags: u8) {
        let _ = self
            .control
            .send(Message::Binary(encode(kind, stream, payload, flags).into()));
    }

    fn urgent_meta(&self, kind: u8, stream: u32, value: &Value) {
        self.urgent(kind, stream, &meta_payload(value), 0);
    }

    fn refuse(&self, stream: u32, reason: &str) {
        self.urgent_meta(frame::RESET, stream, &json!({ "error": reason }));
    }

    fn close(&self, code: u16, reason: &str) {
        let _ = self.control.send(Message::Close(Some(
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::from(code),
                reason: reason.to_string().into(),
            },
        )));
    }
}

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: bytes::Bytes,
}

/// Frames from the relay's byte stream (a frame may span WebSocket messages).
#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Frame>> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < HEADER_BYTES {
                break;
            }
            if self.buffer[0] != PROTOCOL_VERSION {
                bail!("unsupported relay protocol version");
            }
            let kind = self.buffer[1];
            let flags = self.buffer[2];
            let stream = u32::from_be_bytes(self.buffer[3..7].try_into().expect("4 bytes"));
            let length =
                u32::from_be_bytes(self.buffer[7..11].try_into().expect("4 bytes")) as usize;
            let limit = if kind == frame::DATA || kind == frame::WS_DATA {
                MAX_DATA_FRAME
            } else {
                MAX_META_FRAME
            };
            if length > limit {
                bail!("relay frame exceeds the limit");
            }
            if self.buffer.len() < HEADER_BYTES + length {
                break;
            }
            let payload =
                bytes::Bytes::copy_from_slice(&self.buffer[HEADER_BYTES..HEADER_BYTES + length]);
            self.buffer.drain(..HEADER_BYTES + length);
            frames.push(Frame {
                kind,
                flags,
                stream,
                payload,
            });
        }
        Ok(frames)
    }
}

struct ApiFailure {
    status: u16,
    message: String,
    /// The answer's body, for a typed refusal ({ code, surface }).
    body: Value,
}

enum EnrollFailure {
    /// 422 RELAY_SURFACE_UNSUPPORTED: this Marketplace doesn't serve that surface yet.
    Unsupported(String),
    Other(anyhow::Error),
}

impl From<ApiFailure> for EnrollFailure {
    fn from(failure: ApiFailure) -> Self {
        if failure.status == 422
            && failure.body.get("code").and_then(Value::as_str) == Some("RELAY_SURFACE_UNSUPPORTED")
        {
            let surface = failure
                .body
                .get("surface")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    // "Relay surface p is not supported by this service yet"
                    failure
                        .message
                        .strip_prefix("Relay surface ")
                        .and_then(|rest| rest.split_whitespace().next())
                        .map(str::to_string)
                });
            if let Some(surface) = surface.filter(|s| SURFACES.contains(&s.as_str())) {
                return EnrollFailure::Unsupported(surface);
            }
        }
        EnrollFailure::Other(anyhow!("{}", failure.message))
    }
}

/// POST JSON to the Marketplace API with the account's token (blocking, off the runtime).
async fn api_post(
    url: &str,
    token: &str,
    idempotency_key: &str,
    body: &Value,
) -> Result<Value, ApiFailure> {
    let (url, token, key, body) = (
        url.to_string(),
        token.to_string(),
        idempotency_key.to_string(),
        body.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(API_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Idempotency-Key", &key)
            .send_json(&body)
            .map_err(|error| ApiFailure {
                status: 0,
                message: format!("{url}: {error}"),
                body: Value::Null,
            })?;
        let status = response.status().as_u16();
        let answer: Value = response.body_mut().read_json().unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(answer);
        }
        let message = answer
            .get("message")
            .or_else(|| answer.get("detail"))
            .or_else(|| answer.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("the relay service answered {status}"));
        Err(ApiFailure {
            status,
            message,
            body: answer,
        })
    })
    .await
    .unwrap_or_else(|error| {
        Err(ApiFailure {
            status: 0,
            message: error.to_string(),
            body: Value::Null,
        })
    })
}

/// DELETE on the Marketplace API with the account's token: Ok once it is gone (404 too).
async fn api_delete(url: &str, token: &str) -> Result<(), String> {
    let (url, token) = (url.to_string(), token.to_string());
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(API_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let response = agent
            .delete(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|error| format!("{url}: {error}"))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            status => Err(format!("the relay service answered {status}")),
        }
    })
    .await
    .unwrap_or_else(|error| Err(error.to_string()))
}

/// A JWT's `exp`, unchecked (it only says when to stop using it).
fn jwt_expiry(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

fn os_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn backoff(attempt: usize) -> u64 {
    let base = RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)];
    let spread = (base as f64 * 0.2) as i64;
    let offset = if spread > 0 {
        (u64::from_le_bytes(crate::util::random_bytes::<8>()) % (2 * spread as u64 + 1)) as i64
            - spread
    } else {
        0
    };
    (base as i64 + offset).max(1) as u64
}

/// The Marketplace API base as `https://<allowed host>`, or None. Parsed, not prefix-matched:
/// "https://marketplace-api.realtimex.ai.evil.com" and "https://x@marketplace-api.realtimex.ai"
/// name other hosts (or carry user info) and are refused, as is anything with a path, query,
/// fragment or port. Loopback over http (any port) only in debug builds.
fn allowed_api_base(base: &str) -> Option<String> {
    let base = base.trim().trim_end_matches('/');
    let (scheme, authority) = base.split_once("://")?;
    if authority.is_empty()
        || authority
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@' | '\\') || c.is_whitespace())
    {
        return None;
    }
    let authority = authority.to_ascii_lowercase();
    match scheme.to_ascii_lowercase().as_str() {
        "https" if API_HOSTS.contains(&authority.as_str()) => Some(format!("https://{authority}")),
        "http" if cfg!(debug_assertions) => {
            let (host, port) = authority
                .rsplit_once(':')
                .unwrap_or((authority.as_str(), ""));
            let port_ok =
                port.is_empty() || (port.len() <= 5 && port.chars().all(|c| c.is_ascii_digit()));
            (DEBUG_API_HOSTS.contains(&host) && port_ok).then(|| format!("http://{authority}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_marketplace_hosts_are_api_bases() {
        assert_eq!(
            allowed_api_base("https://marketplace-api-next.realtimex.ai/").as_deref(),
            Some("https://marketplace-api-next.realtimex.ai")
        );
        assert_eq!(
            allowed_api_base(" HTTPS://Marketplace-API.realtimex.ai ").as_deref(),
            Some("https://marketplace-api.realtimex.ai")
        );
        for refused in [
            "http://127.0.0.1.evil.com",
            "http://127.0.0.1@evil.com",
            "https://marketplace-api.realtimex.ai.evil.com",
            "https://evil.com@marketplace-api.realtimex.ai",
            "https://marketplace-api.realtimex.ai@evil.com",
            "https://marketplace-api.realtimex.ai:8443",
            "https://marketplace-api.realtimex.ai/v1",
            "https://marketplace-api.realtimex.ai?x=1",
            "https://marketplace-api.realtimex.ai\\@evil.com",
            "http://marketplace-api.realtimex.ai",
            "https://evil.com",
            "marketplace-api.realtimex.ai",
            "",
        ] {
            assert_eq!(allowed_api_base(refused), None, "{refused}");
        }
        // A local Marketplace only in a debug build, and only loopback.
        assert_eq!(
            allowed_api_base("http://127.0.0.1:3001").is_some(),
            cfg!(debug_assertions)
        );
        assert_eq!(
            allowed_api_base("http://localhost").is_some(),
            cfg!(debug_assertions)
        );
        assert_eq!(allowed_api_base("http://127.0.0.1:x"), None);
        assert_eq!(allowed_api_base("http://10.0.0.1:3001"), None);
    }

    const ID: &str = "Hn4cR8tZ1qLs0wXe5uYb7A";

    /// An enrollment answer as the Marketplace gives it.
    fn answer() -> Value {
        json!({
            "exposureId": ID,
            "connector": { "url": "wss://relay.example/connect", "credential": "c", "websocketSubprotocol": WEBSOCKET_SUBPROTOCOL },
            "surfaces": [
                { "surface": "a", "hostname": "x.realtimex.ai", "pathPrefix": format!("/a/{ID}"), "publicBaseUrl": format!("https://x.realtimex.ai/a/{ID}/"), "contentBaseUrl": format!("https://c.realtimex.ai/a/{ID}/") },
                { "surface": "h", "hostname": "s.realtimex.ai", "pathPrefix": format!("/h/{ID}"), "publicBaseUrl": format!("https://s.realtimex.ai/h/{ID}/") },
                { "surface": "p", "hostname": "s.realtimex.ai", "pathPrefix": format!("/p/{ID}"), "publicBaseUrl": format!("https://s.realtimex.ai/p/{ID}/") },
                { "surface": "d", "hostname": "x.realtimex.ai", "pathPrefix": format!("/d/{ID}"), "publicBaseUrl": format!("https://x.realtimex.ai/d/{ID}/") },
                { "surface": "w", "hostname": "x.realtimex.ai", "pathPrefix": format!("/w/{ID}"), "publicBaseUrl": format!("https://x.realtimex.ai/w/{ID}/") },
            ],
            "pathAllowlist": [
                { "surface": "h", "pathPrefix": "/wh" },
                { "surface": "p", "pathPrefix": "/api/peer/v1" },
                { "surface": "p", "pathPrefix": "/api" },
                { "surface": "d", "pathPrefix": "/remote-control" },
            ],
        })
    }

    fn connection() -> Connection {
        Connection::from_stored(&exposure_of(&answer()).unwrap(), "i", "c".into()).unwrap()
    }

    fn open(surface: &str, host: &str, path: &str) -> Value {
        json!({
            "surface": surface,
            "publicHost": host,
            "publicPrefix": format!("/{surface}/{ID}"),
            "path": path,
            "method": "post",
            "headers": {
                "Content-Type": "application/json",
                "Content-Length": "12",
                "X-Forwarded-For": "6.6.6.6",
                "X-RealTimeX-Relay-Source": "desktop",
                "Host": "evil",
                "Connection": "close",
                "Sec-WebSocket-Protocol": "chat, json",
            },
            "remoteAddress": "203.0.113.7",
        })
    }

    #[test]
    fn requests_keep_to_each_surfaces_routes() {
        let request = connection()
            .request(
                &open("d", "X.realtimex.ai", "/remote-control/connect"),
                false,
            )
            .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/remote-control/connect");
        assert_eq!(request.client.to_string(), "203.0.113.7");
        assert_eq!(request.length, Some(12));
        assert!(request
            .headers
            .contains(&("content-type".into(), "application/json".into())));
        assert!(!request.headers.iter().any(|(n, v)| n == "host"
            || n == "connection"
            || n == "content-length"
            || n.starts_with("x-realtimex-")
            || v == "6.6.6.6"));
        for path in [
            "/api/system",
            "/remote-control/../api",
            "//evil/remote-control",
            "/remote-controlx",
        ] {
            assert!(
                connection()
                    .request(&open("d", "x.realtimex.ai", path), false)
                    .is_err(),
                "{path}"
            );
        }
        let mut other = open("d", "x.realtimex.ai", "/remote-control/hello");
        other["publicPrefix"] = json!("/d/someone-else");
        assert!(connection().request(&other, false).is_err());
        other = open("d", "x.realtimex.ai", "/remote-control/hello");
        other["remoteAddress"] = json!("");
        assert!(connection().request(&other, false).is_err());
        // A surface this exposure doesn't carry (workspace share was removed).
        assert!(connection()
            .request(&open("w", "x.realtimex.ai", "/sw"), false)
            .is_err());
        assert!(connection()
            .request(&open("e", "x.realtimex.ai", "/embed"), false)
            .is_err());
    }

    #[test]
    fn artifacts_are_served_on_the_content_host_too_with_their_default_policy() {
        let connection = connection();
        for host in ["x.realtimex.ai", "c.realtimex.ai"] {
            let request = connection
                .request(&open("a", host, "/ar/abc/index.html"), false)
                .unwrap();
            assert_eq!(request.public_host, host);
            assert_eq!(request.public_prefix, format!("/a/{ID}"));
        }
        assert!(connection
            .request(&open("a", "s.realtimex.ai", "/ar/abc"), false)
            .is_err());
        assert!(connection
            .request(&open("a", "c.realtimex.ai", "/wh/token"), false)
            .is_err());
        // The enrollment's list replaces h's defaults whole: no /api/ping.
        assert!(connection
            .request(&open("h", "s.realtimex.ai", "/wh/token"), false)
            .is_ok());
        assert!(connection
            .request(&open("h", "s.realtimex.ai", "/api/ping"), false)
            .is_err());
    }

    #[test]
    fn desktop_messaging_stays_under_its_ceiling_and_methods() {
        let connection = connection();
        assert!(connection
            .request(&open("p", "s.realtimex.ai", "/api/peer/v1/messages"), false)
            .is_ok());
        // "/api" in the policy is outside the ceiling and ignored.
        assert!(connection
            .request(&open("p", "s.realtimex.ai", "/api/system"), false)
            .is_err());
        let mut put = open("p", "s.realtimex.ai", "/api/peer/v1/messages");
        put["method"] = json!("PUT");
        assert!(connection.request(&put, false).is_err());
        assert!(connection
            .request(&open("p", "s.realtimex.ai", "/api/peer/v1/messages"), true)
            .is_err());
        assert!(connection
            .request(&open("d", "x.realtimex.ai", "/remote-control/x"), true)
            .is_err());
    }

    #[test]
    fn websocket_requests_keep_their_subprotocols_apart() {
        let request = connection()
            .request(&open("a", "x.realtimex.ai", "/ar/live"), true)
            .unwrap();
        assert!(request.websocket);
        assert_eq!(request.protocols, vec!["chat", "json"]);
        assert!(!request
            .headers
            .iter()
            .any(|(n, _)| n.starts_with("sec-websocket-")));
    }

    #[test]
    fn an_exposure_from_before_carries_only_remote_control() {
        let old = json!({
            "exposureId": ID,
            "connector": { "url": "wss://relay.example/connect" },
            "routes": [{ "hostname": "x.realtimex.ai", "pathPrefix": format!("/d/{ID}"), "publicBaseUrl": format!("https://x.realtimex.ai/d/{ID}/") }],
            "allowedPrefixes": ["/remote-control"],
        });
        let connection = Connection::from_stored(&old, "i", "c".into()).unwrap();
        assert!(connection.carries(&["d"]));
        assert!(!connection.carries(&SURFACES));
        assert!(connection
            .request(&open("d", "x.realtimex.ai", "/remote-control/hello"), false)
            .is_ok());
        assert!(connection.carries(&["d"]));
        let new = connection();
        assert!(new.carries(&["a", "h", "p", "d"]));
    }

    #[test]
    fn unsupported_surfaces_are_recognized() {
        let failure = |body: Value| ApiFailure {
            status: 422,
            message: "Relay surface p is not supported by this service yet".into(),
            body,
        };
        assert!(matches!(
            EnrollFailure::from(failure(json!({ "code": "RELAY_SURFACE_UNSUPPORTED", "surface": "p" }))),
            EnrollFailure::Unsupported(s) if s == "p"
        ));
        assert!(matches!(
            EnrollFailure::from(failure(json!({ "code": "RELAY_SURFACE_UNSUPPORTED" }))),
            EnrollFailure::Unsupported(s) if s == "p"
        ));
        assert!(matches!(
            EnrollFailure::from(failure(json!({ "code": "OTHER" }))),
            EnrollFailure::Other(_)
        ));
    }

    #[tokio::test]
    async fn frames_round_trip_across_messages() {
        let (data, mut sent) = mpsc::channel(8);
        let (control, _) = mpsc::unbounded_channel();
        let send = Sender { data, control };
        assert!(
            send.meta(frame::HEADERS, 7, &json!({ "status": 200 }))
                .await
        );
        assert!(send.raw(frame::DATA, 7, &[1, 2, 3], 0).await);
        let mut bytes = Vec::new();
        while let Ok(Message::Binary(chunk)) = sent.try_recv() {
            bytes.extend_from_slice(&chunk);
        }
        let mut decoder = Decoder::default();
        let (first, second) = bytes.split_at(5);
        assert!(decoder.feed(first).unwrap().is_empty());
        let frames = decoder.feed(second).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].kind, frames[0].stream), (frame::HEADERS, 7));
        assert_eq!(&frames[1].payload[..], &[1, 2, 3]);
        let mut bad = vec![2u8, frame::DATA, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        assert!(Decoder::default().feed(&bad).is_err());
        bad[0] = 1;
        bad[7..11].copy_from_slice(&((MAX_DATA_FRAME as u32) + 1).to_be_bytes());
        assert!(Decoder::default().feed(&bad).is_err());
    }

    #[tokio::test]
    async fn a_websocket_message_keeps_its_boundary_and_text_flag() {
        let (data, mut sent) = mpsc::channel(8);
        let (control, _) = mpsc::unbounded_channel();
        let send = Sender { data, control };
        let message = vec![b'x'; MAX_DATA_FRAME + 10];
        assert!(send.data(frame::WS_DATA, 3, &message, TEXT_FLAG).await);
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        while let Ok(Message::Binary(chunk)) = sent.try_recv() {
            frames.extend(decoder.feed(&chunk).unwrap());
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].flags, TEXT_FLAG | MORE_FLAG);
        assert_eq!(frames[1].flags, TEXT_FLAG);
        // And back: fragments make one text message.
        let (tx, mut rx) = mpsc::channel(4);
        let mut stream = Stream {
            input: Some(Input::Socket(tx)),
            received: 0,
            limit: MAX_WS_MESSAGE as u64,
            parts: Vec::new(),
            text: false,
            task: tokio::spawn(async {}),
        };
        for frame in frames {
            stream.message(frame.payload, frame.flags).await.unwrap();
        }
        match rx.try_recv().unwrap() {
            Message::Text(text) => assert_eq!(text.len(), MAX_DATA_FRAME + 10),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_body_over_its_limit_is_refused() {
        let (tx, _rx) = mpsc::channel(4);
        let mut stream = Stream {
            input: Some(Input::Body(tx)),
            received: 0,
            limit: 4,
            parts: Vec::new(),
            text: false,
            task: tokio::spawn(async {}),
        };
        assert!(stream.body(bytes::Bytes::from_static(b"abc")).await.is_ok());
        assert!(stream.body(bytes::Bytes::from_static(b"de")).await.is_err());
        assert_eq!(body_limit("m"), 50 * 1024 * 1024);
    }

    #[test]
    fn token_expiry_and_backoff() {
        use base64::Engine;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1800000000}"#);
        assert_eq!(jwt_expiry(&format!("h.{payload}.s")), Some(1_800_000_000));
        assert_eq!(jwt_expiry("opaque"), None);
        for attempt in 0..10 {
            let delay = backoff(attempt);
            let base = RECONNECT_DELAYS[attempt.min(5)];
            assert!(
                delay >= (base as f64 * 0.8) as u64 && delay <= (base as f64 * 1.2).ceil() as u64
            );
        }
    }
}
