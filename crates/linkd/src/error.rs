//! An error a request answers with: its HTTP status, message and machine-readable code, as the
//! RealTimeX and controllers read them.
//!
//! Every error body is `{error, code, retry, retryAfterMs?, delivery?, suggestion?, …details}`
//! (feature `errors-v2`, realtimex-ai-app#3055), a superset of the older `{error, code?}`:
//! - `code`: stable and kebab-case. A site that names none gets its status's ([`status_code`]),
//!   so no error goes out without one.
//! - `retry`: `safe` (repeat the same call), `after` (repeat after `retryAfterMs`, also sent as
//!   `Retry-After`), `refresh` (read the screen, displays or windows again first) or `no`.
//! - `delivery` (input calls): whether input may have landed: `none`, `unknown` (never retry it
//!   automatically) or `done`.
//! - `suggestion`: what an agent can do instead, in English (not localised).
//!
//! Each code's defaults are in [`defaults`]; a site may set its own.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// Whether and how a caller may try the same request again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Repeat the same call unchanged.
    Safe,
    /// Repeat it after `retryAfterMs`.
    After,
    /// Read the screen, displays or windows again, then build a new call.
    Refresh,
    /// Don't: change approach or ask the person.
    No,
}

impl Retry {
    pub fn as_str(self) -> &'static str {
        match self {
            Retry::Safe => "safe",
            Retry::After => "after",
            Retry::Refresh => "refresh",
            Retry::No => "no",
        }
    }
}

/// Whether a failed input call may have reached the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing happened: retrying is safe.
    None,
    /// It may have: never retry automatically.
    Unknown,
    /// It did.
    #[allow(dead_code)]
    Done,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::None => "none",
            Delivery::Unknown => "unknown",
            Delivery::Done => "done",
        }
    }
}

/// What a code means for a caller, unless the site says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Defaults {
    pub retry: Retry,
    pub retry_after_ms: Option<u64>,
    pub delivery: Option<Delivery>,
    pub suggestion: Option<&'static str>,
}

const fn plain(
    retry: Retry,
    delivery: Option<Delivery>,
    suggestion: Option<&'static str>,
) -> Defaults {
    Defaults {
        retry,
        retry_after_ms: None,
        delivery,
        suggestion,
    }
}

const fn after(ms: u64, delivery: Option<Delivery>, suggestion: Option<&'static str>) -> Defaults {
    Defaults {
        retry: Retry::After,
        retry_after_ms: Some(ms),
        delivery,
        suggestion,
    }
}

/// The code a status stands for when a site names none.
pub fn status_code(status: u16) -> &'static str {
    match status {
        401 => "invalid-token",
        403 => "forbidden",
        404 => "not-found",
        409 => "conflict",
        410 => "gone",
        413 => "too-large",
        423 => "screen-locked",
        429 => "rate-limited",
        501 => "unsupported",
        502 | 503 => "unavailable",
        504 => "timeout",
        _ if status >= 500 => "internal",
        _ => "bad-request",
    }
}

/// The retry rule, delivery and suggestion of `code` (realtimex-ai-app#3055's code table);
/// `status` decides for a code the table doesn't list.
pub fn defaults(code: &str, status: u16) -> Defaults {
    const NONE: Option<Delivery> = Some(Delivery::None);
    match code {
        "bad-request" => plain(
            Retry::No,
            NONE,
            Some("Fix the request as the error says; it had no effect."),
        ),
        "not-accepting" | "access-revoked" | "credential-reset" | "wrong-password"
        | "password-required" => plain(Retry::No, None, None),
        "token-expired" | "invalid-token" => plain(
            Retry::Safe,
            NONE,
            Some("Sign in again, then repeat the call."),
        ),
        "view-only" => plain(
            Retry::No,
            NONE,
            Some("Ask the owner for control access, or only read the screen."),
        ),
        "rate-limited" => after(
            1000,
            NONE,
            Some("Wait for retryAfterMs, then try again."),
        ),
        "screen-locked" => after(
            5000,
            NONE,
            Some("The screen is locked: wait until someone unlocks it, or ask the person at the computer."),
        ),
        "input-blocked" => plain(
            Retry::No,
            NONE,
            Some("An administrator window, a UAC prompt or a secure screen has the input: ask the person at the computer."),
        ),
        "permission-missing" => plain(
            Retry::No,
            NONE,
            Some("Ask the person at the computer to grant it to RealTimeX Link in System Settings > Privacy & Security."),
        ),
        "display-not-found" | "display-changed" => plain(
            Retry::Refresh,
            NONE,
            Some("List the displays again and use one of their ids."),
        ),
        "window-not-found" => plain(
            Retry::Refresh,
            NONE,
            Some("List the windows again: this one closed, or its id is from another listing."),
        ),
        "focus-refused" => plain(
            Retry::No,
            Some(Delivery::Unknown),
            Some("Take a screenshot to see what is in front, then click the window instead."),
        ),
        "unsupported" => plain(
            Retry::No,
            NONE,
            Some("This computer can't do that: use screenshots and pointer actions instead."),
        ),
        "unavailable" => after(
            2000,
            NONE,
            Some("Remote control is starting or restarting here; try again shortly."),
        ),
        "busy" => after(500, NONE, Some("Too many waits at once: let one finish first.")),
        // Phase 2, the accessibility layer (ax.rs).
        "stale-ref" => plain(
            Retry::Refresh,
            NONE,
            Some("Take a new snapshot (or find) of the window and use a ref from it."),
        ),
        "ambiguous-target" => plain(
            Retry::Refresh,
            NONE,
            Some("Several elements match: narrow role, name or text, or pick a ref from candidates."),
        ),
        "no-tree" => plain(
            Retry::No,
            NONE,
            Some("This window has no accessibility tree here: use a screenshot and pixel clicks."),
        ),
        "needs-pixels" => plain(
            Retry::No,
            NONE,
            Some("Click the centre of `bounds` with click --native --display, or use a screenshot and pixels."),
        ),
        "app-unresponsive" => after(500, NONE, Some("The app isn't answering yet: try again shortly.")),
        "ax-failed" => plain(
            Retry::No,
            None,
            Some("Take a new snapshot to see the element's state before trying anything else."),
        ),
        "timeout" => plain(Retry::Safe, NONE, None),
        "internal" => plain(Retry::No, Some(Delivery::Unknown), None),
        _ => match status {
            429 => after(1000, None, None),
            502 | 503 => after(2000, None, None),
            504 => plain(Retry::Safe, None, None),
            _ => plain(Retry::No, None, None),
        },
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// The code this site named; None takes its status's ([`ApiError::code_or_status`]). The
    /// live view's DataChannel answers with this one only (unchanged by errors-v2).
    pub code: Option<String>,
    /// `Retry-After` in seconds (and `retryAfterMs`, unless that is set itself).
    pub retry_after: Option<u64>,
    /// This site's own retry rule, delivery and suggestion; None: the code's [`defaults`].
    pub retry: Option<Retry>,
    pub retry_after_ms: Option<u64>,
    pub delivery: Option<Delivery>,
    pub suggestion: Option<String>,
    /// More fields for the answer's body (a typed refusal's particulars).
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            retry_after: None,
            retry: None,
            retry_after_ms: None,
            delivery: None,
            suggestion: None,
            details: None,
        }
    }

    pub fn coded(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.to_string()),
            ..Self::new(status, message)
        }
    }

    /// 400 `bad-request` (from its status: the DataChannel's answers stay as they were).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(500, error.to_string())
    }

    /// The code the body carries: this site's, else its status's.
    pub fn code_or_status(&self) -> String {
        self.code
            .clone()
            .unwrap_or_else(|| status_code(self.status).to_string())
    }

    pub fn retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = Some(retry);
        self
    }

    pub fn with_delivery(mut self, delivery: Delivery) -> Self {
        self.delivery = Some(delivery);
        self
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    /// Add these fields (an object) to the answer's body.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The body's own fields, details aside: `{error, code, retry, retryAfterMs?, delivery?,
    /// suggestion?}`.
    pub fn body(&self) -> Value {
        let code = self.code_or_status();
        let defaults = defaults(&code, self.status);
        let retry = self.retry.unwrap_or(defaults.retry);
        let mut body = json!({ "error": self.message, "code": code, "retry": retry.as_str() });
        if retry == Retry::After {
            let ms = self
                .retry_after_ms
                .or(self.retry_after.map(|seconds| seconds * 1000))
                .or(defaults.retry_after_ms);
            if let Some(ms) = ms {
                body["retryAfterMs"] = json!(ms);
            }
        }
        if let Some(delivery) = self.delivery.or(defaults.delivery) {
            body["delivery"] = json!(delivery.as_str());
        }
        if let Some(suggestion) = self.suggestion.as_deref().or(defaults.suggestion) {
            body["suggestion"] = json!(suggestion);
        }
        body
    }

    /// `Retry-After` in seconds: the site's own, else `retryAfterMs` rounded up.
    fn retry_after_header(&self, body: &Value) -> Option<u64> {
        self.retry_after.or_else(|| {
            body.get("retryAfterMs")
                .and_then(Value::as_u64)
                .map(|ms| ms.div_ceil(1000).max(1))
        })
    }
}

/// The errors-v2 fields for a body built elsewhere as `{error, code?}` (a sign-in's refusal):
/// `code` from the status when it has none, `retry`, and the code's `retryAfterMs` (from
/// `retry_after` seconds when given), delivery and suggestion, each where the body has none.
pub fn complete_body(status: u16, body: &mut Value, retry_after: Option<u64>) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    let code = fields
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| status_code(status).to_string());
    let mut error = ApiError::coded(status, &code, "");
    error.retry_after = retry_after;
    if let Value::Object(more) = error.body() {
        for (name, value) in more {
            if name != "error" {
                fields.entry(name).or_insert(value);
            }
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<ApiError>() {
            Ok(api) => api,
            Err(other) => ApiError::internal(other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status >= 500 {
            eprintln!("[remote-control] {}", self.message);
        }
        let mut body = self.body();
        if let (Some(serde_json::Value::Object(details)), Some(fields)) =
            (&self.details, body.as_object_mut())
        {
            for (name, value) in details {
                fields.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let retry_after = self.retry_after_header(&body);
        let mut response = (status, Json(body)).into_response();
        if let Some(seconds) = retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert("Retry-After", value);
            }
        }
        response
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    async fn rendered(error: ApiError) -> (u16, Option<String>, Value) {
        let response = error.into_response();
        let status = response.status().as_u16();
        let header = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, header, serde_json::from_slice(&bytes).unwrap())
    }

    /// Each constructor, and every error status a site can name.
    fn every_constructor() -> Vec<ApiError> {
        let mut all = vec![
            ApiError::bad_request("no"),
            ApiError::internal("boom"),
            ApiError::coded(409, "request-settled", "settled"),
            ApiError::from(anyhow::anyhow!("plain")),
        ];
        for status in 400..=599 {
            all.push(ApiError::new(status, "x"));
        }
        all
    }

    #[test]
    fn every_api_error_has_a_code_and_a_retry_rule() {
        for error in every_constructor() {
            let body = error.body();
            let code = body["code"].as_str().unwrap_or("");
            assert!(!code.is_empty(), "{} has no code", error.status);
            assert!(
                code.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{code} is kebab-case"
            );
            assert!(
                matches!(
                    body["retry"].as_str(),
                    Some("safe" | "after" | "refresh" | "no")
                ),
                "{body}"
            );
            if body["retry"] == "after" {
                assert!(body["retryAfterMs"].as_u64().is_some(), "{body}");
            }
        }
    }

    #[test]
    fn statuses_name_the_codes_of_the_table() {
        assert_eq!(ApiError::bad_request("x").code_or_status(), "bad-request");
        assert_eq!(ApiError::new(400, "x").code_or_status(), "bad-request");
        assert_eq!(ApiError::new(501, "x").code_or_status(), "unsupported");
        assert_eq!(ApiError::new(502, "x").code_or_status(), "unavailable");
        assert_eq!(ApiError::new(503, "x").code_or_status(), "unavailable");
        assert_eq!(ApiError::new(504, "x").code_or_status(), "timeout");
        assert_eq!(ApiError::internal("x").code_or_status(), "internal");
        // A site's own code wins.
        assert_eq!(
            ApiError::coded(404, "window-not-found", "x").code_or_status(),
            "window-not-found"
        );
        // The DataChannel's code stays the site's own.
        assert_eq!(ApiError::bad_request("x").code, None);
    }

    #[test]
    fn the_code_table_drives_retry_delivery_and_suggestion() {
        let body = ApiError::coded(403, "view-only", "x").body();
        assert_eq!(body["retry"], "no");
        assert_eq!(body["delivery"], "none");
        assert!(body["suggestion"]
            .as_str()
            .unwrap()
            .contains("control access"));

        let body = ApiError::coded(423, "screen-locked", "x").body();
        assert_eq!(
            (body["retry"].as_str(), body["retryAfterMs"].as_u64()),
            (Some("after"), Some(5000))
        );
        let body = ApiError::new(502, "helper exited").body();
        assert_eq!(
            (body["retry"].as_str(), body["retryAfterMs"].as_u64()),
            (Some("after"), Some(2000))
        );
        assert_eq!(
            ApiError::coded(404, "display-not-found", "x").body()["retry"],
            "refresh"
        );
        assert_eq!(
            ApiError::coded(409, "focus-refused", "x").body()["delivery"],
            "unknown"
        );
        assert_eq!(ApiError::internal("x").body()["delivery"], "unknown");
        // Timeouts are safe to repeat for reads; an input call says otherwise itself.
        let read = ApiError::new(504, "x").body();
        assert_eq!(
            (read["retry"].as_str(), read["delivery"].as_str()),
            (Some("safe"), Some("none"))
        );
        let input = ApiError::new(504, "x")
            .with_retry(Retry::No)
            .with_delivery(Delivery::Unknown)
            .body();
        assert_eq!(
            (input["retry"].as_str(), input["delivery"].as_str()),
            (Some("no"), Some("unknown"))
        );
        // A code the table doesn't list: nothing invented beyond the retry rule.
        let body = ApiError::coded(409, "request-settled", "x").body();
        assert_eq!(body["retry"], "no");
        assert!(body.get("delivery").is_none() && body.get("suggestion").is_none());
    }

    #[tokio::test]
    async fn a_body_keeps_the_old_fields_and_adds_the_new_ones() {
        let (status, header, body) = rendered(
            ApiError::coded(404, "display-not-found", "Display 7 is not connected")
                .with_details(json!({ "display": "7", "code": "ignored" })),
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(header, None);
        assert_eq!(body["error"], "Display 7 is not connected");
        assert_eq!(body["code"], "display-not-found");
        assert_eq!(body["display"], "7");
        assert_eq!(body["retry"], "refresh");

        // Retry-After goes with retryAfterMs, rounded up to whole seconds.
        let (_, header, body) = rendered(ApiError::coded(423, "screen-locked", "locked")).await;
        assert_eq!(header.as_deref(), Some("5"));
        assert_eq!(body["retryAfterMs"], 5000);
        let (_, header, body) =
            rendered(ApiError::coded(429, "rate-limited", "slow").retry_after(7)).await;
        assert_eq!(header.as_deref(), Some("7"));
        assert_eq!(body["retryAfterMs"], 7000);
        let (_, header, _) = rendered(ApiError::coded(503, "busy", "busy")).await;
        assert_eq!(header.as_deref(), Some("1"));
    }

    #[test]
    fn a_sign_in_refusal_gains_the_new_fields() {
        let mut body = json!({ "error": "Not accepting", "code": "not-accepting" });
        complete_body(403, &mut body, None);
        assert_eq!(body["error"], "Not accepting");
        assert_eq!(body["code"], "not-accepting");
        assert_eq!(body["retry"], "no");
        let mut body = json!({ "error": "Too many failed attempts" });
        complete_body(429, &mut body, Some(3));
        assert_eq!(body["code"], "rate-limited");
        assert_eq!(
            (body["retry"].as_str(), body["retryAfterMs"].as_u64()),
            (Some("after"), Some(3000))
        );
    }
}
