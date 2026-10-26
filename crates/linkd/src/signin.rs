//! Link's own sign-in to the RealTimeX account, so it works without RealTimeX: the system browser
//! signs in at accounts.realtimex.ai (Keycloak client `realtimex-link`, Authorization Code with
//! PKCE, the answer coming back to a one-time 127.0.0.1 port as RFC 8252 describes) and Link keeps
//! the offline refresh token sealed in its store, renewing access tokens itself.
//!
//! RealTimeX's handover (POST /v1/account/token) stays the quick way in: it signs Link in only
//! while Link has no sign-in of its own, and after Sign out it is only offered ("Continue as …")
//! until the person picks it or signs in again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

use crate::host::Host;

pub mod keys {
    /// Link's own sign-in: { refreshToken (sealed), subject, email, name, signedInAt }.
    pub const SESSION: &str = "signin-session";
    /// Signed out in Link: RealTimeX's handover is only offered, not taken.
    pub const HANDOVER_OFF: &str = "signin-handover-off";
}

const DEFAULT_ISSUER: &str = "https://accounts.realtimex.ai/auth/realms/realtimex";
const CLIENT_ID: &str = "realtimex-link";
const SCOPE: &str = "openid offline_access";
/// How long the browser has to come back.
const SIGN_IN_WINDOW: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The Keycloak realm (RTX_LINK_AUTH_ISSUER for a development realm).
fn issuer() -> String {
    std::env::var("RTX_LINK_AUTH_ISSUER")
        .ok()
        .map(|issuer| issuer.trim().trim_end_matches('/').to_string())
        .filter(|issuer| !issuer.is_empty())
        .unwrap_or_else(|| DEFAULT_ISSUER.to_string())
}

fn endpoint(name: &str) -> String {
    format!("{}/protocol/openid-connect/{name}", issuer())
}

/// An access token and when it expires (unix seconds).
#[derive(Clone)]
struct Access {
    token: String,
    expires_at: i64,
}

#[derive(Default)]
struct Pending {
    task: Option<JoinHandle<()>>,
    error: Option<String>,
}

pub struct SignIn {
    host: Weak<Host>,
    access: tokio::sync::Mutex<Option<Access>>,
    pending: Mutex<Pending>,
    /// RealTimeX's last handover while Link is signed out: (token, email), for "Continue as".
    offered: Mutex<Option<(String, String)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl SignIn {
    pub fn new(host: Weak<Host>) -> Self {
        Self {
            host,
            access: tokio::sync::Mutex::new(None),
            pending: Mutex::new(Pending::default()),
            offered: Mutex::new(None),
        }
    }

    fn host(&self) -> Result<Arc<Host>> {
        self.host.upgrade().ok_or_else(|| anyhow!("shutting down"))
    }

    fn session(&self) -> Option<Value> {
        self.host.upgrade()?.store.get(keys::SESSION)
    }

    /// Link signed in itself (rather than through RealTimeX).
    pub fn signed_in(&self) -> bool {
        self.session().is_some()
    }

    /// RealTimeX's handover may sign Link in: not while Link has its own sign-in, nor after the
    /// person signed out here. Otherwise it is kept as an offer.
    pub fn takes_handover(&self, token: &str) -> bool {
        let Some(host) = self.host.upgrade() else {
            return false;
        };
        let offer = || {
            let email = crate::identity::account_of(token)
                .and_then(|account| account.get("email")?.as_str().map(str::to_string))
                .unwrap_or_default();
            *lock(&self.offered) = Some((token.to_string(), email));
        };
        if self.signed_in() {
            // Another account than Link's own: offered, so the window can offer "Use <it>"
            // (realtimex-ai-app#2836: one App ID needs one account).
            let subject = |account: Option<Value>| {
                account.and_then(|a| a.get("subject").and_then(Value::as_str).map(str::to_string))
            };
            if subject(crate::identity::account_of(token))
                != subject(host.store.get(crate::identity::keys::ACCOUNT))
            {
                offer();
            } else {
                *lock(&self.offered) = None;
            }
            return false;
        }
        if host.store.get(keys::HANDOVER_OFF).is_some() {
            offer();
            return false;
        }
        true
    }

    /// For the window: { signedIn, via, email, name, signingIn, error, offer }.
    pub fn status(&self) -> Value {
        let host = self.host.upgrade();
        let session = self.session();
        let handed = host
            .as_ref()
            .and_then(|host| host.store.get(crate::identity::keys::ACCOUNT));
        let (via, account) = match (&session, &handed) {
            (Some(session), _) => ("link", Some(session.clone())),
            (None, Some(account)) => ("realtimex", Some(account.clone())),
            _ => ("", None),
        };
        let field = |key: &str| {
            account
                .as_ref()
                .and_then(|a| a.get(key))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let pending = lock(&self.pending);
        let signing_in = pending
            .task
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        json!({
            "signedIn": account.is_some(),
            "via": if via.is_empty() { Value::Null } else { json!(via) },
            "email": field("email"),
            "name": field("name"),
            "signingIn": signing_in,
            "error": pending.error,
            "offer": lock(&self.offered).as_ref().map(|(_, email)| email.clone()),
            // §6.2: RealTimeX owns this Link; the window offers no sign-out or other account.
            "managedByRealtimex": host.as_ref().is_some_and(|host| crate::owner::present(&host.store)),
        })
    }

    /// Start a browser sign-in: listen on a one-time 127.0.0.1 port and return the URL to open.
    pub async fn begin(self: &Arc<Self>) -> Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("no local port for the sign-in")?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let verifier = crate::util::base64url(&crate::util::random_bytes::<32>());
        let challenge = crate::util::base64url(&Sha256::digest(verifier.as_bytes()));
        let state = crate::util::base64url(&crate::util::random_bytes::<16>());
        let url = format!(
            "{}?client_id={CLIENT_ID}&response_type=code&scope={}&redirect_uri={}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
            endpoint("auth"),
            encode(SCOPE),
            encode(&redirect),
        );
        let me = self.clone();
        let task = tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                SIGN_IN_WINDOW,
                me.answer(listener, &state, &verifier, &redirect),
            )
            .await
            .unwrap_or_else(|_| Err(anyhow!("The sign-in timed out. Try again.")));
            if let Err(error) = outcome {
                if let Ok(host) = me.host() {
                    (host.log)(&format!("[sign-in] {error:#}"));
                }
                lock(&me.pending).error = Some(format!("{error:#}"));
            }
        });
        let mut pending = lock(&self.pending);
        if let Some(previous) = pending.task.replace(task) {
            previous.abort();
        }
        pending.error = None;
        Ok(url)
    }

    /// Wait for the browser on the one-time port, then trade the code for tokens. Each connection
    /// is served on its own, so one that never sends its request holds up nobody; a request that
    /// isn't this sign-in's answer (another state, no code) gets a 400 and the wait goes on.
    async fn answer(
        self: &Arc<Self>,
        listener: TcpListener,
        state: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<()> {
        // Dropped with this future (a new sign-in, sign out, the time limit): so are the
        // connections still being served.
        let mut connections: JoinSet<Option<Result<()>>> = JoinSet::new();
        // The first real answer finishes the sign-in; a second one (a reload) is turned away.
        let claimed = Arc::new(AtomicBool::new(false));
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let me = self.clone();
                    let (state, verifier, redirect) =
                        (state.to_string(), verifier.to_string(), redirect.to_string());
                    let claimed = claimed.clone();
                    connections.spawn(async move {
                        me.callback(stream, &state, &verifier, &redirect, &claimed)
                            .await
                    });
                }
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Ok(Some(outcome)) = joined {
                        return outcome;
                    }
                }
            }
        }
    }

    /// One connection on the sign-in port. Some(outcome) when it ended the sign-in, None when
    /// the wait goes on.
    async fn callback(
        &self,
        mut stream: TcpStream,
        state: &str,
        verifier: &str,
        redirect: &str,
        claimed: &AtomicBool,
    ) -> Option<Result<()>> {
        let target = request_target(&mut stream).await?;
        let outcome = match judge(&target, state) {
            Callback::NotFound => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                return None;
            }
            Callback::Rejected(message) => {
                let _ = stream
                    .write_all(page("400 Bad Request", "Sign-in failed", message).as_bytes())
                    .await;
                let _ = stream.shutdown().await;
                return None;
            }
            _ if claimed.swap(true, Ordering::SeqCst) => {
                let _ = stream
                    .write_all(
                        page(
                            "409 Conflict",
                            "Sign-in in progress",
                            "RealTimeX Link is already finishing this sign-in. You can close this tab.",
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = stream.shutdown().await;
                return None;
            }
            Callback::Refused(message) => Err(anyhow!("{message}")),
            Callback::Code(code) => self.finish(&code, verifier, redirect).await,
        };
        let page = match &outcome {
            Ok(email) => page(
                "200 OK",
                "Signed in",
                &format!("RealTimeX Link is signed in as {email}. You can close this tab."),
            ),
            Err(error) => page("200 OK", "Sign-in failed", &format!("{error:#}")),
        };
        let _ = stream.write_all(page.as_bytes()).await;
        let _ = stream.shutdown().await;
        Some(outcome.map(|_| ()))
    }

    async fn finish(&self, code: &str, verifier: &str, redirect: &str) -> Result<String> {
        let tokens = token_request(vec![
            ("grant_type", "authorization_code".into()),
            ("client_id", CLIENT_ID.into()),
            ("code", code.into()),
            ("redirect_uri", redirect.into()),
            ("code_verifier", verifier.into()),
        ])
        .await?;
        let (email, access) = self.keep(&tokens)?;
        *self.access.lock().await = Some(access);
        let host = self.host()?;
        host.store.remove(keys::HANDOVER_OFF)?;
        *lock(&self.offered) = None;
        (host.log)(&format!("[sign-in] signed in as {email}"));
        host.relay.wake();
        Ok(email)
    }

    /// Keep a token answer (the sealed refresh token and the account); its access token.
    fn keep(&self, tokens: &Value) -> Result<(String, Access)> {
        let host = self.host()?;
        let access = tokens
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no access token in the answer"))?;
        let refresh = tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no refresh token in the answer"))?;
        let claims = crate::identity::claims_of(access).unwrap_or(Value::Null);
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("the access token names no account"))?;
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or(subject)
            .to_string();
        let name = claims.get("name").cloned().unwrap_or(Value::Null);
        let signed_in_at = self
            .session()
            .and_then(|s| s.get("signedInAt").cloned())
            .unwrap_or_else(|| json!(crate::util::iso_now()));
        host.store.set(
            keys::SESSION,
            &json!({
                "refreshToken": host.seal_secret(refresh)?,
                "subject": subject,
                "email": email,
                "name": name,
                "signedInAt": signed_in_at,
            }),
        )?;
        host.store.set(
            crate::identity::keys::ACCOUNT,
            &json!({ "subject": subject, "email": email, "name": name }),
        )?;
        let access = Access {
            token: access.to_string(),
            expires_at: claims
                .get("exp")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp() + 60),
        };
        Ok((email, access))
    }

    /// A current access token from Link's own sign-in, renewed when needed; None when Link has
    /// none (or the account ended it: then it is signed out).
    pub async fn access_token(&self) -> Option<String> {
        // Held while renewing: Keycloak rotates refresh tokens, so one renewal at a time.
        let mut access = self.access.lock().await;
        if let Some(current) = access.as_ref() {
            if current.expires_at > chrono::Utc::now().timestamp() + 30 && self.signed_in() {
                return Some(current.token.clone());
            }
        }
        let host = self.host.upgrade()?;
        let session = host.store.get(keys::SESSION)?;
        let refresh = host.open_secret(session.get("refreshToken")?)?;
        *access = None;
        match token_request(vec![
            ("grant_type", "refresh_token".into()),
            ("client_id", CLIENT_ID.into()),
            ("refresh_token", refresh),
        ])
        .await
        {
            Ok(tokens) => match self.keep(&tokens) {
                Ok((_, renewed)) => {
                    let token = renewed.token.clone();
                    *access = Some(renewed);
                    Some(token)
                }
                Err(error) => {
                    (host.log)(&format!("[sign-in] renewing failed: {error:#}"));
                    None
                }
            },
            Err(error) => {
                (host.log)(&format!("[sign-in] renewing failed: {error:#}"));
                // The account ended this sign-in (signed out elsewhere, password changed).
                if error.to_string().contains("invalid_grant") {
                    let _ = host.store.remove(keys::SESSION);
                    let _ = host.store.remove(crate::identity::keys::ACCOUNT);
                    lock(&self.pending).error = Some("Your sign-in ended. Sign in again.".into());
                }
                None
            }
        }
    }

    /// Sign out: end the sign-in at accounts.realtimex.ai too, forget the account, and take
    /// RealTimeX's handover only when the person picks it again.
    pub async fn sign_out(&self) -> Result<()> {
        self.end(false).await
    }

    /// RealTimeX signed out (§6.2): Link is signed out too, but takes RealTimeX's next handover.
    pub async fn sign_out_for_realtimex(&self) -> Result<()> {
        self.end(true).await
    }

    /// RealTimeX's handover replaces any sign-in Link made itself (§6.2: the handover wins).
    pub async fn replace_with_handover(&self, token: &str) -> Result<()> {
        let host = self.host()?;
        if self.signed_in() {
            self.end_session().await?;
        }
        host.store.remove(keys::HANDOVER_OFF)?;
        *lock(&self.offered) = None;
        host.relay.set_account(token)
    }

    /// Revoke and forget Link's own session (its refresh token), nothing else.
    async fn end_session(&self) -> Result<()> {
        let host = self.host()?;
        if let Some(task) = lock(&self.pending).task.take() {
            task.abort();
        }
        let mut access = self.access.lock().await;
        let refresh = host
            .store
            .get(keys::SESSION)
            .and_then(|session| host.open_secret(session.get("refreshToken")?));
        if let Some(refresh) = refresh {
            if let Err(error) = token_post(
                &endpoint("revoke"),
                vec![
                    ("client_id", CLIENT_ID.into()),
                    ("token", refresh),
                    ("token_type_hint", "refresh_token".into()),
                ],
            )
            .await
            {
                (host.log)(&format!("[sign-in] revoking failed: {error:#}"));
            }
        }
        *access = None;
        host.store.remove(keys::SESSION)?;
        Ok(())
    }

    /// Signed out here (`for_realtimex`: by RealTimeX, whose next handover is taken; otherwise
    /// in Link, after which RealTimeX's handover is only offered).
    async fn end(&self, for_realtimex: bool) -> Result<()> {
        let host = self.host()?;
        if let Some(task) = lock(&self.pending).task.take() {
            task.abort();
        }
        // Held throughout, as a renewal holds it: one in flight would otherwise keep a refresh
        // token rotated after the one read here, and sign Link back in after this sign-out.
        let mut access = self.access.lock().await;
        let refresh = host
            .store
            .get(keys::SESSION)
            .and_then(|session| host.open_secret(session.get("refreshToken")?));
        if let Some(refresh) = refresh {
            if let Err(error) = token_post(
                &endpoint("revoke"),
                vec![
                    ("client_id", CLIENT_ID.into()),
                    ("token", refresh),
                    ("token_type_hint", "refresh_token".into()),
                ],
            )
            .await
            {
                (host.log)(&format!("[sign-in] revoking failed: {error:#}"));
            }
        }
        *access = None;
        host.store.remove(keys::SESSION)?;
        host.store.remove(crate::identity::keys::ACCOUNT)?;
        if for_realtimex {
            host.store.remove(keys::HANDOVER_OFF)?;
        } else {
            host.store.set(keys::HANDOVER_OFF, &json!(true))?;
        }
        *lock(&self.offered) = None;
        host.relay.forget_account();
        host.relay.wake();
        lock(&self.pending).error = None;
        (host.log)("[sign-in] signed out");
        Ok(())
    }

    /// The account RealTimeX handed over without signing Link in: { subject, email }.
    pub fn offered_account(&self) -> Option<Value> {
        let token = lock(&self.offered)
            .as_ref()
            .map(|(token, _)| token.clone())?;
        crate::identity::account_of(&token)
    }

    pub fn offered_email(&self) -> Option<String> {
        lock(&self.offered).as_ref().map(|(_, email)| email.clone())
    }

    /// "Continue as …" / "Use <RealTimeX's account>": take RealTimeX's last handover after all,
    /// signing Link out of its own account first when it has one.
    pub async fn use_handover(&self) -> Result<()> {
        let host = self.host()?;
        let (token, _) = lock(&self.offered)
            .take()
            .ok_or_else(|| anyhow!("RealTimeX hasn't offered an account; open RealTimeX"))?;
        if self.signed_in() {
            self.sign_out().await?;
        }
        host.store.remove(keys::HANDOVER_OFF)?;
        host.relay.set_account(&token)
    }
}

/// POST a form to Keycloak's token endpoint; its JSON answer, or its error as text.
async fn token_request(form: Vec<(&'static str, String)>) -> Result<Value> {
    token_post(&endpoint("token"), form).await
}

async fn token_post(url: &str, form: Vec<(&'static str, String)>) -> Result<Value> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(&url)
            .send_form(form.iter().map(|(k, v)| (*k, v.as_str())))
            .with_context(|| url.clone())?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let error = answer.get("error").and_then(Value::as_str).unwrap_or("");
            let detail = answer
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("");
            bail!("{status} {error} {detail}");
        }
        Ok(answer)
    })
    .await?
}

/// What a request on the sign-in port brought.
#[derive(Debug, PartialEq)]
enum Callback {
    /// Not /callback: 404, and the wait goes on.
    NotFound,
    /// Not this sign-in's answer (another state, or no code): 400, and the wait goes on.
    Rejected(&'static str),
    /// The account's answer with this state, refusing (the person cancelled): the sign-in ends.
    Refused(String),
    Code(String),
}

fn judge(target: &str, state: &str) -> Callback {
    let Some(query) = target.strip_prefix("/callback?") else {
        return Callback::NotFound;
    };
    let params = parse_query(query);
    let param = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    if !param("state")
        .is_some_and(|given| crate::util::same_bytes(given.as_bytes(), state.as_bytes()))
    {
        Callback::Rejected("The sign-in answer didn't match. Try again.")
    } else if let Some(error) = param("error") {
        Callback::Refused(param("error_description").unwrap_or(error).to_string())
    } else if let Some(code) = param("code").filter(|code| !code.is_empty()) {
        Callback::Code(code.to_string())
    } else {
        Callback::Rejected("The sign-in answer had no code. Try again.")
    }
}

/// The request target of one HTTP request on the sign-in port ("/callback?…").
async fn request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 || buffer.len() > 16 * 1024 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let line = String::from_utf8_lossy(&buffer).lines().next()?.to_string();
    let mut parts = line.split(' ');
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_string)
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(key)?, decode(value)?))
        })
        .collect()
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The page the browser shows after coming back, with this HTTP status ("200 OK").
fn page(status: &str, title: &str, message: &str) -> String {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<style>body{{font-family:system-ui,sans-serif;display:grid;place-items:center;min-height:90vh;margin:0 16px;color:#1f2937;background:#f9fafb}}\
@media (prefers-color-scheme:dark){{body{{color:#e5e7eb;background:#111827}}}}main{{max-width:420px}}h1{{font-size:20px}}</style>\
</head><body><main><h1>{title}</h1><p>{}</p></main></body></html>",
        escape(message),
        title = escape(title),
    );
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_decode() {
        let params = parse_query("code=a%2Eb&state=x+y&error_description=no%20way");
        assert_eq!(params[0], ("code".into(), "a.b".into()));
        assert_eq!(params[1], ("state".into(), "x y".into()));
        assert_eq!(params[2].1, "no way");
        assert!(decode("%zz").is_none());
    }

    #[test]
    fn only_this_sign_ins_answer_counts() {
        let state = "s-123";
        assert_eq!(
            judge("/callback?code=abc&state=s-123", state),
            Callback::Code("abc".into())
        );
        assert_eq!(judge("/favicon.ico", state), Callback::NotFound);
        // Another state, none, or no code: turned away, and the sign-in keeps waiting.
        for target in [
            "/callback?code=abc&state=other",
            "/callback?code=abc",
            "/callback?state=s-123",
            "/callback?state=s-123&code=",
            "/callback?error=access_denied&state=other",
        ] {
            assert!(
                matches!(judge(target, state), Callback::Rejected(_)),
                "{target}"
            );
        }
        // The account's own refusal ends it.
        assert_eq!(
            judge(
                "/callback?error=access_denied&error_description=Cancelled&state=s-123",
                state
            ),
            Callback::Refused("Cancelled".into())
        );
    }

    #[test]
    fn pages_carry_their_status() {
        assert!(page("400 Bad Request", "t", "m").starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(page("200 OK", "<t>", "m").contains("&lt;t&gt;"));
    }

    #[test]
    fn redirects_encode() {
        assert_eq!(
            encode("http://127.0.0.1:5/callback"),
            "http%3A%2F%2F127.0.0.1%3A5%2Fcallback"
        );
        assert_eq!(encode("openid offline_access"), "openid%20offline_access");
    }
}
