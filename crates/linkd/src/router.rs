//! The Remote Control API controllers speak, on this computer's Tailscale address, and through
//! RealTimeX Link's own Public Exposure relay (`public`, relay.rs: the caller's address is the
//! public client's, /hello leaves the machine name out, failed sign-ins are limited per address
//! and in total, and there is no MJPEG live view through the relay) (protocol 2): /hello, sign-in (/connect, connection requests),
//! then with a client token /status, /screenshot, /actions, /frames (MJPEG live view),
//! /peer/offer (the WebRTC live view, whose DataChannel messages come back as helper events),
//! /transfer and /stop. RealTimeX's controller (server/utils/remoteControl/computers.js) and agents' CLI speak it.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

use crate::actions::{is_physical_key, run_actions};
use crate::backend::Backend;
use crate::clients::Authenticated;
use crate::clipboard_share::{self, ClipboardShare, Pulled, POLL_MS};
use crate::error::{ApiError, ApiResult};
use crate::file_browse;
use crate::host::Host;
use crate::local_api::Body64;
use crate::requests::Poll;
use crate::transfers::{Ended, Transfers};
use crate::util::{iso_now, now_ms, unbase64};

const AUTH_FAILURE_WINDOW_MS: i64 = 60 * 1000;
pub const MAX_AUTH_FAILURES: usize = 10;
// Sign-ins over Public Exposure, which anyone on the internet can reach: failed passwords per
// client address, and across all of them.
const PUBLIC_FAILURES_PER_ADDRESS: usize = 5;
const PUBLIC_ADDRESS_WINDOW_MS: i64 = 15 * 60 * 1000;
const PUBLIC_FAILURES_TOTAL: usize = 30;
const PUBLIC_TOTAL_WINDOW_MS: i64 = 60 * 60 * 1000;
// Failed passwords across all addresses on the tailnet listener too: another device on the tailnet
// (or a shared node) can't try passwords without end by changing its address.
const TAILNET_FAILURES_TOTAL: usize = 30;
pub const SERVICE: &str = "realtimex-remote-control";
/// 2 (ADR-2611 §4.5): peer live view, connection requests, client keys, identity tokens.
pub const PROTOCOL_VERSION: u32 = 2;
const FEATURES: [&str; 5] = [
    "peer-view",
    "connect-requests",
    "client-key",
    "identity-token",
    "transfers",
];
const FRAME_BOUNDARY: &str = "rtxframe";
const MIN_FRAME_INTERVAL_MS: u64 = 66;
const DEFAULT_FRAME_INTERVAL_MS: u64 = 500;
/// An unchanged screen still sends a frame this often, so the viewer knows it is live.
const UNCHANGED_FRAME_MS: i64 = 2000;
const MIN_FRAME_WIDTH: f64 = 320.0;
const MAX_FRAME_WIDTH: f64 = 1920.0;
const MAX_TEXT_IN_ACTION: usize = 80;
const MAX_SDP_LENGTH: usize = 64 * 1024;
const PEER_ACCESS_CHECK: Duration = Duration::from_secs(10);
const MAX_PEER_MESSAGE_LENGTH: usize = 64 * 1024;
const PEER_REQUESTS_PER_SECOND: usize = 20;
/// Pointer moves of a person's Control (#2736): their own budget, so moving never delays a click.
/// Batches of physical keys share it (#2909): fast typing sends about 20 small requests a second.
const PEER_MOVES_PER_SECOND: usize = 60;
/// A live view that holds a key and sends no action this long gets its keys released (#2909):
/// longer than any real hold, it catches a viewer that hangs while its DataChannel stays up.
const HELD_KEY_WATCHDOG: Duration = Duration::from_secs(30);
const TRANSFER_ACCESS_CHECK_MS: i64 = 2000;
/// An agent's transfers (POST /transfer) go after this long without a request.
const HTTP_TRANSFER_IDLE_MS: i64 = 10 * 60 * 1000;
const UNAVAILABLE_RETRY_MS: u64 = 2000;
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Why no frame could be taken, in words a viewer can show.
fn screen_problem(message: &str) -> (&'static str, String) {
    let lower = message.to_lowercase();
    if lower.contains("inhibited") || lower.contains("locked") {
        (
            "screen-locked",
            "The screen is locked. Unlock it to see it here.".into(),
        )
    } else {
        ("screen-unavailable", message.to_string())
    }
}

/// What the controller calls this computer's OS (Node's process.platform): ⌘ on macOS.
fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

struct Stream {
    id: u64,
    client_id: String,
    end: Arc<Notify>,
    /// A WebRTC view ends by closing its peer session instead.
    peer: Option<u64>,
}

struct Peer {
    client_id: String,
    token: String,
    address: String,
    closed: AtomicBool,
    held: Mutex<HashSet<String>>,
    /// Physical keys this view holds, in the order they went down (#2909).
    held_keys: Mutex<Vec<String>>,
    /// When its last batch of actions arrived, for the held-key watchdog.
    last_action: Mutex<Instant>,
    watchdog: AtomicBool,
    running: tokio::sync::RwLock<()>,
    requests: Mutex<Vec<i64>>,
    moves: Mutex<Vec<i64>>,
    transfer_client: Mutex<Option<(Option<Authenticated>, i64)>>,
}

pub struct Shared {
    host: Weak<Host>,
    /// The Public Exposure listener (see the module comment).
    public: bool,
    backend: Arc<Backend>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    failures: Mutex<HashMap<String, Vec<i64>>>,
    last_action: Mutex<Option<(serde_json::Map<String, Value>, String)>>,
    streams: Mutex<Vec<Stream>>,
    next_stream: AtomicU64,
    shot_sizes: Mutex<HashMap<String, (u64, u64)>>,
    peers: Mutex<HashMap<u64, Arc<Peer>>>,
    http_sessions: Mutex<HashMap<String, i64>>,
    transfers: Transfers,
    clipboard: ClipboardShare,
    clipboard_polling: AtomicBool,
    home: PathBuf,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    access_timer: Mutex<Option<JoinHandle<()>>>,
}

/// A running listener: the host ends its views and stops it.
pub struct Listener {
    shared: Arc<Shared>,
    stop: Option<oneshot::Sender<()>>,
    server: JoinHandle<()>,
}

impl Listener {
    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// End the open live views of clients `matches` accepts.
    pub fn end_streams(&self, matches: &dyn Fn(&str) -> bool) {
        end_streams(&self.shared, matches);
    }

    /// Stop now: open live views never end by themselves, so they are ended first.
    pub async fn shutdown(mut self) {
        end_streams(&self.shared, &|_| true);
        close_all(&self.shared);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // Long polls finish within their wait; past that, the server is dropped.
        if tokio::time::timeout(Duration::from_secs(3), &mut self.server)
            .await
            .is_err()
        {
            self.server.abort();
        }
    }
}

/// The state behind one set of Remote Control routes (`public`: the Public Exposure relay's).
fn new_shared(
    host: Weak<Host>,
    public: bool,
    log: Arc<dyn Fn(&str) + Send + Sync>,
) -> std::io::Result<Arc<Shared>> {
    let backend = host
        .upgrade()
        .map(|h| h.backend.clone())
        .ok_or_else(|| std::io::Error::other("stopping"))?;
    let (temp_dir, downloads) = Transfers::default_dirs();
    let shared = Arc::new(Shared {
        host,
        public,
        backend,
        log,
        failures: Mutex::new(HashMap::new()),
        last_action: Mutex::new(None),
        streams: Mutex::new(Vec::new()),
        next_stream: AtomicU64::new(1),
        shot_sizes: Mutex::new(HashMap::new()),
        peers: Mutex::new(HashMap::new()),
        http_sessions: Mutex::new(HashMap::new()),
        transfers: Transfers::new(temp_dir, downloads),
        clipboard: ClipboardShare::default(),
        clipboard_polling: AtomicBool::new(false),
        home: PathBuf::from(crate::paths::home_dir()),
        tasks: Mutex::new(Vec::new()),
        access_timer: Mutex::new(None),
    });
    let events = spawn_event_loop(&shared);
    lock(&shared.tasks).push(events);
    Ok(shared)
}

fn routes(shared: &Arc<Shared>) -> Router {
    Router::new()
        .route("/remote-control/hello", get(hello))
        .route("/remote-control/connect", post(connect))
        .route("/remote-control/connect/requests/{id}", get(poll_request))
        .route(
            "/remote-control/connect/requests/{id}/cancel",
            post(cancel_request),
        )
        .route("/remote-control/status", get(status))
        .route("/remote-control/screenshot", post(screenshot))
        .route("/remote-control/actions", post(actions))
        .route("/remote-control/frames", get(frames))
        .route("/remote-control/peer/offer", post(peer_offer))
        .route("/remote-control/transfer", post(transfer))
        .route("/remote-control/stop", post(stop))
        .fallback(|| async { ApiError::new(404, "Not found") })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(shared.clone())
}

/// Serve the Remote Control routes on exactly one address (the Tailscale IP).
pub async fn start_listener(
    address: &str,
    port: u16,
    host: Weak<Host>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
) -> std::io::Result<Listener> {
    let ip: std::net::IpAddr = address
        .parse()
        .map_err(|_| std::io::Error::other("Remote Control must listen on one IP address"))?;
    if ip.is_unspecified() {
        return Err(std::io::Error::other(
            "Remote Control must listen on one IP address",
        ));
    }
    let socket = tokio::net::TcpListener::bind(SocketAddr::new(ip, port)).await?;
    let shared = new_shared(host, false, log)?;
    let routes = routes(&shared);
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let service = routes.into_make_service_with_connect_info::<SocketAddr>();
        let _ = axum::serve(socket, service)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await;
    });
    Ok(Listener {
        shared,
        stop: Some(stop),
        server,
    })
}

/// The Remote Control routes for the Public Exposure relay (relay.rs), served in-process in public
/// mode: the relay gives each request the public client's address as its ConnectInfo.
pub struct PublicRoutes {
    shared: Arc<Shared>,
    pub router: Router,
}

impl PublicRoutes {
    pub fn new(host: Weak<Host>, log: Arc<dyn Fn(&str) + Send + Sync>) -> std::io::Result<Self> {
        let shared = new_shared(host, true, log)?;
        let router = routes(&shared);
        Ok(Self { shared, router })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// End the open live views of clients `matches` accepts.
    pub fn end_streams(&self, matches: &dyn Fn(&str) -> bool) {
        end_streams(&self.shared, matches);
    }

    /// Stop: open live views and sessions end.
    pub fn shutdown(&self) {
        end_streams(&self.shared, &|_| true);
        close_all(&self.shared);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

type Ctx = Arc<Shared>;

fn host_of(shared: &Shared) -> ApiResult<Arc<Host>> {
    shared
        .host
        .upgrade()
        .ok_or_else(|| ApiError::new(503, "Remote Control is stopping"))
}

fn json_status(status: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        Json(body),
    )
        .into_response()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("header"));
    response
}

// --- failed sign-ins ---------------------------------------------------------------------

fn blocked(shared: &Shared, key: &str) -> bool {
    blocked_within(shared, key, AUTH_FAILURE_WINDOW_MS, MAX_AUTH_FAILURES)
}

fn blocked_within(shared: &Shared, key: &str, window_ms: i64, max: usize) -> bool {
    let now = now_ms();
    let mut failures = lock(&shared.failures);
    let recent = failures.entry(key.to_string()).or_default();
    recent.retain(|at| now - at < window_ms);
    recent.len() >= max
}

fn fail(shared: &Shared, key: &str) {
    let now = now_ms();
    let mut failures = lock(&shared.failures);
    let recent = failures.entry(key.to_string()).or_default();
    let window = failure_window(key);
    recent.retain(|at| now - at < window);
    recent.push(now);
}

/// How long a failure is kept: the longest window it is checked against.
fn failure_window(key: &str) -> i64 {
    if key.starts_with("public:") || key == TAILNET_TOTAL {
        PUBLIC_TOTAL_WINDOW_MS
    } else {
        AUTH_FAILURE_WINDOW_MS
    }
}

/// Where failed sign-ins are counted: an IPv4 address, or an IPv6 address's /64. One machine
/// usually has a whole /64 to pick addresses from, so a per-address limit would not hold it.
pub fn source_of(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

/// The failed passwords of all sources, one count per listener.
const PUBLIC_TOTAL: &str = "public:*";
const TAILNET_TOTAL: &str = "tailnet:*";

/// Public sign-ins: blocked after PUBLIC_FAILURES_PER_ADDRESS failed passwords from one source
/// (15 minutes).
fn public_blocked(shared: &Shared, source: &str) -> bool {
    blocked_within(
        shared,
        &format!("public:{source}"),
        PUBLIC_ADDRESS_WINDOW_MS,
        PUBLIC_FAILURES_PER_ADDRESS,
    )
}

/// Failed passwords from everywhere, in the last hour: PUBLIC_FAILURES_TOTAL on Public Exposure,
/// TAILNET_FAILURES_TOTAL on the tailnet.
fn total_key(shared: &Shared) -> &'static str {
    if shared.public {
        PUBLIC_TOTAL
    } else {
        TAILNET_TOTAL
    }
}

fn total_blocked(shared: &Shared) -> bool {
    let max = if shared.public {
        PUBLIC_FAILURES_TOTAL
    } else {
        TAILNET_FAILURES_TOTAL
    };
    blocked_within(shared, total_key(shared), PUBLIC_TOTAL_WINDOW_MS, max)
}

fn clear(shared: &Shared, key: &str) {
    lock(&shared.failures).remove(key);
}

fn too_many() -> Response {
    json_status(429, json!({ "error": "Too many failed attempts" }))
}

// --- sign-in -----------------------------------------------------------------------------

/// Unauthenticated so a controller can find the App ID among its Tailscale peers.
async fn hello(State(shared): State<Ctx>) -> ApiResult<Json<Value>> {
    let mut answer =
        json!({ "service": SERVICE, "version": PROTOCOL_VERSION, "features": FEATURES });
    if let (Some(target), Some(identity)) = (
        answer.as_object_mut(),
        host_of(&shared)?.identity().as_object(),
    ) {
        target.extend(identity.clone());
        // Over Public Exposure the App ID alone should not reveal the computer's name.
        if shared.public {
            target.remove("machine");
        }
    }
    Ok(Json(answer))
}

/// Wrong passwords and refused tokens count separately: a client still polling with a token
/// from before a password change must not lock itself out of signing in with the new one.
///
/// Too many wrong passwords block password sign-ins only (host.admit's password branch): a client
/// key, an identity token or a connection request still get through, so whoever floods this
/// computer with wrong passwords can't lock its own clients out.
async fn connect(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Body64(body): Body64,
) -> Response {
    let address = peer.ip().to_string();
    let source = source_of(peer.ip());
    let key = format!("password:{source}");
    let password_blocked = blocked(&shared, &key)
        || (shared.public && public_blocked(&shared, &source))
        || total_blocked(&shared);
    let host = match host_of(&shared) {
        Ok(host) => host,
        Err(error) => return error.into_response(),
    };
    let via = if shared.public { "public" } else { "tailnet" };
    match host.admit(&body, &address, via, password_blocked).await {
        Ok(admitted) => {
            if admitted.wrong_password {
                fail(&shared, &key);
                if shared.public {
                    fail(&shared, &format!("public:{source}"));
                }
                fail(&shared, total_key(&shared));
            } else if admitted.status == 200 {
                clear(&shared, &key);
                clear(&shared, &format!("public:{source}"));
                clear(&shared, &format!("token:{source}"));
                let name = admitted
                    .body
                    .pointer("/client/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let by = admitted
                    .body
                    .get("admittedBy")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                (shared.log)(&format!(
                    "[remote-control] {name} signed in from {address} ({by})"
                ));
            }
            let mut response = no_store(json_status(admitted.status, admitted.body));
            if let Some(seconds) = admitted.retry_after {
                response
                    .headers_mut()
                    .insert("Retry-After", seconds.to_string().parse().expect("header"));
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

/// A pending request's poll (ADR-2611 §1.4). The request id is the requester's capability; an
/// unknown id counts as a failure like a wrong password.
async fn poll_request(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let key = format!("request:{}", source_of(peer.ip()));
    if blocked(&shared, &key) {
        return too_many();
    }
    let Ok(host) = host_of(&shared) else {
        return ApiError::new(503, "stopping").into_response();
    };
    let wait = query
        .get("wait")
        .and_then(|w| w.parse::<f64>().ok())
        .filter(|w| w.is_finite())
        .unwrap_or(0.0)
        .clamp(0.0, 25.0);
    let answer = host.poll_request(&id, (wait * 1000.0) as u64).await;
    no_store(match answer {
        None => {
            fail(&shared, &key);
            json_status(
                404,
                json!({ "error": "No such request", "code": "unknown-request" }),
            )
        }
        Some(Poll::Claimed) => json_status(410, json!({ "status": "claimed", "code": "claimed" })),
        Some(Poll::Allowed(admit)) => {
            let mut body = json!({ "status": "allowed" });
            if let (Some(target), Some(fields)) = (body.as_object_mut(), admit.as_object()) {
                target.extend(fields.clone());
            }
            json_status(200, body)
        }
        Some(Poll::Waiting { status, expires_at }) => {
            json_status(200, json!({ "status": status, "expiresAt": expires_at }))
        }
    })
}

async fn cancel_request(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> Response {
    let key = format!("request:{}", source_of(peer.ip()));
    if blocked(&shared, &key) {
        return too_many();
    }
    let Ok(host) = host_of(&shared) else {
        return ApiError::new(503, "stopping").into_response();
    };
    match host.cancel_request(&id) {
        Some(status) => json_status(200, json!({ "status": status })),
        None => {
            fail(&shared, &key);
            json_status(
                404,
                json!({ "error": "No such request", "code": "unknown-request" }),
            )
        }
    }
}

/// A signed-in request: its client and token.
struct Signed {
    client: Authenticated,
    token: String,
    address: String,
}

fn signed(
    shared: &Shared,
    headers: &HeaderMap,
    peer: &SocketAddr,
) -> Result<Signed, Box<Response>> {
    let address = peer.ip().to_string();
    let key = format!("token:{}", source_of(peer.ip()));
    if blocked(shared, &key) {
        return Err(Box::new(too_many()));
    }
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = header.strip_prefix("Bearer ").unwrap_or("").to_string();
    let host = host_of(shared).map_err(|error| Box::new(error.into_response()))?;
    let client = if token.is_empty() {
        None
    } else {
        host.authenticate(&token, Some(&address))
    };
    match client {
        Some(client) => Ok(Signed {
            client,
            token,
            address,
        }),
        None => {
            // An expired token is normal: the controller signs in again.
            if !token.is_empty() && host.token_expired(&token) {
                return Err(Box::new(json_status(
                    401,
                    json!({ "error": "This sign-in has expired", "code": "token-expired" }),
                )));
            }
            fail(shared, &key);
            Err(Box::new(json_status(
                401,
                json!({ "error": "Sign in again: this client is not allowed", "code": "invalid-token" }),
            )))
        }
    }
}

fn require(signed: &Signed, permission: &str) -> Result<(), Box<Response>> {
    if signed.client.may(permission) {
        Ok(())
    } else {
        Err(Box::new(json_status(
            403,
            json!({ "error": format!("This client is not allowed to {permission}") }),
        )))
    }
}

macro_rules! signed_with {
    ($shared:expr, $headers:expr, $peer:expr, $permission:expr) => {
        match signed(&$shared, &$headers, &$peer).and_then(|s| require(&s, $permission).map(|_| s))
        {
            Ok(signed) => signed,
            Err(response) => return *response,
        }
    };
}

// --- the screen and input ----------------------------------------------------------------

async fn status(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let _signed = signed_with!(shared, headers, peer, "view");
    Json(shared.backend.status().await).into_response()
}

/// JavaScript's Number(x) || fallback for a JSON field.
fn number_or(value: Option<&Value>, fallback: f64) -> f64 {
    let n = match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    if n == 0.0 || n.is_nan() {
        fallback
    } else {
        n
    }
}

async fn screenshot(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Body64(body): Body64,
) -> Response {
    let signed = signed_with!(shared, headers, peer, "view");
    let quality = number_or(body.get("quality"), 75.0).clamp(30.0, 95.0);
    let shot = match shared
        .backend
        .call("screenshot", json!({ "quality": quality }))
        .await
    {
        Ok(shot) => shot,
        Err(error) => {
            let (code, message) = screen_problem(&error.message);
            return if code == "screen-locked" {
                json_status(423, json!({ "error": message, "code": code }))
            } else {
                error.into_response()
            };
        }
    };
    let (width, height) = (
        shot["width"].as_u64().unwrap_or(0),
        shot["height"].as_u64().unwrap_or(0),
    );
    lock(&shared.shot_sizes).insert(signed.client.id().to_string(), (width, height));
    (shared.log)(&format!(
        "[remote-control] screenshot {width}x{height} for {}",
        signed.client.name()
    ));
    let data = shot["data"].as_str().and_then(unbase64).unwrap_or_default();
    let mime = shot["mime"].as_str().unwrap_or("image/jpeg").to_string();
    let mut response = (StatusCode::OK, data).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        ("content-type", mime),
        ("cache-control", "no-store".into()),
        ("x-screen-width", width.to_string()),
        ("x-screen-height", height.to_string()),
    ] {
        if let Ok(value) = value.parse() {
            headers.insert(name, value);
        }
    }
    response
}

/// What a viewer shows about the latest action: its marker and caption.
fn describe_action(
    action: &Value,
    client: &Authenticated,
    shot: Option<(u64, u64)>,
) -> serde_json::Map<String, Value> {
    let mut shown = serde_json::Map::new();
    shown.insert(
        "type".into(),
        action.get("type").cloned().unwrap_or(Value::Null),
    );
    shown.insert("client".into(), json!(client.name()));
    shown.insert("at".into(), json!(iso_now()));
    if let (Some(x), Some(y)) = (action.get("x"), action.get("y")) {
        shown.insert("x".into(), x.clone());
        shown.insert("y".into(), y.clone());
        let own = action
            .get("width")
            .zip(action.get("height"))
            .filter(|(w, h)| !w.is_null() && !h.is_null());
        match (own, shot) {
            (Some((width, height)), _) => {
                shown.insert("width".into(), width.clone());
                shown.insert("height".into(), height.clone());
            }
            (None, Some((width, height))) => {
                shown.insert("width".into(), json!(width));
                shown.insert("height".into(), json!(height));
            }
            _ => {}
        }
    }
    if action.get("type").and_then(Value::as_str) == Some("type") {
        let text: String = action
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(MAX_TEXT_IN_ACTION)
            .collect();
        shown.insert("text".into(), json!(text));
    }
    if action.get("type").and_then(Value::as_str) == Some("key") {
        shown.insert(
            "keys".into(),
            action.get("keys").cloned().unwrap_or(Value::Null),
        );
    }
    shown
}

/// Run `actions` for `client`: the latest one is shown on every live view.
async fn perform_actions(
    shared: &Arc<Shared>,
    client: &Authenticated,
    actions: Option<&Value>,
    quiet: bool,
    live_view: bool,
) -> ApiResult<Value> {
    let performed = run_actions(&shared.backend, actions, live_view).await?;
    // A person's pointer moves and keystrokes are not news for the other live views, nor for
    // the log.
    if quiet {
        return Ok(json!({ "performed": performed }));
    }
    let list = actions
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let shown = list.iter().rev().find(|a| {
        !is_physical_key(a)
            && a.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty() && t != "wait")
    });
    if let Some(action) = shown {
        let size = lock(&shared.shot_sizes).get(client.id()).copied();
        *lock(&shared.last_action) = Some((
            describe_action(action, client, size),
            client.id().to_string(),
        ));
        announce_action(shared);
    }
    let kinds: Vec<&str> = list
        .iter()
        .map(|a| a.get("type").and_then(Value::as_str).unwrap_or("undefined"))
        .collect();
    (shared.log)(&format!(
        "[remote-control] {performed} action(s) for {}: {}",
        client.name(),
        kinds.join(", ")
    ));
    Ok(json!({ "performed": performed }))
}

async fn actions(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Body64(body): Body64,
) -> Response {
    let signed = signed_with!(shared, headers, peer, "control");
    match perform_actions(&shared, &signed.client, body.get("actions"), false, false).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => error.into_response(),
    }
}

// --- the MJPEG live view -----------------------------------------------------------------

/// JPEG frames as multipart/x-mixed-replace, each with the screen size and the latest action
/// (base64 JSON) in its headers. The next frame is taken only once the last one has gone out.
async fn frames(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if shared.public {
        // A stream would carry the whole screen through the relay; the WebRTC view goes direct.
        return json_status(
            403,
            json!({ "error": "The live view is not available over Public Exposure", "code": "live-view-not-public" }),
        );
    }
    let signed = signed_with!(shared, headers, peer, "view");
    let number = |name: &str| {
        query
            .get(name)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|n| *n != 0.0 && n.is_finite())
    };
    let interval = number("interval")
        .unwrap_or(DEFAULT_FRAME_INTERVAL_MS as f64)
        .max(MIN_FRAME_INTERVAL_MS as f64) as u64;
    let quality = number("quality").unwrap_or(60.0).clamp(30.0, 90.0);
    let max_width = number("maxWidth")
        .unwrap_or(MAX_FRAME_WIDTH)
        .clamp(MIN_FRAME_WIDTH, MAX_FRAME_WIDTH);
    let end = Arc::new(Notify::new());
    let id = shared.next_stream.fetch_add(1, Ordering::SeqCst);
    lock(&shared.streams).push(Stream {
        id,
        client_id: signed.client.id().to_string(),
        end: end.clone(),
        peer: None,
    });
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    (shared.log)(&format!(
        "[remote-control] live view started for {}",
        signed.client.name()
    ));
    let task_shared = shared.clone();
    tokio::spawn(async move {
        let shared = task_shared;
        let mut last_problem: Option<&'static str> = None;
        let mut last_sent: (Option<String>, String, i64) = (None, String::new(), 0);
        loop {
            let started = now_ms();
            let mut wait = interval;
            // Access is checked before every frame: a token that stopped working ends the stream.
            let still = host_of(&shared)
                .ok()
                .and_then(|host| host.authenticate(&signed.token, Some(&signed.address)))
                .is_some_and(|current| current.id() == signed.client.id() && current.may("view"));
            if !still || sender.is_closed() {
                break;
            }
            let shot = tokio::select! {
                shot = shared.backend.call("screenshot", json!({ "quality": quality, "live": true, "maxWidth": max_width })) => shot,
                () = end.notified() => break,
            };
            match shot {
                Ok(shot) => {
                    let action = action_for(&shared, signed.client.id(), false)
                        .map(|shown| {
                            crate::util::base64(Value::Object(shown).to_string().as_bytes())
                        })
                        .unwrap_or_default();
                    let thumb = shot["thumb"].as_str().map(String::from);
                    let unchanged = thumb.is_some()
                        && thumb == last_sent.0
                        && action == last_sent.1
                        && started - last_sent.2 < UNCHANGED_FRAME_MS;
                    if !unchanged {
                        let data = shot["data"].as_str().and_then(unbase64).unwrap_or_default();
                        let mut part = format!(
                            "--{FRAME_BOUNDARY}\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Screen-Width: {}\r\nX-Screen-Height: {}\r\n",
                            shot["mime"].as_str().unwrap_or("image/jpeg"),
                            data.len(),
                            shot["width"],
                            shot["height"]
                        )
                        .into_bytes();
                        if !action.is_empty() {
                            part.extend_from_slice(
                                format!("X-Remote-Control-Action: {action}\r\n").as_bytes(),
                            );
                        }
                        part.extend_from_slice(b"\r\n");
                        part.extend_from_slice(&data);
                        part.extend_from_slice(b"\r\n");
                        if sender.send(Ok(Bytes::from(part))).await.is_err() {
                            break;
                        }
                        last_sent = (thumb, action, started);
                    }
                    last_problem = None;
                }
                Err(error) => {
                    // A JSON part says why there is no picture; frames resume by themselves.
                    let (code, message) = screen_problem(&error.message);
                    if last_problem != Some(code) {
                        (shared.log)(&format!(
                            "[remote-control] live view: {code} ({})",
                            error.message
                        ));
                        let body = json!({ "code": code, "message": message }).to_string();
                        let part = format!("--{FRAME_BOUNDARY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}\r\n", body.len());
                        if sender.send(Ok(Bytes::from(part))).await.is_err() {
                            break;
                        }
                        last_problem = Some(code);
                    }
                    wait = UNAVAILABLE_RETRY_MS;
                }
            }
            let spent = (now_ms() - started).max(0) as u64;
            if wait > spent {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(wait - spent)) => {}
                    () = end.notified() => break,
                    () = sender.closed() => break,
                }
            }
        }
        lock(&shared.streams).retain(|stream| stream.id != id);
        (shared.log)(&format!(
            "[remote-control] live view ended for {}",
            signed.client.name()
        ));
    });
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(receiver));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        format!("multipart/x-mixed-replace; boundary={FRAME_BOUNDARY}")
            .parse()
            .expect("header"),
    );
    headers.insert(header::CACHE_CONTROL, "no-store".parse().expect("header"));
    response
}

/// The latest action as `viewer` may see it: typed text (it may be a password) only for the
/// client that typed it.
fn action_for(shared: &Shared, viewer: &str, mark: bool) -> Option<serde_json::Map<String, Value>> {
    let (mut shown, by) = lock(&shared.last_action).clone()?;
    let own = by == viewer;
    if !own {
        shown.remove("text");
    }
    if mark {
        shown.insert("byViewer".into(), json!(own));
    }
    Some(shown)
}

fn end_streams(shared: &Arc<Shared>, matches: &dyn Fn(&str) -> bool) {
    let (ended, peers): (Vec<Arc<Notify>>, Vec<u64>) = {
        let streams = lock(&shared.streams);
        let chosen: Vec<&Stream> = streams.iter().filter(|s| matches(&s.client_id)).collect();
        (
            chosen
                .iter()
                .filter(|s| s.peer.is_none())
                .map(|s| s.end.clone())
                .collect(),
            chosen.iter().filter_map(|s| s.peer).collect(),
        )
    };
    for end in ended {
        end.notify_waiters();
        end.notify_one();
    }
    for session in peers {
        close_peer(shared, session);
    }
}

// --- the WebRTC live view ----------------------------------------------------------------

fn send_to_peer(shared: &Arc<Shared>, session: u64, message: Value) {
    let backend = shared.backend.clone();
    tokio::spawn(async move {
        let _ = backend
            .call(
                "peer_send",
                json!({ "session": session, "data": message.to_string() }),
            )
            .await;
    });
}

/// The latest action to every live view, marked `byViewer` for the client that sent it.
fn announce_action(shared: &Arc<Shared>) {
    let peers: Vec<(u64, String)> = lock(&shared.peers)
        .iter()
        .map(|(s, p)| (*s, p.client_id.clone()))
        .collect();
    for (session, client_id) in peers {
        if let Some(action) = action_for(shared, &client_id, true) {
            send_to_peer(
                shared,
                session,
                json!({ "type": "action", "action": action }),
            );
        }
    }
}

/// The client behind an open session if it may still `permission`.
fn peer_client(shared: &Shared, peer: &Peer, permission: &str) -> Option<Authenticated> {
    let host = shared.host.upgrade()?;
    let current = host.authenticate(&peer.token, Some(&peer.address))?;
    (current.id() == peer.client_id && current.may(permission)).then_some(current)
}

/// The client for a transfer or a pointer move, checked again at most every few seconds.
fn cached_client(shared: &Shared, peer: &Peer) -> Option<Authenticated> {
    let now = now_ms();
    let mut cached = lock(&peer.transfer_client);
    match cached.as_ref() {
        Some((client, at)) if now - at <= TRANSFER_ACCESS_CHECK_MS && client.is_some() => {
            client.clone()
        }
        _ => {
            let client = peer_client(shared, peer, "view");
            *cached = Some((client.clone(), now));
            client
        }
    }
}

fn close_peer(shared: &Arc<Shared>, session: u64) {
    let Some(peer) = lock(&shared.peers).remove(&session) else {
        return;
    };
    lock(&shared.streams).retain(|s| s.peer != Some(session));
    let key = session.to_string();
    shared.transfers.close_session(&key);
    shared.clipboard.close_session(&key);
    if lock(&shared.peers).is_empty() {
        if let Some(timer) = lock(&shared.access_timer).take() {
            timer.abort();
        }
    }
    // Nothing more runs for this view. A button it still holds would stay down: let go of it
    // once the actions already running are done.
    peer.closed.store(true, Ordering::SeqCst);
    let backend = shared.backend.clone();
    tokio::spawn(async move {
        let _all_done = peer.running.write().await;
        release_keys(&backend, &peer).await;
        let held: Vec<String> = lock(&peer.held).drain().collect();
        if !held.is_empty() {
            let ups: Vec<Value> = held
                .iter()
                .map(|button| json!({ "type": "up", "button": button }))
                .collect();
            let _ = run_actions(&backend, Some(&Value::Array(ups)), false).await;
        }
        let _ = backend
            .call("peer_close", json!({ "session": session }))
            .await;
    });
}

/// Let go of every physical key this view holds, the last pressed first (#2909). The caller makes
/// sure none of its actions are still running.
async fn release_keys(backend: &Arc<Backend>, peer: &Peer) {
    let held: Vec<String> = std::mem::take(&mut *lock(&peer.held_keys));
    if held.is_empty() {
        return;
    }
    let ups: Vec<Value> = held
        .iter()
        .rev()
        .map(|code| json!({ "type": "key", "code": code, "down": false }))
        .collect();
    let _ = run_actions(backend, Some(&Value::Array(ups)), true).await;
}

/// Note the physical keys a batch presses and lets go of, before it runs: a view that closes
/// part way through still has them released.
fn note_held_keys(held: &mut Vec<String>, actions: &[Value]) {
    for action in actions.iter().filter(|a| is_physical_key(a)) {
        let (Some(code), Some(down)) = (
            action.get("code").and_then(Value::as_str),
            action.get("down").and_then(Value::as_bool),
        ) else {
            continue;
        };
        if !down {
            held.retain(|held| held != code);
        } else if !held.iter().any(|held| held == code) {
            held.push(code.to_string());
        }
    }
}

/// While the view holds a key: release its keys once no action came for HELD_KEY_WATCHDOG.
fn watch_held_keys(shared: &Arc<Shared>, peer: &Arc<Peer>) {
    if lock(&peer.held_keys).is_empty() || peer.watchdog.swap(true, Ordering::SeqCst) {
        return;
    }
    let backend = shared.backend.clone();
    let peer = peer.clone();
    tokio::spawn(async move {
        while !peer.closed.load(Ordering::SeqCst) {
            let due = *lock(&peer.last_action) + HELD_KEY_WATCHDOG;
            tokio::time::sleep_until(due.into()).await;
            if !lock(&peer.held_keys).is_empty()
                && lock(&peer.last_action).elapsed() >= HELD_KEY_WATCHDOG
            {
                let _idle = peer.running.write().await;
                release_keys(&backend, &peer).await;
            }
            if lock(&peer.held_keys).is_empty() {
                peer.watchdog.store(false, Ordering::SeqCst);
                // A key pressed just now found this watch still running: go on watching it.
                if lock(&peer.held_keys).is_empty() || peer.watchdog.swap(true, Ordering::SeqCst) {
                    return;
                }
            }
        }
        peer.watchdog.store(false, Ordering::SeqCst);
    });
}

fn close_all(shared: &Arc<Shared>) {
    let sessions: Vec<u64> = lock(&shared.peers).keys().copied().collect();
    for session in sessions {
        close_peer(shared, session);
    }
    let http: Vec<String> = lock(&shared.http_sessions)
        .drain()
        .map(|(k, _)| k)
        .collect();
    for session in http {
        shared.transfers.close_session(&session);
        shared.clipboard.close_session(&session);
    }
    for task in lock(&shared.tasks).drain(..) {
        task.abort();
    }
}

/// Check open live views against their clients' current permissions: one that may no longer
/// view ends; one that may no longer control loses its clipboard watch and unfinished transfers.
pub async fn revalidate_peers(shared: &Arc<Shared>, matches: impl Fn(&str) -> bool) {
    let peers: Vec<(u64, Arc<Peer>)> = lock(&shared.peers)
        .iter()
        .map(|(s, p)| (*s, p.clone()))
        .collect();
    for (session, peer) in peers {
        if !matches(&peer.client_id) {
            continue;
        }
        match peer_client(shared, &peer, "view") {
            None => close_peer(shared, session),
            Some(current) if !current.may("control") => {
                // Control taken back: its held keys go up once its running actions are done.
                let backend = shared.backend.clone();
                let held = peer.clone();
                tokio::spawn(async move {
                    let _idle = held.running.write().await;
                    release_keys(&backend, &held).await;
                });
                let key = session.to_string();
                shared.clipboard.close_session(&key);
                shared.transfers.close_session(&key);
                *lock(&peer.transfer_client) = None;
            }
            Some(_) => {}
        }
    }
}

fn spawn_event_loop(shared: &Arc<Shared>) -> JoinHandle<()> {
    let mut events = shared.backend.subscribe();
    let weak = Arc::downgrade(shared);
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let Some(shared) = weak.upgrade() else { return };
            let Some(session) = event.get("session").and_then(Value::as_u64) else {
                continue;
            };
            if !lock(&shared.peers).contains_key(&session) {
                continue;
            }
            match event.get("event").and_then(Value::as_str) {
                Some("peer_message") => {
                    let data = event
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    tokio::spawn(on_peer_message(shared, session, data));
                }
                Some("peer_state")
                    if event.get("state").and_then(Value::as_str) == Some("closed") =>
                {
                    (shared.log)(&format!(
                        "[remote-control] live view ended (session {session})"
                    ));
                    close_peer(&shared, session);
                }
                _ => {}
            }
        }
    })
}

async fn on_peer_message(shared: Arc<Shared>, session: u64, data: String) {
    let Some(peer) = lock(&shared.peers).get(&session).cloned() else {
        return;
    };
    if data.len() > MAX_PEER_MESSAGE_LENGTH {
        return;
    }
    let Ok(message) = serde_json::from_str::<Value>(&data) else {
        return;
    };
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let reply = |fields: Value| {
        let mut answer = json!({ "id": id });
        if let (Some(target), Some(more)) = (answer.as_object_mut(), fields.as_object()) {
            target.extend(more.clone());
        }
        send_to_peer(&shared, session, answer);
    };
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    if is_transfer(kind) {
        let Some(client) = cached_client(&shared, &peer) else {
            close_peer(&shared, session);
            return;
        };
        reply(
            match run_transfer(&shared, &session.to_string(), &client, &message).await {
                Ok(result) => ok_with(result),
                Err(error) => failed(&error),
            },
        );
        return;
    }
    if kind == "info" {
        // What the viewer adapts to: this computer's OS (shortcuts: ⌘ on macOS, Ctrl elsewhere).
        if peer_client(&shared, &peer, "view").is_none() {
            close_peer(&shared, session);
            return;
        }
        reply(
            json!({ "ok": true, "os": platform_name(), "features": ["transfers", "paste-files", "physical-keys"] }),
        );
        return;
    }
    if kind != "actions" {
        reply(json!({ "ok": false, "error": "Unknown message" }));
        return;
    }
    let now = now_ms();
    let list = message.get("actions").and_then(Value::as_array);
    fn is_move(action: &Value) -> bool {
        action.get("type").and_then(Value::as_str) == Some("move")
    }
    let all = |test: fn(&Value) -> bool| list.is_some_and(|l| !l.is_empty() && l.iter().all(test));
    let moves_only = all(is_move);
    // Typing (#2909): physical keys and moves only, a person's own Control, not an agent's.
    let light = all(|a| is_move(a) || is_physical_key(a));
    {
        let (budget, limit) = if light {
            (&peer.moves, PEER_MOVES_PER_SECOND)
        } else {
            (&peer.requests, PEER_REQUESTS_PER_SECOND)
        };
        let mut recent = lock(budget);
        recent.retain(|at| now - at < 1000);
        if recent.len() >= limit {
            drop(recent);
            reply(
                json!({ "ok": false, "status": 429, "code": "rate-limited", "error": "Too many actions at once; slow down" }),
            );
            return;
        }
        recent.push(now);
    }
    let client = if moves_only {
        cached_client(&shared, &peer)
    } else {
        peer_client(&shared, &peer, "view")
    };
    // Closed while its access was checked: it is not done.
    if peer.closed.load(Ordering::SeqCst) {
        return;
    }
    let Some(client) = client else {
        close_peer(&shared, session);
        return;
    };
    *lock(&peer.last_action) = Instant::now();
    if !client.may("control") {
        // A refused batch releases its keys (#2909): the viewer forgets its own without ups.
        let _idle = peer.running.write().await;
        release_keys(&shared.backend, &peer).await;
        reply(
            json!({ "ok": false, "status": 403, "code": "view-only", "error": "This client is not allowed to control" }),
        );
        return;
    }
    note_held_keys(
        &mut lock(&peer.held_keys),
        list.map_or(&[][..], Vec::as_slice),
    );
    // A drag's press (#2736): let go of it if this view closes before its release arrives.
    for action in list.into_iter().flatten() {
        let button = action
            .get("button")
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty())
            .unwrap_or("left")
            .to_string();
        match action.get("type").and_then(Value::as_str) {
            Some("down") => {
                lock(&peer.held).insert(button);
            }
            Some("up") => {
                lock(&peer.held).remove(&button);
            }
            _ => {}
        }
    }
    let running = peer.running.read().await;
    let performed = perform_actions(&shared, &client, message.get("actions"), light, true).await;
    if performed.is_err() {
        // A failed batch (input-blocked, screen-locked, an error part way) releases every key
        // the view holds: it can't know which of its actions ran, so it forgets its own (#2909).
        release_keys(&shared.backend, &peer).await;
    }
    drop(running);
    watch_held_keys(&shared, &peer);
    match performed {
        Ok(result) => reply(ok_with(result)),
        Err(error) => {
            let (problem, _) = screen_problem(&error.message);
            let code = error
                .code
                .clone()
                .or_else(|| (problem == "screen-locked").then(|| problem.to_string()));
            let mut answer = json!({ "ok": false, "status": error.status, "error": error.message });
            if let Some(code) = code {
                answer["code"] = json!(code);
            }
            reply(answer);
        }
    }
}

fn ok_with(result: Value) -> Value {
    let mut answer = json!({ "ok": true });
    if let (Some(target), Some(fields)) = (answer.as_object_mut(), result.as_object()) {
        target.extend(fields.clone());
    }
    answer
}

fn failed(error: &ApiError) -> Value {
    let mut answer = json!({ "ok": false, "status": error.status, "error": error.message });
    if let Some(code) = &error.code {
        answer["code"] = json!(code);
    }
    answer
}

async fn peer_offer(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Body64(body): Body64,
) -> Response {
    let signed = signed_with!(shared, headers, peer, "view");
    let sdp = body.get("sdp").and_then(Value::as_str).unwrap_or("");
    if !sdp.starts_with("v=0") || sdp.len() > MAX_SDP_LENGTH {
        return json_status(400, json!({ "error": "A WebRTC offer (sdp) is required" }));
    }
    let max_width = number_or(body.get("maxWidth"), 1280.0).clamp(MIN_FRAME_WIDTH, MAX_FRAME_WIDTH);
    let result = match shared
        .backend
        .call("peer_offer", json!({ "sdp": sdp, "maxWidth": max_width }))
        .await
    {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };
    let Some(session) = result.get("session").and_then(Value::as_u64) else {
        return ApiError::new(502, "Remote Control helper gave no session").into_response();
    };
    let entry = Arc::new(Peer {
        client_id: signed.client.id().to_string(),
        token: signed.token.clone(),
        address: signed.address.clone(),
        closed: AtomicBool::new(false),
        held: Mutex::new(HashSet::new()),
        held_keys: Mutex::new(Vec::new()),
        last_action: Mutex::new(Instant::now()),
        watchdog: AtomicBool::new(false),
        running: tokio::sync::RwLock::new(()),
        requests: Mutex::new(Vec::new()),
        moves: Mutex::new(Vec::new()),
        transfer_client: Mutex::new(None),
    });
    lock(&shared.peers).insert(session, entry);
    let id = shared.next_stream.fetch_add(1, Ordering::SeqCst);
    lock(&shared.streams).push(Stream {
        id,
        client_id: signed.client.id().to_string(),
        end: Arc::new(Notify::new()),
        peer: Some(session),
    });
    {
        let mut timer = lock(&shared.access_timer);
        if timer.is_none() {
            let weak = Arc::downgrade(&shared);
            *timer = Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(PEER_ACCESS_CHECK).await;
                    let Some(shared) = weak.upgrade() else { return };
                    revalidate_peers(&shared, |_| true).await;
                }
            }));
        }
    }
    (shared.log)(&format!(
        "[remote-control] live view (WebRTC) for {}, session {session}",
        signed.client.name()
    ));
    Json(json!({ "sdp": result.get("sdp"), "session": session })).into_response()
}

// --- transfers ---------------------------------------------------------------------------

fn is_transfer(kind: &str) -> bool {
    Transfers::handles(kind) || clipboard_share::handles(kind) || file_browse::handles(kind)
}

fn note(shared: &Shared, client: &Authenticated, transfer: Value) {
    if let Some(host) = shared.host.upgrade() {
        host.note_transfer(&client.with_actor(), &transfer);
    }
}

fn note_pulled(shared: &Shared, pulled: Option<Pulled>) {
    let Some(pulled) = pulled else { return };
    let name = pulled
        .client
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");
    (shared.log)(&format!(
        "[remote-control] {name} downloaded {} file(s) ({} bytes)",
        pulled.count, pulled.size
    ));
    if let Some(host) = shared.host.upgrade() {
        host.note_transfer(&pulled.client, &json!({ "kind": "pulled", "count": pulled.count, "name": pulled.name, "size": pulled.size }));
    }
}

/// Work that touches the user's folders. On macOS opening Desktop, Documents or Downloads can wait
/// for the person to answer a privacy prompt: run where waiting cannot freeze the daemon (the
/// runtime moves its other work off this thread).
fn files_work<T>(work: impl FnOnce() -> T) -> T {
    let multi_thread = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi_thread {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

/// One transfer message: full access (Control) includes the clipboard and files.
async fn run_transfer(
    shared: &Arc<Shared>,
    session: &str,
    client: &Authenticated,
    message: &Value,
) -> ApiResult<Value> {
    if !client.may("control") {
        return Err(ApiError::coded(
            403,
            "view-only",
            "This client is not allowed to control",
        ));
    }
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "xfer_begin" => files_work(|| shared.transfers.begin(session, message)),
        "xfer_chunk" => files_work(|| shared.transfers.chunk(session, message)),
        "xfer_cancel" => Ok(shared.transfers.cancel(session, message)),
        "xfer_end" => match files_work(|| shared.transfers.end(session, message))? {
            Ended::Image(png) => {
                let size = png.len();
                shared
                    .backend
                    .call("paste_image", crate::dib::paste_image_params(&png))
                    .await?;
                (shared.log)(&format!(
                    "[remote-control] {} pasted an image ({size} bytes)",
                    client.name()
                ));
                note(shared, client, json!({ "kind": "image", "size": size }));
                Ok(json!({ "pasted": true }))
            }
            Ended::Stored => Ok(json!({ "stored": true })),
            Ended::Saved { name, folder, size } => {
                let folder = folder.to_string_lossy().into_owned();
                (shared.log)(&format!(
                    "[remote-control] {} sent {name} ({size} bytes) to {folder}",
                    client.name()
                ));
                note(
                    shared,
                    client,
                    json!({ "kind": "file", "name": name, "size": size, "folder": folder }),
                );
                Ok(json!({ "savedAs": name, "folder": folder }))
            }
        },
        "xfer_paste" => {
            let group = shared.transfers.take_group(session, message)?;
            // A drop: click where it landed first, so the app under it takes the files.
            if let Some(point) = message.get("point").filter(|p| {
                p.get("x").and_then(Value::as_f64).is_some()
                    && p.get("y").and_then(Value::as_f64).is_some()
            }) {
                let mut click = json!({ "type": "click", "x": point["x"], "y": point["y"] });
                if point.get("width").is_some() {
                    click["width"] = point["width"].clone();
                    click["height"] = point.get("height").cloned().unwrap_or(Value::Null);
                }
                perform_actions(shared, client, Some(&json!([click])), false, false).await?;
            }
            let paths: Vec<String> = group
                .paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            shared
                .backend
                .call("paste_files", json!({ "paths": paths }))
                .await?;
            let names: Vec<String> = group
                .paths
                .iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect();
            (shared.log)(&format!(
                "[remote-control] {} pasted {} file(s) ({} bytes)",
                client.name(),
                names.len(),
                group.size
            ));
            note(
                shared,
                client,
                json!({ "kind": "files", "count": names.len(), "name": names.first(), "size": group.size }),
            );
            Ok(json!({ "pasted": names.len() }))
        }
        "clipboard_watch" => {
            let on = message.get("on") != Some(&Value::Bool(false));
            let first = if on && shared.clipboard.needs_first_seq() {
                Some(
                    shared
                        .backend
                        .call("clipboard_seq", json!({}))
                        .await?
                        .get("seq")
                        .cloned()
                        .unwrap_or(Value::Null),
                )
            } else {
                None
            };
            let answer = shared.clipboard.watch(session, on, first);
            ensure_clipboard_poll(shared);
            Ok(answer)
        }
        "clipboard_get" => {
            let content = shared.backend.call("clipboard_read", json!({})).await?;
            Ok(match shared.clipboard.announce(session, &content) {
                Some(mut shown) => {
                    if let Some(fields) = shown.as_object_mut() {
                        fields.remove("type");
                    }
                    shown
                }
                None => json!({ "kind": "empty" }),
            })
        }
        "pull_chunk" => {
            let (answer, pulled) = files_work(|| {
                shared
                    .clipboard
                    .chunk(session, Some(&client.with_actor()), message)
            })?;
            note_pulled(shared, pulled);
            Ok(answer)
        }
        "pull_close" => {
            let pulled = shared.clipboard.close(session, message);
            note_pulled(shared, pulled);
            Ok(json!({}))
        }
        "fs_list" => files_work(|| file_browse::list(&shared.home, message)),
        "fs_pull" => {
            let (path, name, size) =
                files_work(|| file_browse::pull_target(&shared.home, message))?;
            let batch = message.get("batch").and_then(Value::as_str);
            let pull = shared.clipboard.offer_file(session, path, size, batch);
            Ok(json!({ "pull": pull, "name": name, "size": size }))
        }
        _ => Err(ApiError::bad_request("Unknown message")),
    }
}

/// While a viewer shares the clipboard: check it every POLL_MS, announce new copies.
fn ensure_clipboard_poll(shared: &Arc<Shared>) {
    if shared.clipboard.watching().is_empty()
        || shared.clipboard_polling.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let weak = Arc::downgrade(shared);
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
            let Some(shared) = weak.upgrade() else { return };
            if shared.clipboard.watching().is_empty() {
                shared.clipboard_polling.store(false, Ordering::SeqCst);
                return;
            }
            if let Err(error) = poll_clipboard(&shared).await {
                (shared.log)(&format!(
                    "[remote-control] clipboard check failed: {}",
                    error.message
                ));
            }
        }
    });
    lock(&shared.tasks).push(task);
}

async fn poll_clipboard(shared: &Arc<Shared>) -> ApiResult<()> {
    let answer = shared.backend.call("clipboard_seq", json!({})).await?;
    let seq = answer.get("seq").cloned().unwrap_or(Value::Null);
    if !shared.clipboard.changed(&seq) || answer.get("own") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    let content = shared.backend.call("clipboard_read", json!({})).await?;
    for session in shared.clipboard.watching() {
        // Each copy is checked against the client's permission as it is now.
        let peer = session
            .parse::<u64>()
            .ok()
            .and_then(|s| lock(&shared.peers).get(&s).cloned());
        let allowed = peer.is_some_and(|peer| peer_client(shared, &peer, "control").is_some());
        if !shared.clipboard.is_watching(&session) {
            continue;
        }
        if !allowed {
            shared.clipboard.unwatch(&session);
            continue;
        }
        if let (Some(message), Ok(number)) = (
            shared.clipboard.announce(&session, &content),
            session.parse::<u64>(),
        ) {
            send_to_peer(shared, number, message);
        }
    }
    Ok(())
}

/// Agents: the live view's transfer messages over HTTP, in a session of their own per client.
async fn transfer(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Body64(message): Body64,
) -> Response {
    let signed = signed_with!(shared, headers, peer, "control");
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    if !is_transfer(kind) || kind == "clipboard_watch" {
        return json_status(400, json!({ "error": "Unknown transfer message" }));
    }
    let session = format!("http:{}", signed.client.id());
    let first = {
        let mut sessions = lock(&shared.http_sessions);
        let first = sessions.is_empty();
        sessions.insert(session.clone(), now_ms());
        first
    };
    if first {
        let weak = Arc::downgrade(&shared);
        let sweep = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis((HTTP_TRANSFER_IDLE_MS / 4) as u64)).await;
                let Some(shared) = weak.upgrade() else { return };
                let idle: Vec<String> = {
                    let mut sessions = lock(&shared.http_sessions);
                    let idle: Vec<String> = sessions
                        .iter()
                        .filter(|(_, at)| now_ms() - **at >= HTTP_TRANSFER_IDLE_MS)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for key in &idle {
                        sessions.remove(key);
                    }
                    idle
                };
                for key in idle {
                    shared.transfers.close_session(&key);
                    shared.clipboard.close_session(&key);
                }
                if lock(&shared.http_sessions).is_empty() {
                    return;
                }
            }
        });
        lock(&shared.tasks).push(sweep);
    }
    match run_transfer(&shared, &session, &signed.client, &message).await {
        Ok(result) => no_store(Json(result).into_response()),
        Err(error) => error.into_response(),
    }
}

/// Stop the desktop helper, which ends every live view and session on this computer: that takes
/// Control. A view-only client stops its own live views only.
async fn stop(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let signed = signed_with!(shared, headers, peer, "view");
    if !signed.client.may("control") {
        let id = signed.client.id().to_string();
        end_streams(&shared, &|client_id: &str| client_id == id);
        return Json(json!({ "stopped": true })).into_response();
    }
    match shared.backend.stop().await {
        Ok(()) => Json(json!({ "stopped": true })).into_response(),
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_keys_follow_a_batch_in_order() {
        let mut held = Vec::new();
        let batch = [
            json!({ "type": "key", "code": "ShiftLeft", "down": true }),
            json!({ "type": "key", "code": "KeyA", "down": true }),
            json!({ "type": "move", "x": 1, "y": 2 }),
            // A repeat keeps its place; an up for a key not held changes nothing.
            json!({ "type": "key", "code": "KeyA", "down": true, "repeat": true }),
            json!({ "type": "key", "code": "KeyB", "down": false }),
            json!({ "type": "key", "keys": "ctrl+l" }),
            json!({ "type": "key", "code": "ControlLeft", "down": true }),
            json!({ "type": "key", "code": "KeyA", "down": false }),
        ];
        note_held_keys(&mut held, &batch);
        assert_eq!(held, vec!["ShiftLeft", "ControlLeft"]);
        note_held_keys(
            &mut held,
            &[json!({ "type": "key", "code": "ShiftLeft", "down": false })],
        );
        assert_eq!(held, vec!["ControlLeft"]);
    }

    #[test]
    fn a_locked_screen_is_said_so() {
        assert_eq!(
            screen_problem("Screen capture inhibited").0,
            "screen-locked"
        );
        assert_eq!(screen_problem("no display").0, "screen-unavailable");
        assert!(matches!(platform_name(), "darwin" | "win32" | "linux"));
    }

    #[test]
    fn failures_count_per_ipv4_address_or_ipv6_64() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        assert_eq!(source_of(ip("203.0.113.7")), "203.0.113.7");
        assert_eq!(source_of(ip("::ffff:203.0.113.7")), "203.0.113.7");
        assert_eq!(
            source_of(ip("2001:db8:aa:bb:1:2:3:4")),
            "2001:db8:aa:bb::/64"
        );
        assert_eq!(
            source_of(ip("2001:db8:aa:bb:ffff::9")),
            source_of(ip("2001:db8:aa:bb::1"))
        );
        assert_ne!(
            source_of(ip("2001:db8:aa:bc::1")),
            source_of(ip("2001:db8:aa:bb::1"))
        );
        // The totals are kept as long as they are checked: an hour.
        assert_eq!(failure_window(PUBLIC_TOTAL), PUBLIC_TOTAL_WINDOW_MS);
        assert_eq!(failure_window(TAILNET_TOTAL), PUBLIC_TOTAL_WINDOW_MS);
        assert_eq!(failure_window("password:1.2.3.4"), AUTH_FAILURE_WINDOW_MS);
    }

    #[test]
    fn javascript_numbers_with_a_fallback() {
        assert_eq!(number_or(Some(&json!(0)), 75.0), 75.0);
        assert_eq!(number_or(Some(&json!("40")), 75.0), 40.0);
        assert_eq!(number_or(None, 75.0), 75.0);
    }
}
