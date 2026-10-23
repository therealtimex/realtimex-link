//! Pending connection requests (ADR-2611 §1.4): a new computer
//! without the password asks, the person at this computer allows or denies. In memory only: a
//! restart drops them and the controller sees "expired". Each request has two ids: `id`, the
//! poll capability returned only to the requester, and `ref`, which this computer's UI uses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::error::ApiError;
use crate::util::{base64url, iso, now_ms, random_bytes};

pub const REQUEST_TTL_MS: i64 = 60 * 1000;
/// Settled requests are kept this long so a poll in flight still learns the outcome.
const SETTLED_TTL_MS: i64 = 60 * 1000;
pub const MAX_WAIT_MS: u64 = 25 * 1000;
const MAX_NAME_LENGTH: usize = 80;
// Requests not from this computer's account (ADR-2611 §1.5).
pub const PER_SOURCE_LIMIT: usize = 3;
const PER_SOURCE_WINDOW_MS: i64 = 10 * 60 * 1000;
pub const TOTAL_LIMIT: usize = 10;
const TOTAL_WINDOW_MS: i64 = 60 * 60 * 1000;
pub const MAX_PENDING_OTHER: usize = 3;

/// Control and format characters removed, at most 80 characters: names are attacker-chosen.
pub fn clean_name(value: Option<&Value>) -> String {
    let text = value.and_then(Value::as_str).unwrap_or("");
    let kept: String = text
        .chars()
        .filter(|c| !c.is_control() && !is_format(*c))
        .collect();
    let units: Vec<u16> = kept.trim().encode_utf16().take(MAX_NAME_LENGTH).collect();
    String::from_utf16_lossy(&units)
}

/// Unicode Cf (format) characters that matter in names: bidi controls, zero-width marks, BOM…
fn is_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

fn new_id() -> String {
    base64url(&random_bytes::<16>())
}

fn at(ms: i64) -> String {
    iso(chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default())
}

#[derive(Clone, Debug)]
pub struct Request {
    pub id: String,
    pub reference: String,
    pub client_id: String,
    pub client_name: String,
    pub machine: String,
    pub same_account: bool,
    pub verified_machine_name: Option<String>,
    pub via: String,
    pub remote_address: Option<String>,
    pub tailnet_login: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: String,
    pub grant: Option<Value>,
    pub admit: Option<Value>,
    pub decided_at: Option<i64>,
}

impl Request {
    /// What this computer's own UI sees: no poll capability, no admission.
    pub fn visible(&self) -> Value {
        json!({
            "ref": self.reference,
            "clientId": self.client_id,
            "clientName": self.client_name,
            "machine": self.machine,
            "account": { "sameAccount": self.same_account, "verifiedMachineName": self.verified_machine_name },
            "via": self.via,
            "remoteAddress": self.remote_address,
            "tailnetLogin": self.tailnet_login,
            "createdAt": at(self.created_at),
            "expiresAt": at(self.expires_at),
            "status": self.status,
            "grant": self.grant,
            "decidedAt": self.decided_at.map(at),
        })
    }
}

pub struct NewRequest {
    pub client_id: String,
    pub client_name: Option<Value>,
    pub machine: Option<Value>,
    pub same_account: bool,
    pub verified_machine_name: Option<String>,
    pub via: String,
    pub remote_address: Option<String>,
    pub tailnet_login: Option<String>,
}

/// The requester's poll answer.
pub enum Poll {
    Claimed,
    Allowed(Value),
    Waiting { status: String, expires_at: String },
}

type Hook = Box<dyn Fn(&Request) + Send + Sync>;

#[derive(Default)]
struct State {
    requests: HashMap<String, Request>,
    wakers: HashMap<String, Arc<Notify>>,
    other_sources: HashMap<String, Vec<i64>>,
    other_total: Vec<i64>,
}

pub struct RequestStore {
    state: Mutex<State>,
    on_create: Hook,
    on_settle: Hook,
}

fn rate_limited(retry_after_ms: i64) -> ApiError {
    let seconds = ((retry_after_ms.max(0) + 999) / 1000).max(1) as u64;
    ApiError::coded(
        429,
        "rate-limited",
        "Too many requests to connect; try again later",
    )
    .retry_after(seconds)
}

impl RequestStore {
    pub fn new(on_create: Hook, on_settle: Hook) -> Self {
        Self {
            state: Mutex::new(State::default()),
            on_create,
            on_settle,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Expire what is due and forget what was settled long ago; returns the newly expired.
    fn sweep(state: &mut State, now: i64) -> Vec<Request> {
        let mut expired = Vec::new();
        for request in state.requests.values_mut() {
            if request.status == "pending" && now >= request.expires_at {
                request.status = "expired".into();
                request.decided_at = Some(now);
                expired.push(request.clone());
            }
        }
        state.requests.retain(|_, r| {
            r.decided_at
                .is_none_or(|decided| now - decided < SETTLED_TTL_MS)
        });
        for request in &expired {
            if let Some(waker) = state.wakers.remove(&request.id) {
                waker.notify_waiters();
            }
        }
        let live: Vec<String> = state.requests.keys().cloned().collect();
        state.wakers.retain(|id, _| live.contains(id));
        expired
    }

    fn settled(&self, expired: Vec<Request>) {
        for request in expired {
            (self.on_settle)(&request);
        }
    }

    /// A pending request for this client, or its existing one. 429 for too many requests not
    /// from this computer's account.
    pub fn create(&self, new: NewRequest) -> Result<(Request, bool), ApiError> {
        let now = now_ms();
        let (result, expired) = {
            let mut state = self.lock();
            let expired = Self::sweep(&mut state, now);
            let existing = state
                .requests
                .values()
                .find(|r| r.client_id == new.client_id && r.status == "pending")
                .cloned();
            if let Some(existing) = existing {
                (Ok((existing, false)), expired)
            } else {
                match Self::check_other_limits(&mut state, &new, now) {
                    Err(error) => (Err(error), expired),
                    Ok(()) => {
                        let request = Request {
                            id: new_id(),
                            reference: new_id(),
                            client_id: new.client_id,
                            client_name: Some(clean_name(new.client_name.as_ref()))
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| "Unnamed computer".into()),
                            machine: clean_name(new.machine.as_ref()),
                            same_account: new.same_account,
                            verified_machine_name: new.verified_machine_name,
                            via: if new.via == "public" {
                                "public".into()
                            } else {
                                "tailnet".into()
                            },
                            remote_address: new.remote_address,
                            tailnet_login: new.tailnet_login,
                            created_at: now,
                            expires_at: now + REQUEST_TTL_MS,
                            status: "pending".into(),
                            grant: None,
                            admit: None,
                            decided_at: None,
                        };
                        state.requests.insert(request.id.clone(), request.clone());
                        (Ok((request, true)), expired)
                    }
                }
            }
        };
        self.settled(expired);
        if let Ok((request, true)) = &result {
            (self.on_create)(request);
        }
        result
    }

    fn check_other_limits(state: &mut State, new: &NewRequest, now: i64) -> Result<(), ApiError> {
        if new.same_account {
            return Ok(());
        }
        let source = new
            .remote_address
            .clone()
            .unwrap_or_else(|| "unknown".into());
        state.other_total.retain(|t| now - t < TOTAL_WINDOW_MS);
        let recent: Vec<i64> = state
            .other_sources
            .get(&source)
            .map_or_else(Vec::new, |list| {
                list.iter()
                    .copied()
                    .filter(|t| now - t < PER_SOURCE_WINDOW_MS)
                    .collect()
            });
        let pending_other = state
            .requests
            .values()
            .filter(|r| r.status == "pending" && !r.same_account)
            .count();
        if recent.len() >= PER_SOURCE_LIMIT {
            return Err(rate_limited(PER_SOURCE_WINDOW_MS - (now - recent[0])));
        }
        if state.other_total.len() >= TOTAL_LIMIT {
            return Err(rate_limited(TOTAL_WINDOW_MS - (now - state.other_total[0])));
        }
        if pending_other >= MAX_PENDING_OTHER {
            return Err(rate_limited(REQUEST_TTL_MS));
        }
        let mut recent = recent;
        recent.push(now);
        state.other_sources.insert(source, recent);
        state.other_total.push(now);
        Ok(())
    }

    pub fn by_ref(&self, reference: &str) -> Option<Request> {
        let expired;
        let found = {
            let mut state = self.lock();
            expired = Self::sweep(&mut state, now_ms());
            state
                .requests
                .values()
                .find(|r| r.reference == reference)
                .cloned()
        };
        self.settled(expired);
        found
    }

    /// Open requests, for this computer's own UI.
    pub fn pending(&self) -> Vec<Value> {
        let (list, expired) = {
            let mut state = self.lock();
            let expired = Self::sweep(&mut state, now_ms());
            let mut list: Vec<&Request> = state
                .requests
                .values()
                .filter(|r| r.status == "pending")
                .collect();
            list.sort_by_key(|r| r.created_at);
            (list.into_iter().map(Request::visible).collect(), expired)
        };
        self.settled(expired);
        list
    }

    /// The requester's poll: waits up to `wait_ms` (at most 25 s) for a change. An allowed
    /// request is answered with its admission exactly once; it is "claimed" afterwards.
    pub async fn poll(&self, id: &str, wait_ms: u64) -> Option<Poll> {
        let wait = {
            let mut state = self.lock();
            Self::sweep(&mut state, now_ms());
            let request = state.requests.get(id)?;
            if request.status == "pending" && wait_ms > 0 {
                let until_expiry = (request.expires_at - now_ms()).max(0) as u64;
                let waker = state
                    .wakers
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::new(Notify::new()))
                    .clone();
                Some((waker, wait_ms.min(MAX_WAIT_MS).min(until_expiry + 50)))
            } else {
                None
            }
        };
        if let Some((waker, ms)) = wait {
            let _ = tokio::time::timeout(Duration::from_millis(ms), waker.notified()).await;
        }
        let (answer, expired) = {
            let mut state = self.lock();
            let expired = Self::sweep(&mut state, now_ms());
            let request = state.requests.get_mut(id);
            let answer = request.map(|request| match request.status.as_str() {
                "claimed" => Poll::Claimed,
                "allowed" => {
                    request.status = "claimed".into();
                    Poll::Allowed(request.admit.clone().unwrap_or(json!({})))
                }
                status => Poll::Waiting {
                    status: status.to_string(),
                    expires_at: at(request.expires_at),
                },
            });
            (answer, expired)
        };
        self.settled(expired);
        answer
    }

    fn settle(
        &self,
        matches: impl Fn(&Request) -> bool,
        status: &str,
        grant: Option<Value>,
        admit: Option<Value>,
    ) -> bool {
        let settled = {
            let mut state = self.lock();
            let now = now_ms();
            Self::sweep(&mut state, now);
            let Some(request) = state.requests.values_mut().find(|r| matches(r)) else {
                return false;
            };
            if request.status != "pending" {
                return false;
            }
            request.status = status.into();
            request.decided_at = Some(now);
            request.grant = grant;
            request.admit = admit;
            let settled = request.clone();
            if let Some(waker) = state.wakers.remove(&settled.id) {
                waker.notify_waiters();
            }
            settled
        };
        (self.on_settle)(&settled);
        true
    }

    /// The requester gave up (or connected with the password). Idempotent.
    pub fn withdraw(&self, id: &str) -> Option<String> {
        self.settle(|r| r.id == id, "withdrawn", None, None);
        let state = self.lock();
        state.requests.get(id).map(|r| r.status.clone())
    }

    /// Any open request of this client is withdrawn (it got in some other way).
    pub fn withdraw_client(&self, client_id: &str) {
        while self.settle(
            |r| r.client_id == client_id && r.status == "pending",
            "withdrawn",
            None,
            None,
        ) {}
    }

    pub fn allow(&self, reference: &str, grant: Value, admit: Value) -> bool {
        self.settle(
            |r| r.reference == reference,
            "allowed",
            Some(grant),
            Some(admit),
        )
    }

    pub fn deny(&self, reference: &str) -> bool {
        self.settle(|r| r.reference == reference, "denied", None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> RequestStore {
        RequestStore::new(Box::new(|_| {}), Box::new(|_| {}))
    }

    fn ask(store: &RequestStore, client: &str, from: &str) -> Result<(Request, bool), ApiError> {
        store.create(NewRequest {
            client_id: client.into(),
            client_name: Some(json!("Laptop\u{202e}")),
            machine: None,
            same_account: false,
            verified_machine_name: None,
            via: "tailnet".into(),
            remote_address: Some(from.into()),
            tailnet_login: None,
        })
    }

    #[tokio::test]
    async fn allowed_once_then_claimed() {
        let store = store();
        let (request, created) = ask(&store, "c1", "100.64.0.2").unwrap();
        assert!(created);
        assert_eq!(request.client_name, "Laptop");
        assert!(!ask(&store, "c1", "100.64.0.2").unwrap().1);
        assert_eq!(store.pending().len(), 1);
        assert!(store.pending()[0].get("id").is_none());
        assert!(store.allow(
            &request.reference,
            json!({ "view": true }),
            json!({ "token": "t" })
        ));
        assert!(!store.deny(&request.reference));
        assert!(
            matches!(store.poll(&request.id, 0).await, Some(Poll::Allowed(admit)) if admit["token"] == "t")
        );
        assert!(matches!(
            store.poll(&request.id, 0).await,
            Some(Poll::Claimed)
        ));
        assert!(store.poll("unknown", 0).await.is_none());
    }

    #[tokio::test]
    async fn a_waiting_poll_wakes_when_decided() {
        let store = Arc::new(store());
        let (request, _) = ask(&store, "c1", "100.64.0.2").unwrap();
        let poller = {
            let store = store.clone();
            let id = request.id.clone();
            tokio::spawn(async move { store.poll(&id, 20_000).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.deny(&request.reference);
        let answer = tokio::time::timeout(Duration::from_secs(2), poller)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(answer, Some(Poll::Waiting { status, .. }) if status == "denied"));
    }

    #[test]
    fn limits_requests_from_other_accounts() {
        let store = store();
        for n in 0..MAX_PENDING_OTHER {
            ask(&store, &format!("c{n}"), &format!("100.64.0.{n}")).unwrap();
        }
        let error = ask(&store, "c9", "100.64.0.9").unwrap_err();
        assert_eq!(
            (error.status, error.code.as_deref()),
            (429, Some("rate-limited"))
        );
        assert!(error.retry_after.is_some());
    }
}
