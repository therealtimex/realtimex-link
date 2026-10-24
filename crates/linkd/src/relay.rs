//! RealTimeX Link's own Public Exposure (#2753): other computers reach this one through
//! RealTimeX's relay (https://x.realtimex.ai/d/<exposureId>) without Tailscale, while RealTimeX is
//! closed too. Link enrolls its own exposure (its own install ID, only Remote Control's surface
//! `d`), so it never contends with RealTimeX's for the one connector an exposure may have.
//!
//! Enrolling (and recovering a lost credential) needs the signed-in account's access token, which
//! RealTimeX hands over (POST /v1/account/token); the tunnel itself needs only the connector
//! credential enrollment returns, kept sealed in the store. The wire protocol is rtx-relay/1, as
//! RealTimeX's own connector speaks it (server/utils/publicExposure/relayConnector.cjs there):
//! binary frames over one WebSocket, HTTP streams forwarded into the public Remote Control routes
//! in-process (router::PublicRoutes).

use std::collections::HashMap;
use std::net::SocketAddr;
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

pub mod keys {
    /// This Link's own relay install ID (a UUID, made once).
    pub const INSTALL_ID: &str = "relay-install-id";
    /// The enrolled exposure: { exposureId, connector: { url, protocol, websocketSubprotocol },
    /// routes: [{ hostname, pathPrefix, publicBaseUrl }], allowedPrefixes }.
    pub const EXPOSURE: &str = "relay-exposure";
    /// The connector credential, sealed.
    pub const CREDENTIAL: &str = "relay-credential";
}

const SURFACE: &str = "d";
const DEFAULT_API_BASE: &str = "https://marketplace-api-next.realtimex.ai";
const PROTOCOL_NAME: &str = "rtx-relay/1";
const WEBSOCKET_SUBPROTOCOL: &str = "rtx-relay-v1";
const PROTOCOL_VERSION: u8 = 1;
const HEADER_BYTES: usize = 11;
const MAX_DATA_FRAME: usize = 64 * 1024;
const MAX_META_FRAME: usize = 32 * 1024;
const DEFAULT_ALLOWED_PREFIX: &str = "/remote-control";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The relay pings every 10 s; nothing for this long means the connection is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(35);
const RECONNECT_DELAYS: [u64; 6] = [1, 2, 5, 10, 30, 60];
/// After 4409 (another connection still holds the lease, up to 30 s).
const ALREADY_ACTIVE_FLOOR: Duration = Duration::from_secs(30);
const CLOSE_AUTH_FAILED: u16 = 4401;
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
    pub const CANCEL: u8 = 7;
    pub const RESET: u8 = 8;
    pub const WS_OPEN: u8 = 9;
    pub const WS_CLOSE: u8 = 12;
    pub const PING: u8 = 13;
    pub const PONG: u8 = 14;
    pub const GOAWAY: u8 = 15;
}

/// The account's access token as RealTimeX last handed it over, and its API base.
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
    /// Enrollment must be repeated (no exposure yet, or the relay refused the credential).
    needs_enrollment: bool,
}

pub struct Relay {
    host: Weak<Host>,
    account: Mutex<Option<Account>>,
    status: Mutex<Status>,
    /// Woken when something that decides the relay changed (a token, settings, a stop).
    changed: Notify,
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
            status: Mutex::new(Status {
                state: "off",
                ..Default::default()
            }),
            changed: Notify::new(),
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

    /// RealTimeX hands over the signed-in account's access token (and its API base).
    pub fn set_account(&self, token: &str, api_base: Option<&str>) -> Result<()> {
        let token = token.trim();
        if token.is_empty() || token.contains(char::is_whitespace) {
            bail!("token must be the account's access token");
        }
        let api_base = api_base
            .map(|base| base.trim().trim_end_matches('/').to_string())
            .filter(|base| base.starts_with("https://") || base.starts_with("http://127.0.0.1"))
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
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
            api_base,
            expires_at: jwt_expiry(token),
        });
        self.wake();
        Ok(())
    }

    /// The Marketplace API base RealTimeX named (also the identity tokens' issuer).
    pub fn api_base(&self) -> String {
        lock(&self.account)
            .as_ref()
            .map(|account| account.api_base.clone())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
    }

    fn account(&self) -> Option<Account> {
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

    /// For the settings page: { state, url, detail }.
    pub fn state(&self) -> Value {
        let status = lock(&self.status);
        let url = self.host.upgrade().and_then(|host| {
            host.store
                .get(keys::EXPOSURE)?
                .pointer("/routes/0/publicBaseUrl")?
                .as_str()
                .map(str::to_string)
        });
        json!({ "state": status.state, "url": url, "detail": status.detail })
    }

    fn set_status(&self, state: &'static str, detail: Option<String>) {
        let mut status = lock(&self.status);
        status.state = state;
        status.detail = detail;
    }

    fn wanted(host: &Host) -> bool {
        host.settings().allow_incoming && host.backend.available()
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
            let needs = lock(&self.status).needs_enrollment || self.connection(&host).is_none();
            if needs {
                match self.account() {
                    None => {
                        self.set_status(
                            "not-set-up",
                            Some("Waiting for RealTimeX to sign this computer in".into()),
                        );
                        drop(host);
                        self.changed.notified().await;
                        continue;
                    }
                    Some(account) => match self.enroll(&host, &account).await {
                        Ok(()) => lock(&self.status).needs_enrollment = false,
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
                    ALREADY_ACTIVE_FLOOR + jitter(ALREADY_ACTIVE_FLOOR / 5)
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

    /// The stored exposure, ready to connect: (exposure, credential).
    fn connection(&self, host: &Host) -> Option<Connection> {
        let exposure = host.store.get(keys::EXPOSURE)?;
        let credential = host.open_secret(&host.store.get(keys::CREDENTIAL)?)?;
        let routes = exposure
            .get("routes")?
            .as_array()?
            .iter()
            .filter_map(|route| {
                Some((
                    route.get("hostname")?.as_str()?.to_lowercase(),
                    route.get("pathPrefix")?.as_str()?.to_string(),
                ))
            })
            .collect::<Vec<_>>();
        let allowed = exposure
            .get("allowedPrefixes")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| vec![DEFAULT_ALLOWED_PREFIX.to_string()]);
        Some(Connection {
            exposure_id: exposure.get("exposureId")?.as_str()?.to_string(),
            install_id: host.store.get(keys::INSTALL_ID)?.as_str()?.to_string(),
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

    /// Enroll (or refresh) this Link's exposure with only surface d; a lost credential is
    /// recovered first.
    async fn enroll(&self, host: &Host, account: &Account) -> Result<()> {
        let install_id = Self::install_id(host)?;
        let credential = host
            .store
            .get(keys::CREDENTIAL)
            .and_then(|sealed| host.open_secret(&sealed));
        let machine = crate::host::machine_name();
        let body = json!({
            "installId": install_id,
            "connectorCredential": credential,
            "runtimeChannel": "app",
            "appName": "RealTimeX Link",
            "appVersion": env!("CARGO_PKG_VERSION"),
            "machineName": machine,
            "osPlatform": os_platform(),
            "surfaces": [SURFACE],
        });
        let key = format!(
            "relay-{}",
            hex(&Sha256::digest(
                json!({ "installId": install_id, "routes": ["remoteControl"], "appName": "RealTimeX Link", "appVersion": env!("CARGO_PKG_VERSION") })
                    .to_string()
                    .as_bytes()
            ))
        );
        let url = format!("{}/v1/relay-exposures", account.api_base);
        let answer = match api_post(&url, &account.token, &key, &body).await {
            Ok(answer) => answer,
            Err(ApiFailure { status: 409, .. }) => {
                // The service wants the credential this install lost: recover (it rotates).
                let exposure_id = host
                    .store
                    .get(keys::EXPOSURE)
                    .and_then(|e| {
                        e.get("exposureId")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .ok_or_else(|| anyhow!("the relay knows this install, but not its exposure"))?;
                let recover_key = format!(
                    "relay-recovery-{}",
                    hex(&Sha256::digest(
                        json!({ "exposureId": exposure_id, "installId": install_id })
                            .to_string()
                            .as_bytes()
                    ))
                );
                let recovered = api_post(
                    &format!(
                        "{}/v1/relay-exposures/{exposure_id}/recover",
                        account.api_base
                    ),
                    &account.token,
                    &recover_key,
                    &json!({ "installId": install_id }),
                )
                .await
                .map_err(|f| anyhow!("{}", f.message))?;
                self.save(host, &recovered)?;
                let mut fields = body_fields(&body);
                fields.insert(
                    "connectorCredential".into(),
                    recovered
                        .pointer("/connector/credential")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                let body = Value::Object(fields);
                api_post(&url, &account.token, &format!("{key}-recovered"), &body)
                    .await
                    .map_err(|f| anyhow!("{}", f.message))?
            }
            Err(failure) => return Err(anyhow!("{}", failure.message)),
        };
        self.save(host, &answer)?;
        (host.log)(&format!(
            "[relay] enrolled: {}",
            answer
                .get("exposureId")
                .and_then(Value::as_str)
                .unwrap_or("")
        ));
        Ok(())
    }

    /// Keep an enrollment answer: the exposure (without the credential) and the sealed credential.
    fn save(&self, host: &Host, answer: &Value) -> Result<()> {
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
            .filter(|s| s.get("surface").and_then(Value::as_str) == Some(SURFACE))
            .map(|s| {
                json!({
                    "hostname": s.get("hostname"),
                    "pathPrefix": s.get("pathPrefix"),
                    "publicBaseUrl": s.get("publicBaseUrl"),
                })
            })
            .collect();
        if routes.is_empty() {
            bail!("the relay did not give this computer Remote Control's route");
        }
        let allowed: Vec<Value> = answer
            .get("pathAllowlist")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.get("surface").and_then(Value::as_str) == Some(SURFACE))
            .filter_map(|p| p.get("pathPrefix").cloned())
            .collect();
        let exposure = json!({
            "exposureId": exposure_id,
            "connector": {
                "url": url,
                "protocol": connector.get("protocol"),
                "websocketSubprotocol": connector.get("websocketSubprotocol"),
            },
            "routes": routes,
            "allowedPrefixes": allowed,
        });
        let mut values = vec![(keys::EXPOSURE, exposure)];
        if let Some(credential) = connector.get("credential").and_then(Value::as_str) {
            values.push((keys::CREDENTIAL, host.seal_secret(credential)?));
        }
        host.store.transaction(&values, &[])?;
        Ok(())
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
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let send = Sender(out);
        send.meta(
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
                            if let Some(host) = self.host.upgrade() {
                                (host.log)(&format!("[relay] ready: {}", connection.exposure_id));
                            }
                        }
                    }
                    continue;
                }
                if let Err(error) = self.handle(frame, connection, routes, &send, &mut streams) {
                    fatal = Some(error.to_string());
                    break;
                }
                if streams.contains_key(&0) {
                    // GOAWAY (see handle): stop without reconnecting.
                    streams.remove(&0);
                    fatal = Some(String::new());
                }
            }
            match fatal {
                Some(reason) if reason.is_empty() => break Ended::Stopped,
                Some(reason) => {
                    send.close(1011, "connector protocol error");
                    break Ended::lost(ready, reason);
                }
                None => {}
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
    fn handle(
        self: &Arc<Self>,
        frame: Frame,
        connection: &Connection,
        routes: &Arc<PublicRoutes>,
        send: &Sender,
        streams: &mut HashMap<u32, Stream>,
    ) -> Result<()> {
        if frame.stream == 0 {
            match frame.kind {
                frame::PING => send.raw(frame::PONG, 0, &frame.payload, 0),
                frame::GOAWAY => {
                    streams.insert(
                        0,
                        Stream {
                            body: None,
                            task: tokio::spawn(async {}),
                        },
                    );
                }
                _ => {}
            }
            return Ok(());
        }
        match frame.kind {
            frame::OPEN => {
                let value: Value = serde_json::from_slice(&frame.payload)
                    .context("the relay sent a request that is not JSON")?;
                if let Some(old) = streams.remove(&frame.stream) {
                    old.task.abort();
                }
                match connection.request(&value) {
                    Ok(request) => {
                        let stream = self.open(frame.stream, request, routes.clone(), send.clone());
                        streams.insert(frame.stream, stream);
                    }
                    Err(reason) => {
                        send.meta(frame::RESET, frame.stream, &json!({ "error": reason }))
                    }
                }
            }
            frame::WS_OPEN => send.meta(
                frame::RESET,
                frame.stream,
                &json!({ "error": "Remote Control has no WebSocket routes" }),
            ),
            frame::DATA => match streams.get(&frame.stream) {
                Some(stream) => {
                    if let Some(body) = &stream.body {
                        let _ = body.send(Ok(frame.payload));
                    }
                }
                None => send.meta(
                    frame::RESET,
                    frame.stream,
                    &json!({ "error": "unknown stream" }),
                ),
            },
            frame::END => match streams.get_mut(&frame.stream) {
                Some(stream) => stream.body = None,
                None => send.meta(
                    frame::RESET,
                    frame.stream,
                    &json!({ "error": "unknown stream" }),
                ),
            },
            frame::CANCEL | frame::WS_CLOSE => {
                if let Some(stream) = streams.remove(&frame.stream) {
                    stream.task.abort();
                }
            }
            frame::RESET => {
                if let Some(stream) = streams.remove(&frame.stream) {
                    stream.task.abort();
                }
            }
            _ => {}
        }
        // Finished streams answer END themselves; drop their entries now and then.
        streams.retain(|id, stream| *id == 0 || !stream.task.is_finished());
        Ok(())
    }

    /// Run one request through the public routes and stream the answer back.
    fn open(
        self: &Arc<Self>,
        id: u32,
        request: RelayRequest,
        routes: Arc<PublicRoutes>,
        send: Sender,
    ) -> Stream {
        let (body, chunks) = mpsc::unbounded_channel::<Result<bytes::Bytes, std::io::Error>>();
        let task = tokio::spawn(async move {
            let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(chunks);
            let mut builder = axum::http::Request::builder()
                .method(request.method.as_str())
                .uri(&request.path);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let Ok(mut http) = builder.body(Body::from_stream(stream)) else {
                send.meta(frame::RESET, id, &json!({ "error": "bad request" }));
                return;
            };
            http.extensions_mut()
                .insert(ConnectInfo(SocketAddr::new(request.client, 0)));
            let response = match routes.router.clone().oneshot(http).await {
                Ok(response) => response,
                Err(never) => match never {},
            };
            let mut headers = serde_json::Map::new();
            for (name, value) in response.headers() {
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
            send.meta(
                frame::HEADERS,
                id,
                &json!({ "status": response.status().as_u16(), "headers": headers }),
            );
            let mut data = response.into_body().into_data_stream();
            while let Some(chunk) = data.next().await {
                match chunk {
                    Ok(chunk) => {
                        for part in chunk.chunks(MAX_DATA_FRAME) {
                            send.raw(frame::DATA, id, part, 0);
                        }
                    }
                    Err(error) => {
                        send.meta(frame::RESET, id, &json!({ "error": error.to_string() }));
                        return;
                    }
                }
            }
            send.raw(frame::END, id, &[], 0);
        });
        Stream {
            body: Some(body),
            task,
        }
    }
}

struct Connection {
    exposure_id: String,
    install_id: String,
    url: String,
    subprotocol: String,
    credential: String,
    /// (hostname, pathPrefix) of each public route to this computer.
    routes: Vec<(String, String)>,
    allowed: Vec<String>,
}

struct RelayRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    client: std::net::IpAddr,
}

impl Connection {
    /// The local request for an OPEN, or why the stream is refused (it alone is reset).
    fn request(&self, value: &Value) -> Result<RelayRequest, String> {
        let text = |name: &str| value.get(name).and_then(Value::as_str).unwrap_or("");
        if text("surface") != SURFACE {
            return Err("unauthorized surface".into());
        }
        let route = (
            text("publicHost").trim().to_lowercase(),
            text("publicPrefix").trim().to_string(),
        );
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
        let allowed = self.allowed.iter().any(|prefix| {
            pathname == prefix
                || pathname.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
        });
        if !safe || !allowed {
            return Err("unauthorized request path".into());
        }
        let method = match text("method").to_uppercase() {
            m if m.is_empty() => "GET".to_string(),
            m => m,
        };
        let mut headers = Vec::new();
        if let Some(map) = value.get("headers").and_then(Value::as_object) {
            for (name, raw) in map {
                let name = name.to_lowercase();
                if HOP_BY_HOP.contains(&name.as_str())
                    || name == "host"
                    || name == "content-length"
                    || name.starts_with("x-forwarded-")
                {
                    continue;
                }
                let values: Vec<String> = match raw {
                    Value::Array(list) => list
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                    Value::String(v) => vec![v.clone()],
                    _ => continue,
                };
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
        headers.push(("x-forwarded-for".into(), client.to_string()));
        headers.push(("x-forwarded-proto".into(), "https".into()));
        Ok(RelayRequest {
            method,
            path: path.to_string(),
            headers,
            client,
        })
    }
}

struct Stream {
    /// The request body's remaining chunks (None after END).
    body: Option<mpsc::UnboundedSender<Result<bytes::Bytes, std::io::Error>>>,
    task: JoinHandle<()>,
}

enum Ended {
    Stopped,
    AuthFailed,
    AlreadyActive,
    Lost { was_ready: bool, reason: String },
}

impl Ended {
    fn lost(was_ready: bool, reason: String) -> Self {
        Ended::Lost { was_ready, reason }
    }
}

#[derive(Clone)]
struct Sender(mpsc::UnboundedSender<Message>);

impl Sender {
    fn raw(&self, kind: u8, stream: u32, payload: &[u8], flags: u8) {
        let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
        bytes.push(PROTOCOL_VERSION);
        bytes.push(kind);
        bytes.push(flags);
        bytes.extend_from_slice(&stream.to_be_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        let _ = self.0.send(Message::Binary(bytes.into()));
    }

    fn meta(&self, kind: u8, stream: u32, value: &Value) {
        let text = value.to_string();
        let payload = if text.len() > MAX_META_FRAME {
            json!({ "error": "metadata too large" }).to_string()
        } else {
            text
        };
        self.raw(kind, stream, payload.as_bytes(), 0);
    }

    fn close(&self, code: u16, reason: &str) {
        let _ = self.0.send(Message::Close(Some(
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::from(code),
                reason: reason.to_string().into(),
            },
        )));
    }
}

struct Frame {
    kind: u8,
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
            let stream = u32::from_be_bytes(self.buffer[3..7].try_into().expect("4 bytes"));
            let length =
                u32::from_be_bytes(self.buffer[7..11].try_into().expect("4 bytes")) as usize;
            let limit = if kind == frame::DATA || kind == 11 {
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
            })?;
        let status = response.status().as_u16();
        let answer: Value = response.body_mut().read_json().unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(answer);
        }
        let message = answer
            .get("message")
            .or_else(|| answer.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("the relay service answered {status}"));
        Err(ApiFailure { status, message })
    })
    .await
    .unwrap_or_else(|error| {
        Err(ApiFailure {
            status: 0,
            message: error.to_string(),
        })
    })
}

/// The registration body's fields other than the credential.
fn body_fields(body: &Value) -> serde_json::Map<String, Value> {
    let mut fields = body.as_object().cloned().unwrap_or_default();
    fields.remove("connectorCredential");
    fields
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

fn jitter(max: Duration) -> Duration {
    let millis = max.as_millis().max(1) as u64;
    Duration::from_millis(u64::from_le_bytes(crate::util::random_bytes::<8>()) % millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        Connection {
            exposure_id: "Hn4cR8tZ1qLs0wXe5uYb7A".into(),
            install_id: "i".into(),
            url: "wss://relay.example/connect".into(),
            subprotocol: WEBSOCKET_SUBPROTOCOL.into(),
            credential: "c".into(),
            routes: vec![("x.realtimex.ai".into(), "/d/Hn4cR8tZ1qLs0wXe5uYb7A".into())],
            allowed: vec![DEFAULT_ALLOWED_PREFIX.into()],
        }
    }

    fn open(path: &str) -> Value {
        json!({
            "surface": "d",
            "publicHost": "X.realtimex.ai",
            "publicPrefix": "/d/Hn4cR8tZ1qLs0wXe5uYb7A",
            "path": path,
            "method": "post",
            "headers": { "Content-Type": "application/json", "X-Forwarded-For": "6.6.6.6", "Host": "evil", "Connection": "close" },
            "remoteAddress": "203.0.113.7",
        })
    }

    #[test]
    fn requests_keep_to_remote_controls_routes() {
        let request = connection()
            .request(&open("/remote-control/connect"))
            .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/remote-control/connect");
        assert_eq!(request.client.to_string(), "203.0.113.7");
        assert!(request
            .headers
            .contains(&("content-type".into(), "application/json".into())));
        assert!(request
            .headers
            .contains(&("x-forwarded-for".into(), "203.0.113.7".into())));
        assert!(!request
            .headers
            .iter()
            .any(|(n, v)| n == "host" || n == "connection" || v == "6.6.6.6"));
        for path in [
            "/api/system",
            "/remote-control/../api",
            "//evil/remote-control",
            "/remote-controlx",
        ] {
            assert!(connection().request(&open(path)).is_err(), "{path}");
        }
        let mut other = open("/remote-control/hello");
        other["publicPrefix"] = json!("/d/someone-else");
        assert!(connection().request(&other).is_err());
        other = open("/remote-control/hello");
        other["surface"] = json!("w");
        assert!(connection().request(&other).is_err());
        other = open("/remote-control/hello");
        other["remoteAddress"] = json!("");
        assert!(connection().request(&other).is_err());
    }

    #[test]
    fn frames_round_trip_across_messages() {
        let (out, mut sent) = mpsc::unbounded_channel();
        let send = Sender(out);
        send.meta(frame::HEADERS, 7, &json!({ "status": 200 }));
        send.raw(frame::DATA, 7, &[1, 2, 3], 0);
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
