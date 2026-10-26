//! The daemon's local API, for RealTimeX and the lite app's window: JSON over HTTP on 127.0.0.1
//! only, as server/utils/remoteHost/client.js speaks it. Every request needs the bearer
//! token from the data folder (same-user trust, like a Docker socket); a request addressed to any
//! other host name is refused, so a web page can't reach it through DNS rebinding.
//!
//! The one request without the token is POST /v1/hello: the daemon proves it holds the token
//! before a client sends it. Otherwise a process of another user that took over the port of a
//! daemon that died (linkd.json still names it) would be handed the token.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::host::{Host, Settings};
use crate::store::SCHEMA_VERSION;
use crate::util::same_bytes;

/// The local API's protocol: a caller that speaks another one says so instead of misreading it.
pub const PROTOCOL: u32 = 1;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// RealTimeX's full-list pushes (PUT /v1/shared, later /v1/artifacts and /v1/webhooks).
const MAX_LIST_BYTES: usize = 1024 * 1024;
pub const FEATURES: &[&str] = &[
    "host",
    "autostart",
    "window",
    "relay",
    "hello",
    "appearance",
    "language",
    // realtimex-ai-app#2836 / realtimex-link#6 M1 (README): GET/POST /v1/running; GET
    // /v1/exposure and POST /v1/exposure/adopt; PUT/DELETE/GET /v1/origin; GET/PUT /v1/shared.
    "running",
    "exposure",
    "origin",
    "shared",
    // M2: PUT /v1/artifacts (Link serves published artifacts and the widget's assets). M3: PUT
    // /v1/webhooks, GET /v1/inbox, POST /v1/inbox/ack (the webhook inbox).
    "artifacts",
    "webhooks",
];
/// POST /v1/hello's proof: HMAC-SHA256 keyed with the token over this, the daemon's port, and the
/// caller's nonce (see `hello_proof`).
pub const HELLO_CONTEXT: &str = "linkd-hello";
const HELLO_PATH: &str = "/v1/hello";
const MIN_NONCE_LENGTH: usize = 16;
const MAX_NONCE_LENGTH: usize = 256;

pub struct LocalState {
    pub token: String,
    /// The port this API listens on, which /v1/hello's proof names.
    pub port: u16,
    pub version: String,
    pub started_at: String,
    pub host: Arc<Host>,
    /// Asked to stop (POST /v1/shutdown): where no signal reaches the daemon, as on Windows.
    pub shutdown: Arc<tokio::sync::Notify>,
    /// The running rtx-link pids by role (POST /v1/window/open): one window and one menu bar /
    /// tray icon at a time. On macOS and Windows one process is both ("window"); on Linux the icon
    /// runs on its own ("tray") and each opened window is a process ("window").
    pub window: Mutex<std::collections::HashMap<String, u32>>,
    /// How often another rtx-link asked the running one to show its window (GET /v1/status).
    pub show_window: AtomicU64,
}

type Shared = Arc<LocalState>;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route(HELLO_PATH, post(hello))
        .route("/v1/version", get(version))
        .route("/v1/status", get(status))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/autostart", get(autostart).post(set_autostart))
        .route("/v1/appearance", get(appearance).post(set_appearance))
        .route("/v1/window/open", post(open_window))
        .route("/v1/running", get(running).post(set_running))
        .route("/v1/exposure", get(exposure))
        .route("/v1/exposure/adopt", post(adopt_exposure))
        .route("/v1/exposure/notice-seen", post(exposure_notice_seen))
        .route(
            "/v1/origin",
            get(origin).put(set_origin).delete(clear_origin),
        )
        .route(
            "/v1/shared",
            get(shared)
                .put(set_shared)
                .layer(DefaultBodyLimit::max(MAX_LIST_BYTES)),
        )
        .route(
            "/v1/artifacts",
            axum::routing::put(set_artifacts).layer(DefaultBodyLimit::max(MAX_LIST_BYTES)),
        )
        .route(
            "/v1/webhooks",
            axum::routing::put(set_webhooks).layer(DefaultBodyLimit::max(MAX_LIST_BYTES)),
        )
        .route("/v1/inbox", get(inbox))
        .route("/v1/inbox/ack", post(inbox_ack))
        .route("/v1/account", get(account))
        .route("/v1/account/token", post(account_token))
        .route("/v1/account/sign-in", post(sign_in))
        .route("/v1/account/sign-out", post(sign_out))
        .route("/v1/account/release", post(account_release))
        .route("/v1/account/use-realtimex", post(use_realtimex))
        .route("/v1/update", get(update_status))
        .route("/v1/update/check", post(update_check))
        .route("/v1/update/install", post(update_install))
        .route("/v1/host/status", get(host_status))
        .route("/v1/host/overview", get(host_overview))
        .route("/v1/host/settings", post(host_settings))
        .route("/v1/host/connection-requests", get(connection_requests))
        .route("/v1/host/connection-requests/decide", post(decide_request))
        .route("/v1/host/password", get(password))
        .route("/v1/host/password/regenerate", post(regenerate_password))
        .route("/v1/host/password/set", post(set_password))
        .route("/v1/host/clients", get(clients))
        .route("/v1/host/clients/permissions", post(client_permissions))
        .route("/v1/host/clients/revoke", post(revoke_client))
        .route("/v1/host/clients/allow", post(allow_client))
        .route("/v1/host/clients/disconnect", post(disconnect_client))
        .route("/v1/host/clients/remove", post(remove_client))
        .route("/v1/host/permissions/request", post(request_permission))
        .route("/v1/host/stop-session", post(stop_session))
        .fallback(|| async { ApiError::new(404, "Not found") })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

async fn guard(State(state): State<Shared>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            name
        } else {
            host
        }
    });
    if name != "127.0.0.1" && name != "localhost" {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            Json(json!({ "error": "Not a local request" })),
        )
            .into_response();
    }
    // Asked before the client trusts this port with its token: it carries none.
    if request.uri().path() == HELLO_PATH {
        return next.run(request).await;
    }
    let given = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| strip_bearer(value).to_string())
        .unwrap_or_default();
    if !same_bytes(given.as_bytes(), state.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        )
            .into_response();
    }
    next.run(request).await
}

fn strip_bearer(value: &str) -> &str {
    let trimmed = value.trim_start();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim_start()
    } else {
        trimmed
    }
}

/// A JSON body, `{}` when there is none; one that is not JSON is a 400.
pub struct Body64(pub Value);

impl<S: Send + Sync> axum::extract::FromRequest<S> for Body64 {
    type Rejection = ApiError;

    async fn from_request(request: Request<Body>, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(request, state)
            .await
            .map_err(|_| ApiError::new(413, "Request body too large"))?;
        if bytes.is_empty() {
            return Ok(Self(json!({})));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::bad_request("Request body is not JSON"))?;
        Ok(Self(if value.is_object() { value } else { json!({}) }))
    }
}

/// The proof POST /v1/hello answers with: lowercase hex of HMAC-SHA256, keyed with the token's
/// text, over "linkd-hello:<port>:<nonce>". The port is part of it so that a process on another
/// port can't pass on a live daemon's answer as its own.
pub fn hello_proof(token: &str, port: u16, nonce: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
    let message = format!("{HELLO_CONTEXT}:{port}:{nonce}");
    ring::hmac::sign(&key, message.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// { nonce } (16 to 256 characters, fresh for each try): { hmac, protocol }. The caller checks
/// the hmac against the api-token file, in constant time, before it sends that token.
async fn hello(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let nonce = body.get("nonce").and_then(Value::as_str).unwrap_or("");
    if !(MIN_NONCE_LENGTH..=MAX_NONCE_LENGTH).contains(&nonce.len()) {
        return Err(ApiError::bad_request(format!(
            "nonce must be {MIN_NONCE_LENGTH} to {MAX_NONCE_LENGTH} characters"
        )));
    }
    Ok(Json(json!({
        "hmac": hello_proof(&state.token, state.port, nonce),
        "protocol": PROTOCOL,
    })))
}

async fn version(State(state): State<Shared>) -> Json<Value> {
    Json(json!({ "protocol": PROTOCOL, "version": state.version, "features": FEATURES }))
}

async fn status(State(state): State<Shared>) -> Json<Value> {
    Json(json!({
        "pid": std::process::id(),
        "version": state.version,
        "protocol": PROTOCOL,
        "schema": SCHEMA_VERSION,
        "startedAt": state.started_at,
        "showWindow": state.show_window.load(Ordering::Relaxed),
        // The window follows it (light, dark, system) as it polls.
        "appearance": state.host.appearance(),
        "language": state.host.language(),
        "running": state.host.settings().keep_running,
        // GET /v1/exposure's answer.
        "exposure": state.host.relay.exposure(),
        // Which Link this is (instance.rs): { default, label }.
        "instance": crate::instance::status(),
        // The tray's Quit wording: RealTimeX owns this Link (it starts it again while Keep
        // running is on), or a login item starts it.
        "realtimexPresent": crate::owner::present(&state.host.store),
        "autostartEnabled": crate::paths::is_standard() && crate::autostart::enabled(),
    }))
}

type Answer = ApiResult<Json<Value>>;

/// Stop the daemon: the answer goes out first, then it stops as on SIGTERM. { app: true, pid }:
/// Quit in RealTimeX Link, which ends its other processes too (an open window would start the
/// daemon again).
async fn shutdown(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    if body.get("app") == Some(&Value::Bool(true)) {
        let caller = body.get("pid").and_then(Value::as_u64);
        let pids: Vec<u32> = state
            .window
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .copied()
            .filter(|pid| Some(u64::from(*pid)) != caller)
            .collect();
        for pid in pids {
            end_process(pid);
        }
    }
    state.shutdown.notify_one();
    Ok(Json(json!({ "stopping": true })))
}

#[cfg(unix)]
fn end_process(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        if pid > 0 {
            // SAFETY: SIGTERM to a process this daemon registered; nothing else is touched.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

/// Windows: the window and the icon are one process, the caller.
#[cfg(not(unix))]
fn end_process(_pid: u32) {}

/// RealTimeX Link's window and icon, one of each at a time: { pid, role } (role "window", the
/// default, or "tray") makes this rtx-link the running one for its role, unless another is alive;
/// then a window is asked to show itself (showWindow in /v1/status goes up) and this one is told
/// to exit. { owner: true }: keep going. `tray`: whether an icon runs.
async fn open_window(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let pid = body
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| ApiError::bad_request("pid is required"))?;
    let role = match body.get("role").and_then(Value::as_str) {
        None | Some("window") => "window",
        Some("tray") => "tray",
        Some(_) => return Err(ApiError::bad_request("role is window or tray")),
    };
    let mut roles = state.window.lock().unwrap_or_else(|p| p.into_inner());
    let running = |roles: &std::collections::HashMap<String, u32>, role: &str| {
        roles
            .get(role)
            .copied()
            .filter(|pid| crate::lock::alive(*pid))
    };
    let owner = match running(&roles, role) {
        Some(other) if other != pid => {
            if role == "window" {
                state.show_window.fetch_add(1, Ordering::Relaxed);
            }
            false
        }
        _ => {
            roles.insert(role.to_string(), pid);
            true
        }
    };
    let tray = running(&roles, "tray").is_some();
    Ok(Json(json!({ "owner": owner, "tray": tray })))
}

/// "Keep RealTimeX Link running": { on }.
async fn running(State(state): State<Shared>) -> Answer {
    Ok(Json(json!({ "on": state.host.settings().keep_running })))
}

/// { on: bool }: Link's own switch, which RealTimeX's Sharing page shows too. RealTimeX turns
/// it on only when the person asks there (Turn on), never by itself.
async fn set_running(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let on = body
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("on must be true or false"))?;
    state.host.set_keep_running(on).await?;
    running(State(state)).await
}

/// This computer's Public Exposure, for RealTimeX's Sharing page (relay.rs `exposure`).
async fn exposure(State(state): State<Shared>) -> Answer {
    Ok(Json(state.host.relay.exposure()))
}

/// One RealTimeX per Link (realtimex-ai-app#2836 C3): its pushes name the exposure Link adopted
/// from it, and anything else is refused.
fn same_exposure(state: &LocalState, body: &Value) -> Result<(), ApiError> {
    let named = body.get("exposureId").and_then(Value::as_str).unwrap_or("");
    match state.host.relay.adopted_id() {
        Some(adopted) if adopted == named => Ok(()),
        adopted => Err(ApiError::coded(
            409,
            "exposure-mismatch",
            "This isn't the exposure RealTimeX Link carries for RealTimeX",
        )
        .with_details(json!({ "appId": adopted }))),
    }
}

/// RealTimeX's servers: { registered, targets }.
async fn origin(State(state): State<Shared>) -> Answer {
    Ok(Json(crate::relay::Origin::view(
        state.host.relay.origin.get().as_ref(),
    )))
}

/// { exposureId, targets: { a, e, h, m, p: "http://127.0.0.1:<port>" }, token? }: RealTimeX
/// runs, and the relay's requests for its surfaces go there (relay/origin.rs).
async fn set_origin(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    same_exposure(&state, &body)?;
    let origin = crate::relay::Origin::from_body(&body).map_err(ApiError::bad_request)?;
    let answer = crate::relay::Origin::view(Some(&origin));
    state.host.relay.origin.set(origin);
    (state.host.log)("[relay] RealTimeX registered as the origin");
    Ok(Json(answer))
}

/// RealTimeX is quitting: its surfaces answer "desktop offline" until it registers again.
async fn clear_origin(State(state): State<Shared>) -> Answer {
    state.host.relay.origin.clear();
    (state.host.log)("[relay] RealTimeX's origin is gone");
    Ok(Json(json!({ "ok": true })))
}

/// { exposureId, installId, account: <Keycloak subject> }: take over RealTimeX's exposure, so
/// this computer has one App ID (relay.rs `adopt`): { appId, formerAppIds }. 409 other-account,
/// signed-out or adopted-other.
async fn adopt_exposure(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let text = |name: &str| body.get(name).and_then(Value::as_str).unwrap_or("").trim();
    let exposure_id = Some(text("exposureId")).filter(|id| !id.is_empty());
    let answer = state
        .host
        .relay
        .adopt(exposure_id, text("installId"), text("account"))
        .await?;
    Ok(Json(answer))
}

/// The window showed "This computer now has one App ID".
async fn exposure_notice_seen(State(state): State<Shared>) -> Answer {
    state.host.relay.notice_seen()?;
    Ok(Json(json!({})))
}

/// "Shared from this computer", as RealTimeX last pushed it, and whether RealTimeX runs:
/// { items, origin }.
async fn shared(State(state): State<Shared>) -> Answer {
    let store = &state.host.store;
    Ok(Json(json!({
        "items": crate::shared::get(store),
        "origin": state.host.relay.origin.registered(),
        // What Link itself does while RealTimeX is closed (M2, M3).
        "servedArtifacts": crate::artifacts::published_count(store),
        "webhookEndpoints": crate::webhooks::endpoint_count(store),
        "waiting": crate::webhooks::waiting(store),
    })))
}

/// { exposureId, items: [{ kind, count, names? }] }: the whole list, replacing the last one
/// (shared.rs).
async fn set_shared(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    same_exposure(&state, &body)?;
    let items = crate::shared::parse(&body)?;
    crate::shared::set(&state.host.store, &items)?;
    Ok(Json(json!({ "ok": true })))
}

/// { exposureId, items: [{ id, dir, entry, status, expiresAt? }], embedAssetsDir? }: the
/// published artifacts Link serves on surface a, and the widget's assets on surface e
/// (artifacts.rs). The whole list, replacing the last one.
async fn set_artifacts(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    same_exposure(&state, &body)?;
    let kept = crate::artifacts::parse(&body)?;
    crate::artifacts::set(&state.host.store, &kept)?;
    Ok(Json(json!({ "ok": true })))
}

/// { exposureId, items: [{ tokens, enabled, maxBodyBytes, perMinute, ackStatus, handshake,
/// verifyTokenSha256? }] }: the webhook endpoints the inbox takes deliveries for while
/// RealTimeX isn't running (webhooks.rs). The whole list, replacing the last one.
async fn set_webhooks(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    same_exposure(&state, &body)?;
    let kept = crate::webhooks::parse(&body)?;
    crate::webhooks::set(&state.host.store, &kept)?;
    Ok(Json(json!({ "ok": true })))
}

/// ?after=<id>&limit=<1..100>: kept deliveries, oldest first: { items, total }.
async fn inbox(
    State(state): State<Shared>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Answer {
    let limit = query
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(50);
    let page = crate::webhooks::inbox_page(
        &state.host.store,
        query.get("after").map(String::as_str),
        limit,
    )?;
    Ok(Json(page))
}

/// { exposureId, ids }: RealTimeX has these deliveries; they go. { acked }.
async fn inbox_ack(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    same_exposure(&state, &body)?;
    let ids: Vec<String> = body
        .get("ids")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("ids must be a list"))?
        .iter()
        .filter_map(|id| id.as_str().map(str::to_string))
        .take(1000)
        .collect();
    let acked = crate::webhooks::ack(&state.host.store, &ids)?;
    Ok(Json(json!({ "acked": acked })))
}

/// The account Link is signed in to (signin.rs): { signedIn, via, email, name, signingIn, error,
/// offer }.
/// ?check=1 (the window opening): whether RealTimeX is still present is checked first, which
/// decides whether Sign out shows (owner.rs).
async fn account(
    State(state): State<Shared>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Answer {
    if query.contains_key("check") && crate::owner::check_presence(&state.host.store) {
        state.host.relay.wake();
    }
    Ok(Json(state.host.signin.status()))
}

/// The signed-in account's access token, from RealTimeX: { token, apiBase?, installId? }.
/// With its installId (§6.2, §6.3 A1) the first RealTimeX install to hand over owns Link, and
/// its handover always replaces Link's own sign-in; another install gets 409 adopted-other.
/// Without one (an older RealTimeX) it is ignored once there is an owner, and otherwise signs
/// Link in only while Link has no sign-in of its own ({ accepted: false }: offered).
async fn account_token(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("token is required"))?;
    let api_base = body.get("apiBase").and_then(Value::as_str);
    let store = &state.host.store;
    match body.get("installId").and_then(Value::as_str) {
        Some(install_id) => {
            crate::owner::claim(store, install_id, crate::util::now_ms())?;
            state
                .host
                .relay
                .take_handover(token, api_base)
                .await
                .map_err(|error| ApiError::bad_request(error.to_string()))?;
            Ok(Json(json!({ "accepted": true })))
        }
        None if crate::owner::present(store) => {
            Ok(Json(json!({ "accepted": false, "ignored": true })))
        }
        None => {
            let accepted = state
                .host
                .relay
                .handover(token, api_base)
                .map_err(|error| ApiError::bad_request(error.to_string()))?;
            Ok(Json(json!({ "accepted": accepted })))
        }
    }
}

/// { installId, movesTo? }: the owning RealTimeX lets this Link go (Host::release_by_realtimex):
/// it is uninstalled, or with movesTo "instance" a dev RealTimeX moves its exposure to a Link
/// instance of its own. { released, droppedExposure }; 409 adopted-other from anyone but the
/// owner, and when no RealTimeX owns Link ("not yours, nothing to release").
async fn account_release(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let install_id = body.get("installId").and_then(Value::as_str).unwrap_or("");
    let moves = moves_to_instance(&body)?;
    let dropped = state.host.release_by_realtimex(install_id, moves).await?;
    Ok(Json(
        json!({ "released": true, "droppedExposure": dropped }),
    ))
}

/// Start Link's own sign-in: { url } for the browser, which comes back to a one-time local port.
async fn sign_in(State(state): State<Shared>) -> Answer {
    let url = state
        .host
        .signin
        .begin()
        .await
        .map_err(|error| ApiError::new(500, error.to_string()))?;
    Ok(Json(json!({ "url": url })))
}

async fn sign_out(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let host = &state.host;
    crate::owner::check_presence(&host.store);
    if crate::owner::present(&host.store) {
        // RealTimeX owns Link (§6.2): only it signs Link out, and its next handover signs Link in.
        let install_id = body
            .get("installId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ApiError::coded(
                    409,
                    "managed-by-realtimex",
                    "RealTimeX manages this account; sign out in RealTimeX",
                )
            })?;
        crate::owner::check(&host.store, install_id)?;
        host.signin
            .sign_out_for_realtimex()
            .await
            .map_err(|error| ApiError::new(500, error.to_string()))?;
        return Ok(Json(json!({})));
    }
    host.signin
        .sign_out()
        .await
        .map_err(|error| ApiError::new(500, error.to_string()))?;
    Ok(Json(json!({})))
}

/// Link's own updates (update.rs): { current, latest, available, page, busy, error, checkedAt,
/// selfUpdate }. Only the default instance updates itself (instance.rs); another answers
/// { current, available: false, selfUpdate: false } and refuses check and install.
async fn update_status(State(state): State<Shared>) -> Answer {
    if !crate::paths::is_standard() {
        return Ok(Json(json!({
            "current": state.version,
            "available": false,
            "selfUpdate": false,
        })));
    }
    let mut status = state.host.updater.status();
    if let Some(fields) = status.as_object_mut() {
        fields.insert("selfUpdate".into(), json!(true));
    }
    Ok(Json(status))
}

fn only_the_default_instance_updates() -> ApiResult<()> {
    if crate::paths::is_standard() {
        return Ok(());
    }
    Err(ApiError::coded(
        409,
        "not-default-instance",
        "Only the default RealTimeX Link updates itself",
    ))
}

async fn update_check(State(state): State<Shared>) -> Answer {
    only_the_default_instance_updates()?;
    let status = state
        .host
        .updater
        .check()
        .await
        .map_err(|error| ApiError::new(500, error.to_string()))?;
    Ok(Json(status))
}

/// Update now: { started } (the new Link starts itself), or { open } with the release page for
/// the window to open (Linux).
async fn update_install(State(state): State<Shared>) -> Answer {
    only_the_default_instance_updates()?;
    let answer = state
        .host
        .updater
        .install()
        .await
        .map_err(|error| ApiError::new(500, error.to_string()))?;
    Ok(Json(answer))
}

/// Release's movesTo: absent (uninstalled) or "instance" (a dev RealTimeX's own Link instance).
fn moves_to_instance(body: &Value) -> ApiResult<bool> {
    match body.get("movesTo") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(to)) if to == "instance" => Ok(true),
        Some(_) => Err(ApiError::bad_request("movesTo must be \"instance\"")),
    }
}

/// "Continue as …": sign in with the account RealTimeX offered.
async fn use_realtimex(State(state): State<Shared>) -> Answer {
    state
        .host
        .signin
        .use_handover()
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(json!({})))
}

/// Start at login (autostart.rs): { enabled, available, optedOut }. Available: rtx-link is
/// installed next to this daemon, in the installed Link, so there is something to start. Opted
/// out: turned off here, so hosting doesn't add it back. Only the default instance has a login
/// item (instance.rs): another answers enabled and available false, defaultInstance false.
async fn autostart(State(state): State<Shared>) -> Answer {
    let standard = crate::paths::is_standard();
    Ok(Json(json!({
        "enabled": standard && crate::autostart::enabled(),
        "available": standard && crate::autostart::available(),
        "optedOut": state.host.start_at_login_opted_out(),
        "defaultInstance": standard,
    })))
}

/// { enabled: bool }: false turns starting at login off and keeps it off; true lets the login
/// item follow hosting again (there while this computer allows remote control).
async fn set_autostart(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let on = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("enabled must be true or false"))?;
    state.host.set_start_at_login(on)?;
    autostart(State(state)).await
}

async fn appearance(State(state): State<Shared>) -> Answer {
    Ok(Json(json!({
        "theme": state.host.appearance(),
        "language": state.host.language(),
    })))
}

/// { theme?: "light" | "dark" | "system", language?: BCP-47 tag | "system" }, at least one:
/// RealTimeX's, posted when it starts and when it changes, or the choice in Link's Settings.
/// "system" follows the OS. Both are checked before either is kept.
async fn set_appearance(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let field = |name: &str| -> Result<Option<String>, ApiError> {
        match body.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(ApiError::bad_request(format!("{name} must be a string"))),
        }
    };
    let (theme, language) = (field("theme")?, field("language")?);
    if theme.is_none() && language.is_none() {
        return Err(ApiError::bad_request("theme or language is required"));
    }
    if let Some(theme) = &theme {
        if !matches!(theme.as_str(), "light" | "dark" | "system") {
            return Err(ApiError::bad_request("theme must be light, dark or system"));
        }
    }
    if let Some(language) = &language {
        if language != "system" && crate::host::language_tag(language).is_none() {
            return Err(ApiError::bad_request(
                "language must be a language tag or system",
            ));
        }
    }
    if let Some(theme) = &theme {
        state.host.set_appearance(theme)?;
    }
    if let Some(language) = &language {
        state.host.set_language(language)?;
    }
    appearance(State(state)).await
}

/// RealTimeX's hosting state, once: { state: { clients, settings… }, password: { password,
/// changedAt, generation } }.
async fn host_status(State(state): State<Shared>) -> Answer {
    Ok(Json(state.host.status().await))
}

async fn host_overview(State(state): State<Shared>) -> Answer {
    Ok(Json(state.host.overview().await))
}

async fn host_settings(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let host = &state.host;
    if let Some(policy) = body.get("newComputerPolicy").filter(|v| !v.is_null()) {
        host.set_new_computer_policy(policy.as_str().unwrap_or(""))
            .await?;
    }
    if let Some(permissions) = body.get("defaultPermissions").filter(|v| !v.is_null()) {
        host.set_default_permissions(permissions).await?;
    }
    if let Some(allow) = body.get("allowIncoming").filter(|v| !v.is_null()) {
        host.set_allow_incoming(allow.as_bool() == Some(true))
            .await?;
    }
    let Settings {
        default_permissions,
        allow_incoming,
        new_computer_policy,
        ..
    } = host.settings();
    Ok(Json(json!({
        "defaultPermissions": default_permissions,
        "allowIncoming": allow_incoming,
        "newComputerPolicy": new_computer_policy,
        "publicExposure": host.public_state(),
    })))
}

async fn connection_requests(State(state): State<Shared>) -> Answer {
    Ok(Json(state.host.connection_requests()))
}

async fn decide_request(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let reference = body.get("requestRef").and_then(Value::as_str).unwrap_or("");
    let decision = body.get("decision").and_then(Value::as_str).unwrap_or("");
    Ok(Json(
        state
            .host
            .decide_connection_request(reference, decision)
            .await?,
    ))
}

async fn password(State(state): State<Shared>) -> Answer {
    Ok(Json(json!({ "password": state.host.password() })))
}

async fn regenerate_password(State(state): State<Shared>) -> Answer {
    state.host.change_password(None).await?;
    Ok(Json(json!({})))
}

async fn set_password(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    if password.is_empty() {
        return Err(ApiError::bad_request("password is required"));
    }
    state.host.change_password(Some(password)).await?;
    Ok(Json(json!({})))
}

async fn clients(State(state): State<Shared>) -> Answer {
    Ok(Json(json!({ "clients": state.host.clients.list() })))
}

fn id_of(body: &Value) -> String {
    body.get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

async fn client_permissions(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let permissions = body.get("permissions").cloned().unwrap_or(json!({}));
    let client = state
        .host
        .set_client_permissions(&id_of(&body), &permissions)
        .await?;
    Ok(Json(json!({ "client": client })))
}

async fn revoke_client(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    Ok(Json(
        json!({ "client": state.host.set_client_revoked(&id_of(&body), true).await? }),
    ))
}

async fn allow_client(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    Ok(Json(
        json!({ "client": state.host.set_client_revoked(&id_of(&body), false).await? }),
    ))
}

async fn disconnect_client(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    Ok(Json(
        json!({ "client": state.host.disconnect_client(&id_of(&body)).await? }),
    ))
}

async fn remove_client(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    state.host.remove_client(&id_of(&body)).await?;
    Ok(Json(json!({})))
}

async fn request_permission(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    if name != "screenRecording" && name != "accessibility" {
        return Err(ApiError::bad_request("Unknown permission"));
    }
    state.host.request_permission(name).await?;
    Ok(Json(json!({})))
}

async fn stop_session(State(state): State<Shared>) -> Answer {
    state.host.stop_session().await;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_moves_only_to_an_instance() {
        assert!(!moves_to_instance(&json!({ "installId": "x" })).unwrap());
        assert!(moves_to_instance(&json!({ "movesTo": "instance" })).unwrap());
        let bogus = moves_to_instance(&json!({ "movesTo": "bogus" })).unwrap_err();
        assert_eq!(bogus.status, 400);
        assert!(moves_to_instance(&json!({ "movesTo": true })).is_err());
    }

    #[test]
    fn bearer_prefix_in_any_case() {
        assert_eq!(strip_bearer("Bearer abc"), "abc");
        assert_eq!(strip_bearer("bearer   abc"), "abc");
        assert_eq!(strip_bearer("abc"), "abc");
    }

    #[test]
    fn hello_proves_the_token_for_this_port_and_nonce() {
        // What a client computes: in Node, crypto.createHmac("sha256", token)
        // .update(`linkd-hello:${port}:${nonce}`).digest("hex").
        let proof = hello_proof("token", 7000, "nonce-0123456789");
        assert_eq!(
            proof,
            "0951928c6c00bc5c4f3e9628f20424305372b20f170d80b13ee212f402181af6"
        );
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"token");
        let tag: Vec<u8> = (0..proof.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&proof[i..i + 2], 16).unwrap())
            .collect();
        assert!(ring::hmac::verify(&key, b"linkd-hello:7000:nonce-0123456789", &tag).is_ok());
        assert_ne!(proof, hello_proof("other", 7000, "nonce-0123456789"));
        assert_ne!(proof, hello_proof("token", 7001, "nonce-0123456789"));
        assert_ne!(proof, hello_proof("token", 7000, "nonce-0123456780"));
    }
}
