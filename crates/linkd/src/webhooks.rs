//! The webhook inbox (realtimex-ai-app#2836 M3, contract §6.1): while RealTimeX isn't running,
//! Link takes webhook deliveries for it. RealTimeX pushes its endpoints (PUT /v1/webhooks); Link
//! checks only what an edge can (the URL token, enabled, size, rate), keeps each delivery
//! byte-exact in the store (store.rs `inbox`), answers as RealTimeX's ingress would, and
//! RealTimeX pulls and acknowledges them when it starts (GET /v1/inbox, POST /v1/inbox/ack),
//! checking signatures and replay windows itself. Meta's and Pancake's GET handshakes are
//! answered here too, and kept in the inbox so RealTimeX records them.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::artifacts::{Answer, Body};
use crate::error::ApiError;
use crate::store::{Delivery, InboxRefusal, Store};

pub const KEY: &str = "webhooks";
/// RealTimeX's MAX_WEBHOOK_PAYLOAD_SIZE_BYTES.
pub const MAX_BODY: u64 = 15 * 1024 * 1024;
/// Deliveries are kept this long (7 days).
pub const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Per endpoint, at most this many deliveries or bytes wait.
const QUOTA_DELIVERIES: u64 = 1000;
const QUOTA_BYTES: u64 = 100 * 1024 * 1024;
const FULL_RETRY_AFTER: u64 = 300;
const MAX_ENDPOINTS: usize = 5000;
/// The paths RealTimeX's ingress answers on, before the token.
const ROUTES: [&str; 3] = ["/wh/", "/wh/inbound/", "/api/v1/webhook-ingress/inbound/"];

/// One endpoint as RealTimeX pushed it.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub tokens: Vec<String>,
    pub enabled: bool,
    pub max_body: u64,
    pub per_minute: u32,
    pub ack_status: u16,
    pub handshake: String,
    pub verify_token_sha256: Option<String>,
}

/// PUT /v1/webhooks's body, checked: what the store keeps ({ items }).
pub fn parse(body: &Value) -> Result<Value, ApiError> {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("items must be a list"))?;
    if items.len() > MAX_ENDPOINTS {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_ENDPOINTS} endpoints"
        )));
    }
    let mut kept = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let bad = |what: &str| ApiError::bad_request(format!("items[{index}]: {what}"));
        let tokens: Vec<String> = item
            .get("tokens")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("tokens must be a list"))?
            .iter()
            .map(|t| {
                t.as_str()
                    .filter(|t| {
                        !t.is_empty()
                            && t.len() <= 200
                            && t.chars()
                                .all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c))
                    })
                    .map(str::to_string)
                    .ok_or_else(|| bad("tokens must be URL tokens"))
            })
            .collect::<Result<_, _>>()?;
        if tokens.is_empty() {
            return Err(bad("tokens must not be empty"));
        }
        let number = |name: &str, default: u64, max: u64| -> Result<u64, ApiError> {
            match item.get(name) {
                None | Some(Value::Null) => Ok(default),
                Some(value) => value
                    .as_u64()
                    .filter(|n| (1..=max).contains(n))
                    .ok_or_else(|| bad(&format!("{name} must be 1 to {max}"))),
            }
        };
        let ack_status = number("ackStatus", 202, 299)?;
        if !matches!(ack_status, 200 | 202 | 204) {
            return Err(bad("ackStatus must be 200, 202 or 204"));
        }
        let handshake = item
            .get("handshake")
            .and_then(Value::as_str)
            .unwrap_or("none");
        if !matches!(handshake, "none" | "hub_challenge" | "accept_any") {
            return Err(bad("handshake must be none, hub_challenge or accept_any"));
        }
        let verify = match item.get("verifyTokenSha256") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::String(hash))
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                json!(hash.to_ascii_lowercase())
            }
            Some(_) => return Err(bad("verifyTokenSha256 must be 64 hex digits")),
        };
        kept.push(json!({
            "tokens": tokens,
            "enabled": item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            "maxBodyBytes": number("maxBodyBytes", 5 * 1024 * 1024, MAX_BODY)?,
            "perMinute": number("perMinute", 60, 1000)?,
            "ackStatus": ack_status,
            "handshake": handshake,
            "verifyTokenSha256": verify,
        }));
    }
    Ok(json!({ "items": kept }))
}

pub fn set(store: &Store, value: &Value) -> Result<(), ApiError> {
    store.set(KEY, value)?;
    Ok(())
}

pub fn endpoint_count(store: &Store) -> u64 {
    store
        .get(KEY)
        .and_then(|kept| kept.get("items").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0) as u64
}

/// RealTimeX's normalizePublicSlug: lowercase, anything but [a-z0-9_-] a dash, no dashes at the
/// ends.
fn normalize(token: &str) -> String {
    let lowered: String = token
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    lowered.trim_matches('-').to_string()
}

fn endpoint(store: &Store, token: &str) -> Option<Endpoint> {
    let kept = store.get(KEY)?;
    let normalized = normalize(token);
    kept.get("items")?.as_array()?.iter().find_map(|item| {
        let tokens: Vec<String> = item
            .get("tokens")?
            .as_array()?
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();
        if !tokens.iter().any(|t| t == token || *t == normalized) {
            return None;
        }
        Some(Endpoint {
            tokens,
            enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            max_body: item
                .get("maxBodyBytes")
                .and_then(Value::as_u64)
                .unwrap_or(MAX_BODY),
            per_minute: item.get("perMinute").and_then(Value::as_u64).unwrap_or(60) as u32,
            ack_status: item.get("ackStatus").and_then(Value::as_u64).unwrap_or(202) as u16,
            handshake: item
                .get("handshake")
                .and_then(Value::as_str)
                .unwrap_or("none")
                .to_string(),
            verify_token_sha256: item
                .get("verifyTokenSha256")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    })
}

/// The token of an ingress path (/wh/<token>, and RealTimeX's two longer forms), or None.
pub fn token_of(pathname: &str) -> Option<&str> {
    // The longest form first: /wh/inbound/<token> must not read "inbound" as a token.
    let mut routes = ROUTES;
    routes.sort_by_key(|r| std::cmp::Reverse(r.len()));
    routes.iter().find_map(|route| {
        let token = pathname.strip_prefix(route)?;
        (!token.is_empty() && !token.contains('/')).then_some(token)
    })
}

/// Fixed one-minute windows per endpoint, as RealTimeX's admission counts them.
#[derive(Default)]
pub struct Rates(Mutex<HashMap<String, (i64, u32)>>);

impl Rates {
    /// Counts one more; Err(seconds to wait) when the endpoint is over its limit.
    fn admit(&self, key: &str, per_minute: u32, now: i64) -> Result<(), u64> {
        let mut windows = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let window = now - now.rem_euclid(60_000);
        let entry = windows.entry(key.to_string()).or_insert((window, 0));
        if entry.0 != window {
            *entry = (window, 0);
        }
        if entry.1 >= per_minute {
            return Err(((window + 60_000 - now) / 1000).max(1) as u64);
        }
        entry.1 += 1;
        if windows.len() > 10_000 {
            windows.retain(|_, (start, _)| *start == window);
        }
        Ok(())
    }
}

fn json_answer(status: u16, body: Value) -> Answer {
    Answer {
        status,
        headers: vec![
            (
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            ),
            ("cache-control".into(), "no-store".into()),
        ],
        body: Body::Bytes(body.to_string().into_bytes()),
    }
}

fn refused(status: u16, code: &str, error: &str) -> Answer {
    json_answer(
        status,
        json!({ "success": false, "error": error, "code": code }),
    )
}

fn not_found() -> Answer {
    refused(
        404,
        "WEBHOOK_ENDPOINT_NOT_FOUND",
        "Webhook endpoint not found.",
    )
}

/// One request to an ingress path while RealTimeX isn't there to take it: kept in the inbox,
/// or the answer RealTimeX's ingress would give. `path` keeps its query; `body` is whole.
pub fn receive(
    store: &Store,
    rates: &Rates,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Answer {
    let pathname = path.split(['?', '#']).next().unwrap_or("");
    let Some(token) = token_of(pathname) else {
        return refused(404, "WEBHOOK_PUBLIC_ROUTE_NOT_FOUND", "Not found.");
    };
    let Some(endpoint) = endpoint(store, token) else {
        return not_found();
    };
    if !endpoint.enabled {
        return refused(
            410,
            "WEBHOOK_ENDPOINT_DISABLED",
            "Webhook endpoint is disabled.",
        );
    }
    let now = crate::util::now_ms();
    match method {
        "GET" | "HEAD" => handshake(store, &endpoint, path, headers, now),
        "POST" => {
            if body.len() as u64 > endpoint.max_body {
                return refused(
                    413,
                    "WEBHOOK_PAYLOAD_TOO_LARGE",
                    "Webhook payload is too large.",
                );
            }
            if let Err(wait) = rates.admit(&endpoint.tokens[0], endpoint.per_minute, now) {
                let mut answer =
                    refused(429, "WEBHOOK_RATE_LIMITED", "Too many webhook deliveries.");
                answer
                    .headers
                    .push(("retry-after".into(), wait.to_string()));
                return answer;
            }
            let delivery = Delivery {
                id: crate::util::uuid(),
                account: account(store),
                endpoint: endpoint.tokens[0].clone(),
                received_at: now,
                method: "POST".into(),
                path: path.to_string(),
                headers: json!(headers),
                body: body.to_vec(),
            };
            match store.inbox_add(&delivery, QUOTA_DELIVERIES, QUOTA_BYTES) {
                Ok(Ok(())) => {
                    let body = json!({
                        "success": true,
                        "accepted": true,
                        "duplicate": false,
                        "queued": true,
                        "deliveryId": delivery.id,
                    });
                    let mut answer = json_answer(endpoint.ack_status, body);
                    if endpoint.ack_status == 204 {
                        answer.body = Body::Bytes(Vec::new());
                    }
                    answer
                }
                Ok(Err(InboxRefusal::Full { .. })) => {
                    let mut answer = refused(
                        429,
                        "WEBHOOK_INBOX_FULL",
                        "Too many deliveries are waiting for this endpoint.",
                    );
                    answer
                        .headers
                        .push(("retry-after".into(), FULL_RETRY_AFTER.to_string()));
                    answer
                }
                Err(error) => refused(500, "WEBHOOK_INBOX_FAILED", &error.to_string()),
            }
        }
        _ => {
            let mut answer = refused(405, "WEBHOOK_METHOD_NOT_ALLOWED", "Method not allowed.");
            answer.headers.push(("allow".into(), "GET, POST".into()));
            answer
        }
    }
}

/// The GET handshake, as channelProfile.js's handshake setting says. An answered one (accepted
/// or rejected) is kept as a GET entry, for RealTimeX to record when it pulls.
fn handshake(
    store: &Store,
    endpoint: &Endpoint,
    path: &str,
    headers: &[(String, String)],
    now: i64,
) -> Answer {
    let text = |status: u16, body: &str| Answer {
        status,
        headers: vec![
            ("content-type".into(), "text/plain; charset=utf-8".into()),
            ("cache-control".into(), "no-store".into()),
        ],
        body: Body::Bytes(body.as_bytes().to_vec()),
    };
    let answer = match endpoint.handshake.as_str() {
        "accept_any" => text(200, "ok"),
        "hub_challenge" => {
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let param = |name: &str| {
                query.split('&').find_map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (decode_query(key) == name).then(|| decode_query(value))
                })
            };
            let (mode, verify, challenge) = (
                param("hub.mode"),
                param("hub.verify_token"),
                param("hub.challenge"),
            );
            match (mode.as_deref(), verify, challenge) {
                (Some("subscribe"), Some(verify), Some(challenge))
                    if !verify.is_empty() && !challenge.is_empty() =>
                {
                    let hash = crate::util::sha256_hex(verify.as_bytes());
                    let matches = endpoint
                        .verify_token_sha256
                        .as_deref()
                        .is_some_and(|expected| {
                            crate::util::same_bytes(hash.as_bytes(), expected.as_bytes())
                        });
                    if matches {
                        text(200, &challenge)
                    } else {
                        refused(
                            403,
                            "WEBHOOK_HANDSHAKE_REJECTED",
                            "Webhook verification token is invalid.",
                        )
                    }
                }
                _ => {
                    return refused(
                        400,
                        "WEBHOOK_HANDSHAKE_INVALID",
                        "Webhook verification request is invalid.",
                    )
                }
            }
        }
        _ => {
            return refused(
                404,
                "WEBHOOK_PUBLIC_ROUTE_NOT_FOUND",
                "This webhook has no verification handshake.",
            )
        }
    };
    let delivery = Delivery {
        id: crate::util::uuid(),
        account: account(store),
        endpoint: endpoint.tokens[0].clone(),
        received_at: now,
        method: "GET".into(),
        path: path.to_string(),
        headers: json!(headers),
        body: Vec::new(),
    };
    // Recording it is best effort: the sender has its answer either way.
    let _ = store.inbox_add(&delivery, QUOTA_DELIVERIES, QUOTA_BYTES);
    answer
}

fn decode_query(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => match text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The account deliveries are kept for, and read by: the one that owns the current exposure.
/// After RealTimeX switched accounts the previous account's stay, unreadable, until it comes
/// back or they expire.
fn account(store: &Store) -> String {
    crate::relay::exposure_subject(store).unwrap_or_default()
}

/// GET /v1/inbox's answer, the current account's deliveries: { items: [{ id, endpoint,
/// receivedAt, method, path, headers, body (base64) }], total }.
pub fn inbox_page(store: &Store, after: Option<&str>, limit: usize) -> Result<Value, ApiError> {
    use base64::Engine;
    let (items, total) = store.inbox_list(&account(store), after, limit.clamp(1, 100))?;
    let items: Vec<Value> = items
        .into_iter()
        .map(|d| {
            json!({
                "id": d.id,
                "endpoint": d.endpoint,
                "receivedAt": chrono::DateTime::<chrono::Utc>::from_timestamp_millis(d.received_at)
                    .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                "method": d.method,
                "path": d.path,
                "headers": d.headers,
                "body": base64::engine::general_purpose::STANDARD.encode(&d.body),
            })
        })
        .collect();
    Ok(json!({ "items": items, "total": total }))
}

/// POST /v1/inbox/ack: only the current account's deliveries go.
pub fn ack(store: &Store, ids: &[String]) -> Result<usize, ApiError> {
    Ok(store.inbox_ack(&account(store), ids)?)
}

/// How many of the current account's deliveries wait, for the window's Shared card.
pub fn waiting(store: &Store) -> u64 {
    store
        .inbox_counts(&account(store))
        .map(|counts| counts.values().sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .set(crate::relay::keys::SUBJECT, &json!({ "subject": "u1" }))
            .unwrap();
        let verify = crate::util::sha256_hex(b"meta-secret");
        let kept = parse(&json!({
            "exposureId": "x",
            "items": [
                { "tokens": ["wh_0123456789abcdef0123", "my-github"], "maxBodyBytes": 10, "perMinute": 2 },
                { "tokens": ["messenger-1"], "ackStatus": 200, "handshake": "hub_challenge", "verifyTokenSha256": verify },
                { "tokens": ["PancakeIdAbc_-"], "ackStatus": 200, "handshake": "accept_any" },
                { "tokens": ["off-one"], "enabled": false },
            ],
        }))
        .unwrap();
        set(&store, &kept).unwrap();
        (dir, store)
    }

    fn post(store: &Store, rates: &Rates, path: &str, body: &[u8]) -> Answer {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), "Bearer flow".to_string()),
        ];
        receive(store, rates, "POST", path, &headers, body)
    }

    fn json_of(answer: &Answer) -> Value {
        match &answer.body {
            Body::Bytes(bytes) => serde_json::from_slice(bytes).unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    #[test]
    fn deliveries_are_kept_byte_exact_and_answered_like_realtimex() {
        let (_dir, store) = store();
        let rates = Rates::default();
        let answer = post(
            &store,
            &rates,
            "/wh/wh_0123456789abcdef0123?x=1",
            b"{\"a\":1}",
        );
        assert_eq!(answer.status, 202);
        let body = json_of(&answer);
        assert_eq!(body["success"], true);
        assert_eq!(body["queued"], true);
        // The other paths and the slug reach the same endpoint; a slug in another case too.
        assert_eq!(
            post(&store, &rates, "/wh/inbound/My-GitHub", b"x").status,
            202
        );
        let page = inbox_page(&store, None, 10).unwrap();
        assert_eq!(page["total"], 2);
        let first = &page["items"][0];
        assert_eq!(first["id"], body["deliveryId"]);
        assert_eq!(first["endpoint"], "wh_0123456789abcdef0123");
        assert_eq!(first["path"], "/wh/wh_0123456789abcdef0123?x=1");
        assert_eq!(first["method"], "POST");
        assert_eq!(first["body"], "eyJhIjoxfQ==");
        assert_eq!(first["headers"][1], json!(["authorization", "Bearer flow"]));
        assert!(first["receivedAt"].as_str().unwrap().ends_with('Z'));
        // Messenger's endpoint acknowledges with 200.
        assert_eq!(
            post(
                &store,
                &rates,
                "/api/v1/webhook-ingress/inbound/messenger-1",
                b"{}"
            )
            .status,
            200
        );
    }

    #[test]
    fn the_edge_checks_token_enabled_size_and_rate() {
        let (_dir, store) = store();
        let rates = Rates::default();
        let code = |answer: &Answer| json_of(answer)["code"].as_str().unwrap_or("").to_string();
        let unknown = post(&store, &rates, "/wh/nope", b"{}");
        assert_eq!(
            (unknown.status, code(&unknown)),
            (404, "WEBHOOK_ENDPOINT_NOT_FOUND".into())
        );
        let off = post(&store, &rates, "/wh/off-one", b"{}");
        assert_eq!(
            (off.status, code(&off)),
            (410, "WEBHOOK_ENDPOINT_DISABLED".into())
        );
        let big = post(&store, &rates, "/wh/my-github", &[b'x'; 11]);
        assert_eq!(
            (big.status, code(&big)),
            (413, "WEBHOOK_PAYLOAD_TOO_LARGE".into())
        );
        assert_eq!(post(&store, &rates, "/wh/my-github", b"1").status, 202);
        assert_eq!(post(&store, &rates, "/wh/my-github", b"2").status, 202);
        let limited = post(&store, &rates, "/wh/wh_0123456789abcdef0123", b"3");
        assert_eq!(
            (limited.status, code(&limited)),
            (429, "WEBHOOK_RATE_LIMITED".into())
        );
        assert!(limited.headers.iter().any(|(n, _)| n == "retry-after"));
        assert_eq!(
            receive(&store, &rates, "PUT", "/wh/my-github", &[], b"").status,
            405
        );
        assert_eq!(post(&store, &rates, "/wh/", b"{}").status, 404);
        assert_eq!(store.inbox_list("u1", None, 10).unwrap().1, 2);
    }

    #[test]
    fn handshakes_are_answered_and_kept_for_realtimex() {
        let (_dir, store) = store();
        let rates = Rates::default();
        let get = |path: &str| receive(&store, &rates, "GET", path, &[], b"");
        let ok = get(
            "/wh/messenger-1?hub.mode=subscribe&hub.verify_token=meta-secret&hub.challenge=12345",
        );
        assert_eq!(ok.status, 200);
        assert_eq!(ok.body, Body::Bytes(b"12345".to_vec()));
        let wrong = get("/wh/messenger-1?hub.mode=subscribe&hub.verify_token=nope&hub.challenge=1");
        assert_eq!(wrong.status, 403);
        assert_eq!(
            get("/wh/messenger-1?hub.mode=unsubscribe&hub.verify_token=x&hub.challenge=1").status,
            400
        );
        assert_eq!(get("/wh/messenger-1").status, 400);
        assert_eq!(get("/wh/PancakeIdAbc_-").body, Body::Bytes(b"ok".to_vec()));
        assert_eq!(get("/wh/my-github").status, 404);
        // The accepted and rejected handshakes and Pancake's: three GET entries.
        let page = inbox_page(&store, None, 10).unwrap();
        let methods: Vec<&str> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["method"].as_str().unwrap())
            .collect();
        assert_eq!(methods, ["GET", "GET", "GET"]);
    }

    #[test]
    fn tokens_and_bad_pushes() {
        assert_eq!(token_of("/wh/abc"), Some("abc"));
        assert_eq!(token_of("/wh/inbound/abc"), Some("abc"));
        assert_eq!(token_of("/api/v1/webhook-ingress/inbound/abc"), Some("abc"));
        assert_eq!(token_of("/wh/abc/more"), None);
        assert_eq!(token_of("/wh/"), None);
        assert_eq!(normalize("My GitHub!"), "my-github");
        for body in [
            json!({}),
            json!({ "items": [{ "tokens": [] }] }),
            json!({ "items": [{ "tokens": ["a/b"] }] }),
            json!({ "items": [{ "tokens": ["a"], "ackStatus": 201 }] }),
            json!({ "items": [{ "tokens": ["a"], "maxBodyBytes": 20 * 1024 * 1024 }] }),
            json!({ "items": [{ "tokens": ["a"], "handshake": "magic" }] }),
            json!({ "items": [{ "tokens": ["a"], "verifyTokenSha256": "plain" }] }),
        ] {
            assert!(parse(&body).is_err(), "{body}");
        }
    }
}
