//! The Remote Control API controllers speak, on this computer's Tailscale address, and through
//! RealTimeX Link's own Public Exposure relay (`public`, relay.rs: the caller's address is the
//! public client's, /hello leaves the machine name out, failed sign-ins are limited per address
//! and in total, and there is no MJPEG live view through the relay) (protocol 2): /hello, sign-in (/connect, connection requests),
//! then with a client token /status, /screenshot, /actions, /frames (MJPEG live view),
//! /peer/offer (the WebRTC live view, whose DataChannel messages come back as helper events),
//! /displays, /transfer and /stop. RealTimeX's controller (server/utils/remoteControl/computers.js) and agents' CLI speak it.
//! Which display a view shows is its own (displays.rs, realtimex-link#12); a call that names none
//! means the primary display, as before.

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
use crate::codes::physical_key;
use crate::displays;
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
const FEATURES: [&str; 6] = [
    "peer-view",
    "connect-requests",
    "client-key",
    "identity-token",
    "transfers",
    // realtimex-link#12: GET /displays, and `display` on /screenshot, /actions and /frames.
    "displays",
];
/// What a live view's `info` lists: the features the viewer adapts to.
const PEER_FEATURES: [&str; 4] = ["transfers", "paste-files", "physical-keys", "displays"];
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
    /// The display this view shows (realtimex-link#12), always a concrete id: rtx-control
    /// captures it, and its actions' moves land on it. None only while no display list could
    /// be read (then rtx-control's own primary, as before).
    display: Mutex<Option<String>>,
    /// It follows the primary: it never chose one, or its own went away. When the primary
    /// changes, it is handed over (its held input let go first), never switched silently.
    follows: AtomicBool,
    /// What the viewer was last told it shows, and the list it was told: a fresh list is
    /// pushed only when that changes.
    told: Mutex<(Option<String>, Vec<Value>)>,
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
    /// The latest screenshot's size per client and display (None: the primary).
    shot_sizes: Mutex<HashMap<ShotOf, (u64, u64)>>,
    peers: Mutex<HashMap<u64, Arc<Peer>>>,
    /// Reads the display list every displays::POLL while a WebRTC view is open.
    display_poll: Mutex<Option<JoinHandle<()>>>,
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
        display_poll: Mutex::new(None),
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
        .route("/remote-control/displays", get(list_displays))
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
/// A client's screenshots of one display (None: the primary).
type ShotOf = (String, Option<String>);

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
    let display = displays::named(body.get("display"));
    let mut params = json!({ "quality": quality });
    if let Some(display) = &display {
        params["display"] = json!(display);
    }
    let shot = match shared.backend.call("screenshot", params).await {
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
    lock(&shared.shot_sizes).insert(
        (signed.client.id().to_string(), display.clone()),
        (width, height),
    );
    (shared.log)(&format!(
        "[remote-control] screenshot {width}x{height} for {}",
        signed.client.name()
    ));
    let data = shot["data"].as_str().and_then(unbase64).unwrap_or_default();
    let mime = shot["mime"].as_str().unwrap_or("image/jpeg").to_string();
    let mut response = (StatusCode::OK, data).into_response();
    let headers = response.headers_mut();
    let mut fields = vec![
        ("content-type", mime),
        ("cache-control", "no-store".into()),
        ("x-screen-width", width.to_string()),
        ("x-screen-height", height.to_string()),
    ];
    // Which display the picture is (realtimex-link#12): the one named, else the primary.
    if let Some(shown) = shot["display"].as_str() {
        fields.push(("x-screen-display", shown.to_string()));
    }
    for (name, value) in fields {
        if let Ok(value) = value.parse() {
            headers.insert(name, value);
        }
    }
    response
}

/// The displays (realtimex-link#12), in the picker's order: `{ displays: [{ id, name, primary,
/// x, y, width, height, scale }] }`.
async fn list_displays(
    State(shared): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let _signed = signed_with!(shared, headers, peer, "view");
    match fresh_displays(&shared).await {
        Ok(list) => no_store(Json(json!({ "displays": list })).into_response()),
        Err(error) => error.into_response(),
    }
}

/// The display list as it is now.
async fn fresh_displays(shared: &Shared) -> ApiResult<Vec<Value>> {
    let answer = shared.backend.call("displays", json!({})).await?;
    Ok(displays::list_of(&answer))
}

/// 404 `display-not-found` unless `display` (when one is named) is attached now: checked before
/// anything of a request runs, so an agent never acts on another screen (design §3.3).
async fn require_display(shared: &Shared, display: Option<&str>) -> ApiResult<()> {
    let Some(display) = display else {
        return Ok(());
    };
    if displays::has(&fresh_displays(shared).await?, display) {
        Ok(())
    } else {
        Err(ApiError::coded(
            404,
            crate::backend::DISPLAY_NOT_FOUND,
            format!("Display {display} is not connected"),
        ))
    }
}

/// What a viewer shows about the latest action: its marker and caption.
/// `display`: the display it was aimed at, so a viewer shows the marker only there
/// (realtimex-link#12).
fn describe_action(
    action: &Value,
    client: &Authenticated,
    shot: Option<(u64, u64)>,
    display: Option<&str>,
) -> serde_json::Map<String, Value> {
    let mut shown = serde_json::Map::new();
    shown.insert(
        "type".into(),
        action.get("type").cloned().unwrap_or(Value::Null),
    );
    shown.insert("client".into(), json!(client.name()));
    shown.insert("at".into(), json!(iso_now()));
    if let Some(display) = display {
        shown.insert("display".into(), json!(display));
    }
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

/// Run `actions` for `client` on `display` (None: the primary): the latest one is shown on every
/// live view, marked with `shown_on` (the display it landed on, when known).
async fn perform_actions(
    shared: &Arc<Shared>,
    client: &Authenticated,
    actions: Option<&Value>,
    quiet: bool,
    live_view: bool,
    display: Option<&str>,
    shown_on: Option<&str>,
) -> ApiResult<Value> {
    let performed = run_actions(&shared.backend, actions, live_view, display).await?;
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
        let size = lock(&shared.shot_sizes)
            .get(&(client.id().to_string(), display.map(str::to_string)))
            .copied();
        *lock(&shared.last_action) = Some((
            describe_action(action, client, size, shown_on),
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
    let display = displays::named(body.get("display"));
    if let Err(error) = require_display(&shared, display.as_deref()).await {
        return error.into_response();
    }
    let actions = body.get("actions");
    let display = display.as_deref();
    match perform_actions(&shared, &signed.client, actions, false, false, display, display).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => error.into_response(),
    }
}

// --- the MJPEG live view -----------------------------------------------------------------

/// JPEG frames as multipart/x-mixed-replace, each with the screen size, its display and the
/// latest action (base64 JSON) in its headers. The next frame is taken only once the last one
/// has gone out. `?display=<id>` shows that display (realtimex-link#12): unknown at the start is
/// 404; one that goes away later switches the stream to the primary, which its headers show
/// (no JSON part: an older client reads one as "no picture").
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
    let mut display = query.get("display").filter(|d| !d.is_empty()).cloned();
    if let Err(error) = require_display(&shared, display.as_deref()).await {
        return error.into_response();
    }
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
            let mut params = json!({ "quality": quality, "live": true, "maxWidth": max_width });
            if let Some(display) = &display {
                params["display"] = json!(display);
            }
            let shot = tokio::select! {
                shot = shared.backend.call("screenshot", params) => shot,
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
                        if let Some(shown) = shot["display"].as_str() {
                            part.extend_from_slice(
                                format!("X-Screen-Display: {shown}\r\n").as_bytes(),
                            );
                        }
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
                // Its display went away: the primary from now on, at once.
                Err(error)
                    if display.is_some() && displays::stream_falls_back(error.code.as_deref()) =>
                {
                    (shared.log)(&format!(
                        "[remote-control] live view: display {} went away or can't be captured; showing the primary",
                        display.as_deref().unwrap_or("")
                    ));
                    display = None;
                    continue;
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
        if let Some(poll) = lock(&shared.display_poll).take() {
            poll.abort();
        }
    }
    // Nothing more runs for this view. A button it still holds would stay down: let go of it
    // once the actions already running are done.
    peer.closed.store(true, Ordering::SeqCst);
    let backend = shared.backend.clone();
    tokio::spawn(async move {
        let _all_done = peer.running.write().await;
        release_input(&backend, &peer).await;
        let _ = backend
            .call("peer_close", json!({ "session": session }))
            .await;
    });
}

/// Let go of every key and mouse button this view holds (its keys first, as a refused batch
/// does). The caller makes sure none of its actions are still running.
async fn release_input(backend: &Arc<Backend>, peer: &Peer) {
    release_keys(backend, peer).await;
    let held: Vec<String> = lock(&peer.held).drain().collect();
    if let Some(ups) = button_ups(&held) {
        let _ = run_actions(backend, Some(&ups), false, None).await;
    }
}

/// The ups for these held mouse buttons, as one batch; None when none is held.
fn button_ups(held: &[String]) -> Option<Value> {
    (!held.is_empty()).then(|| {
        Value::Array(
            held.iter()
                .map(|button| json!({ "type": "up", "button": button }))
                .collect(),
        )
    })
}

/// Let go of every physical key this view holds, the last pressed first (#2909). The caller makes
/// sure none of its actions are still running.
async fn release_keys(backend: &Arc<Backend>, peer: &Peer) {
    let held: Vec<String> = std::mem::take(&mut *lock(&peer.held_keys));
    for up in release_actions(&held) {
        // One up each: an up that fails doesn't keep the others down (rtx-control keeps the
        // failed one held and lets go of it when its session ends), and no batch limit applies.
        let _ = run_actions(backend, Some(&Value::Array(vec![up])), true, None).await;
    }
}

/// The ups for these held keys, the last pressed first.
fn release_actions(held: &[String]) -> Vec<Value> {
    held.iter()
        .rev()
        .map(|code| json!({ "type": "key", "code": code, "down": false }))
        .collect()
}

/// The physical key actions Link presses: a code with a key here, and a boolean `down`.
fn key_transitions(actions: &[Value]) -> impl Iterator<Item = (&str, bool)> {
    actions
        .iter()
        .filter(|a| is_physical_key(a))
        .filter_map(|action| {
            let code = action.get("code").and_then(Value::as_str)?;
            let down = action.get("down").and_then(Value::as_bool)?;
            physical_key(code).map(|_| (code, down))
        })
}

/// Before a batch runs: every key it may press is held, and none it lets go of is forgotten
/// yet. If the batch fails or stops part way, its downs and the keys held before it are all
/// released (review of !6).
fn hold_downs(held: &mut Vec<String>, actions: &[Value]) {
    for (code, down) in key_transitions(actions) {
        if down && !held.iter().any(|held| held == code) {
            held.push(code.to_string());
        }
    }
}

/// After the whole batch ran: the keys it let go of are no longer held.
fn settle_held(held: &mut Vec<String>, actions: &[Value]) {
    for (code, down) in key_transitions(actions) {
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
                // Its display went away (realtimex-link#12): the view falls back at once.
                // Bound to the display that failed: a view that shows another by the time it is
                // handled (a poll or its viewer moved it already) is left alone.
                Some("peer_display_lost") => {
                    let text = |name: &str| event.get(name).and_then(Value::as_str);
                    let Some(display) = text("display") else {
                        continue;
                    };
                    let lost = Lost {
                        session,
                        display: display.to_string(),
                        reason: text("reason").unwrap_or(displays::DISPLAY_REMOVED).to_string(),
                    };
                    tokio::spawn(async move { check_views(&shared, Some(lost)).await });
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
        // What the viewer adapts to: this computer's OS (shortcuts: ⌘ on macOS, Ctrl elsewhere),
        // and its displays with the one this view shows (realtimex-link#12).
        if peer_client(&shared, &peer, "view").is_none() {
            close_peer(&shared, session);
            return;
        }
        let list = fresh_displays(&shared).await.unwrap_or_default();
        let shown = displays::shown(lock(&peer.display).as_deref(), &list);
        if lock(&peer.display).is_none() && shown.is_some() {
            // No list at its start: the next check hands it to the primary concretely.
            check_views(&shared, None).await;
        }
        *lock(&peer.told) = (shown.clone(), list.clone());
        reply(json!({
            "ok": true,
            "os": platform_name(),
            "features": PEER_FEATURES,
            "displays": list,
            "display": shown,
        }));
        return;
    }
    if kind == "select_display" {
        if !within_budget(&peer.requests, PEER_REQUESTS_PER_SECOND) {
            reply(rate_limited());
            return;
        }
        // Choosing a screen is viewing, not control.
        if peer_client(&shared, &peer, "view").is_none() {
            close_peer(&shared, session);
            return;
        }
        let requested = displays::named(message.get("display"));
        reply(select_display(&shared, session, &peer, requested).await);
        return;
    }
    if kind != "actions" {
        reply(json!({ "ok": false, "error": "Unknown message" }));
        return;
    }
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
        if !within_budget(budget, limit) {
            reply(rate_limited());
            return;
        }
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
    if let Some(answer) = act(&shared, &peer, &client, &message, light).await {
        reply(answer);
    }
}

/// Run a batch of a live view's actions on the display it shows, once its running actions
/// allow; None when the view closed meanwhile (nothing to answer).
async fn act(
    shared: &Arc<Shared>,
    peer: &Arc<Peer>,
    client: &Authenticated,
    message: &Value,
    light: bool,
) -> Option<Value> {
    let list = message.get("actions").and_then(Value::as_array);
    let running = peer.running.read().await;
    // Closed while it waited: close_peer releases what the view held, and nothing more runs.
    if peer.closed.load(Ordering::SeqCst) {
        return None;
    }
    // A batch aimed at another screen than the one shown now (a late click after a switch, a
    // fallback or a handover) is refused before anything of it runs (realtimex-link#12). What
    // is checked is what runs: the concrete display this view shows, which its moves name, so
    // the helper can't resolve "the primary" to another screen in between (review of !15).
    let shown = lock(&peer.display).clone();
    let aimed = displays::named(message.get("display"));
    if let Err(refused) = displays::batch_allowed(aimed.as_deref(), shown.as_deref()) {
        return Some(refused);
    }
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
    let actions = list.map_or(&[][..], Vec::as_slice);
    hold_downs(&mut lock(&peer.held_keys), actions);
    let performed = perform_actions(
        shared,
        client,
        message.get("actions"),
        light,
        true,
        shown.as_deref(),
        shown.as_deref(),
    )
    .await;
    match &performed {
        Ok(_) => settle_held(&mut lock(&peer.held_keys), actions),
        // A failed batch (input-blocked, screen-locked, an error part way, a refused action)
        // releases every key the view holds: it can't know which of its actions ran, so it
        // forgets its own (#2909).
        Err(_) => release_keys(&shared.backend, peer).await,
    }
    drop(running);
    watch_held_keys(shared, peer);
    Some(match performed {
        Ok(result) => ok_with(result),
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
            answer
        }
    })
}

/// One more request of this view within `limit` a second, or false when it is over.
fn within_budget(budget: &Mutex<Vec<i64>>, limit: usize) -> bool {
    let now = now_ms();
    let mut recent = lock(budget);
    recent.retain(|at| now - at < 1000);
    if recent.len() >= limit {
        return false;
    }
    recent.push(now);
    true
}

fn rate_limited() -> Value {
    json!({ "ok": false, "status": 429, "code": "rate-limited", "error": "Too many actions at once; slow down" })
}

/// Show another display on this view (realtimex-link#12): checked against a fresh list, the
/// view's held keys and buttons let go (a drag or a held key never crosses screens), then the
/// helper switches its picture. The session, Control, transfers, the clipboard watch and the
/// quality stay. The display already shown answers `ok` and changes nothing.
async fn select_display(
    shared: &Arc<Shared>,
    session: u64,
    peer: &Arc<Peer>,
    requested: Option<String>,
) -> Value {
    let list = match fresh_displays(shared).await {
        Ok(list) => list,
        Err(error) => return failed(&error),
    };
    let current = displays::shown(lock(&peer.display).as_deref(), &list);
    let Some(requested) = requested else {
        return json!({ "ok": false, "status": 400, "error": "display is required", "display": current, "displays": list });
    };
    if !displays::has(&list, &requested) {
        *lock(&peer.told) = (current.clone(), list.clone());
        return json!({
            "ok": false,
            "status": 404,
            "code": crate::backend::DISPLAY_NOT_FOUND,
            "error": format!("Display {requested} is not connected"),
            "display": current,
            "displays": list,
        });
    }
    if current.as_deref() == Some(requested.as_str()) {
        // Chosen now: it stays there when the primary moves.
        if lock(&peer.display).is_some() {
            peer.follows.store(false, Ordering::SeqCst);
        }
        *lock(&peer.told) = (current.clone(), list.clone());
        return json!({ "ok": true, "display": requested, "displays": list });
    }
    let _idle = peer.running.write().await;
    if peer.closed.load(Ordering::SeqCst) {
        return json!({ "ok": false, "error": "This live view has ended" });
    }
    release_input(&shared.backend, peer).await;
    let switched = shared
        .backend
        .call("peer_display", json!({ "session": session, "display": requested }))
        .await;
    if let Err(error) = switched {
        let mut answer = failed(&error);
        answer["display"] = json!(current);
        answer["displays"] = json!(list);
        return answer;
    }
    *lock(&peer.display) = Some(requested.clone());
    peer.follows.store(false, Ordering::SeqCst);
    *lock(&peer.told) = (Some(requested.clone()), list.clone());
    (shared.log)(&format!(
        "[remote-control] live view (session {session}) shows display {requested}"
    ));
    json!({ "ok": true, "display": requested, "displays": list })
}

/// While a WebRTC view is open: read the display list every displays::POLL and tell each view
/// what changed for it (`check_views`).
fn ensure_display_poll(shared: &Arc<Shared>) {
    let mut poll = lock(&shared.display_poll);
    if poll.is_some() {
        return;
    }
    let weak = Arc::downgrade(shared);
    *poll = Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(displays::POLL).await;
            let Some(shared) = weak.upgrade() else { return };
            if lock(&shared.peers).is_empty() {
                *lock(&shared.display_poll) = None;
                return;
            }
            check_views(&shared, None).await;
        }
    }));
}

/// rtx-control lost the display a view showed (`peer_display_lost`): which view, which display,
/// and why.
struct Lost {
    session: u64,
    display: String,
    reason: String,
}

/// Read the display list and bring each open view (or only the one in `lost`, whose display
/// rtx-control just lost, and why) up to date. A view whose display went away, or that follows
/// the primary when the primary changed, is handed over: its running actions finish, its held
/// keys and buttons are let go, then rtx-control shows the new display. A view whose list or
/// display changed gets a `displays` push.
async fn check_views(shared: &Arc<Shared>, lost: Option<Lost>) {
    let Ok(list) = fresh_displays(shared).await else {
        return;
    };
    if list.is_empty() {
        // Nothing listed (enumeration failed): nothing to tell anyone.
        return;
    }
    let peers: Vec<(u64, Arc<Peer>)> = lock(&shared.peers)
        .iter()
        .filter(|(session, _)| lost.as_ref().is_none_or(|lost| lost.session == **session))
        .map(|(session, peer)| (*session, peer.clone()))
        .collect();
    for (session, peer) in peers {
        let shown = lock(&peer.display).clone();
        let follows = peer.follows.load(Ordering::SeqCst);
        let (told, told_list) = lock(&peer.told).clone();
        let checked = match &lost {
            // A loss of a display this view no longer shows: already handled (review of !15).
            Some(lost) if shown.as_deref() != Some(lost.display.as_str()) => continue,
            Some(lost) => displays::lost(Some(&lost.display), &lost.reason, &list),
            None => displays::check(shown.as_deref(), follows, told.as_deref(), &told_list, &list),
        };
        if let Some(target) = &checked.switch_to {
            let _idle = peer.running.write().await;
            // Closed, or moved meanwhile (a switch, another check, the same loss seen by a
            // poll): what was decided is about a display it no longer shows.
            if peer.closed.load(Ordering::SeqCst) || *lock(&peer.display) != shown {
                continue;
            }
            release_input(&shared.backend, &peer).await;
            let switched = shared
                .backend
                .call("peer_display", json!({ "session": session, "display": target }))
                .await;
            if let Err(error) = switched {
                (shared.log)(&format!(
                    "[remote-control] live view (session {session}): display {target} not shown: {}",
                    error.message
                ));
                continue;
            }
            *lock(&peer.display) = Some(target.clone());
            // On the primary now: it follows it from here.
            peer.follows.store(true, Ordering::SeqCst);
            (shared.log)(&format!(
                "[remote-control] live view (session {session}) shows display {target} (was {})",
                shown.as_deref().unwrap_or("the primary")
            ));
        }
        // Moved meanwhile without a switch here: this push would tell of an older display.
        let expected = checked.switch_to.clone().or(shown.clone());
        if *lock(&peer.display) != expected {
            continue;
        }
        let fallback = checked
            .push
            .as_ref()
            .is_some_and(|push| push.get("fallback").is_some());
        if told_list.is_empty() && !fallback {
            // A viewer that never asked (`info`) hears nothing it didn't ask for.
            let now = lock(&peer.display).clone();
            *lock(&peer.told) = (now, list.clone());
            continue;
        }
        if let Some(push) = checked.push {
            let pushed: Vec<Value> = displays::list_of(&push);
            *lock(&peer.told) = (push["display"].as_str().map(String::from), pushed);
            send_to_peer(shared, session, push);
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
    let mut params = json!({ "sdp": sdp, "maxWidth": max_width });
    // A display the viewer showed before (a hint for the same session, realtimex-link#12): used
    // while it is attached, else the view starts on the primary.
    // A view always shows a concrete display, the primary unless it had one (realtimex-link#12;
    // review of !15): what it shows and where its clicks land can't come apart.
    let attached = fresh_displays(&shared).await.unwrap_or_default();
    let hint = displays::named(body.get("display")).filter(|hint| displays::has(&attached, hint));
    let follows = hint.is_none();
    let display = hint.or_else(|| displays::primary(&attached));
    if let Some(display) = &display {
        params["display"] = json!(display);
    }
    let result = match shared.backend.call("peer_offer", params).await {
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
        display: Mutex::new(display),
        follows: AtomicBool::new(follows),
        told: Mutex::new((None, Vec::new())),
    });
    lock(&shared.peers).insert(session, entry);
    ensure_display_poll(&shared);
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
                // On the display this view shows (realtimex-link#12).
                let peer = session
                    .parse::<u64>()
                    .ok()
                    .and_then(|number| lock(&shared.peers).get(&number).cloned());
                let mut display = None;
                if let Some(peer) = peer {
                    display = lock(&peer.display).clone();
                }
                let display = display.as_deref();
                perform_actions(shared, client, Some(&json!([click])), false, false, display, display)
                    .await?;
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

    fn key(code: &str, down: bool) -> Value {
        json!({ "type": "key", "code": code, "down": down })
    }

    fn codes(held: &[String]) -> Vec<&str> {
        held.iter().map(String::as_str).collect()
    }

    #[test]
    fn held_keys_follow_a_batch_that_ran() {
        let mut held = Vec::new();
        let batch = [
            key("ShiftLeft", true),
            key("KeyA", true),
            json!({ "type": "move", "x": 1, "y": 2 }),
            // A repeat keeps its place; an up for a key not held changes nothing.
            json!({ "type": "key", "code": "KeyA", "down": true, "repeat": true }),
            key("KeyB", false),
            json!({ "type": "key", "keys": "ctrl+l" }),
            key("ControlLeft", true),
            key("KeyA", false),
        ];
        hold_downs(&mut held, &batch);
        assert_eq!(codes(&held), ["ShiftLeft", "KeyA", "ControlLeft"]);
        settle_held(&mut held, &batch);
        assert_eq!(codes(&held), ["ShiftLeft", "ControlLeft"]);
        let up = [key("ShiftLeft", false)];
        hold_downs(&mut held, &up);
        settle_held(&mut held, &up);
        assert_eq!(codes(&held), ["ControlLeft"]);
    }

    /// Review of !6: a down that ran before a later action failed is still released.
    #[test]
    fn a_batch_that_fails_part_way_releases_what_it_pressed() {
        let mut held = Vec::new();
        let batch = [
            key("ShiftLeft", true),
            json!({ "type": "move", "x": 1, "y": 2 }),
            key("ShiftLeft", false),
        ];
        hold_downs(&mut held, &batch);
        // The move failed: no settle_held. What release_keys sends:
        assert_eq!(release_actions(&held), vec![key("ShiftLeft", false)]);
    }

    /// Review of !6: a refused batch's up for a held key doesn't forget it.
    #[test]
    fn a_refused_batch_keeps_the_keys_held_before_it() {
        let mut held = vec!["ShiftLeft".to_string()];
        let batch = [json!({ "type": "zap" }), key("ShiftLeft", false)];
        assert!(crate::actions::plan_actions(Some(&Value::Array(batch.to_vec())), true).is_err());
        hold_downs(&mut held, &batch);
        assert_eq!(release_actions(&held), vec![key("ShiftLeft", false)]);
    }

    /// Review of !6: codes Link skips are not tracked, and more keys than a batch takes are all
    /// released.
    #[test]
    fn only_keys_link_presses_are_held_and_all_of_them_are_released() {
        let mut held = Vec::new();
        let mut unknown: Vec<Value> = (0..50).map(|n| key(&format!("Lang{n}"), true)).collect();
        unknown.push(key("ShiftLeft", true));
        hold_downs(&mut held, &unknown);
        settle_held(&mut held, &unknown);
        assert_eq!(codes(&held), ["ShiftLeft"]);

        let all: Vec<Value> = crate::codes::KEYS
            .iter()
            .map(|k| key(k.code, true))
            .collect();
        assert!(all.len() > crate::actions::MAX_ACTIONS);
        let mut held = Vec::new();
        hold_downs(&mut held, &all);
        let ups = release_actions(&held);
        assert_eq!(ups.len(), crate::codes::KEYS.len());
        assert_eq!(ups[0], key("NumpadEqual", false));
        // Each up goes alone, so every one is a valid batch.
        for up in ups {
            let one = crate::actions::plan_actions(Some(&json!([up])), true).unwrap();
            assert_eq!(one[0].len(), 1);
        }
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

    /// A stand-in for rtx-control that writes every request line to `requests` and answers
    /// `displays` with the list in `displays.json` (both next to it); everything else is ok.
    #[cfg(unix)]
    fn fake_helper(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("rtx-control");
        let script = format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> '{requests}'
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"op":"displays"'*) printf '{{"id":%s,"ok":true,"displays":%s}}\n' "$id" "$(cat '{list}')" ;;
    *) printf '{{"id":%s,"ok":true}}\n' "$id" ;;
  esac
done
"#,
            requests = dir.join("requests").display(),
            list = dir.join("displays.json").display(),
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn test_shared(backend: Arc<Backend>, dir: &std::path::Path) -> Arc<Shared> {
        Arc::new(Shared {
            host: Weak::new(),
            public: false,
            backend,
            log: Arc::new(|_: &str| {}),
            failures: Mutex::new(HashMap::new()),
            last_action: Mutex::new(None),
            streams: Mutex::new(Vec::new()),
            next_stream: AtomicU64::new(1),
            shot_sizes: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            display_poll: Mutex::new(None),
            http_sessions: Mutex::new(HashMap::new()),
            transfers: Transfers::new(dir.join("temp"), dir.join("downloads")),
            clipboard: ClipboardShare::default(),
            clipboard_polling: AtomicBool::new(false),
            home: dir.to_path_buf(),
            tasks: Mutex::new(Vec::new()),
            access_timer: Mutex::new(None),
        })
    }

    #[cfg(unix)]
    fn test_peer() -> Arc<Peer> {
        Arc::new(Peer {
            client_id: "c".into(),
            token: "t".into(),
            address: "100.64.0.2".into(),
            closed: AtomicBool::new(false),
            held: Mutex::new(HashSet::new()),
            held_keys: Mutex::new(Vec::new()),
            last_action: Mutex::new(Instant::now()),
            watchdog: AtomicBool::new(false),
            running: tokio::sync::RwLock::new(()),
            requests: Mutex::new(Vec::new()),
            moves: Mutex::new(Vec::new()),
            transfer_client: Mutex::new(None),
            display: Mutex::new(Some("1".into())),
            follows: AtomicBool::new(true),
            told: Mutex::new((None, Vec::new())),
        })
    }

    #[cfg(unix)]
    fn requests_of(dir: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("requests"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// realtimex-link#12 with a fake helper: a switch lets go of the view's held keys and buttons
    /// before the picture changes; an unknown display is refused and changes nothing; the
    /// display shown answers ok and does nothing; an unplugged display falls back to the primary
    /// with a push that says so.
    #[cfg(unix)]
    #[tokio::test]
    async fn select_refuse_and_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let two = json!([
            { "id": "1", "name": "", "primary": true, "x": 0, "y": 0, "width": 1920, "height": 1080, "scale": 1.0 },
            { "id": "2", "name": "", "primary": false, "x": 1920, "y": 0, "width": 2560, "height": 1440, "scale": 1.0 },
        ]);
        std::fs::write(dir.path().join("displays.json"), two.to_string()).unwrap();
        let backend = Backend::with_detected(crate::backend::Detected {
            binary: Some(fake_helper(dir.path())),
            reason: None,
            code: None,
        });
        let shared = test_shared(backend, dir.path());
        let peer = test_peer();
        lock(&shared.peers).insert(7, peer.clone());
        lock(&peer.held).insert("left".into());
        lock(&peer.held_keys).push("ShiftLeft".into());

        let answer = select_display(&shared, 7, &peer, Some("2".into())).await;
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(answer["display"], "2");
        assert_eq!(answer["displays"], two);
        assert_eq!(lock(&peer.display).as_deref(), Some("2"));
        assert!(lock(&peer.held).is_empty() && lock(&peer.held_keys).is_empty());
        let ops: Vec<String> = requests_of(dir.path())
            .iter()
            .map(|r| {
                let op = r["op"].as_str().unwrap_or("").to_string();
                match op.as_str() {
                    "key" => format!("key {} {}", r["code"].as_str().unwrap(), r["pressed"]),
                    "button" => format!("button {} {}", r["button"].as_str().unwrap(), r["pressed"]),
                    "peer_display" => format!("peer_display {} {}", r["session"], r["display"]),
                    _ => op,
                }
            })
            .filter(|op| op != "start" && op != "displays")
            .collect();
        assert_eq!(
            ops,
            ["key ShiftLeft false", "button left false", "peer_display 7 \"2\""]
        );

        // Unknown: refused, the view stays where it is.
        let refused = select_display(&shared, 7, &peer, Some("9".into())).await;
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["code"], "display-not-found");
        assert_eq!(refused["display"], "2");
        assert_eq!(lock(&peer.display).as_deref(), Some("2"));
        // The one shown: ok, and no switch.
        let switches = |dir: &std::path::Path| {
            requests_of(dir)
                .iter()
                .filter(|r| r["op"] == "peer_display")
                .count()
        };
        assert_eq!(switches(dir.path()), 1);
        let same = select_display(&shared, 7, &peer, Some("2".into())).await;
        assert_eq!(same["ok"], true);
        assert_eq!(switches(dir.path()), 1);

        // Display 2 is unplugged: the view shows the primary and hears why.
        let one = json!([two[0].clone()]);
        std::fs::write(dir.path().join("displays.json"), one.to_string()).unwrap();
        check_views(&shared, None).await;
        assert_eq!(lock(&peer.display).as_deref(), Some("1"));
        assert!(peer.follows.load(Ordering::SeqCst), "on the primary, it follows it now");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let requests = requests_of(dir.path());
        let back = requests.iter().rev().find(|r| r["op"] == "peer_display").unwrap();
        assert_eq!(back["session"], 7);
        assert_eq!(back["display"], "1");
        let pushed: Value = requests
            .iter()
            .filter(|r| r["op"] == "peer_send")
            .find_map(|r| serde_json::from_str(r["data"].as_str()?).ok())
            .expect("a displays push");
        assert_eq!(pushed["type"], "displays");
        assert_eq!(pushed["display"], "1");
        assert_eq!(pushed["fallback"], json!({ "from": "2", "reason": "display_removed" }));
    }

    #[cfg(unix)]
    struct Fake {
        dir: tempfile::TempDir,
        shared: Arc<Shared>,
        peer: Arc<Peer>,
    }

    /// A view (session 7) showing display "1" and following the primary, with a fake helper
    /// listing `list`.
    #[cfg(unix)]
    fn fake_view(list: &Value) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("displays.json"), list.to_string()).unwrap();
        let backend = Backend::with_detected(crate::backend::Detected {
            binary: Some(fake_helper(dir.path())),
            reason: None,
            code: None,
        });
        let shared = test_shared(backend, dir.path());
        let peer = test_peer();
        lock(&shared.peers).insert(7, peer.clone());
        *lock(&peer.told) = (Some("1".into()), list.as_array().unwrap().clone());
        Fake { dir, shared, peer }
    }

    #[cfg(unix)]
    fn list_of(ids: &[(&str, bool)]) -> Value {
        Value::Array(
            ids.iter()
                .enumerate()
                .map(|(n, (id, primary))| {
                    json!({ "id": id, "name": "", "primary": primary, "x": 1920 * n, "y": 0,
                        "width": 1920, "height": 1080, "scale": 1.0 })
                })
                .collect(),
        )
    }

    /// What reached the fake helper since it started, without `start` and `displays`.
    #[cfg(unix)]
    fn ops_of(dir: &std::path::Path) -> Vec<String> {
        requests_of(dir)
            .iter()
            .filter_map(|r| {
                Some(match r["op"].as_str()? {
                    "start" | "displays" | "peer_send" => return None,
                    "key" => format!("key {} {}", r["code"].as_str()?, r["pressed"]),
                    "button" => format!("button {} {}", r["button"].as_str()?, r["pressed"]),
                    "peer_display" => format!("peer_display {}", r["display"]),
                    "move" => format!("move {}", r["display"]),
                    other => other.to_string(),
                })
            })
            .collect()
    }

    #[cfg(unix)]
    fn pushed(dir: &std::path::Path) -> Vec<Value> {
        requests_of(dir)
            .iter()
            .filter(|r| r["op"] == "peer_send")
            .filter_map(|r| serde_json::from_str(r["data"].as_str()?).ok())
            .collect()
    }

    #[cfg(unix)]
    fn controller() -> Authenticated {
        Authenticated {
            client: json!({ "id": "c", "name": "Test", "permissions": { "view": true, "control": true } }),
            actor: "test",
        }
    }

    /// Review of !15, finding 2a: a view that never chose a screen, holding a button and a
    /// modifier, when its primary is unplugged (or another display becomes the primary): the
    /// held input is let go before the picture moves, and the viewer hears why.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_primary_view_is_handed_over_with_its_input_let_go() {
        for (after, fallback) in [
            (list_of(&[("2", true)]), Some("display_removed")),
            (list_of(&[("1", false), ("2", true)]), None),
        ] {
            let fake = fake_view(&list_of(&[("1", true), ("2", false)]));
            lock(&fake.peer.held).insert("left".into());
            lock(&fake.peer.held_keys).push("ShiftLeft".into());
            std::fs::write(fake.dir.path().join("displays.json"), after.to_string()).unwrap();
            check_views(&fake.shared, None).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                ops_of(fake.dir.path()),
                ["key ShiftLeft false", "button left false", "peer_display \"2\""]
            );
            assert!(lock(&fake.peer.held).is_empty() && lock(&fake.peer.held_keys).is_empty());
            assert_eq!(lock(&fake.peer.display).as_deref(), Some("2"));
            let push = pushed(fake.dir.path()).pop().expect("a displays push");
            assert_eq!(push["display"], "2");
            assert_eq!(push["fallback"]["reason"].as_str(), fallback);
        }
    }

    /// Review of !15, finding 2b: a batch aimed at display 1. Before the handover it runs on
    /// display 1 by name (never "whatever the helper's primary is now"); after the handover to
    /// 2 it is refused with 409 display-changed, and nothing of it runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_batch_aimed_at_the_old_primary_never_lands_on_the_new_one() {
        let fake = fake_view(&list_of(&[("1", true), ("2", false)]));
        // The helper's primary is 2 already; Link's next look hasn't come yet.
        std::fs::write(
            fake.dir.path().join("displays.json"),
            list_of(&[("1", false), ("2", true)]).to_string(),
        )
        .unwrap();
        let click = json!({ "id": 3, "type": "actions", "display": "1",
            "actions": [{ "type": "click", "x": 5, "y": 6 }] });
        let answer = act(&fake.shared, &fake.peer, &controller(), &click, false).await.unwrap();
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(
            ops_of(fake.dir.path()),
            ["move \"1\"", "button left true", "button left false"]
        );
        // The look: handed over to 2. The same batch now is refused.
        check_views(&fake.shared, None).await;
        let before = ops_of(fake.dir.path()).len();
        let answer = act(&fake.shared, &fake.peer, &controller(), &click, false).await.unwrap();
        assert_eq!(answer["status"], 409);
        assert_eq!(answer["code"], "display-changed");
        assert_eq!(answer["display"], "2");
        let after = ops_of(fake.dir.path());
        assert_eq!(after.len(), before, "nothing ran: {after:?}");
    }

    /// Review of !15, finding 3: an injected capture failure (rtx-control's peer_display_lost
    /// with display_unavailable, its list already without that display). The view falls back
    /// to the primary, its held input let go first, with that exact reason and a list that
    /// leaves the display out.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_display_that_cannot_be_captured_now_falls_back_with_its_reason() {
        let fake = fake_view(&list_of(&[("1", true), ("2", false)]));
        let chosen = select_display(&fake.shared, 7, &fake.peer, Some("2".into())).await;
        assert_eq!(chosen["ok"], true);
        lock(&fake.peer.held_keys).push("ControlLeft".into());
        std::fs::write(fake.dir.path().join("displays.json"), list_of(&[("1", true)]).to_string())
            .unwrap();
        check_views(&fake.shared, Some(lost_event("2", displays::DISPLAY_UNAVAILABLE))).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = ops_of(fake.dir.path());
        assert_eq!(&ops[ops.len() - 2..], ["key ControlLeft false", "peer_display \"1\""]);
        assert_eq!(lock(&fake.peer.display).as_deref(), Some("1"));
        let push = pushed(fake.dir.path()).pop().expect("a displays push");
        assert_eq!(push["fallback"], json!({ "from": "2", "reason": "display_unavailable" }));
        assert_eq!(push["display"], "1");
        assert_eq!(push["displays"], list_of(&[("1", true)]));
        // A lost display still listed (the list not yet filtered) is left out all the same.
        let fake = fake_view(&list_of(&[("1", true), ("2", false)]));
        *lock(&fake.peer.display) = Some("2".into());
        check_views(&fake.shared, Some(lost_event("2", displays::DISPLAY_UNAVAILABLE))).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let push = pushed(fake.dir.path()).pop().unwrap();
        assert_eq!(push["displays"], list_of(&[("1", true)]));
        assert_eq!(push["fallback"]["reason"], "display_unavailable");
    }

    #[cfg(unix)]
    fn lost_event(display: &str, reason: &str) -> Lost {
        Lost {
            session: 7,
            display: display.into(),
            reason: reason.into(),
        }
    }

    /// Fallbacks the viewer was told of, and the switches the helper got, so far.
    #[cfg(unix)]
    fn fallbacks_and_switches(dir: &std::path::Path) -> (Vec<Value>, usize) {
        let fallbacks = pushed(dir)
            .into_iter()
            .filter_map(|push| push.get("fallback").cloned())
            .collect();
        let switches = ops_of(dir).iter().filter(|op| op.starts_with("peer_display")).count();
        (fallbacks, switches)
    }

    /// Review of !15 (second), regression a: display 1's loss event is held up while the view
    /// moves to 2 (by a poll's fallback, or its viewer's switch; done before the event is
    /// handled, or while it waits for the view's actions). With 2 and 3 attached, the old event
    /// changes nothing: 2 stays, no input is let go again, no switch, and no push says 2 was
    /// lost or leaves it out.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_late_loss_event_leaves_a_view_that_moved_on_alone() {
        for how in ["poll", "switch", "while-waiting"] {
            let fake = fake_view(&list_of(&[("1", false), ("2", true), ("3", false)]));
            match how {
                // 1 unplugged; the poll hands the view to the primary, 2.
                "poll" => {
                    std::fs::write(
                        fake.dir.path().join("displays.json"),
                        list_of(&[("2", true), ("3", false)]).to_string(),
                    )
                    .unwrap();
                    check_views(&fake.shared, None).await;
                }
                "switch" => {
                    let answer = select_display(&fake.shared, 7, &fake.peer, Some("2".into())).await;
                    assert_eq!(answer["ok"], true);
                }
                // The event is handled while a switch holds the view: it waits, then finds 2.
                _ => {
                    let held = fake.peer.running.write().await;
                    let shared = fake.shared.clone();
                    let late = tokio::spawn(async move {
                        check_views(&shared, Some(lost_event("1", displays::DISPLAY_REMOVED))).await
                    });
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    assert!(!late.is_finished(), "it waits for the switch");
                    *lock(&fake.peer.display) = Some("2".into());
                    drop(held);
                    late.await.unwrap();
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(lock(&fake.peer.display).as_deref(), Some("2"), "{how}");
            let before_ops = ops_of(fake.dir.path());
            let before_pushes = pushed(fake.dir.path()).len();
            lock(&fake.peer.held_keys).push("ShiftLeft".into());
            check_views(&fake.shared, Some(lost_event("1", displays::DISPLAY_REMOVED))).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(lock(&fake.peer.display).as_deref(), Some("2"), "{how}");
            assert_eq!(ops_of(fake.dir.path()), before_ops, "{how}: nothing let go or switched");
            assert_eq!(lock(&fake.peer.held_keys).len(), 1, "{how}");
            assert_eq!(pushed(fake.dir.path()).len(), before_pushes, "{how}: nothing pushed");
            for push in pushed(fake.dir.path()) {
                assert_ne!(push["fallback"]["from"], "2", "{how}: {push}");
                assert!(displays::has(&displays::list_of(&push), "2"), "{how}: {push}");
            }
        }
    }

    /// Review of !15 (second), regression b: one loss, seen by the poll and by the event in
    /// either order, is one fallback and one switch.
    #[cfg(unix)]
    #[tokio::test]
    async fn one_loss_is_one_fallback_whoever_sees_it_first() {
        for event_first in [true, false] {
            let fake = fake_view(&list_of(&[("1", true), ("2", false)]));
            std::fs::write(fake.dir.path().join("displays.json"), list_of(&[("2", true)]).to_string())
                .unwrap();
            let event = || Some(lost_event("1", displays::DISPLAY_REMOVED));
            if event_first {
                check_views(&fake.shared, event()).await;
                check_views(&fake.shared, None).await;
            } else {
                check_views(&fake.shared, None).await;
                check_views(&fake.shared, event()).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (fallbacks, switches) = fallbacks_and_switches(fake.dir.path());
            assert_eq!(switches, 1, "event first: {event_first}");
            assert_eq!(
                fallbacks,
                [json!({ "from": "1", "reason": "display_removed" })],
                "event first: {event_first}"
            );
            assert_eq!(lock(&fake.peer.display).as_deref(), Some("2"));
        }
    }

    #[test]
    fn javascript_numbers_with_a_fallback() {
        assert_eq!(number_or(Some(&json!(0)), 75.0), 75.0);
        assert_eq!(number_or(Some(&json!("40")), 75.0), 40.0);
        assert_eq!(number_or(None, 75.0), 75.0);
    }
}
