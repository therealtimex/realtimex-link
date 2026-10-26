//! RealTimeX behind Link (realtimex-ai-app#2836): while RealTimeX runs it registers its servers
//! here (PUT /v1/origin, one target per surface), and the relay's requests for published
//! artifacts, the embed widget, webhooks, the mobile app and desktop messaging are passed to them
//! over loopback, with the headers RealTimeX's own connector set (relayConnector.cjs): where the
//! request came from, on which surface, and that it came from the public relay. With a token, each
//! also carries it, so RealTimeX can tell Link's relayed requests from any other local caller.
//!
//! Nothing registered, or nothing listening there any more: "desktop offline" (503).
//!
//! The origin is kept in memory only: RealTimeX registers again at each start, and a target nobody
//! answers on is forgotten, so a later process on that port is never handed public requests.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;

use super::{frame, response_headers, RelayRequest, Sender, TEXT_FLAG};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a public client waits before trying an offline computer again.
const RETRY_AFTER: u64 = 30;
/// RealTimeX's marker for requests from the public relay (desktopLoopbackSession.js there).
const RELAY_SOURCE_HEADER: &str = "x-realtimex-relay-source";
const PUBLIC_RELAY_SOURCE: &str = "public-relay";
const RELAY_SURFACE_HEADER: &str = "x-realtimex-relay-surface";
/// The origin's token, on every request Link passes to it.
pub const ORIGIN_TOKEN_HEADER: &str = "x-realtimex-link-origin";
const MIN_TOKEN: usize = 32;
const MAX_TOKEN: usize = 256;
/// The surfaces RealTimeX serves (Remote Control's `d` is Link's own).
const ORIGIN_SURFACES: [&str; 5] = ["a", "e", "h", "m", "p"];

/// RealTimeX's servers: the loopback port of each surface it serves, and its optional token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub targets: BTreeMap<String, u16>,
    pub token: Option<String>,
}

/// "http://127.0.0.1:<port>" exactly (a trailing slash allowed), as its port.
fn loopback_port(url: &str) -> Option<u16> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let port = rest.strip_suffix('/').unwrap_or(rest);
    if port.is_empty() || port.len() > 5 || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port > 0)
}

impl Origin {
    /// PUT /v1/origin's body: { targets: { a, e, h, m, p: "http://127.0.0.1:<port>" }, token? }.
    pub fn from_body(body: &Value) -> Result<Self, String> {
        let map = body
            .get("targets")
            .and_then(Value::as_object)
            .ok_or("targets must name each surface's http://127.0.0.1:<port>")?;
        let mut targets = BTreeMap::new();
        for (surface, url) in map {
            if !ORIGIN_SURFACES.contains(&surface.as_str()) {
                return Err(format!("{surface} is not a surface RealTimeX serves"));
            }
            let port = url
                .as_str()
                .and_then(loopback_port)
                .ok_or_else(|| format!("targets.{surface} must be http://127.0.0.1:<port>"))?;
            targets.insert(surface.clone(), port);
        }
        if targets.is_empty() {
            return Err("targets must name at least one surface".into());
        }
        let token = match body.get("token") {
            None | Some(Value::Null) => None,
            Some(Value::String(token))
                if (MIN_TOKEN..=MAX_TOKEN).contains(&token.len())
                    && token.chars().all(|c| c.is_ascii_graphic()) =>
            {
                Some(token.clone())
            }
            Some(_) => {
                return Err(format!(
                    "token must be {MIN_TOKEN} to {MAX_TOKEN} visible ASCII characters"
                ))
            }
        };
        Ok(Self { targets, token })
    }

    /// For GET /v1/origin: { registered, targets }.
    pub fn view(origin: Option<&Origin>) -> Value {
        let targets: serde_json::Map<String, Value> = origin
            .map(|origin| {
                origin
                    .targets
                    .iter()
                    .map(|(surface, port)| {
                        (surface.clone(), json!(format!("http://127.0.0.1:{port}")))
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({ "registered": origin.is_some(), "targets": targets })
    }
}

/// Where one stream goes: the port for its surface, the token, and which registration it was.
pub(super) struct Target {
    port: u16,
    token: Option<String>,
    surface: String,
    generation: u64,
}

/// The registered origin (and how many registrations there were), shared with the streams.
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<(Option<Origin>, u64)>>);

impl Registry {
    fn current(&self) -> std::sync::MutexGuard<'_, (Option<Origin>, u64)> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set(&self, origin: Origin) {
        let mut current = self.current();
        current.0 = Some(origin);
        current.1 += 1;
    }

    pub fn clear(&self) {
        let mut current = self.current();
        current.0 = None;
        current.1 += 1;
    }

    pub fn get(&self) -> Option<Origin> {
        self.current().0.clone()
    }

    pub fn registered(&self) -> bool {
        self.current().0.is_some()
    }

    pub(super) fn clone_handle(&self) -> Registry {
        self.clone()
    }

    /// The registered target of this surface, if any.
    pub(super) fn target(&self, surface: &str) -> Option<Target> {
        let current = self.current();
        let origin = current.0.as_ref()?;
        Some(Target {
            port: *origin.targets.get(surface)?,
            token: origin.token.clone(),
            surface: surface.to_string(),
            generation: current.1,
        })
    }

    /// Nothing answered on a target's port: that surface is offline until RealTimeX registers
    /// again (unless it already did since). With no target left, no origin is registered.
    fn forget(&self, target: &Target) {
        let mut current = self.current();
        if current.1 != target.generation {
            return;
        }
        if let Some(origin) = current.0.as_mut() {
            origin.targets.remove(&target.surface);
            if origin.targets.is_empty() {
                current.0 = None;
            }
        }
    }
}

/// The headers RealTimeX's own connector set on a relayed request.
fn forwarded(request: &RelayRequest, token: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("x-forwarded-host", request.public_host.clone()),
        ("x-forwarded-prefix", request.public_prefix.clone()),
        ("x-forwarded-proto", "https".into()),
        ("x-forwarded-for", request.client.to_string()),
        (RELAY_SURFACE_HEADER, request.surface.clone()),
        (RELAY_SOURCE_HEADER, PUBLIC_RELAY_SOURCE.into()),
    ];
    if let Some(token) = token {
        headers.push((ORIGIN_TOKEN_HEADER, token.to_string()));
    }
    headers
}

async fn connect(target: &Target, registry: &Registry) -> Option<TcpStream> {
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((Ipv4Addr::LOCALHOST, target.port)),
    )
    .await
    {
        Ok(Ok(stream)) => Some(stream),
        _ => {
            registry.forget(target);
            None
        }
    }
}

/// Pass one HTTP request to the origin and stream its answer back.
pub(super) async fn http(
    id: u32,
    request: RelayRequest,
    body: mpsc::Receiver<bytes::Bytes>,
    target: Option<Target>,
    registry: Registry,
    send: Sender,
) {
    let Some(target) = target else {
        return offline(id, &request, &send).await;
    };
    let Some(stream) = connect(&target, &registry).await else {
        return offline(id, &request, &send).await;
    };
    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
            Ok(handshake) => handshake,
            Err(error) => return reset(id, &send, &error.to_string()).await,
        };
    let connection = tokio::spawn(connection);
    let body = StreamBody::new(
        tokio_stream::wrappers::ReceiverStream::new(body)
            .map(|chunk| Ok::<_, Infallible>(hyper::body::Frame::data(chunk))),
    );
    let mut builder = hyper::Request::builder()
        .method(request.method.as_str())
        .uri(&request.path)
        .header("host", format!("127.0.0.1:{}", target.port));
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    for (name, value) in forwarded(&request, target.token.as_deref()) {
        builder = builder.header(name, value);
    }
    let Ok(outgoing) = builder.body(body) else {
        return reset(id, &send, "bad request").await;
    };
    let response = match sender.send_request(outgoing).await {
        Ok(response) => response,
        Err(error) => {
            connection.abort();
            return reset(id, &send, &error.to_string()).await;
        }
    };
    let (parts, mut answer) = response.into_parts();
    if !send
        .meta(
            frame::HEADERS,
            id,
            &json!({ "status": parts.status.as_u16(), "headers": response_headers(&parts.headers) }),
        )
        .await
    {
        connection.abort();
        return;
    }
    while let Some(next) = answer.frame().await {
        match next {
            Ok(part) => {
                let part = match part.into_data() {
                    Ok(chunk) => {
                        if !send.data(frame::DATA, id, &chunk, 0).await {
                            connection.abort();
                            return;
                        }
                        continue;
                    }
                    Err(part) => part,
                };
                if let Ok(trailers) = part.into_trailers() {
                    send.meta(
                        frame::TRAILERS,
                        id,
                        &Value::Object(response_headers(&trailers)),
                    )
                    .await;
                }
            }
            Err(error) => {
                connection.abort();
                return reset(id, &send, &error.to_string()).await;
            }
        }
    }
    send.raw(frame::END, id, &[], 0).await;
}

/// Pass one WebSocket to the origin: accepted once the origin accepts, then messages both ways
/// (a message keeps its boundary and text flag across frames) until either side closes.
pub(super) async fn websocket(
    id: u32,
    request: RelayRequest,
    mut input: mpsc::Receiver<Message>,
    target: Option<Target>,
    registry: Registry,
    send: Sender,
) {
    let Some(target) = target else {
        return reset(id, &send, "desktop offline").await;
    };
    let Some(stream) = connect(&target, &registry).await else {
        return reset(id, &send, "desktop offline").await;
    };
    let url = format!("ws://127.0.0.1:{}{}", target.port, request.path);
    let mut outgoing = match url.into_client_request() {
        Ok(outgoing) => outgoing,
        Err(error) => return reset(id, &send, &error.to_string()).await,
    };
    let headers = request
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .chain(
            forwarded(&request, target.token.as_deref())
                .into_iter()
                .map(|(name, value)| (name, value)),
        )
        .collect::<Vec<_>>();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            outgoing.headers_mut().append(name, value);
        }
    }
    if !request.protocols.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&request.protocols.join(", ")) {
            outgoing
                .headers_mut()
                .insert("sec-websocket-protocol", value);
        }
    }
    let (socket, response) = match tokio_tungstenite::client_async(outgoing, stream).await {
        Ok(accepted) => accepted,
        Err(error) => return reset(id, &send, &error.to_string()).await,
    };
    let protocol = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(|value| json!({ "sec-websocket-protocol": value }))
        .unwrap_or_else(|| json!({}));
    if !send
        .meta(
            frame::WS_ACCEPT,
            id,
            &json!({ "status": 101, "headers": protocol }),
        )
        .await
    {
        return;
    }
    let (mut sink, mut source) = socket.split();
    let mut reading = true;
    loop {
        tokio::select! {
            message = source.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if !send.data(frame::WS_DATA, id, text.as_str().as_bytes(), TEXT_FLAG).await {
                        return;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if !send.data(frame::WS_DATA, id, &bytes, 0).await {
                        return;
                    }
                }
                Some(Ok(Message::Close(close))) => {
                    let (code, reason) = close
                        .map(|c| (u16::from(c.code), c.reason.as_str().chars().take(123).collect::<String>()))
                        .unwrap_or((1005, String::new()));
                    send.meta(frame::WS_CLOSE, id, &json!({ "code": code, "reason": reason }))
                        .await;
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return reset(id, &send, &error.to_string()).await,
                None => {
                    send.meta(frame::WS_CLOSE, id, &json!({ "code": 1006, "reason": "" }))
                        .await;
                    return;
                }
            },
            message = input.recv(), if reading => match message {
                Some(message) => {
                    if sink.send(message).await.is_err() {
                        return reset(id, &send, "the origin closed the WebSocket").await;
                    }
                }
                // The relay's side is done: close, and the origin's close answers.
                None => {
                    reading = false;
                    let _ = sink.send(Message::Close(None)).await;
                }
            },
        }
    }
}

async fn reset(id: u32, send: &Sender, reason: &str) {
    send.meta(frame::RESET, id, &json!({ "error": reason }))
        .await;
}

/// "Desktop offline": RealTimeX isn't running here. A page for a browser, JSON otherwise.
async fn offline(id: u32, request: &RelayRequest, send: &Sender) {
    let browser = request
        .headers
        .iter()
        .any(|(name, value)| name == "accept" && value.contains("text/html"));
    let (content_type, body) = if browser {
        ("text/html; charset=utf-8", OFFLINE_PAGE.to_string())
    } else {
        (
            "application/json",
            json!({ "error": "desktop offline", "code": "desktop-offline" }).to_string(),
        )
    };
    respond(
        id,
        send,
        503,
        json!({
            "content-type": content_type,
            "retry-after": RETRY_AFTER.to_string(),
            "cache-control": "no-store",
        }),
        body,
    )
    .await;
}

/// A whole answer from Link itself.
pub(super) async fn answer(id: u32, send: &Sender, status: u16, content_type: &str, body: String) {
    respond(
        id,
        send,
        status,
        json!({ "content-type": content_type, "cache-control": "no-store" }),
        body,
    )
    .await;
}

async fn respond(id: u32, send: &Sender, status: u16, headers: Value, body: String) {
    if send
        .meta(
            frame::HEADERS,
            id,
            &json!({ "status": status, "headers": headers }),
        )
        .await
        && send.data(frame::DATA, id, body.as_bytes(), 0).await
    {
        send.raw(frame::END, id, &[], 0).await;
    }
}

const OFFLINE_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>This computer is offline</title>
<style>
body { margin: 0; min-height: 100vh; display: grid; place-items: center; font: 16px/1.5 system-ui, sans-serif; background: #f7f7f8; color: #1f2328; }
main { max-width: 28rem; padding: 2rem; text-align: center; }
h1 { font-size: 1.25rem; margin: 0 0 .5rem; }
p { margin: 0; color: #59636e; }
@media (prefers-color-scheme: dark) { body { background: #16181d; color: #e6e8eb; } p { color: #9aa3ad; } }
</style>
</head>
<body>
<main>
<h1>This computer is offline</h1>
<p>RealTimeX isn't open on the computer that shares this. Try again in a little while.</p>
</main>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RelayRequest {
        RelayRequest {
            surface: "e".into(),
            method: "GET".into(),
            path: "/embed/x".into(),
            headers: vec![("accept".into(), "text/html,*/*".into())],
            client: "203.0.113.7".parse().unwrap(),
            public_host: "c.realtimex.ai".into(),
            public_prefix: "/e/ID".into(),
            length: None,
            websocket: false,
            protocols: Vec::new(),
        }
    }

    #[test]
    fn an_origin_names_loopback_targets_and_maybe_a_long_token() {
        let token = "t".repeat(32);
        let origin = Origin::from_body(&json!({
            "targets": { "a": "http://127.0.0.1:3001", "e": "http://127.0.0.1:3395/" },
            "token": token,
        }))
        .unwrap();
        assert_eq!(
            origin,
            Origin {
                targets: BTreeMap::from([("a".into(), 3001), ("e".into(), 3395)]),
                token: Some(token.clone()),
            }
        );
        assert!(
            Origin::from_body(&json!({ "targets": { "h": "http://127.0.0.1:3001" } }))
                .unwrap()
                .token
                .is_none()
        );
        for body in [
            json!({ "token": token }),
            json!({ "targets": {} }),
            json!({ "targets": { "d": "http://127.0.0.1:3001" } }),
            json!({ "targets": { "a": "http://localhost:3001" } }),
            json!({ "targets": { "a": "http://127.0.0.1:0" } }),
            json!({ "targets": { "a": "http://127.0.0.1:70000" } }),
            json!({ "targets": { "a": "http://127.0.0.1:3001/api" } }),
            json!({ "targets": { "a": "https://127.0.0.1:3001" } }),
            json!({ "targets": { "a": 3001 } }),
            json!({ "targets": { "a": "http://127.0.0.1:3001" }, "token": "short" }),
        ] {
            assert!(Origin::from_body(&body).is_err(), "{body}");
        }
    }

    #[test]
    fn each_surface_has_its_target_and_a_dead_one_is_forgotten() {
        let registry = Registry::default();
        assert!(registry.target("a").is_none());
        registry.set(Origin {
            targets: BTreeMap::from([("e".into(), 3395), ("h".into(), 3001)]),
            token: None,
        });
        assert_eq!(registry.target("e").unwrap().port, 3395);
        let webhooks = registry.target("h").unwrap();
        assert_eq!(webhooks.port, 3001);
        assert!(registry.target("m").is_none());
        // An answer about a registration made before doesn't forget the current one.
        let stale = registry.target("e").unwrap();
        registry.set(registry.get().unwrap());
        registry.forget(&stale);
        assert!(registry.target("e").is_some());
        registry.forget(&registry.target("e").unwrap());
        assert!(registry.target("e").is_none());
        assert!(registry.registered());
        registry.forget(&registry.target("h").unwrap());
        assert!(!registry.registered());
        let _ = webhooks;
    }

    #[test]
    fn relayed_requests_carry_realtimexs_forwarding_headers() {
        let headers = forwarded(&request(), Some("secret"));
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("x-forwarded-host"), Some("c.realtimex.ai"));
        assert_eq!(get("x-forwarded-prefix"), Some("/e/ID"));
        assert_eq!(get("x-forwarded-proto"), Some("https"));
        assert_eq!(get("x-forwarded-for"), Some("203.0.113.7"));
        assert_eq!(get("x-realtimex-relay-surface"), Some("e"));
        assert_eq!(get("x-realtimex-relay-source"), Some("public-relay"));
        assert_eq!(get(ORIGIN_TOKEN_HEADER), Some("secret"));
        assert!(!forwarded(&request(), None)
            .iter()
            .any(|(n, _)| *n == ORIGIN_TOKEN_HEADER));
    }

    #[tokio::test]
    async fn without_an_origin_a_browser_gets_the_offline_page() {
        let (data, mut sent) = mpsc::channel(8);
        let (control, _) = mpsc::unbounded_channel();
        let send = Sender { data, control };
        http(
            5,
            request(),
            mpsc::channel(1).1,
            None,
            Registry::default(),
            send,
        )
        .await;
        let mut decoder = super::super::Decoder::default();
        let mut frames = Vec::new();
        while let Ok(Message::Binary(chunk)) = sent.try_recv() {
            frames.extend(decoder.feed(&chunk).unwrap());
        }
        let headers: Value = serde_json::from_slice(&frames[0].payload).unwrap();
        assert_eq!(headers["status"], 503);
        assert_eq!(headers["headers"]["retry-after"], "30");
        assert!(String::from_utf8_lossy(&frames[1].payload).contains("This computer is offline"));
        assert_eq!(frames[2].kind, frame::END);
    }

    #[tokio::test]
    async fn requests_reach_the_origin_with_their_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buffer = [0u8; 4096];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                seen.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-from: origin\r\n\r\nok")
                .await
                .unwrap();
            String::from_utf8_lossy(&seen).to_lowercase()
        });
        let registry = Registry::default();
        registry.set(Origin {
            targets: BTreeMap::from([("a".into(), port)]),
            token: Some("t".repeat(32)),
        });
        let (data, mut sent) = mpsc::channel(8);
        let (control, _) = mpsc::unbounded_channel();
        let send = Sender { data, control };
        let (body, rx) = mpsc::channel(1);
        drop(body);
        let mut request = request();
        request.surface = "a".into();
        request.path = "/ar/x/index.html".into();
        http(9, request, rx, registry.target("a"), registry, send).await;
        let seen = server.await.unwrap();
        assert!(seen.starts_with("get /ar/x/index.html http/1.1"));
        assert!(seen.contains(&format!("host: 127.0.0.1:{port}")));
        assert!(seen.contains("x-realtimex-relay-source: public-relay"));
        assert!(seen.contains("x-realtimex-relay-surface: a"));
        assert!(seen.contains(&format!("x-realtimex-link-origin: {}", "t".repeat(32))));
        let mut decoder = super::super::Decoder::default();
        let mut frames = Vec::new();
        while let Ok(Message::Binary(chunk)) = sent.try_recv() {
            frames.extend(decoder.feed(&chunk).unwrap());
        }
        let headers: Value = serde_json::from_slice(&frames[0].payload).unwrap();
        assert_eq!(headers["status"], 200);
        assert_eq!(headers["headers"]["x-from"], "origin");
        assert!(headers["headers"].get("content-length").is_none());
        assert_eq!(&frames[1].payload[..], b"ok");
        assert_eq!(frames.last().unwrap().kind, frame::END);
    }
}
