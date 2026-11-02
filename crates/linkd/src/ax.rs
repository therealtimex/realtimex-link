//! The accessibility layer (realtimex-ai-app#3055 Phase 2, §2.3–2.5 and Addendum 2): apps,
//! snapshot, find, get, act and batch on top of the pinned agent-desktop CLI, which rtx-control
//! runs (`ax` op, crates/control/src/ax.rs). Served to remote clients under
//! `/remote-control/ax/*` (router.rs) and to agents on this computer under `/v1/ax/*`
//! (local_api.rs, with `X-RTX-Caller`); the handlers, bodies, results and codes are the same.
//!
//! - **Our contract, not agent-desktop's:** nodes are normalised here (`ref, role, name, value?,
//!   states, secret?, bounds?, actions, children?`), so an upstream bump can't change what
//!   clients read. Bounds are in that display's native pixels, as window frames are.
//! - **Windows by Link's ids:** `window` is an id from Link's own list (`m:`, `w:`, `x:`, and
//!   `a:` on GNOME Wayland, where only agent-desktop lists windows). Minimized windows aren't in
//!   it on macOS and Windows.
//! - **Refs are owned:** each ref is recorded for 15 minutes with the client (or the local caller)
//!   it was given to, its window and what the element allows; anyone else gets `stale-ref`.
//! - **Passwords:** an element marked `secure` never has its value returned, by any route
//!   (`secret: true`, `value: null` for get). Filling one is allowed; nothing logs values.
//! - **Guards:** `type` falls back to `set-value`, then to a click and paste, and stops at once
//!   when a step may have landed (§2.4.3); no-tree and needs-pixels say what to do instead.
//! - **One at a time:** one agent-desktop per rtx-control, at most 4 calls queued for at most 5 s
//!   (else `busy`).
//! - **Activity:** reads are `ax-read` rows (one per client and window per minute), each act an
//!   `ax-act` row with app, action, role and how it was done; never a name, value or title.
//!
//! The honest limit for agents on this computer (Addendum 2, Fact 3): a local agent runs as the
//! same OS user. Link holds no per-app policy; any allow-list is a guard rail with full
//! visibility in the Activity log, not containment.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::audit::{Audit, Outcome, Who};
use crate::backend::Backend;
use crate::error::{ApiError, ApiResult, Delivery, Retry};
use crate::winid::WindowId;

/// Which platforms serve `ax` (QA can switch one off here): the feature is listed, and the
/// routes answer, only where this says so and the CLI is bundled. `RTX_LINK_AX=0|1` overrides it
/// on one computer (tests and QA).
pub const PLATFORMS: [(&str, bool); 3] = [("macos", true), ("windows", true), ("linux", true)];

/// How long a ref is kept for its owner.
pub const REF_TTL: Duration = Duration::from_secs(15 * 60);
/// Refs kept per owner at most (the oldest go first).
const MAX_REFS: usize = 20_000;
const MAX_TEXT: usize = 4096;
/// The longest text agent-desktop 1.0.2 reads whole (`MAX_OBSERVATION_FIELD_BYTES`, Linux
/// `DEFAULT_MAX_FIELD_BYTES`: 64 KiB). A longer value comes back unknown, or may be cut, so the
/// `type` fallback never rewrites a field it can't read whole.
const WHOLE_TEXT: usize = 64 * 1024;
const MAX_STEPS: usize = 10;
const MAX_QUEUED: usize = 4;
const QUEUE_WAIT: Duration = Duration::from_secs(5);
/// A batch's deadline ([`batch_timeout`]): 6 s a step, at least 20 s, at most 60 s. The app's
/// client waits as long (realtimex-ai-app#3055, QA: a find-then-click takes ~5 s on Windows).
const BATCH_PER_STEP: Duration = Duration::from_secs(6);
const BATCH_MIN: Duration = Duration::from_secs(20);
const BATCH_MAX: Duration = Duration::from_secs(60);
/// The tree's depth when the client names none: agent-desktop's most. At 30, web pages in Edge
/// lost their form fields (QA of #3055); a deeper walk stays within the snapshot's 10 s.
const DEPTH_DEFAULT: u64 = 50;
/// The client may ask for 1–60 (§2.4.2); agent-desktop 1.0.2 takes at most 50.
const DEPTH_CLI_MAX: u64 = 50;
const FIND_LIMIT_DEFAULT: u64 = 20;
const FIND_LIMIT_MAX: u64 = 50;
const CANDIDATES: usize = 5;
/// The CLI's own deadlines (crates/control/src/ax.rs caps them the same way).
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(8);
const ACT_TIMEOUT: Duration = Duration::from_secs(8);
/// How much longer than the CLI's own deadline rtx-linkd waits for rtx-control's answer (§2.2).
const ANSWER_GRACE: Duration = Duration::from_secs(2);
/// rtx-control's version check, run before a call when it is due, has this budget of its own
/// (crates/control/src/ax.rs `VERSION_CHECK`), so it never eats into the call's deadline.
const VERSION_CHECK: Duration = Duration::from_secs(5);

/// How long a call may still take: a batch's time ([`batch_timeout`]) is a deadline, and each
/// step's CLI call is capped by what is left of it (§2.3). Single calls have none.
#[derive(Clone, Copy, Debug, Default)]
pub struct Budget {
    deadline: Option<Instant>,
}

impl Budget {
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
        }
    }

    /// `timeout` capped by what is left; None when nothing is.
    pub fn cap(&self, timeout: Duration) -> Option<Duration> {
        match self.deadline {
            None => Some(timeout),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                (!left.is_zero()).then(|| timeout.min(left))
            }
        }
    }
}

/// The batch's time is over before this call could start: nothing was sent.
fn out_of_time() -> ApiError {
    ApiError::coded(504, "timeout", "The batch ran out of time").with_delivery(Delivery::None)
}

/// A failure of the runner itself (not one agent-desktop answered with), as a read or an action
/// sees it (§1.1, §2.2 "Crashes"):
/// - `ax-crashed` (it ended without an answer) is `unavailable` for a read, and `ax-failed` with
///   `delivery:"unknown"` for an action, which may have happened before it ended;
/// - `timeout` of an action is `retry:"no"`, `delivery:"unknown"`, so the agent never types twice.
fn runner_failure(error: ApiError, acting: bool) -> ApiError {
    match (error.code_or_status().as_str(), acting) {
        ("ax-crashed", false) => {
            let mut unavailable = ApiError::coded(503, "unavailable", "agent-desktop ended without an answer");
            unavailable.retry_after_ms = error.retry_after_ms;
            unavailable
        }
        ("ax-crashed", true) => ApiError::coded(409, "ax-failed", "agent-desktop ended without an answer; the action may have happened")
            .with_delivery(Delivery::Unknown)
            .with_retry(Retry::No)
            .with_suggestion("Read the element's state with get or a new snapshot before trying anything else."),
        ("timeout", true) => ApiError {
            code: Some("timeout".into()),
            ..error
        }
        .with_delivery(Delivery::Unknown)
        .with_retry(Retry::No)
        .with_suggestion("The action may have happened: read the element's state with get or a new snapshot before trying again."),
        _ => error,
    }
}

/// A reading CLI call within `budget`.
async fn read_cli<D: Desktop>(desktop: &D, argv: Vec<String>, timeout: Duration, budget: Budget) -> ApiResult<Value> {
    let timeout = budget.cap(timeout).ok_or_else(out_of_time)?;
    desktop.cli(argv, timeout).await.map_err(|error| runner_failure(error, false))
}

/// An acting CLI call within `budget`.
async fn act_cli<D: Desktop>(desktop: &D, argv: Vec<String>, timeout: Duration, budget: Budget) -> ApiResult<Value> {
    let timeout = budget.cap(timeout).ok_or_else(out_of_time)?;
    desktop.cli(argv, timeout).await.map_err(|error| runner_failure(error, true))
}

/// The actions an element may be asked for, exactly (§2.3).
pub const ACTIONS: [&str; 8] = [
    "click",
    "set-value",
    "type",
    "toggle",
    "select",
    "expand",
    "collapse",
    "scroll-to",
];

/// Whether this build serves `ax` here: the platform's switch, and the CLI next to rtx-linkd.
pub fn enabled() -> bool {
    match std::env::var("RTX_LINK_AX").as_deref() {
        Ok("0") => return false,
        Ok("1") => return true,
        _ => {}
    }
    let os = std::env::consts::OS;
    PLATFORMS.iter().any(|(name, on)| *name == os && *on) && bundled()
}

fn bundled() -> bool {
    if std::env::var_os("RTX_AGENT_DESKTOP_BIN").is_some_and(|p| !p.is_empty()) {
        return true;
    }
    let name = if cfg!(windows) {
        "agent-desktop.exe"
    } else {
        "agent-desktop"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .is_some_and(|path| path.is_file())
}

/// Who is asking: whose refs, for the Activity log, and whether it may act.
#[derive(Clone, Debug)]
pub struct Caller {
    pub owner: String,
    pub who: Who,
    pub control: bool,
    /// An agent on this computer (`/v1/ax/*`): no pixel fallback (it would move the person's own
    /// pointer, A3), and its own wording for a missing permission.
    pub local: bool,
}

impl Caller {
    pub fn remote(client: &crate::clients::Authenticated, via: &str) -> Self {
        Self {
            owner: client.id().to_string(),
            who: Who::client(client, via),
            control: client.may("control"),
            local: false,
        }
    }

    /// `X-RTX-Caller: {"id":"<workspaceId>:<threadId>","name":"<thread name>"}`, required on
    /// every `/v1/ax/*` call (≤ 512 bytes): refs are owned per caller id.
    pub fn local(header: Option<&str>) -> ApiResult<Self> {
        let refused = || {
            ApiError::bad_request(
                "X-RTX-Caller is required: {\"id\":\"<workspaceId>:<threadId>\",\"name\":\"<thread name>\"}",
            )
        };
        let text = header.filter(|h| !h.is_empty() && h.len() <= 512).ok_or_else(refused)?;
        let value: Value = serde_json::from_str(text).map_err(|_| refused())?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(refused)?;
        let name = value.get("name").and_then(Value::as_str).unwrap_or("").trim();
        Ok(Self {
            owner: format!("local:{id}"),
            who: Who::local(id, name),
            control: true,
            local: true,
        })
    }
}

// --- agent-desktop's answers --------------------------------------------------------------

/// An agent-desktop failure as it said it: its code, delivery and retry.
#[derive(Clone, Debug, PartialEq)]
pub struct CliError {
    pub code: String,
    pub message: String,
    pub delivery: Option<String>,
    pub retry: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub details: Value,
}

impl CliError {
    fn of(envelope: &Value) -> Self {
        let error = &envelope["error"];
        let text = |v: &Value| v.as_str().map(str::to_string);
        Self {
            code: text(&error["code"]).unwrap_or_else(|| "INTERNAL".into()),
            message: text(&error["message"]).unwrap_or_else(|| "agent-desktop failed".into()),
            delivery: text(&error["disposition"]["delivery"]),
            retry: text(&error["disposition"]["retry"]),
            retry_after_ms: error["recovery"]["retry_after_ms"].as_u64(),
            details: error["details"].clone(),
        }
    }

    /// Input may have reached the app.
    fn may_have_landed(&self) -> bool {
        !matches!(self.delivery.as_deref(), None | Some("not_delivered"))
    }
}

/// The data of a successful envelope, or its error.
pub fn data(envelope: &Value) -> Result<Value, CliError> {
    if envelope.get("ok") == Some(&Value::Bool(true)) {
        Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(CliError::of(envelope))
    }
}

/// Delivery as Link says it: `none`, `done`, or `unknown`.
fn delivery_of(delivery: Option<&str>) -> Delivery {
    match delivery {
        None | Some("not_delivered") => Delivery::None,
        Some("delivered_verified") => Delivery::Done,
        _ => Delivery::Unknown,
    }
}

/// agent-desktop's code as Link's (§2.4.6); retry and delivery from its disposition, made
/// stricter where the two disagree. `bounds`/`display` ride along for needs-pixels.
pub fn map_error(error: &CliError, acting: bool, bounds: Option<&Value>) -> ApiError {
    let delivery = if acting {
        delivery_of(error.delivery.as_deref())
    } else {
        Delivery::None
    };
    let (status, code) = match error.code.as_str() {
        "STALE_REF" | "SNAPSHOT_NOT_FOUND" => (409, "stale-ref"),
        "WINDOW_NOT_FOUND" => (404, "window-not-found"),
        "AMBIGUOUS_TARGET" => (409, "ambiguous-target"),
        "APP_UNRESPONSIVE" => (503, "app-unresponsive"),
        "TIMEOUT" => (504, "timeout"),
        "PERM_DENIED" => (409, "permission-missing"),
        "PLATFORM_NOT_SUPPORTED" => (501, "unsupported"),
        // No semantic way to do it: the agent clicks the element's centre instead (§2.4.5).
        "POLICY_DENIED" | "ACTION_NOT_SUPPORTED" => (422, "needs-pixels"),
        _ => (409, "ax-failed"),
    };
    let mut api = ApiError::coded(status, code, error.message.clone());
    match code {
        "app-unresponsive" => {
            api.retry = Some(Retry::After);
            api.retry_after_ms = Some(error.retry_after_ms.unwrap_or(500));
        }
        "timeout" => {
            api.retry = Some(if acting { Retry::No } else { Retry::Safe });
        }
        _ => {}
    }
    // An action that timed out may have happened, whatever agent-desktop's delivery says (§1.1).
    let delivery = if acting && code == "timeout" {
        Delivery::Unknown
    } else {
        delivery
    };
    if code == "ax-failed" {
        api.retry = Some(if delivery == Delivery::None {
            Retry::Safe
        } else {
            Retry::No
        });
    }
    if acting {
        api.delivery = Some(delivery);
        // Never retried automatically when it may have landed, whatever the code says.
        if delivery != Delivery::None || error.retry.as_deref() == Some("unsafe") {
            api.retry = Some(Retry::No);
        }
    }
    if code == "needs-pixels" {
        if let Some(bounds) = bounds {
            api.details = Some(json!({ "bounds": bounds, "display": bounds.get("display") }));
        }
    }
    api
}

// --- the node contract ----------------------------------------------------------------------

/// What Link remembers about a ref it gave out.
#[derive(Clone, Debug, PartialEq)]
pub struct RefEntry {
    pub owner: String,
    /// Link's window id, and agent-desktop's.
    pub window: String,
    pub cli_window: String,
    pub app: String,
    pub role: String,
    /// The allow-listed actions it advertised; None when unknown (from find).
    pub actions: Option<Vec<String>>,
    pub secret: bool,
    pub bounds: Option<Value>,
    pub at: Instant,
}

/// agent-desktop's action names as ours.
fn action_of(name: &str) -> Option<&'static str> {
    let plain: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    Some(match plain.as_str() {
        "click" | "press" | "invoke" => "click",
        "setvalue" => "set-value",
        "typetext" | "type" => "type",
        "toggle" => "toggle",
        "select" => "select",
        "expand" => "expand",
        "collapse" => "collapse",
        "scrollto" | "scrollintoview" => "scroll-to",
        _ => return None,
    })
}

fn truncated(text: &str) -> (String, bool) {
    if text.len() <= MAX_TEXT {
        return (text.to_string(), false);
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Roles that are password fields by themselves. agent-desktop 1.0.2 reports these as
/// `textfield` plus the `secure` state; a raw role is honoured too, should one ever come through.
const SECRET_ROLES: [&str; 4] = ["passwordtext", "securetextfield", "axsecuretextfield", "password"];

/// A password field, by any reliable signal agent-desktop 1.0.2 gives (§2.5, G2): the `secure`
/// state, which its state vocabulary (`core/src/tree/state.rs`) sets from the platform's own
/// marker:
/// - macOS: role or subrole `AXSecureTextField` (`macos/src/tree/query/node_read.rs`);
/// - Windows: UI Automation `IsPassword` (`windows/src/tree/states.rs`);
/// - Linux: AT-SPI role `password text`, number 40 (`linux/src/tree/vocabulary/states.rs`).
///
/// Or a password role, should one come through raw. Never a guess from the name.
fn is_secret(raw: &Value) -> bool {
    let secure_state = raw
        .get("states")
        .and_then(Value::as_array)
        .is_some_and(|states| states.iter().any(|s| s.as_str() == Some("secure")));
    let secret_role = |key: &str| {
        raw.get(key).and_then(Value::as_str).is_some_and(|role| {
            let plain: String = role
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            SECRET_ROLES.contains(&plain.as_str())
        })
    };
    secure_state || secret_role("role") || secret_role("subrole")
}

/// A frame in global coordinates (points on macOS, pixels elsewhere) as `{display, x, y, width,
/// height}` in the native pixels of the display holding most of it (as rtx-control places
/// window frames).
pub fn place(frame: [f64; 4], displays: &[Value], points: bool) -> Option<Value> {
    let [x, y, width, height] = frame;
    let number = |d: &Value, k: &str| d.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    displays
        .iter()
        .filter_map(|display| {
            let scale = number(display, "scale");
            let scale = if points && scale > 0.0 { scale } else { 1.0 };
            let (dx, dy) = (number(display, "x"), number(display, "y"));
            let (dw, dh) = (number(display, "width") / scale, number(display, "height") / scale);
            let overlap =
                |a: f64, size: f64, b: f64, other: f64| ((a + size).min(b + other) - a.max(b)).max(0.0);
            let area = overlap(x, width, dx, dw) * overlap(y, height, dy, dh);
            (area > 0.0).then_some((area, display, scale, dx, dy))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, display, scale, dx, dy)| {
            let pixels = |value: f64| (value * scale).round() as i64;
            json!({
                "display": display.get("id"),
                "x": pixels(x - dx),
                "y": pixels(y - dy),
                "width": pixels(width),
                "height": pixels(height),
            })
        })
}

fn bounds_of(raw: &Value, displays: &[Value]) -> Option<Value> {
    let b = raw.get("bounds")?;
    let n = |k: &str| b.get(k).and_then(Value::as_f64);
    let frame = [n("x")?, n("y")?, n("width")?, n("height")?];
    if frame[2] <= 0.0 || frame[3] <= 0.0 {
        return None;
    }
    place(frame, displays, cfg!(target_os = "macos"))
}

/// Where a walk of a tree is: whose refs, which window, the displays for bounds.
pub struct Walk<'a> {
    pub owner: &'a str,
    pub window: &'a str,
    pub cli_window: &'a str,
    pub app: &'a str,
    pub displays: &'a [Value],
    pub refs: Vec<(String, RefEntry)>,
    pub nodes: usize,
}

impl Walk<'_> {
    /// One node in Link's shape, its ref recorded; children when `deep`.
    pub fn node(&mut self, raw: &Value, deep: bool) -> Value {
        self.nodes += 1;
        let secret = is_secret(raw);
        let role = raw.get("role").and_then(Value::as_str).unwrap_or("").to_string();
        let mut node = Map::new();
        let reference = raw
            .get("ref_id")
            .or_else(|| raw.get("ref"))
            .and_then(Value::as_str)
            .filter(|r| r.starts_with('@'));
        if let Some(r) = reference {
            node.insert("ref".into(), json!(r));
        }
        node.insert("role".into(), json!(role));
        if let Some(name) = raw.get("name").and_then(Value::as_str) {
            let (name, cut) = truncated(name);
            node.insert("name".into(), json!(name));
            if cut {
                node.insert("truncated".into(), json!(true));
            }
        }
        if secret {
            // Never a password's value, from any route (§2.5).
            node.insert("secret".into(), json!(true));
        } else if let Some(value) = raw.get("value").and_then(Value::as_str) {
            let (value, cut) = truncated(value);
            node.insert("value".into(), json!(value));
            if cut {
                node.insert("truncated".into(), json!(true));
            }
        }
        let states: Vec<Value> = raw
            .get("states")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter(|s| s.is_string()).cloned().collect())
            .unwrap_or_default();
        node.insert("states".into(), Value::Array(states));
        let bounds = bounds_of(raw, self.displays);
        if let Some(bounds) = &bounds {
            node.insert("bounds".into(), bounds.clone());
        }
        let advertised = raw.get("available_actions").or_else(|| raw.get("actions"));
        let mut actions: Vec<String> = Vec::new();
        for name in advertised.and_then(Value::as_array).into_iter().flatten() {
            if let Some(action) = name.as_str().and_then(action_of) {
                if !actions.iter().any(|a| a == action) {
                    actions.push(action.to_string());
                }
            }
        }
        // A plain click target always takes a click; anything with a value can be set.
        node.insert("actions".into(), json!(actions));
        if let Some(r) = reference {
            self.refs.push((
                r.to_string(),
                RefEntry {
                    owner: self.owner.to_string(),
                    window: self.window.to_string(),
                    cli_window: self.cli_window.to_string(),
                    app: self.app.to_string(),
                    role: role.clone(),
                    actions: advertised.is_some().then(|| actions.clone()),
                    secret,
                    bounds: bounds.clone(),
                    at: Instant::now(),
                },
            ));
        }
        if deep {
            if let Some(children) = raw.get("children").and_then(Value::as_array) {
                let shown: Vec<Value> = children.iter().map(|c| self.node(c, true)).collect();
                if !shown.is_empty() {
                    node.insert("children".into(), Value::Array(shown));
                }
            }
        }
        Value::Object(node)
    }
}

// --- refs and the queue ---------------------------------------------------------------------

/// Link's side of the layer: refs by owner, Wayland's `a:` window ids, the one-at-a-time queue.
pub struct Ax {
    refs: Mutex<HashMap<(String, String), RefEntry>>,
    /// GNOME Wayland: `a:<n>` → agent-desktop's window id, and back.
    wayland: Mutex<(HashMap<String, String>, HashMap<String, String>)>,
    slot: tokio::sync::Semaphore,
    waiting: AtomicUsize,
    pub state_dir: Option<PathBuf>,
}

impl Ax {
    pub fn new(state_dir: Option<PathBuf>) -> Self {
        Self {
            refs: Mutex::new(HashMap::new()),
            wayland: Mutex::new((HashMap::new(), HashMap::new())),
            slot: tokio::sync::Semaphore::new(1),
            waiting: AtomicUsize::new(0),
            state_dir,
        }
    }

    fn refs(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), RefEntry>> {
        self.refs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Keep refs a snapshot or find gave `owner` (15 minutes; the oldest beyond MAX_REFS go).
    pub fn remember(&self, refs: Vec<(String, RefEntry)>) {
        let now = Instant::now();
        let mut map = self.refs();
        map.retain(|_, entry| now.duration_since(entry.at) < REF_TTL);
        for (reference, entry) in refs {
            map.insert((entry.owner.clone(), reference), entry);
        }
        if map.len() > MAX_REFS {
            let mut ages: Vec<((String, String), Instant)> =
                map.iter().map(|(k, e)| (k.clone(), e.at)).collect();
            ages.sort_by_key(|(_, at)| *at);
            for (key, _) in ages.into_iter().take(map.len() - MAX_REFS) {
                map.remove(&key);
            }
        }
    }

    /// The ref as given to `owner`; anyone else's, unknown or expired is `stale-ref`.
    pub fn lookup(&self, owner: &str, reference: &str) -> ApiResult<RefEntry> {
        if !is_ref(reference) {
            return Err(ApiError::bad_request("ref must be a ref from snapshot or find (@…:e<n>)"));
        }
        let map = self.refs();
        match map.get(&(owner.to_string(), reference.to_string())) {
            Some(entry) if entry.at.elapsed() < REF_TTL => Ok(entry.clone()),
            _ => Err(ApiError::coded(
                409,
                "stale-ref",
                "This ref isn't one this caller got from a snapshot or find, or it expired",
            )),
        }
    }

    /// One agent-desktop at a time: at most MAX_QUEUED waiting, each at most QUEUE_WAIT.
    pub async fn enter(&self) -> ApiResult<tokio::sync::SemaphorePermit<'_>> {
        let busy = || {
            let mut error = ApiError::coded(503, "busy", "Accessibility calls are queued up here");
            error.retry_after_ms = Some(500);
            error.with_suggestion("Wait a moment and try again.")
        };
        // `waiting` counts the calls not yet holding the slot: at most MAX_QUEUED of them.
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            return Err(busy());
        }
        let permit = tokio::time::timeout(QUEUE_WAIT, self.slot.acquire()).await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        match permit {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(busy()),
        }
    }

    /// GNOME Wayland's windows by `a:` ids, stable for this run of Link.
    fn wayland_id(&self, cli_id: &str) -> String {
        let mut map = self.wayland.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = map.1.get(cli_id) {
            return id.clone();
        }
        let id = format!("a:{}", map.0.len() + 1);
        map.0.insert(id.clone(), cli_id.to_string());
        map.1.insert(cli_id.to_string(), id.clone());
        id
    }

    fn wayland_window(&self, id: &str) -> Option<String> {
        self.wayland
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .get(id)
            .cloned()
    }
}

/// `@<snapshot>:e<n>` (as rtx-control checks it again).
pub fn is_ref(text: &str) -> bool {
    let Some((snapshot, element)) = text.strip_prefix('@').and_then(|r| r.split_once(':')) else {
        return false;
    };
    let Some(number) = element.strip_prefix('e') else {
        return false;
    };
    !snapshot.is_empty()
        && snapshot
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

// --- what the layer needs from the desktop --------------------------------------------------

/// The desktop as the layer uses it: Link's window list and displays, the CLI, and a pixel
/// click-and-paste. Live is rtx-control; tests script it.
pub trait Desktop: Sync {
    fn windows(&self) -> impl Future<Output = ApiResult<Value>> + Send;
    fn displays(&self) -> impl Future<Output = ApiResult<Vec<Value>>> + Send;
    /// agent-desktop's envelope (ok or not) for `argv`.
    fn cli(&self, argv: Vec<String>, timeout: Duration) -> impl Future<Output = ApiResult<Value>> + Send;
    /// Click the centre of `bounds` (moving the real pointer), then paste `text`.
    fn paste_at(&self, bounds: &Value, text: &str) -> impl Future<Output = ApiResult<()>> + Send;
}

/// rtx-control behind the layer.
pub struct Live<'a> {
    pub backend: &'a Arc<Backend>,
    pub ax: &'a Ax,
}

impl Desktop for Live<'_> {
    async fn windows(&self) -> ApiResult<Value> {
        let mut answer = self
            .backend
            .call_unstarted("windows", json!({}), Duration::from_secs(15))
            .await?;
        crate::appkey::add_keys(&mut answer);
        Ok(answer)
    }

    async fn displays(&self) -> ApiResult<Vec<Value>> {
        let answer = self
            .backend
            .call_unstarted("displays", json!({}), Duration::from_secs(15))
            .await?;
        Ok(crate::displays::list_of(&answer))
    }

    async fn cli(&self, argv: Vec<String>, timeout: Duration) -> ApiResult<Value> {
        let _one = self.ax.enter().await?;
        let mut params = json!({ "argv": argv, "timeoutMs": timeout.as_millis() as u64 });
        if let Some(dir) = &self.ax.state_dir {
            params["stateDir"] = json!(dir);
        }
        // rtx-control may run its version check first, within its own budget.
        let answer = self
            .backend
            .call_unstarted("ax", params, timeout + ANSWER_GRACE + VERSION_CHECK)
            .await?;
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn paste_at(&self, bounds: &Value, text: &str) -> ApiResult<()> {
        let n = |k: &str| bounds.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let display = bounds.get("display").and_then(Value::as_str);
        let space = crate::displays::list_of(
            &self
                .backend
                .call_unstarted("displays", json!({}), Duration::from_secs(15))
                .await?,
        )
        .into_iter()
        .find(|d| d.get("id").and_then(Value::as_str) == display)
        .map(|d| (d["width"].clone(), d["height"].clone()))
        .ok_or_else(|| ApiError::coded(404, "display-not-found", "The element's display is gone"))?;
        // Native pixels of that display: the action path's own space.
        let actions = json!([
            { "type": "click", "x": n("x") + n("width") / 2.0, "y": n("y") + n("height") / 2.0,
              "width": space.0, "height": space.1 },
            { "type": "type", "text": text, "method": "paste" },
        ]);
        crate::actions::run_actions(self.backend, Some(&actions), false, display)
            .await
            .map(|_| ())
    }
}

// --- the operations -------------------------------------------------------------------------

fn text_field<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A window of Link's list, with the id agent-desktop knows it by.
#[derive(Clone, Debug)]
pub struct Target {
    pub window: String,
    pub cli_window: String,
    pub app: String,
    pub app_key: Option<String>,
    pub pid: Option<u64>,
}

fn listed(windows: &Value) -> Vec<Value> {
    windows
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Link's window `id` as agent-desktop knows it (G2): macOS and Windows windows by their native
/// number (`w-<CGWindowID>`; `w-<HWND>` in decimal, as agent-desktop 1.0.2 writes and parses it:
/// `windows/src/system/window/window_ops.rs`, while Link's own `w:` id is hex); X11 by pid (and
/// title when one app has several), since agent-desktop lists AT-SPI windows there; `a:` ids
/// from the Wayland list.
pub async fn target<D: Desktop>(desktop: &D, ax: &Ax, id: &str, budget: Budget) -> ApiResult<Target> {
    let not_found = || {
        ApiError::coded(
            404,
            "window-not-found",
            "No such window here: list the windows again",
        )
    };
    if id.starts_with("a:") {
        let cli_window = ax.wayland_window(id).ok_or_else(not_found)?;
        let apps = wayland_listed(desktop, ax, budget).await?;
        let found = apps.iter().find(|w| w["id"] == id).ok_or_else(not_found)?;
        return Ok(Target {
            window: id.to_string(),
            cli_window,
            app: found["app"].as_str().unwrap_or("").to_string(),
            app_key: found["appKey"].as_str().map(str::to_string),
            pid: found["pid"].as_u64(),
        });
    }
    let parsed = WindowId::parse(id).ok_or_else(|| {
        ApiError::bad_request("window must be a window id from GET /windows or ax/apps")
    })?;
    let windows = listed(&desktop.windows().await?);
    let window = windows
        .iter()
        .find(|w| w.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(not_found)?;
    // Minimized windows aren't offered (macOS and Windows list them, off screen).
    if window.get("minimized") == Some(&Value::Bool(true)) {
        return Err(not_found());
    }
    let app = window["app"].as_str().unwrap_or("").to_string();
    let pid = window["pid"].as_u64();
    let cli_window = match parsed {
        WindowId::Mac(number) => format!("w-{number}"),
        WindowId::Win(hwnd) => format!("w-{hwnd}"),
        WindowId::X11(_) => {
            let title = window["title"].as_str().unwrap_or("");
            let list = data(&read_cli(desktop, vec!["list-windows".into()], LIST_TIMEOUT, budget).await?)
                .map_err(|e| map_error(&e, false, None))?;
            let theirs: Vec<&Value> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|w| w["pid"].as_u64() == pid)
                .collect();
            let chosen = match theirs.as_slice() {
                [one] => Some(*one),
                many => {
                    let titled: Vec<&&Value> = many.iter().filter(|w| w["title"] == title).collect();
                    match titled.as_slice() {
                        [one] => Some(**one),
                        _ => None,
                    }
                }
            };
            chosen
                .and_then(|w| w["id"].as_str())
                .map(str::to_string)
                .ok_or_else(not_found)?
        }
    };
    Ok(Target {
        window: id.to_string(),
        cli_window,
        app,
        app_key: window["appKey"].as_str().map(str::to_string),
        pid,
    })
}

/// GNOME Wayland: Link can't list windows; agent-desktop can, under `a:` ids.
pub async fn wayland_windows<D: Desktop>(desktop: &D, ax: &Ax) -> ApiResult<Vec<Value>> {
    wayland_listed(desktop, ax, Budget::default()).await
}

async fn wayland_listed<D: Desktop>(desktop: &D, ax: &Ax, budget: Budget) -> ApiResult<Vec<Value>> {
    let list = data(&read_cli(desktop, vec!["list-windows".into()], LIST_TIMEOUT, budget).await?)
        .map_err(|e| map_error(&e, false, None))?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w.get("minimized") != Some(&Value::Bool(true)))
        .filter_map(|w| {
            let cli_id = w["id"].as_str()?;
            let pid = w["pid"].as_u64();
            Some(json!({
                "id": ax.wayland_id(cli_id),
                "pid": pid,
                "app": w["app_name"],
                "appName": w["app_name"],
                "appKey": pid.and_then(|p| crate::appkey::of_pid(p as u32)),
                "title": w["title"],
                "display": null,
                "frame": null,
                "focused": w["is_focused"],
                "minimized": false,
            }))
        })
        .collect())
}

/// Link's windows, or on GNOME Wayland agent-desktop's (`a:` ids).
pub async fn windows<D: Desktop>(desktop: &D, ax: &Ax) -> ApiResult<Vec<Value>> {
    match desktop.windows().await {
        Ok(answer) => Ok(listed(&answer)),
        Err(error) if error.code.as_deref() == Some("unsupported") => wayland_windows(desktop, ax).await,
        Err(error) => Err(error),
    }
}

/// `GET ax/apps` → `{apps:[{app, appName, appKey, pid, windows:[{id, title, display}]}]}`.
pub async fn apps<D: Desktop>(desktop: &D, ax: &Ax) -> ApiResult<Value> {
    let mut apps: Vec<Value> = Vec::new();
    for window in windows(desktop, ax).await? {
        if window.get("minimized") == Some(&Value::Bool(true)) {
            continue;
        }
        let shown = json!({ "id": window["id"], "title": window["title"], "display": window["display"] });
        let key = (window["app"].clone(), window["pid"].clone());
        match apps.iter_mut().find(|a| (a["app"].clone(), a["pid"].clone()) == key) {
            Some(app) => {
                if let Some(list) = app["windows"].as_array_mut() {
                    list.push(shown);
                }
            }
            None => apps.push(json!({
                "app": window["app"],
                "appName": window["appName"],
                "appKey": window["appKey"],
                "pid": window["pid"],
                "windows": [shown],
            })),
        }
    }
    Ok(json!({ "apps": apps }))
}

/// Why a window gave no tree (§2.4.4): `elevated`, `a11y-off` or `empty`.
fn no_tree(target: &Target, caller: &Caller) -> ApiError {
    let reason = no_tree_reason(target);
    let suggestion = match reason {
        "elevated" => "This app runs as administrator: start RealTimeX Link with this computer (service mode), or use a screenshot and pixels.",
        "a11y-off" => "Relaunch the app with --force-renderer-accessibility (Electron, Chromium) or GNOME_ACCESSIBILITY=1 (Firefox), or use a screenshot and pixels.",
        _ => "This window shows no controls to accessibility: use a screenshot and pixels.",
    };
    let _ = caller;
    ApiError::coded(422, "no-tree", "The window has no accessibility tree here")
        .with_details(json!({ "reason": reason }))
        .with_suggestion(suggestion)
}

fn no_tree_reason(target: &Target) -> &'static str {
    if cfg!(windows) {
        if let Some(pid) = target.pid {
            if crate::appkey::elevated(pid as u32) == Some(true)
                && crate::appkey::elevated(std::process::id()) == Some(false)
            {
                return "elevated";
            }
        }
        return "empty";
    }
    if cfg!(target_os = "linux") && browser_like(&target.app) {
        return "a11y-off";
    }
    "empty"
}

fn browser_like(app: &str) -> bool {
    let app = app.to_ascii_lowercase();
    ["chrom", "electron", "firefox", "code", "slack", "discord", "brave", "edge"]
        .iter()
        .any(|name| app.contains(name))
}

fn depth_of(body: &Value) -> ApiResult<u64> {
    match body.get("maxDepth") {
        None | Some(Value::Null) => Ok(DEPTH_DEFAULT),
        Some(value) => value
            .as_u64()
            .filter(|d| (1..=60).contains(d))
            .map(|d| d.min(DEPTH_CLI_MAX))
            .ok_or_else(|| ApiError::bad_request("maxDepth must be from 1 to 60")),
    }
}

/// `POST ax/snapshot {window, maxDepth=50, interactive=true, root?}`.
pub async fn snapshot<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, Target)> {
    let window = text_field(body, "window").ok_or_else(|| ApiError::bad_request("window is required"))?;
    let depth = depth_of(body)?;
    let target = target(desktop, ax, window, Budget::default()).await?;
    let mut argv = vec![
        "snapshot".to_string(),
        format!("--window-id={}", target.cli_window),
        format!("--max-depth={depth}"),
        "--include-bounds".to_string(),
    ];
    if body.get("interactive") != Some(&Value::Bool(false)) {
        argv.push("--interactive-only".into());
    }
    if let Some(root) = text_field(body, "root") {
        let entry = ax.lookup(&caller.owner, root)?;
        if entry.window != target.window {
            return Err(ApiError::coded(409, "stale-ref", "root is from another window"));
        }
        argv.push(format!("--root={root}"));
    }
    let envelope = read_cli(desktop, argv, SNAPSHOT_TIMEOUT, Budget::default()).await?;
    let answer = match data(&envelope) {
        Ok(answer) => answer,
        Err(error) if error.code == "APP_NOT_FOUND" && cfg!(target_os = "linux") => {
            return Err(ApiError::coded(422, "no-tree", "The app isn't on the accessibility bus")
                .with_details(json!({ "reason": "a11y-off" }))
                .with_suggestion("Relaunch the app with --force-renderer-accessibility (Electron, Chromium) or GNOME_ACCESSIBILITY=1 (Firefox), or use a screenshot and pixels."));
        }
        Err(error) => return Err(map_error(&error, false, None)),
    };
    let displays = desktop.displays().await.unwrap_or_default();
    let mut walk = Walk {
        owner: &caller.owner,
        window: &target.window,
        cli_window: &target.cli_window,
        app: &target.app,
        displays: &displays,
        refs: Vec::new(),
        nodes: 0,
    };
    let root = walk.node(answer.get("tree").unwrap_or(&Value::Null), true);
    let refs = walk.refs.len();
    let nodes = walk.nodes;
    if refs == 0 {
        return Err(no_tree(&target, caller));
    }
    ax.remember(std::mem::take(&mut walk.refs));
    let result = json!({
        "snapshot": answer.get("snapshot_id"),
        "window": target.window,
        "app": target.app,
        "appKey": target.app_key,
        "refs": refs,
        "nodes": nodes,
        "root": root,
        "truncated": answer.get("complete") == Some(&Value::Bool(false)),
    });
    Ok((result, target))
}

/// `POST ax/find {window, role?, name?, text?, exact?, limit=20 (≤50)}` → `{matches}` (no
/// children). `exact: true`: the name or text whole, not a part of it.
pub async fn find<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, Target)> {
    let window = text_field(body, "window").ok_or_else(|| ApiError::bad_request("window is required"))?;
    let limit = match body.get("limit") {
        None | Some(Value::Null) => FIND_LIMIT_DEFAULT,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=FIND_LIMIT_MAX).contains(n))
            .ok_or_else(|| ApiError::bad_request("limit must be from 1 to 50"))?,
    };
    let target = target(desktop, ax, window, Budget::default()).await?;
    let matches = find_in(desktop, ax, caller, &target, body, limit, Budget::default()).await?;
    Ok((json!({ "matches": matches }), target))
}

async fn find_in<D: Desktop>(
    desktop: &D,
    ax: &Ax,
    caller: &Caller,
    target: &Target,
    query: &Value,
    limit: u64,
    budget: Budget,
) -> ApiResult<Vec<Value>> {
    let mut argv = vec!["find".to_string(), format!("--window-id={}", target.cli_window)];
    let mut any = false;
    for name in ["role", "name", "text"] {
        if let Some(value) = text_field(query, name) {
            argv.push(format!("--{name}={value}"));
            any = true;
        }
    }
    if !any {
        return Err(ApiError::bad_request("find needs role, name or text"));
    }
    if query.get("exact") == Some(&Value::Bool(true)) {
        argv.push("--exact".into());
    }
    argv.push(format!("--limit={limit}"));
    let answer = data(&read_cli(desktop, argv, READ_TIMEOUT, budget).await?).map_err(|e| map_error(&e, false, None))?;
    let raw: Vec<Value> = match answer.get("matches").and_then(Value::as_array) {
        Some(list) => list.clone(),
        None => answer.get("match").cloned().into_iter().filter(|m| !m.is_null()).collect(),
    };
    // agent-desktop 1.0.2 gives bounds only for matches without a ref; place them when it does.
    // A ref's bounds are read when needs-pixels wants them (`bounds_for`).
    let displays = if raw.iter().any(|m| m.get("bounds").is_some()) {
        desktop.displays().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut walk = Walk {
        owner: &caller.owner,
        window: &target.window,
        cli_window: &target.cli_window,
        app: &target.app,
        displays: &displays,
        refs: Vec::new(),
        nodes: 0,
    };
    let matches: Vec<Value> = raw
        .iter()
        // Context matches have no ref: nothing to act on.
        .filter(|m| m.get("ref_id").and_then(Value::as_str).is_some())
        .map(|m| {
            let mut node = walk.node(m, false);
            // agent-desktop 1.0.2's find gives no actions: what the role takes, for an
            // interactive element (its ref still allows any action, as unknown).
            let advertised = m.get("available_actions").or_else(|| m.get("actions")).is_some();
            if !advertised && m.get("interactive") == Some(&Value::Bool(true)) {
                node["actions"] = json!(likely_actions(node["role"].as_str().unwrap_or("")));
            }
            node
        })
        .collect();
    ax.remember(std::mem::take(&mut walk.refs));
    Ok(matches)
}

/// The actions an interactive element of `role` takes, as snapshots list them for it, for
/// find's matches (agent-desktop's find doesn't say).
fn likely_actions(role: &str) -> Vec<&'static str> {
    let role: String = role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    match role.as_str() {
        "button" | "link" | "menuitem" | "listitem" | "treeitem" | "cell" | "image" => vec!["click"],
        "checkbox" | "switch" | "togglebutton" | "radiobutton" => vec!["click", "toggle"],
        "textfield" | "searchfield" | "textarea" | "securetextfield" | "passwordtext" => {
            vec!["click", "set-value", "type"]
        }
        "combobox" => vec!["click", "set-value", "type", "select", "expand", "collapse"],
        "menubutton" | "popupbutton" => vec!["click", "expand", "collapse"],
        "tab" => vec!["click", "select"],
        "slider" | "spinbutton" | "incrementor" => vec!["set-value"],
        _ => Vec::new(),
    }
}

/// Of several matches, the one named exactly as the query's `name` (ignoring case and spaces
/// around it), when only one is: `find --name Clear` acts on "Clear", not "Clear entry" or
/// "Clear all memory" (QA of realtimex-ai-app#3055).
fn exact_match<'a>(matches: &'a [Value], query: &Value) -> Option<&'a Value> {
    let wanted = text_field(query, "name")?.trim().to_lowercase();
    let mut exact = matches.iter().filter(|m| {
        m["name"].as_str().is_some_and(|name| name.trim().to_lowercase() == wanted)
    });
    let one = exact.next()?;
    exact.next().is_none().then_some(one)
}

/// `POST ax/get {ref, property: value|name|states|bounds}` → `{ref, property, value, secret?}`.
pub async fn get<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, RefEntry)> {
    let reference = text_field(body, "ref").ok_or_else(|| ApiError::bad_request("ref is required"))?;
    let property = text_field(body, "property").unwrap_or("value");
    let entry = ax.lookup(&caller.owner, reference)?;
    let value = get_property(desktop, &entry, reference, property, Budget::default()).await?;
    Ok((get_answer(reference, property, value, &entry), entry))
}

/// The answer to a `get`: a value or name over 4 KB cut for the client, with `truncated`.
fn get_answer(reference: &str, property: &str, value: Value, entry: &RefEntry) -> Value {
    let (value, cut) = match value.as_str() {
        Some(text) if matches!(property, "value" | "name") => {
            let (text, cut) = truncated(text);
            (json!(text), cut)
        }
        _ => (value, false),
    };
    let mut answer = json!({ "ref": reference, "property": property, "value": value });
    if cut {
        answer["truncated"] = json!(true);
    }
    if entry.secret && property == "value" {
        answer["secret"] = json!(true);
    }
    answer
}

/// A property as agent-desktop reads it, whole: the `type` fallback rewrites and checks a field
/// from this, so only the answer to the client is cut ([`get_answer`]).
async fn get_property<D: Desktop>(
    desktop: &D,
    entry: &RefEntry,
    reference: &str,
    property: &str,
    budget: Budget,
) -> ApiResult<Value> {
    // agent-desktop 1.0.2's `get --property`: `title` is the element's name, while `text` is its
    // value for a field (core/src/commands/get.rs), so `name` must never ask for `text`.
    let cli_property = match property {
        "value" => "value",
        "name" => "title",
        "states" => "states",
        "bounds" => "bounds",
        _ => return Err(ApiError::bad_request("property must be value, name, states or bounds")),
    };
    // A password's value: never read, never returned (§2.5).
    if entry.secret && property == "value" {
        return Ok(Value::Null);
    }
    let argv = vec![
        "get".to_string(),
        format!("--property={cli_property}"),
        "--".to_string(),
        reference.to_string(),
    ];
    let answer = data(&read_cli(desktop, argv, READ_TIMEOUT, budget).await?).map_err(|e| map_error(&e, false, None))?;
    let value = answer.get("value").cloned().unwrap_or(Value::Null);
    Ok(match property {
        "bounds" => {
            let displays = desktop.displays().await.unwrap_or_default();
            bounds_of(&json!({ "bounds": value }), &displays).unwrap_or(Value::Null)
        }
        _ => value,
    })
}

/// What an act did: how, whether it landed, and the value read back (never a password's).
#[derive(Debug, PartialEq)]
pub struct Acted {
    pub via: &'static str,
    pub delivery: Delivery,
    pub value: Option<Value>,
}

fn landed(answer: &Value) -> Delivery {
    delivery_of(answer["disposition"]["delivery"].as_str())
}

/// The element's bounds for a pixel fallback: from its snapshot, else read now (a ref from
/// `find` has none: agent-desktop 1.0.2 gives bounds only for matches without a ref).
async fn bounds_for<D: Desktop>(desktop: &D, entry: &RefEntry, reference: &str, budget: Budget) -> Option<Value> {
    if let Some(bounds) = &entry.bounds {
        return Some(bounds.clone());
    }
    get_property(desktop, entry, reference, "bounds", budget)
        .await
        .ok()
        .filter(|bounds| bounds.is_object())
}

/// needs-pixels for `entry`, with its `bounds` and `display` when they can be had (§2.4.5).
async fn needs_pixels<D: Desktop>(
    desktop: &D,
    entry: &RefEntry,
    reference: &str,
    budget: Budget,
    error: ApiError,
) -> ApiError {
    if error.details.is_some() {
        return error;
    }
    match bounds_for(desktop, entry, reference, budget).await {
        Some(bounds) => {
            let display = bounds.get("display").cloned();
            error.with_details(json!({ "bounds": bounds, "display": display }))
        }
        None => error.with_suggestion(NO_BOUNDS),
    }
}

/// needs-pixels for an element whose place on the screen isn't known (GNOME Wayland gives none).
const NO_BOUNDS: &str = "Take a screenshot, find the element in it and click it there by pixels.";

/// The type fallback chain and every other action (§2.4.3, §2.4.5). `budget`: what is left of a
/// batch's time.
pub async fn act_on<D: Desktop>(
    desktop: &D,
    entry: &RefEntry,
    reference: &str,
    action: &str,
    value: Option<&str>,
    pixels_allowed: bool,
    budget: Budget,
) -> ApiResult<Acted> {
    if !ACTIONS.contains(&action) {
        return Err(ApiError::bad_request(format!(
            "action must be one of {}",
            ACTIONS.join(", ")
        )));
    }
    let needs_value = matches!(action, "set-value" | "type" | "select");
    let value = match (needs_value, value) {
        (true, Some(v)) => Some(v),
        (true, None) => return Err(ApiError::bad_request(format!("{action} needs value"))),
        (false, _) => None,
    };
    let advertised = |name: &str| entry.actions.as_ref().is_none_or(|list| list.iter().any(|a| a == name));
    if action == "type" {
        return type_chain(desktop, entry, reference, value.unwrap_or(""), pixels_allowed, budget).await;
    }
    // Not something this element does: the agent clicks its centre instead.
    if !advertised(action) {
        let error = ApiError::coded(422, "needs-pixels", format!("This element can't {action} through accessibility"))
            .with_delivery(Delivery::None);
        return Err(needs_pixels(desktop, entry, reference, budget, error).await);
    }
    let mut argv = vec![action.to_string(), "--".to_string(), reference.to_string()];
    if let Some(value) = value {
        argv.push(value.to_string());
    }
    let envelope = act_cli(desktop, argv, ACT_TIMEOUT, budget).await?;
    match data(&envelope) {
        Ok(answer) => Ok(Acted {
            // How text went in; any other action went through accessibility as itself.
            via: if action == "set-value" { "set-value" } else { "accessibility" },
            delivery: landed(&answer),
            value: read_back(&answer, entry),
        }),
        Err(error) => {
            let error = map_error(&error, true, entry.bounds.as_ref());
            if error.code.as_deref() == Some("needs-pixels") {
                return Err(needs_pixels(desktop, entry, reference, budget, error).await);
            }
            Err(error)
        }
    }
}

/// The value an action left, for the answer: never a password's (marked in the snapshot, or in
/// the state agent-desktop read after acting).
fn read_back(answer: &Value, entry: &RefEntry) -> Option<Value> {
    if entry.secret || is_secret(&answer["post_state"]) {
        return None;
    }
    answer["post_state"]["value"]
        .as_str()
        .map(|v| json!(truncated(v).0))
}

/// Whether `old + text` can go through set-value: `old` can't have been cut by agent-desktop,
/// and the result can be read back whole to check it.
fn appendable(old: &str, text: &str) -> bool {
    old.len() + text.len() < WHOLE_TEXT
}

fn may_have_typed() -> ApiError {
    ApiError::coded(409, "ax-failed", "The text may have been typed; it wasn't verified")
        .with_delivery(Delivery::Unknown)
        .with_retry(Retry::No)
        .with_suggestion("Read the value with get, then use set-value.")
}

/// §2.4.3: `type`; else (nothing delivered, and the element can be set, and its value is
/// readable and not a password) `set-value(old + text)` checked with `get`; else a pixel click at
/// its centre and a paste; else needs-pixels. Any step that may have landed stops the chain.
async fn type_chain<D: Desktop>(
    desktop: &D,
    entry: &RefEntry,
    reference: &str,
    text: &str,
    pixels_allowed: bool,
    budget: Budget,
) -> ApiResult<Acted> {
    let envelope = act_cli(
        desktop,
        vec!["type".into(), "--".into(), reference.into(), text.into()],
        ACT_TIMEOUT,
        budget,
    )
    .await?;
    let refused = match data(&envelope) {
        Ok(answer) => {
            let delivery = landed(&answer);
            // Typed, but nobody can say it arrived once (Electron on macOS): stop here.
            if delivery == Delivery::Unknown {
                return Err(may_have_typed());
            }
            return Ok(Acted {
                via: "type",
                delivery,
                value: read_back(&answer, entry),
            });
        }
        Err(error) => error,
    };
    if refused.may_have_landed() {
        return Err(may_have_typed());
    }
    if !matches!(refused.code.as_str(), "POLICY_DENIED" | "ACTION_NOT_SUPPORTED") {
        return Err(map_error(&refused, true, entry.bounds.as_ref()));
    }
    // 2. set-value with the text appended (at the end, not the caret). It rewrites the whole
    // field, so only from a value read whole, and only to one that can be read back whole;
    // otherwise nothing is written here and the chain goes on.
    let settable = entry.actions.as_ref().is_some_and(|a| a.iter().any(|x| x == "set-value"));
    if settable && !entry.secret {
        let old = match get_property(desktop, entry, reference, "value", budget).await {
            Ok(Value::String(old)) if appendable(&old, text) => Some(old),
            _ => None,
        };
        if let Some(old) = old {
            let wanted = format!("{old}{text}");
            let set = act_cli(
                desktop,
                vec!["set-value".into(), "--".into(), reference.into(), wanted.clone()],
                ACT_TIMEOUT,
                budget,
            )
            .await?;
            match data(&set) {
                Ok(_) => {
                    let now = get_property(desktop, entry, reference, "value", budget).await;
                    return match now {
                        Ok(Value::String(now)) if now == wanted => Ok(Acted {
                            via: "set-value",
                            delivery: Delivery::Done,
                            value: Some(json!(truncated(&now).0)),
                        }),
                        // Set, but not to what was asked: it may be half there.
                        _ => Err(may_have_typed()),
                    };
                }
                Err(error) if error.may_have_landed() => return Err(may_have_typed()),
                Err(_) => {}
            }
        }
    }
    // 3. A click at its centre and a paste (the real pointer moves); not for local agents.
    let bounds = bounds_for(desktop, entry, reference, budget).await;
    if pixels_allowed {
        if let Some(bounds) = &bounds {
            desktop.paste_at(bounds, text).await.map_err(|error| {
                ApiError {
                    delivery: Some(Delivery::Unknown),
                    retry: Some(Retry::No),
                    ..error
                }
            })?;
            return Ok(Acted {
                via: "paste",
                delivery: Delivery::Unknown,
                value: None,
            });
        }
    }
    // 4. The agent does it with pixels.
    let mut error = ApiError::coded(422, "needs-pixels", "This element takes no typing through accessibility")
        .with_delivery(Delivery::None);
    match bounds {
        Some(bounds) => {
            let display = bounds.get("display").cloned();
            error = error.with_details(json!({ "bounds": bounds, "display": display }));
        }
        None => error = error.with_suggestion(NO_BOUNDS),
    }
    Err(error)
}

/// `POST ax/act {ref, action, value?}` → `{ref, action, via, delivery, value?}`.
pub async fn act<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, RefEntry, &'static str)> {
    let reference = text_field(body, "ref").ok_or_else(|| ApiError::bad_request("ref is required"))?;
    let action = text_field(body, "action").ok_or_else(|| ApiError::bad_request("action is required"))?;
    let value = body.get("value").and_then(Value::as_str);
    let entry = ax.lookup(&caller.owner, reference)?;
    let acted = act_on(desktop, &entry, reference, action, value, !caller.local, Budget::default()).await?;
    let mut answer = json!({
        "ref": reference,
        "action": action,
        "via": acted.via,
        "delivery": if acted.delivery == Delivery::Done { "done" } else { "unknown" },
    });
    if let Some(value) = acted.value {
        answer["value"] = value;
    }
    Ok((answer, entry, acted.via))
}

/// What a batch step is.
enum Step<'a> {
    Find { query: &'a Value, then: Option<&'a Value> },
    Act(&'a Value),
    Get(&'a Value),
}

fn step_of(step: &Value) -> ApiResult<Step<'_>> {
    if let Some(query) = step.get("find") {
        return Ok(Step::Find { query, then: step.get("then") });
    }
    if let Some(act) = step.get("act") {
        return Ok(Step::Act(act));
    }
    if let Some(get) = step.get("get") {
        return Ok(Step::Get(get));
    }
    Err(ApiError::bad_request("each step is {find, then?}, {act} or {get}"))
}

/// Whether a batch only reads (view access) or also acts (control).
pub fn batch_acts(body: &Value) -> bool {
    body.get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|step| step.get("act").is_some() || step.get("then").is_some_and(|t| !t.is_null()))
}

fn error_json(error: &ApiError) -> Value {
    let mut body = error.body();
    if let (Some(fields), Some(Value::Object(details))) = (body.as_object_mut(), &error.details) {
        for (k, v) in details {
            fields.entry(k.clone()).or_insert(v.clone());
        }
    }
    body
}

/// One step's activity: an act's row, or a read of the window. `via` only when the act
/// happened (how it went in); a failed act names none.
pub enum Note {
    Read { window: String, app: String },
    Act { entry: Option<RefEntry>, action: String, via: Option<&'static str>, error: Option<ApiError> },
}

/// A batch step that would run past the deadline is cut off this long after it, as rtx-linkd
/// waits for any CLI answer (its CLI calls are already capped by what is left).
const STEP_GRACE: Duration = ANSWER_GRACE;

/// `POST ax/batch {window, steps:[…≤10], stopOnError=true}` → `{results:[{ok, data|error}]}`,
/// within [`batch_timeout`] in all. The rows to write come back with it.
pub async fn batch<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, Vec<Note>)> {
    let steps = body.get("steps").and_then(Value::as_array).map_or(0, Vec::len);
    // Its CLI calls end by the deadline less the answer grace, so the whole answer is in by then
    // (the relay in front of a remote batch waits 60 s for it).
    let deadline = Instant::now() + batch_timeout(steps) - STEP_GRACE;
    batch_until(desktop, ax, caller, body, deadline).await
}

/// How long a batch of `steps` steps may take: 6 s a step, from 20 s to 60 s (10 steps).
pub fn batch_timeout(steps: usize) -> Duration {
    (BATCH_PER_STEP * steps.min(MAX_STEPS) as u32).clamp(BATCH_MIN, BATCH_MAX)
}

/// The batch with its deadline: each step's CLI calls are capped by what is left of it, and a
/// step still running at the deadline (plus the answer grace) is cut off as a timeout.
async fn batch_until<D: Desktop>(
    desktop: &D,
    ax: &Ax,
    caller: &Caller,
    body: &Value,
    deadline: Instant,
) -> ApiResult<(Value, Vec<Note>)> {
    let window = text_field(body, "window").ok_or_else(|| ApiError::bad_request("window is required"))?;
    let steps = body
        .get("steps")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty() && s.len() <= MAX_STEPS)
        .ok_or_else(|| ApiError::bad_request("steps must be 1 to 10 steps"))?;
    let parsed: Vec<Step> = steps.iter().map(step_of).collect::<ApiResult<_>>()?;
    let stop_on_error = body.get("stopOnError") != Some(&Value::Bool(false));
    let budget = Budget::until(deadline);
    let target = target(desktop, ax, window, budget).await?;
    let mut results = Vec::new();
    let mut notes = Vec::new();
    for step in parsed {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            results.push(json!({ "ok": false, "error": error_json(&out_of_time()) }));
            break;
        }
        let acting = step.acts();
        let mut step_notes = Vec::new();
        let ran = tokio::time::timeout(
            left + STEP_GRACE,
            run_step(desktop, ax, caller, &target, &step, budget, &mut step_notes),
        )
        .await;
        let outcome = match ran {
            Ok(outcome) => outcome,
            Err(_) => {
                let late = runner_failure(out_of_time(), acting);
                if let Some((entry, action)) = step.act_target(ax, caller) {
                    step_notes.push(Note::Act { entry, action, via: None, error: Some(late.clone()) });
                }
                Err(late)
            }
        };
        notes.append(&mut step_notes);
        match outcome {
            Ok(data) => results.push(json!({ "ok": true, "data": data })),
            Err(error) => {
                results.push(json!({ "ok": false, "error": error_json(&error) }));
                if stop_on_error {
                    break;
                }
            }
        }
    }
    Ok((json!({ "results": results }), notes))
}

impl Step<'_> {
    fn acts(&self) -> bool {
        match self {
            Step::Act(_) => true,
            Step::Find { then, .. } => (*then).is_some_and(|t| !t.is_null()),
            Step::Get(_) => false,
        }
    }

    /// The ref and action of an acting step cut off before it said: for its Activity row.
    fn act_target(&self, ax: &Ax, caller: &Caller) -> Option<(Option<RefEntry>, String)> {
        match self {
            Step::Act(act) => Some((
                ax.lookup(&caller.owner, text_field(act, "ref").unwrap_or("")).ok(),
                text_field(act, "action").unwrap_or("").to_string(),
            )),
            Step::Find { then: Some(then), .. } if !then.is_null() => {
                Some((None, text_field(then, "action").unwrap_or("").to_string()))
            }
            _ => None,
        }
    }
}

/// One act of a batch, its row noted.
async fn act_step<D: Desktop>(
    desktop: &D,
    entry: RefEntry,
    reference: &str,
    act: &Value,
    caller: &Caller,
    budget: Budget,
    notes: &mut Vec<Note>,
) -> ApiResult<Value> {
    let action = text_field(act, "action").unwrap_or("");
    let value = act.get("value").and_then(Value::as_str);
    let acted = act_on(desktop, &entry, reference, action, value, !caller.local, budget).await;
    notes.push(Note::Act {
        entry: Some(entry),
        action: action.to_string(),
        via: acted.as_ref().ok().map(|a| a.via),
        error: acted.as_ref().err().cloned(),
    });
    acted.map(|acted| acted_json(reference, action, acted))
}

/// One step of a batch, within `budget`; its rows go to `notes` as they happen.
async fn run_step<D: Desktop>(
    desktop: &D,
    ax: &Ax,
    caller: &Caller,
    target: &Target,
    step: &Step<'_>,
    budget: Budget,
    notes: &mut Vec<Note>,
) -> ApiResult<Value> {
    let other_window = || ApiError::coded(409, "stale-ref", "This ref is from another window");
    match step {
        Step::Get(get_step) => {
            let reference = text_field(get_step, "ref").unwrap_or("");
            let property = text_field(get_step, "property").unwrap_or("value");
            let entry = ax.lookup(&caller.owner, reference)?;
            if entry.window != target.window {
                return Err(other_window());
            }
            notes.push(Note::Read { window: target.window.clone(), app: target.app.clone() });
            let value = get_property(desktop, &entry, reference, property, budget).await?;
            Ok(get_answer(reference, property, value, &entry))
        }
        Step::Act(act) => {
            let reference = text_field(act, "ref").unwrap_or("");
            let entry = ax.lookup(&caller.owner, reference)?;
            if entry.window != target.window {
                return Err(other_window());
            }
            act_step(desktop, entry, reference, act, caller, budget, notes).await
        }
        Step::Find { query, then } => {
            notes.push(Note::Read { window: target.window.clone(), app: target.app.clone() });
            let matches = find_in(desktop, ax, caller, target, query, CANDIDATES as u64 + 1, budget).await?;
            let chosen = match matches.as_slice() {
                [one] => Some(one),
                many => exact_match(many, query),
            };
            match ((*then).filter(|t| !t.is_null()), matches.as_slice(), chosen) {
                (None, _, _) => Ok(json!({ "matches": matches })),
                (Some(_), [], _) => Err(ApiError::coded(409, "stale-ref", "Nothing matched to act on")),
                (Some(then), _, Some(one)) => {
                    let reference = one["ref"].as_str().unwrap_or("").to_string();
                    let entry = ax.lookup(&caller.owner, &reference)?;
                    act_step(desktop, entry, &reference, then, caller, budget, notes).await
                }
                (Some(_), many, None) => Err(ApiError::coded(409, "ambiguous-target", "More than one element matched")
                    .with_details(json!({ "candidates": many.iter().take(CANDIDATES).cloned().collect::<Vec<_>>() }))),
            }
        }
    }
}

fn acted_json(reference: &str, action: &str, acted: Acted) -> Value {
    let mut answer = json!({
        "ref": reference,
        "action": action,
        "via": acted.via,
        "delivery": if acted.delivery == Delivery::Done { "done" } else { "unknown" },
    });
    if let Some(value) = acted.value {
        answer["value"] = value;
    }
    answer
}

// --- the routes' common part ----------------------------------------------------------------

/// Which operation a route is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Apps,
    Windows,
    Snapshot,
    Find,
    Get,
    Act,
    Batch,
}

/// Refusals before anything runs: the feature, and access (reads need view, acts control).
fn admit(caller: &Caller, op: Op, body: &Value) -> ApiResult<()> {
    if !enabled() {
        return Err(ApiError::coded(501, "unsupported", "The accessibility layer isn't available on this computer")
            .with_suggestion("Use a screenshot and pixel clicks."));
    }
    let acts = op == Op::Act || (op == Op::Batch && batch_acts(body));
    if acts && !caller.control {
        return Err(ApiError::coded(403, "view-only", "This client is not allowed to control"));
    }
    Ok(())
}

/// A permission-missing answer as this caller can act on it.
fn for_caller(error: ApiError, caller: &Caller) -> ApiError {
    if caller.local && error.code.as_deref() == Some("permission-missing") {
        return error.with_suggestion("Open RealTimeX Link and grant it Accessibility in System Settings > Privacy & Security.");
    }
    error
}

/// Run one route for `caller` and write its Activity rows. `audit`: where rows go.
pub async fn serve(
    backend: &Arc<Backend>,
    ax: &Ax,
    audit: &Audit,
    caller: &Caller,
    op: Op,
    body: &Value,
) -> ApiResult<Value> {
    let desktop = Live { backend, ax };
    let result = run(&desktop, ax, audit, caller, op, body).await;
    result.map_err(|error| for_caller(error, caller))
}

pub async fn run<D: Desktop>(desktop: &D, ax: &Ax, audit: &Audit, caller: &Caller, op: Op, body: &Value) -> ApiResult<Value> {
    let read_failed = |error: &ApiError, window: &str| {
        audit.note(
            caller.who.clone(),
            "ax-read",
            Outcome::of_status(error.status),
            Some(&error.code_or_status()),
            json!({ "app": window_app_hint(window) }),
        );
    };
    if let Err(error) = admit(caller, op, body) {
        let kind = if matches!(op, Op::Act) || (op == Op::Batch && batch_acts(body)) { "ax-act" } else { "ax-read" };
        audit.note(caller.who.clone(), kind, Outcome::of_status(error.status), Some(&error.code_or_status()), json!({}));
        return Err(error);
    }
    match op {
        Op::Apps => {
            let answer = apps(desktop, ax).await;
            match &answer {
                Ok(_) => audit.ax_read(caller.who.clone(), "apps", None),
                Err(error) => read_failed(error, ""),
            }
            answer
        }
        Op::Windows => {
            let answer = windows(desktop, ax).await.map(|list| json!({ "windows": list }));
            audit.outcome(caller.who.clone(), "windows", &answer, json!({}));
            answer
        }
        Op::Snapshot | Op::Find => {
            let window = text_field(body, "window").unwrap_or("").to_string();
            let answer = if op == Op::Snapshot {
                snapshot(desktop, ax, caller, body).await
            } else {
                find(desktop, ax, caller, body).await
            };
            match answer {
                Ok((answer, target)) => {
                    audit.ax_read(caller.who.clone(), &target.window, Some(&target.app));
                    Ok(answer)
                }
                Err(error) => {
                    read_failed(&error, &window);
                    Err(error)
                }
            }
        }
        Op::Get => match get(desktop, ax, caller, body).await {
            Ok((answer, entry)) => {
                audit.ax_read(caller.who.clone(), &entry.window, Some(&entry.app));
                Ok(answer)
            }
            Err(error) => {
                read_failed(&error, "");
                Err(error)
            }
        },
        Op::Act => {
            let reference = text_field(body, "ref").unwrap_or("");
            let action = text_field(body, "action").unwrap_or("").to_string();
            let entry = ax.lookup(&caller.owner, reference).ok();
            let answer = act(desktop, ax, caller, body).await;
            // How it went in, when it did; a failed act names no way.
            let (via, error) = match &answer {
                Ok((_, _, via)) => (Some(*via), None),
                Err(error) => (None, Some(error.clone())),
            };
            note_act(audit, caller, entry.as_ref(), &action, via, error.as_ref());
            answer.map(|(answer, _, _)| answer)
        }
        Op::Batch => {
            let answer = batch(desktop, ax, caller, body).await;
            match &answer {
                Ok((_, notes)) => {
                    for note in notes {
                        match note {
                            Note::Read { window, app } => audit.ax_read(caller.who.clone(), window, Some(app)),
                            Note::Act { entry, action, via, error } => {
                                note_act(audit, caller, entry.as_ref(), action, *via, error.as_ref())
                            }
                        }
                    }
                }
                Err(error) => {
                    let kind = if batch_acts(body) { "ax-act" } else { "ax-read" };
                    audit.note(caller.who.clone(), kind, Outcome::of_status(error.status), Some(&error.code_or_status()), json!({}));
                }
            }
            answer.map(|(answer, _)| answer)
        }
    }
}

fn window_app_hint(_window: &str) -> Option<String> {
    None
}

/// An `ax-act` row: app (process or bundle name), action, role, and how; never a value or name.
fn note_act(audit: &Audit, caller: &Caller, entry: Option<&RefEntry>, action: &str, via: Option<&str>, error: Option<&ApiError>) {
    let mut detail = json!({ "action": action });
    if let Some(via) = via {
        detail["via"] = json!(via);
    }
    if let Some(entry) = entry {
        detail["app"] = json!(entry.app);
        detail["role"] = json!(entry.role);
    }
    match error {
        None => audit.note(caller.who.clone(), "ax-act", Outcome::Ok, None, detail),
        Some(error) => audit.note(
            caller.who.clone(),
            "ax-act",
            Outcome::of_status(error.status),
            Some(&error.code_or_status()),
            detail,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted desktop: Link's windows, displays, and the CLI's answers in order.
    struct Fake {
        windows: Value,
        answers: Mutex<VecDeque<Value>>,
        calls: Mutex<Vec<Vec<String>>>,
        timeouts: Mutex<Vec<Duration>>,
        pasted: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answers: Vec<Value>) -> Self {
            Self {
                windows: json!({ "windows": [
                    { "id": "m:42", "pid": 7, "app": "com.apple.calculator", "appName": "Calculator", "appKey": "bundle:com.apple.calculator", "title": "Calculator", "display": "1", "minimized": false },
                    { "id": "w:2a", "pid": 9, "app": "notepad.exe", "appName": "Notepad", "title": "Untitled", "display": "1", "minimized": true },
                    { "id": "w:2a0c46", "pid": 11, "app": "excel.exe", "appName": "Excel", "title": "Book1", "display": "1", "minimized": false },
                ]}),
                answers: Mutex::new(answers.into()),
                calls: Mutex::new(Vec::new()),
                timeouts: Mutex::new(Vec::new()),
                pasted: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Desktop for Fake {
        async fn windows(&self) -> ApiResult<Value> {
            Ok(self.windows.clone())
        }
        async fn displays(&self) -> ApiResult<Vec<Value>> {
            Ok(vec![json!({ "id": "1", "x": 0, "y": 0, "width": 2880, "height": 1800, "scale": 2.0, "primary": true })])
        }
        async fn cli(&self, argv: Vec<String>, timeout: Duration) -> ApiResult<Value> {
            self.calls.lock().unwrap().push(argv);
            self.timeouts.lock().unwrap().push(timeout);
            let answer = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ApiError::coded(503, "unavailable", "no more answers"))?;
            // A failure of the runner itself (rtx-control or rtx-linkd's wait), not agent-desktop's.
            match answer.get("runner") {
                Some(failure) => {
                    let status = failure["status"].as_u64().unwrap_or(502) as u16;
                    Err(match failure["code"].as_str() {
                        Some(code) => ApiError::coded(status, code, "runner failed"),
                        None => ApiError::new(status, "runner failed"),
                    })
                }
                None => Ok(answer),
            }
        }
        async fn paste_at(&self, _bounds: &Value, text: &str) -> ApiResult<()> {
            self.pasted.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn ok(data: Value) -> Value {
        json!({ "version": "2.5", "ok": true, "data": data })
    }

    fn failed(code: &str, delivery: &str) -> Value {
        json!({ "version": "2.5", "ok": false, "error": { "code": code, "message": "no",
            "disposition": { "delivery": delivery, "retry": if delivery == "not_delivered" { "safe" } else { "unsafe" } } } })
    }

    fn caller(owner: &str) -> Caller {
        Caller {
            owner: owner.into(),
            who: Who { client_id: Some(owner.into()), actor: "agent".into(), via: "public".into(), ..Who::default() },
            control: true,
            local: false,
        }
    }

    fn tree() -> Value {
        json!({ "app": "Calculator", "complete": true, "ref_count": 3, "snapshot_id": "s1", "window": { "id": "w-42", "title": "Calculator" },
            "tree": { "role": "window", "name": "Calculator", "children": [
                { "ref_id": "@s1:e1", "role": "button", "name": "7", "states": [], "available_actions": ["Click"],
                  "bounds": { "x": 100.0, "y": 50.0, "width": 40.0, "height": 20.0 } },
                { "ref_id": "@s1:e2", "role": "textfield", "name": "Display", "value": "15", "states": ["focused"],
                  "available_actions": ["Click", "SetValue", "TypeText"] },
                { "ref_id": "@s1:e3", "role": "textfield", "name": "Password", "value": "hunter2", "states": ["secure"],
                  "available_actions": ["SetValue", "TypeText"] },
            ]}})
    }

    async fn snapshot_of(fake: &Fake, ax: &Ax, owner: &str) -> Value {
        snapshot(fake, ax, &caller(owner), &json!({ "window": "m:42" })).await.unwrap().0
    }

    fn block<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    #[test]
    fn agent_desktop_codes_map_to_links() {
        let map = |code: &str, delivery: &str, acting: bool| {
            let error = CliError::of(&failed(code, delivery));
            let api = map_error(&error, acting, None);
            let body = api.body();
            (api.status, body["code"].as_str().unwrap().to_string(), body["retry"].as_str().unwrap().to_string(),
             body.get("delivery").and_then(Value::as_str).map(str::to_string))
        };
        assert_eq!(map("STALE_REF", "not_delivered", false).1, "stale-ref");
        assert_eq!(map("SNAPSHOT_NOT_FOUND", "not_delivered", false).2, "refresh");
        assert_eq!(map("WINDOW_NOT_FOUND", "not_delivered", false).0, 404);
        assert_eq!(map("AMBIGUOUS_TARGET", "not_delivered", false).1, "ambiguous-target");
        let unresponsive = map_error(&CliError::of(&failed("APP_UNRESPONSIVE", "not_delivered")), false, None).body();
        assert_eq!((unresponsive["retry"].as_str(), unresponsive["retryAfterMs"].as_u64()), (Some("after"), Some(500)));
        assert_eq!(map("TIMEOUT", "not_delivered", false).2, "safe");
        assert_eq!(map("TIMEOUT", "delivery_uncertain", true).2, "no");
        assert_eq!(map("PERM_DENIED", "not_delivered", false).1, "permission-missing");
        assert_eq!(map("PLATFORM_NOT_SUPPORTED", "not_delivered", false).0, 501);
        assert_eq!(map("POLICY_DENIED", "not_delivered", true).1, "needs-pixels");
        assert_eq!(map("ACTION_NOT_SUPPORTED", "not_delivered", true).1, "needs-pixels");
        // ACTION_FAILED: its delivery passed through; retry safe only when nothing landed.
        assert_eq!(map("ACTION_FAILED", "not_delivered", true), (409, "ax-failed".into(), "safe".into(), Some("none".into())));
        assert_eq!(map("ACTION_FAILED", "delivered_unverified", true), (409, "ax-failed".into(), "no".into(), Some("unknown".into())));
        assert_eq!(map("SOMETHING_NEW", "not_delivered", false).1, "ax-failed");
        // Stricter where they disagree: an "unsafe" retry is never safe.
        let mut unsafe_retry = CliError::of(&failed("ACTION_FAILED", "not_delivered"));
        unsafe_retry.retry = Some("unsafe".into());
        assert_eq!(map_error(&unsafe_retry, true, None).body()["retry"], "no");
    }

    #[test]
    fn a_snapshot_is_normalised_and_hides_passwords() {
        let fake = Fake::new(vec![ok(tree())]);
        let ax = Ax::new(None);
        let answer = block(snapshot_of(&fake, &ax, "c1"));
        assert_eq!(answer["refs"], 3);
        assert_eq!(answer["window"], "m:42");
        assert_eq!(answer["appKey"], "bundle:com.apple.calculator");
        let children = answer["root"]["children"].as_array().unwrap();
        assert_eq!(children[0]["actions"], json!(["click"]));
        // Points on macOS, native pixels here: the display's scale applies.
        if cfg!(target_os = "macos") {
            assert_eq!(children[0]["bounds"], json!({ "display": "1", "x": 200, "y": 100, "width": 80, "height": 40 }));
        }
        assert_eq!(children[1]["value"], "15");
        assert_eq!(children[1]["actions"], json!(["click", "set-value", "type"]));
        let password = &children[2];
        assert_eq!(password["secret"], true);
        assert!(password.get("value").is_none());
        assert!(!answer.to_string().contains("hunter2"));
        let call = &fake.calls()[0];
        assert_eq!(call[0], "snapshot");
        assert!(call.contains(&"--window-id=w-42".to_string()));
        assert!(call.contains(&"--max-depth=50".to_string()));
        assert!(call.contains(&"--interactive-only".to_string()));
    }

    #[test]
    fn find_and_get_hide_passwords_too() {
        let fake = Fake::new(vec![
            ok(tree()),
            ok(json!({ "matches": [
                { "ref_id": "@s2:e3", "role": "textfield", "name": "Password", "value": "hunter2", "states": ["secure"] },
                { "ref_id": null, "role": "group", "name": "context" },
            ]})),
        ]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "c1"));
        let c1 = caller("c1");
        let (found, _) = block(find(&fake, &ax, &c1, &json!({ "window": "m:42", "role": "textfield" }))).unwrap();
        let matches = found["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["secret"], true);
        assert!(!found.to_string().contains("hunter2"));
        // get value of a password: null and secret, without asking agent-desktop at all.
        let calls = fake.calls().len();
        let (got, _) = block(get(&fake, &ax, &c1, &json!({ "ref": "@s1:e3", "property": "value" }))).unwrap();
        assert_eq!(got, json!({ "ref": "@s1:e3", "property": "value", "value": null, "secret": true }));
        assert_eq!(fake.calls().len(), calls);
    }

    #[test]
    fn a_batch_hides_passwords_and_reads_need_only_view() {
        let fake = Fake::new(vec![ok(tree())]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "c1"));
        let body = json!({ "window": "m:42", "steps": [{ "get": { "ref": "@s1:e3", "property": "value" } }] });
        assert!(!batch_acts(&body));
        let (answer, notes) = block(batch(&fake, &ax, &caller("c1"), &body)).unwrap();
        assert_eq!(answer["results"][0], json!({ "ok": true, "data": { "ref": "@s1:e3", "property": "value", "value": null, "secret": true } }));
        assert!(matches!(notes[0], Note::Read { .. }));
        assert!(batch_acts(&json!({ "steps": [{ "find": { "name": "7" }, "then": { "action": "click" } }] })));
        // A view-only client can read, never act.
        let mut viewer = caller("c1");
        viewer.control = false;
        std::env::set_var("RTX_LINK_AX", "1");
        assert!(admit(&viewer, Op::Batch, &body).is_ok());
        assert_eq!(admit(&viewer, Op::Act, &json!({})).unwrap_err().code.as_deref(), Some("view-only"));
    }

    #[test]
    fn refs_belong_to_whoever_got_them() {
        let fake = Fake::new(vec![ok(tree())]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "client-a"));
        assert!(ax.lookup("client-a", "@s1:e1").is_ok());
        // Another client, and another local caller, get stale-ref for the same ref.
        assert_eq!(ax.lookup("client-b", "@s1:e1").unwrap_err().code.as_deref(), Some("stale-ref"));
        let local_a = Caller::local(Some(r#"{"id":"w1:t1","name":"Fix invoices"}"#)).unwrap();
        let local_b = Caller::local(Some(r#"{"id":"w1:t2","name":"Other"}"#)).unwrap();
        assert_eq!(local_a.owner, "local:w1:t1");
        assert_eq!(local_a.who.client_id.as_deref(), Some("local:w1:t1"));
        assert_eq!(local_a.who.client_name.as_deref(), Some("Fix invoices"));
        assert_eq!((local_a.who.actor.as_str(), local_a.who.via.as_str()), ("agent", "local"));
        let fake = Fake::new(vec![ok(tree())]);
        block(snapshot(&fake, &ax, &local_a, &json!({ "window": "m:42" }))).unwrap();
        assert!(ax.lookup(&local_a.owner, "@s1:e2").is_ok());
        assert!(ax.lookup(&local_b.owner, "@s1:e2").is_err());
        assert!(ax.lookup("client-a", "not-a-ref").is_err());
        // The header is required, JSON, and small.
        assert!(Caller::local(None).is_err());
        assert!(Caller::local(Some("thread")).is_err());
        assert!(Caller::local(Some(&format!(r#"{{"id":"{}"}}"#, "x".repeat(600)))).is_err());
        assert!(Caller::local(Some(r#"{"id":""}"#)).is_err());
    }

    fn entry(actions: &[&str], secret: bool, bounds: bool) -> RefEntry {
        RefEntry {
            owner: "c1".into(),
            window: "m:42".into(),
            cli_window: "w-42".into(),
            app: "com.apple.TextEdit".into(),
            role: "textfield".into(),
            actions: Some(actions.iter().map(|a| a.to_string()).collect()),
            secret,
            bounds: bounds.then(|| json!({ "display": "1", "x": 10, "y": 10, "width": 100, "height": 20 })),
            at: Instant::now(),
        }
    }

    #[test]
    fn type_falls_back_to_set_value_then_paste_then_pixels() {
        // 1. type works.
        let fake = Fake::new(vec![ok(json!({ "disposition": { "delivery": "delivered_verified" } }))]);
        let acted = block(act_on(&fake, &entry(&["type"], false, false), "@s1:e2", "type", Some("abc"), true, Budget::default())).unwrap();
        assert_eq!((acted.via, acted.delivery), ("type", Delivery::Done));
        // 2. refused, nothing delivered: get, set-value(old + text), get to check.
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": "Hello " })),
            ok(json!({ "disposition": { "delivery": "delivered_verified" } })),
            ok(json!({ "value": "Hello world" })),
        ]);
        let acted = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("world"), true, Budget::default())).unwrap();
        assert_eq!(acted.via, "set-value");
        assert_eq!(fake.calls()[2], vec!["set-value", "--", "@s1:e2", "Hello world"]);
        // 3. not settable: a click at its centre and a paste.
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let acted = block(act_on(&fake, &entry(&["type"], false, true), "@s1:e2", "type", Some("world"), true, Budget::default())).unwrap();
        assert_eq!(acted.via, "paste");
        assert_eq!(*fake.pasted.lock().unwrap(), vec!["world".to_string()]);
        // ...never for an agent on this computer (the person's own pointer), nor without bounds.
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let error = block(act_on(&fake, &entry(&["type"], false, true), "@s1:e2", "type", Some("x"), false, Budget::default())).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("needs-pixels"));
        assert!(fake.pasted.lock().unwrap().is_empty());
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let error = block(act_on(&fake, &entry(&["type"], false, false), "@s1:e2", "type", Some("x"), true, Budget::default())).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("needs-pixels"));
        // With no bounds to click, the suggestion doesn't name any (GNOME Wayland).
        assert_eq!(error.suggestion.as_deref(), Some(NO_BOUNDS));
        // A password is never read back for the set-value path: straight to paste.
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let acted = block(act_on(&fake, &entry(&["set-value"], true, true), "@s1:e3", "type", Some("pw"), true, Budget::default())).unwrap();
        assert_eq!(acted.via, "paste");
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn type_stops_when_it_may_have_landed() {
        // Typed but unverified (Electron on macOS): stop, no fallback, delivery unknown.
        let fake = Fake::new(vec![ok(json!({ "disposition": { "delivery": "delivered_unverified" } }))]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("abc"), true, Budget::default())).unwrap_err();
        let body = error.body();
        assert_eq!((body["code"].as_str(), body["delivery"].as_str(), body["retry"].as_str()), (Some("ax-failed"), Some("unknown"), Some("no")));
        assert!(body["suggestion"].as_str().unwrap().contains("set-value"));
        assert_eq!(fake.calls().len(), 1);
        assert!(fake.pasted.lock().unwrap().is_empty());
        // A failure that may have landed stops too.
        let fake = Fake::new(vec![failed("ACTION_FAILED", "delivered_unverified")]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("abc"), true, Budget::default())).unwrap_err();
        assert_eq!(error.delivery, Some(Delivery::Unknown));
        assert_eq!(fake.calls().len(), 1);
        // set-value may have landed half way: stop before pasting.
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": "a" })),
            failed("ACTION_FAILED", "delivery_uncertain"),
        ]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("b"), true, Budget::default())).unwrap_err();
        assert_eq!(error.delivery, Some(Delivery::Unknown));
        assert!(fake.pasted.lock().unwrap().is_empty());
    }

    #[test]
    fn type_keeps_a_value_over_4_kb_whole() {
        // 5000+ bytes with a 3-byte '€' across the 4096-byte cut: set-value gets all of it plus
        // the text, the check compares all of it, and only the answer is cut (at a char boundary).
        let old = format!("{}€{}", "a".repeat(4095), "b".repeat(1000));
        let wanted = format!("{old}x");
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": old })),
            ok(json!({ "disposition": { "delivery": "delivered_verified" } })),
            ok(json!({ "value": wanted })),
        ]);
        let acted = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("x"), true, Budget::default())).unwrap();
        assert_eq!((acted.via, acted.delivery), ("set-value", Delivery::Done));
        assert_eq!(fake.calls()[2], vec!["set-value".to_string(), "--".into(), "@s1:e2".into(), wanted.clone()]);
        assert_eq!(acted.value, Some(json!("a".repeat(4095))));
        // A read-back cut at 4 KB is not what was asked: never reported as done.
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": old })),
            ok(json!({ "disposition": { "delivery": "delivered_verified" } })),
            ok(json!({ "value": truncated(&wanted).0 })),
        ]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("x"), true, Budget::default())).unwrap_err();
        assert_eq!(error.delivery, Some(Delivery::Unknown));
        // The text itself carries the result over 4 KB, ending in a multi-byte char.
        let text = format!("{}é", "y".repeat(4094));
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": "z" })),
            ok(json!({ "disposition": { "delivery": "delivered_verified" } })),
            ok(json!({ "value": format!("z{text}") })),
        ]);
        let acted = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some(&text), true, Budget::default())).unwrap();
        assert_eq!(fake.calls()[2][3], format!("z{text}"));
        assert_eq!(acted.value, Some(json!(format!("z{}", "y".repeat(4094)))));
    }

    #[test]
    fn type_never_rewrites_a_value_it_cannot_read_whole() {
        // At agent-desktop's 64 KiB limit the value may be cut: no set-value; nothing written,
        // so the chain goes on to the paste, or needs-pixels with nothing delivered.
        let long = "a".repeat(WHOLE_TEXT - 1);
        let fake = Fake::new(vec![failed("ACTION_NOT_SUPPORTED", "not_delivered"), ok(json!({ "value": long }))]);
        let acted = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("x"), true, Budget::default())).unwrap();
        assert_eq!(acted.via, "paste");
        assert!(fake.calls().iter().all(|call| call[0] != "set-value"));
        let fake = Fake::new(vec![failed("ACTION_NOT_SUPPORTED", "not_delivered"), ok(json!({ "value": long }))]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("x"), false, Budget::default())).unwrap_err();
        assert_eq!((error.code.as_deref(), error.delivery), (Some("needs-pixels"), Some(Delivery::None)));
        assert!(fake.calls().iter().all(|call| call[0] != "set-value"));
        // A value agent-desktop couldn't read (unknown, so null): the same.
        let fake = Fake::new(vec![failed("ACTION_NOT_SUPPORTED", "not_delivered"), ok(json!({ "value": null }))]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("x"), false, Budget::default())).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("needs-pixels"));
        assert_eq!(fake.calls().len(), 2);
        assert!(appendable(&"a".repeat(4096), "x"));
        assert!(!appendable(&"a".repeat(WHOLE_TEXT - 1), "x"));
    }

    #[test]
    fn get_cuts_only_the_answer_at_4_kb() {
        let long = format!("{}€{}", "a".repeat(4095), "b".repeat(1000));
        let fake = Fake::new(vec![ok(tree()), ok(json!({ "property": "value", "ref": "@s1:e2", "value": long }))]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "c1"));
        let (got, _) = block(get(&fake, &ax, &caller("c1"), &json!({ "ref": "@s1:e2" }))).unwrap();
        assert_eq!(got["value"], json!("a".repeat(4095)));
        assert_eq!(got["truncated"], json!(true));
        let fake = Fake::new(vec![ok(tree()), ok(json!({ "property": "value", "ref": "@s1:e2", "value": "short" }))]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "c1"));
        let (got, _) = block(get(&fake, &ax, &caller("c1"), &json!({ "ref": "@s1:e2" }))).unwrap();
        assert_eq!(got["value"], "short");
        assert!(got.get("truncated").is_none());
    }

    #[test]
    fn an_action_the_element_lacks_needs_pixels() {
        let fake = Fake::new(vec![]);
        let error = block(act_on(&fake, &entry(&["click"], false, true), "@s1:e1", "toggle", None, true, Budget::default())).unwrap_err();
        let body = error_json(&error);
        assert_eq!(body["code"], "needs-pixels");
        assert_eq!(body["display"], "1");
        assert_eq!(body["bounds"]["width"], 100);
        assert!(fake.calls().is_empty());
        // Never anything outside the allow-list.
        assert!(block(act_on(&fake, &entry(&["click"], false, true), "@s1:e1", "press", None, true, Budget::default())).is_err());
    }

    #[test]
    fn the_screen_locked_refusal_reaches_the_caller() {
        // rtx-control refuses ax while the screen is locked (its `ax` op): 423, retry after.
        struct Locked;
        impl Desktop for Locked {
            async fn windows(&self) -> ApiResult<Value> {
                Err(ApiError::coded(423, "screen-locked", "The screen is locked"))
            }
            async fn displays(&self) -> ApiResult<Vec<Value>> {
                Ok(Vec::new())
            }
            async fn cli(&self, _: Vec<String>, _: Duration) -> ApiResult<Value> {
                Err(ApiError::coded(423, "screen-locked", "The screen is locked"))
            }
            async fn paste_at(&self, _: &Value, _: &str) -> ApiResult<()> {
                Ok(())
            }
        }
        let ax = Ax::new(None);
        let error = block(snapshot(&Locked, &ax, &caller("c1"), &json!({ "window": "m:42" }))).unwrap_err();
        assert_eq!((error.status, error.code.as_deref()), (423, Some("screen-locked")));
        assert_eq!(error.body()["retry"], "after");
        let error = block(apps(&Locked, &ax)).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("screen-locked"));
    }

    #[test]
    fn windows_map_to_agent_desktops_and_minimized_ones_are_not_offered() {
        let ax = Ax::new(None);
        let fake = Fake::new(vec![]);
        let target = block(target(&fake, &ax, "m:42", Budget::default())).unwrap();
        assert_eq!(target.cli_window, "w-42");
        assert_eq!(block(super::target(&fake, &ax, "w:2a", Budget::default())).unwrap_err().code.as_deref(), Some("window-not-found"));
        assert_eq!(block(super::target(&fake, &ax, "m:1", Budget::default())).unwrap_err().code.as_deref(), Some("window-not-found"));
        assert_eq!(block(super::target(&fake, &ax, "nope", Budget::default())).unwrap_err().status, 400);
        // An empty tree is no-tree, with a reason.
        let fake = Fake::new(vec![ok(json!({ "complete": true, "snapshot_id": "s9", "tree": { "role": "window" } }))]);
        let error = block(snapshot(&fake, &ax, &caller("c1"), &json!({ "window": "m:42" }))).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("no-tree"));
        assert!(error.details.unwrap()["reason"].is_string());
        // Depth: 1–60 asked, at most agent-desktop's 50 sent.
        assert_eq!(depth_of(&json!({ "maxDepth": 60 })).unwrap(), 50);
        assert!(depth_of(&json!({ "maxDepth": 0 })).is_err());
    }

    #[test]
    fn activity_rows_hold_no_names_or_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(dir.path()).unwrap());
        let audit = Audit::start(store);
        let ax = Ax::new(None);
        std::env::set_var("RTX_LINK_AX", "1");
        let fake = Fake::new(vec![ok(tree()), ok(json!({ "disposition": { "delivery": "delivered_verified" }, "post_state": { "value": "hunter2" } }))]);
        let c1 = caller("c1");
        block(run(&fake, &ax, &audit, &c1, Op::Snapshot, &json!({ "window": "m:42" }))).unwrap();
        block(run(&fake, &ax, &audit, &c1, Op::Act, &json!({ "ref": "@s1:e3", "action": "set-value", "value": "hunter2" }))).unwrap();
        audit.flush_reads(true);
        audit.flush();
        let page = audit.page(&crate::audit::Query { limit: 200, ..Default::default() }).unwrap();
        let text = page.to_string();
        for secret in ["hunter2", "Password", "Calculator\"", "Display"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let kinds: Vec<&str> = page["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"ax-act") && kinds.contains(&"ax-read"));
    }

    /// rtx-control's failure (or rtx-linkd's own wait running out), as the Fake hands it back.
    fn runner(status: u16, code: Option<&str>) -> Value {
        json!({ "runner": { "status": status, "code": code } })
    }

    fn fields(error: &ApiError) -> (Option<String>, Option<String>, Option<String>) {
        let body = error.body();
        let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
        (text("code"), text("retry"), text("delivery"))
    }

    fn strs(code: &str, retry: &str, delivery: &str) -> (Option<String>, Option<String>, Option<String>) {
        (Some(code.into()), Some(retry.into()), Some(delivery.into()))
    }

    #[test]
    fn an_act_that_times_out_may_have_happened() {
        // rtx-control killed the CLI at its deadline, and rtx-linkd's own wait ran out (no code).
        for late in [runner(504, Some("timeout")), runner(504, None)] {
            let fake = Fake::new(vec![late.clone()]);
            let error = block(act_on(&fake, &entry(&["click"], false, true), "@s1:e1", "click", None, true, Budget::default())).unwrap_err();
            assert_eq!(fields(&error), strs("timeout", "no", "unknown"));
            // type too: never a second attempt after a timeout.
            let fake = Fake::new(vec![late.clone()]);
            let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("abc"), true, Budget::default())).unwrap_err();
            assert_eq!(fields(&error), strs("timeout", "no", "unknown"));
            assert_eq!(fake.calls().len(), 1);
            assert!(fake.pasted.lock().unwrap().is_empty());
            // The set-value step of the chain as well.
            let fake = Fake::new(vec![failed("ACTION_NOT_SUPPORTED", "not_delivered"), ok(json!({ "value": "a" })), late.clone()]);
            let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("b"), true, Budget::default())).unwrap_err();
            assert_eq!(fields(&error), strs("timeout", "no", "unknown"));
            assert!(fake.pasted.lock().unwrap().is_empty());
        }
        // agent-desktop's own TIMEOUT on an action: unknown, even when it says not delivered.
        let api = map_error(&CliError::of(&failed("TIMEOUT", "not_delivered")), true, None);
        assert_eq!(fields(&api), strs("timeout", "no", "unknown"));
        // A read that times out stays safe to repeat.
        let fake = Fake::new(vec![ok(tree()), runner(504, Some("timeout"))]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "c1"));
        let error = block(get(&fake, &ax, &caller("c1"), &json!({ "ref": "@s1:e2", "property": "value" }))).unwrap_err();
        assert_eq!(fields(&error), strs("timeout", "safe", "none"));
    }

    #[test]
    fn a_crashed_runner_is_unavailable_for_reads_and_unknown_for_acts() {
        // rtx-control's `ax-crashed` (backend.rs passes the code on with a 409).
        let crashed = || runner(409, Some("ax-crashed"));
        let ax = Ax::new(None);
        let fake = Fake::new(vec![ok(tree()), crashed()]);
        block(snapshot_of(&fake, &ax, "c1"));
        let error = block(get(&fake, &ax, &caller("c1"), &json!({ "ref": "@s1:e2", "property": "value" }))).unwrap_err();
        assert_eq!(error.status, 503);
        let body = error.body();
        assert_eq!((body["code"].as_str(), body["retry"].as_str()), (Some("unavailable"), Some("after")));
        assert_eq!(body["retryAfterMs"], 2000);
        let fake = Fake::new(vec![crashed()]);
        let error = block(snapshot(&fake, &ax, &caller("c1"), &json!({ "window": "m:42" }))).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("unavailable"));
        // An action may have happened before it crashed.
        let fake = Fake::new(vec![crashed()]);
        let error = block(act_on(&fake, &entry(&["click"], false, true), "@s1:e1", "click", None, true, Budget::default())).unwrap_err();
        assert_eq!(error.status, 409);
        assert_eq!(fields(&error), strs("ax-failed", "no", "unknown"));
        let fake = Fake::new(vec![crashed()]);
        let error = block(act_on(&fake, &entry(&["type"], false, true), "@s1:e2", "type", Some("x"), true, Budget::default())).unwrap_err();
        assert_eq!(fields(&error), strs("ax-failed", "no", "unknown"));
        assert!(fake.pasted.lock().unwrap().is_empty());
    }

    #[test]
    fn passwords_are_hidden_on_every_marker() {
        // The `secure` state agent-desktop 1.0.2 sets on each platform, and password roles.
        for raw in [
            json!({ "role": "textfield", "states": ["secure"], "value": "hunter2" }),
            json!({ "role": "textfield", "states": ["focused", "secure", "haspopup"], "value": "hunter2" }),
            json!({ "role": "password text", "value": "hunter2" }),
            json!({ "role": "AXSecureTextField", "value": "hunter2" }),
            json!({ "role": "textfield", "subrole": "AXSecureTextField", "value": "hunter2" }),
        ] {
            assert!(is_secret(&raw), "{raw}");
            let mut walk = Walk { owner: "c1", window: "m:42", cli_window: "w-42", app: "a", displays: &[], refs: Vec::new(), nodes: 0 };
            let node = walk.node(&raw, false);
            assert_eq!(node["secret"], true);
            assert!(!node.to_string().contains("hunter2"), "{node}");
        }
        // Never a guess from the name, and an unmarked field keeps its value.
        assert!(!is_secret(&json!({ "role": "textfield", "name": "Password", "value": "x" })));
        assert!(!is_secret(&json!({ "role": "textfield", "states": ["focused"] })));
        // T30 (GTK 4.22, agent-desktop 1.0.2): Gtk.PasswordEntry, an InputPurpose.PASSWORD entry
        // and Adw.PasswordEntryRow are `secure`; an Entry with visibility off shows only bullets.
        let found = json!({ "matches": [
            { "interactive": true, "ref_id": "@s3:e1", "role": "textfield", "states": ["secure"], "value": null },
            { "interactive": true, "ref_id": "@s3:e2", "role": "textfield", "states": ["haspopup"], "value": "••••••••••" },
            { "interactive": true, "ref_id": "@s3:e3", "role": "textfield", "states": ["secure", "haspopup"], "value": null },
            { "interactive": true, "name": "AdwRow", "ref_id": "@s3:e4", "role": "textfield", "states": ["secure"], "value": null },
        ]});
        let marked: Vec<bool> = found["matches"].as_array().unwrap().iter().map(is_secret).collect();
        assert_eq!(marked, vec![true, false, true, true]);
        // What an action reads back is hidden when the state after it says secure.
        let answer = json!({ "post_state": { "value": "hunter2", "states": ["secure"] } });
        assert_eq!(read_back(&answer, &entry(&["set-value"], false, false)), None);
        let answer = json!({ "post_state": { "value": "hello" } });
        assert_eq!(read_back(&answer, &entry(&["set-value"], false, false)), Some(json!("hello")));
    }

    #[test]
    fn get_name_reads_the_name_never_the_value() {
        let fake = Fake::new(vec![ok(tree()), ok(json!({ "property": "title", "ref": "@s1:e2", "value": "Display" }))]);
        let ax = Ax::new(None);
        block(snapshot_of(&fake, &ax, "c1"));
        let (got, _) = block(get(&fake, &ax, &caller("c1"), &json!({ "ref": "@s1:e2", "property": "name" }))).unwrap();
        assert_eq!(got["value"], "Display");
        // agent-desktop's `text` is a field's value; `title` is the name.
        assert_eq!(fake.calls()[1], vec!["get", "--property=title", "--", "@s1:e2"]);
    }

    #[test]
    fn the_queue_holds_at_most_four_waiting_calls() {
        block(async {
            let ax = Arc::new(Ax::new(None));
            let running = ax.enter().await.unwrap();
            let mut waiting = Vec::new();
            for _ in 0..MAX_QUEUED {
                let ax = ax.clone();
                waiting.push(tokio::spawn(async move { ax.enter().await.map(|_| ()) }));
            }
            while ax.waiting.load(Ordering::SeqCst) < MAX_QUEUED {
                tokio::task::yield_now().await;
            }
            // A fifth would wait: busy at once.
            let busy = ax.enter().await.unwrap_err();
            assert_eq!((busy.status, busy.code.as_deref()), (503, Some("busy")));
            assert_eq!(busy.body()["retryAfterMs"], 500);
            assert_eq!(ax.waiting.load(Ordering::SeqCst), MAX_QUEUED);
            drop(running);
            for call in waiting {
                assert!(call.await.unwrap().is_ok());
            }
        });
    }

    #[test]
    fn a_ref_from_find_gets_its_bounds_when_needs_pixels_wants_them() {
        // find gives a ref without bounds or actions (agent-desktop 1.0.2); the element refuses
        // the toggle, and Link reads its bounds for the pixel fallback.
        let fake = Fake::new(vec![
            ok(json!({ "matches": [{ "interactive": true, "ref_id": "@s2:e5", "role": "button", "name": "Bold", "states": [] }] })),
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "property": "bounds", "ref": "@s2:e5", "value": { "x": 100.0, "y": 50.0, "width": 40.0, "height": 20.0 } })),
        ]);
        let ax = Ax::new(None);
        let c1 = caller("c1");
        let (found, _) = block(find(&fake, &ax, &c1, &json!({ "window": "m:42", "name": "Bold" }))).unwrap();
        assert!(found["matches"][0].get("bounds").is_none());
        let error = block(act(&fake, &ax, &c1, &json!({ "ref": "@s2:e5", "action": "toggle" }))).unwrap_err();
        let body = error_json(&error);
        assert_eq!(body["code"], "needs-pixels");
        assert_eq!(body["display"], "1");
        assert!(body["bounds"]["width"].as_i64().unwrap() > 0);
        assert_eq!(fake.calls()[2], vec!["get", "--property=bounds", "--", "@s2:e5"]);
        // A match that carries bounds has them placed on a display.
        let fake = Fake::new(vec![ok(json!({ "matches": [
            { "interactive": true, "ref_id": "@s4:e1", "role": "button", "name": "OK", "states": [],
              "bounds": { "x": 10.0, "y": 10.0, "width": 30.0, "height": 10.0 } }] }))]);
        let (found, _) = block(find(&fake, &ax, &c1, &json!({ "window": "m:42", "name": "OK" }))).unwrap();
        assert_eq!(found["matches"][0]["bounds"]["display"], "1");
    }

    #[test]
    fn a_windows_window_goes_to_agent_desktop_in_decimal() {
        // Link's `w:` id is the HWND in hex; agent-desktop 1.0.2 writes and parses `w-<decimal>`.
        let ax = Ax::new(None);
        let target = block(target(&Fake::new(vec![]), &ax, "w:2a0c46", Budget::default())).unwrap();
        assert_eq!(target.cli_window, "w-2755654");
    }

    #[test]
    fn find_then_acts_on_the_one_named_exactly() {
        let ax = Ax::new(None);
        let c1 = caller("c1");
        let clears = ok(json!({ "matches": [
            { "interactive": true, "ref_id": "@f1:e1", "role": "button", "name": "Clear all memory", "states": [] },
            { "interactive": true, "ref_id": "@f1:e2", "role": "button", "name": "Clear entry", "states": [] },
            { "interactive": true, "ref_id": "@f1:e3", "role": "button", "name": " clear ", "states": [] },
        ]}));
        let body = json!({ "window": "m:42", "steps": [{ "find": { "name": "Clear" }, "then": { "action": "click" } }] });
        let fake = Fake::new(vec![clears.clone(), ok(json!({ "disposition": { "delivery": "delivered_verified" } }))]);
        let (answer, _) = block(batch(&fake, &ax, &c1, &body)).unwrap();
        assert_eq!(answer["results"][0]["ok"], true, "{answer}");
        assert_eq!(answer["results"][0]["data"]["ref"], "@f1:e3");
        assert_eq!(fake.calls()[1], vec!["click", "--", "@f1:e3"]);
        // No exact one (or two of them): still ambiguous.
        let body = json!({ "window": "m:42", "steps": [{ "find": { "name": "Cle" }, "then": { "action": "click" } }] });
        let fake = Fake::new(vec![clears]);
        let (answer, _) = block(batch(&fake, &ax, &c1, &body)).unwrap();
        assert_eq!(answer["results"][0]["error"]["code"], "ambiguous-target");
        // `exact: true` goes to agent-desktop as --exact.
        let fake = Fake::new(vec![ok(json!({ "matches": [] }))]);
        block(find(&fake, &ax, &c1, &json!({ "window": "m:42", "name": "Clear", "exact": true }))).unwrap();
        assert!(fake.calls()[0].contains(&"--exact".to_string()), "{:?}", fake.calls());
    }

    #[test]
    fn find_matches_say_what_their_role_takes() {
        let ax = Ax::new(None);
        let fake = Fake::new(vec![ok(json!({ "matches": [
            { "interactive": true, "ref_id": "@f2:e1", "role": "button", "name": "7", "states": [] },
            { "interactive": true, "ref_id": "@f2:e2", "role": "textfield", "name": "To", "states": [] },
            { "interactive": false, "ref_id": "@f2:e3", "role": "statictext", "name": "7", "states": [] },
        ]}))]);
        let (found, _) = block(find(&fake, &ax, &caller("c1"), &json!({ "window": "m:42", "name": "7" }))).unwrap();
        assert_eq!(found["matches"][0]["actions"], json!(["click"]));
        assert_eq!(found["matches"][1]["actions"], json!(["click", "set-value", "type"]));
        assert_eq!(found["matches"][2]["actions"], json!([]));
        // Its ref still allows any action: what agent-desktop allows wasn't said.
        assert!(ax.lookup("c1", "@f2:e2").unwrap().actions.is_none());
    }

    #[test]
    fn a_batch_has_6_s_a_step_from_20_to_60_s() {
        assert_eq!(batch_timeout(0), Duration::from_secs(20));
        assert_eq!(batch_timeout(1), Duration::from_secs(20));
        assert_eq!(batch_timeout(3), Duration::from_secs(20));
        assert_eq!(batch_timeout(4), Duration::from_secs(24));
        assert_eq!(batch_timeout(7), Duration::from_secs(42));
        assert_eq!(batch_timeout(10), Duration::from_secs(60));
        assert_eq!(batch_timeout(50), Duration::from_secs(60));
    }

    #[test]
    fn a_batch_step_gets_only_what_is_left_of_the_20_s() {
        let ax = Ax::new(None);
        let c1 = caller("c1");
        let fake = Fake::new(vec![ok(tree())]);
        block(snapshot_of(&fake, &ax, "c1"));
        let body = json!({ "window": "m:42", "steps": [
            { "get": { "ref": "@s1:e2", "property": "value" } },
            { "act": { "ref": "@s1:e1", "action": "click" } },
        ]});
        // 3 s left: the get's 8 s are capped to them.
        let fake = Fake::new(vec![ok(json!({ "value": "15" })), ok(json!({ "disposition": { "delivery": "delivered_verified" } }))]);
        let (answer, _) = block(batch_until(&fake, &ax, &c1, &body, Instant::now() + Duration::from_secs(3))).unwrap();
        assert_eq!(answer["results"][0]["ok"], true);
        let timeouts = fake.timeouts.lock().unwrap().clone();
        assert_eq!(timeouts.len(), 2);
        assert!(timeouts.iter().all(|t| *t <= Duration::from_secs(3) && *t > Duration::from_secs(2)), "{timeouts:?}");
        // Nothing left: the step isn't run, and says so.
        let fake = Fake::new(vec![]);
        let (answer, notes) = block(batch_until(&fake, &ax, &c1, &body, Instant::now())).unwrap();
        assert_eq!(answer["results"].as_array().unwrap().len(), 1);
        assert_eq!(answer["results"][0]["error"]["code"], "timeout");
        assert_eq!(answer["results"][0]["error"]["delivery"], "none");
        assert!(fake.calls().is_empty() && notes.is_empty());
        // A step's own deadline is what is left, not its full 8 s.
        assert_eq!(Budget::until(Instant::now() + Duration::from_millis(500)).cap(ACT_TIMEOUT).map(|t| t <= Duration::from_millis(500)), Some(true));
        assert_eq!(Budget::until(Instant::now()).cap(ACT_TIMEOUT), None);
        assert_eq!(Budget::default().cap(ACT_TIMEOUT), Some(ACT_TIMEOUT));
    }

    #[test]
    fn a_failed_act_is_logged_without_a_way_in() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(dir.path()).unwrap());
        let audit = Audit::start(store);
        let ax = Ax::new(None);
        std::env::set_var("RTX_LINK_AX", "1");
        let fake = Fake::new(vec![
            ok(tree()),
            failed("ACTION_FAILED", "not_delivered"),
            ok(json!({ "disposition": { "delivery": "delivered_verified" } })),
        ]);
        let c1 = caller("c1");
        block(run(&fake, &ax, &audit, &c1, Op::Snapshot, &json!({ "window": "m:42" }))).unwrap();
        // A failed click: no `via`. A set-value that worked: `via: set-value`.
        assert!(block(run(&fake, &ax, &audit, &c1, Op::Act, &json!({ "ref": "@s1:e1", "action": "click" }))).is_err());
        block(run(&fake, &ax, &audit, &c1, Op::Act, &json!({ "ref": "@s1:e2", "action": "set-value", "value": "1" }))).unwrap();
        audit.flush();
        let page = audit.page(&crate::audit::Query { limit: 200, ..Default::default() }).unwrap();
        let acts: Vec<&Value> = page["entries"].as_array().unwrap().iter().filter(|e| e["kind"] == "ax-act").collect();
        assert_eq!(acts.len(), 2, "{page}");
        let failed_click = acts.iter().find(|e| e["detail"]["action"] == "click").unwrap();
        assert!(failed_click["detail"].get("via").is_none(), "{failed_click}");
        let set = acts.iter().find(|e| e["detail"]["action"] == "set-value").unwrap();
        assert_eq!(set["detail"]["via"], "set-value");
    }
}
