//! "Allow my own computers" (ADR-2611 §2, Marketplace contract realtimex-marketplace-api#35):
//! a controller signed in to the same RealTimeX account sends a single-use identity token with
//! its connect request (`identity`), minted by the Marketplace for this computer's App ID. It is
//! verified offline against the Marketplace's published keys: EdDSA (Ed25519), type
//! `rtx-rc-identity+jwt`, issuer the API base, audience `rtx-remote-control:<App ID>`, within its
//! validity, issued after this daemon started, subject this computer's account, each `jti` once.
//!
//! This computer's account is the one RealTimeX handed over (POST /v1/account/token, its `sub`),
//! kept in the store so a restart without RealTimeX still knows it. The keys are kept there too,
//! refreshed at most hourly and at most once per 5 minutes for an unknown key ID. Any failure
//! means "not the same account", never an error.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::store::Store;

pub mod keys {
    /// { subject, email }: this computer's RealTimeX account.
    pub const ACCOUNT: &str = "account";
    /// { keys: [JWK], fetchedAt }: the Marketplace's identity keys, last good set.
    pub const IDENTITY_KEYS: &str = "identity-keys";
}

pub const TOKEN_TYPE: &str = "rtx-rc-identity+jwt";
pub const AUDIENCE_PREFIX: &str = "rtx-remote-control:";
const SKEW_S: i64 = 60;
const KEYS_REFRESH_MS: i64 = 60 * 60 * 1000;
const UNKNOWN_KID_REFETCH_MS: i64 = 5 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// A verified same-account controller.
pub struct SameAccount {
    /// The machine name the Marketplace vouches for (its install is verified), if any.
    pub verified_machine_name: Option<String>,
}

pub struct IdentityVerifier {
    started_ms: i64,
    last_unknown_kid_fetch: Mutex<i64>,
    /// jti -> forget after (ms).
    seen: Mutex<HashMap<String, i64>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn b64url(part: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .ok()
}

fn json_part(part: &str) -> Option<Value> {
    serde_json::from_slice(&b64url(part)?).ok()
}

/// A JWT's claims, unchecked (the token came straight from the account's sign-in).
pub fn claims_of(token: &str) -> Option<Value> {
    json_part(token.split('.').nth(1)?)
}

/// The `sub` and `email` of a JWT, unchecked: the token RealTimeX handed over names the account.
pub fn account_of(token: &str) -> Option<Value> {
    let claims = claims_of(token)?;
    let subject = claims.get("sub")?.as_str().filter(|s| !s.is_empty())?;
    Some(json!({ "subject": subject, "email": claims.get("email") }))
}

impl IdentityVerifier {
    pub fn new() -> Self {
        Self {
            started_ms: now_ms(),
            last_unknown_kid_fetch: Mutex::new(0),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// The token's verdict for `app_id`, given this computer's account subject and the API base.
    pub async fn verify(
        &self,
        store: &Store,
        token: &str,
        app_id: &str,
        api_base: &str,
    ) -> Option<SameAccount> {
        let subject = store
            .get(keys::ACCOUNT)?
            .get("subject")?
            .as_str()?
            .to_string();
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 || app_id.is_empty() {
            return None;
        }
        let header = json_part(parts[0])?;
        let kid = header.get("kid")?.as_str()?.to_string();
        if header.get("typ").and_then(Value::as_str) != Some(TOKEN_TYPE)
            || header.get("crit").is_some()
            || header.get("alg").and_then(Value::as_str) != Some("EdDSA")
        {
            return None;
        }
        let jwk = self.key_for(store, &kid, api_base).await?;
        if jwk.get("kty").and_then(Value::as_str) != Some("OKP")
            || jwk.get("crv").and_then(Value::as_str) != Some("Ed25519")
        {
            return None;
        }
        let public = b64url(jwk.get("x")?.as_str()?)?;
        let signature = b64url(parts[2])?;
        let signed = format!("{}.{}", parts[0], parts[1]);
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public)
            .verify(signed.as_bytes(), &signature)
            .ok()?;
        let claims = json_part(parts[1])?;
        let number = |name: &str| claims.get(name).and_then(Value::as_i64);
        let now_s = now_ms() / 1000;
        let (nbf, exp, iat) = (number("nbf")?, number("exp")?, number("iat")?);
        if claims.get("iss").and_then(Value::as_str) != Some(api_base)
            || claims.get("aud").and_then(Value::as_str)
                != Some(format!("{AUDIENCE_PREFIX}{app_id}").as_str())
            || !(nbf - SKEW_S <= now_s && now_s < exp + SKEW_S)
            || iat < self.started_ms / 1000 - SKEW_S
            || claims.get("sub").and_then(Value::as_str) != Some(subject.as_str())
        {
            return None;
        }
        let jti = claims.get("jti").and_then(Value::as_str)?;
        if !self.first_use(jti, exp) {
            return None;
        }
        let install = claims.get("rtx_install");
        let verified_machine_name = install
            .filter(|i| i.get("installVerified") == Some(&Value::Bool(true)))
            .and_then(|i| i.get("machineName"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        Some(SameAccount {
            verified_machine_name,
        })
    }

    fn first_use(&self, jti: &str, exp_s: i64) -> bool {
        let now = now_ms();
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.retain(|_, until| *until > now);
        if seen.contains_key(jti) {
            return false;
        }
        seen.insert(jti.to_string(), (exp_s + SKEW_S) * 1000);
        true
    }

    async fn key_for(&self, store: &Store, kid: &str, api_base: &str) -> Option<Value> {
        let find = |set: &Value| {
            set.get("keys")?
                .as_array()?
                .iter()
                .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid))
                .cloned()
        };
        let kept = store.get(keys::IDENTITY_KEYS).unwrap_or(json!({}));
        let fetched_at = kept.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0);
        let stale = now_ms() - fetched_at >= KEYS_REFRESH_MS;
        if !stale {
            if let Some(key) = find(&kept) {
                return Some(key);
            }
        }
        // Stale, or an unknown key ID (rotated keys): fetch, at most every 5 minutes for the latter.
        if !stale {
            let mut last = self
                .last_unknown_kid_fetch
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if now_ms() - *last < UNKNOWN_KID_REFETCH_MS {
                return None;
            }
            *last = now_ms();
        }
        match fetch_keys(api_base).await {
            Some(set) => {
                let _ = store.set(keys::IDENTITY_KEYS, &set);
                find(&set)
            }
            // The last good set when the Marketplace can't be reached.
            None => find(&kept),
        }
    }
}

/// GET <api base>/v1/remote-control/identity-keys: { keys: [JWK] }.
async fn fetch_keys(api_base: &str) -> Option<Value> {
    if !api_base.starts_with("https://") {
        return None;
    }
    let url = format!("{api_base}/v1/remote-control/identity-keys");
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .build()
            .into();
        let answer: Value = agent.get(&url).call().ok()?.body_mut().read_json().ok()?;
        answer.get("keys").filter(|k| k.is_array())?;
        Some(json!({ "keys": answer["keys"], "fetchedAt": now_ms() }))
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn enc(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    struct Fixture {
        store: Store,
        pair: Ed25519KeyPair,
        _dir: tempfile::TempDir,
    }

    const API: &str = "https://api.example";

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        store
            .set(keys::ACCOUNT, &json!({ "subject": "user-1" }))
            .unwrap();
        store
            .set(
                keys::IDENTITY_KEYS,
                &json!({ "keys": [{ "kid": "k1", "kty": "OKP", "crv": "Ed25519", "x": enc(pair.public_key().as_ref()) }], "fetchedAt": now_ms() }),
            )
            .unwrap();
        Fixture {
            store,
            pair,
            _dir: dir,
        }
    }

    fn token(pair: &Ed25519KeyPair, claims: Value) -> String {
        let header = enc(json!({ "alg": "EdDSA", "typ": TOKEN_TYPE, "kid": "k1" })
            .to_string()
            .as_bytes());
        let body = enc(claims.to_string().as_bytes());
        let signature = pair.sign(format!("{header}.{body}").as_bytes());
        format!("{header}.{body}.{}", enc(signature.as_ref()))
    }

    fn claims(jti: &str) -> Value {
        let now = now_ms() / 1000;
        json!({
            "iss": API, "aud": "rtx-remote-control:APP", "sub": "user-1", "jti": jti,
            "nbf": now - 5, "iat": now, "exp": now + 60,
            "rtx_install": { "installVerified": true, "machineName": "Mac" },
        })
    }

    #[tokio::test]
    async fn same_account_tokens_pass_once() {
        let f = fixture();
        let verifier = IdentityVerifier::new();
        let good = token(&f.pair, claims("j1"));
        let same = verifier.verify(&f.store, &good, "APP", API).await.unwrap();
        assert_eq!(same.verified_machine_name.as_deref(), Some("Mac"));
        assert!(
            verifier.verify(&f.store, &good, "APP", API).await.is_none(),
            "replayed"
        );
    }

    #[tokio::test]
    async fn anything_else_is_not_the_same_account() {
        let f = fixture();
        let verifier = IdentityVerifier::new();
        let mut other = claims("j2");
        other["sub"] = json!("user-2");
        assert!(verifier
            .verify(&f.store, &token(&f.pair, other), "APP", API)
            .await
            .is_none());
        assert!(verifier
            .verify(&f.store, &token(&f.pair, claims("j3")), "OTHER", API)
            .await
            .is_none());
        let mut expired = claims("j4");
        expired["exp"] = json!(now_ms() / 1000 - 300);
        assert!(verifier
            .verify(&f.store, &token(&f.pair, expired), "APP", API)
            .await
            .is_none());
        let mut issuer = claims("j5");
        issuer["iss"] = json!("https://evil.example");
        assert!(verifier
            .verify(&f.store, &token(&f.pair, issuer), "APP", API)
            .await
            .is_none());
        let mut forged = token(&f.pair, claims("j6"));
        forged.push('A');
        assert!(verifier
            .verify(&f.store, &forged, "APP", API)
            .await
            .is_none());
        f.store.set(keys::ACCOUNT, &json!({})).unwrap();
        assert!(verifier
            .verify(&f.store, &token(&f.pair, claims("j7")), "APP", API)
            .await
            .is_none());
    }

    #[test]
    fn the_account_comes_from_the_handed_token() {
        let body = enc(json!({ "sub": "user-1", "email": "a@b.c" })
            .to_string()
            .as_bytes());
        assert_eq!(
            account_of(&format!("h.{body}.s")).unwrap()["subject"],
            "user-1"
        );
        assert!(account_of("opaque").is_none());
    }
}
