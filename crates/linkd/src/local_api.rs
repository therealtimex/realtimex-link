//! The daemon's local API, for RealTimeX and the lite app's window: JSON over HTTP on 127.0.0.1
//! only, as server/utils/remoteHost/client.js speaks it. Every request needs the bearer
//! token from the data folder (same-user trust, like a Docker socket); a request addressed to any
//! other host name is refused, so a web page can't reach it through DNS rebinding.

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
pub const FEATURES: &[&str] = &["host", "autostart", "window", "relay"];

pub struct LocalState {
    pub token: String,
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
        .route("/v1/version", get(version))
        .route("/v1/status", get(status))
        .route("/v1/import", post(import))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/autostart", get(autostart).post(set_autostart))
        .route("/v1/window/open", post(open_window))
        .route("/v1/account", get(account))
        .route("/v1/account/token", post(account_token))
        .route("/v1/account/sign-in", post(sign_in))
        .route("/v1/account/sign-out", post(sign_out))
        .route("/v1/account/use-realtimex", post(use_realtimex))
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

async fn version(State(state): State<Shared>) -> Json<Value> {
    Json(json!({ "protocol": PROTOCOL, "version": state.version, "features": FEATURES }))
}

async fn status(State(state): State<Shared>) -> Json<Value> {
    let imported = state
        .host
        .store
        .meta_get("legacy_import")
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    Json(json!({
        "pid": std::process::id(),
        "version": state.version,
        "protocol": PROTOCOL,
        "schema": SCHEMA_VERSION,
        "startedAt": state.started_at,
        "imported": imported,
        "showWindow": state.show_window.load(Ordering::Relaxed),
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

/// The account Link is signed in to (signin.rs): { signedIn, via, email, name, signingIn, error,
/// offer }.
async fn account(State(state): State<Shared>) -> Answer {
    Ok(Json(state.host.signin.status()))
}

/// The signed-in account's access token, from RealTimeX: { token, apiBase }. It signs Link in
/// unless Link has its own sign-in or was signed out here (then { accepted: false }: offered).
async fn account_token(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("token is required"))?;
    let accepted = state
        .host
        .relay
        .handover(token, body.get("apiBase").and_then(Value::as_str))
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(json!({ "accepted": accepted })))
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

async fn sign_out(State(state): State<Shared>) -> Answer {
    state
        .host
        .signin
        .sign_out()
        .await
        .map_err(|error| ApiError::new(500, error.to_string()))?;
    Ok(Json(json!({})))
}

/// "Continue as …": sign in with the account RealTimeX offered.
async fn use_realtimex(State(state): State<Shared>) -> Answer {
    state
        .host
        .signin
        .use_handover()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(json!({})))
}

/// Start at login (autostart.rs): { enabled, available }. Available: rtx-link is installed next to
/// this daemon, so there is something to start.
async fn autostart() -> Answer {
    Ok(Json(json!({
        "enabled": crate::autostart::enabled(),
        "available": crate::autostart::app_binary().is_some(),
    })))
}

/// { enabled: bool }: add or remove the login item.
async fn set_autostart(Body64(body): Body64) -> Answer {
    let on = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("enabled must be true or false"))?;
    crate::autostart::set(on).map_err(ApiError::internal)?;
    autostart().await
}

/// RealTimeX's hosting state, once: { state: { clients, settings… }, password: { password,
/// changedAt, generation } }.
async fn import(State(state): State<Shared>, Body64(body): Body64) -> Answer {
    Ok(Json(state.host.import_legacy(&body).await?))
}

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
    fn bearer_prefix_in_any_case() {
        assert_eq!(strip_bearer("Bearer abc"), "abc");
        assert_eq!(strip_bearer("bearer   abc"), "abc");
        assert_eq!(strip_bearer("abc"), "abc");
    }
}
