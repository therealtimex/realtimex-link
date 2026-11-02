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
const MAX_STEPS: usize = 10;
const MAX_QUEUED: usize = 4;
const QUEUE_WAIT: Duration = Duration::from_secs(5);
const BATCH_TIMEOUT: Duration = Duration::from_secs(20);
const DEPTH_DEFAULT: u64 = 30;
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
        "ax-failed" => {
            api.retry = Some(if delivery == Delivery::None {
                Retry::Safe
            } else {
                Retry::No
            });
        }
        _ => {}
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

fn is_secret(raw: &Value) -> bool {
    raw.get("states")
        .and_then(Value::as_array)
        .is_some_and(|states| states.iter().any(|s| s.as_str() == Some("secure")))
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
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED + 1 {
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
        let answer = self
            .backend
            .call_unstarted("ax", params, timeout + Duration::from_secs(2))
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
/// number (`w-<CGWindowID>`, `w-<HWND, decimal>`); X11 by pid (and title when one app has
/// several), since agent-desktop lists AT-SPI windows there; `a:` ids from the Wayland list.
pub async fn target<D: Desktop>(desktop: &D, ax: &Ax, id: &str) -> ApiResult<Target> {
    let not_found = || {
        ApiError::coded(
            404,
            "window-not-found",
            "No such window here: list the windows again",
        )
    };
    if id.starts_with("a:") {
        let cli_window = ax.wayland_window(id).ok_or_else(not_found)?;
        let apps = wayland_windows(desktop, ax).await?;
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
            let list = data(&desktop.cli(vec!["list-windows".into()], LIST_TIMEOUT).await?)
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
    let list = data(&desktop.cli(vec!["list-windows".into()], LIST_TIMEOUT).await?)
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

/// `POST ax/snapshot {window, maxDepth=30, interactive=true, root?}`.
pub async fn snapshot<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, Target)> {
    let window = text_field(body, "window").ok_or_else(|| ApiError::bad_request("window is required"))?;
    let depth = depth_of(body)?;
    let target = target(desktop, ax, window).await?;
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
    let envelope = desktop.cli(argv, SNAPSHOT_TIMEOUT).await?;
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

/// `POST ax/find {window, role?, name?, text?, limit=20 (≤50)}` → `{matches}` (no children).
pub async fn find<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, Target)> {
    let window = text_field(body, "window").ok_or_else(|| ApiError::bad_request("window is required"))?;
    let limit = match body.get("limit") {
        None | Some(Value::Null) => FIND_LIMIT_DEFAULT,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=FIND_LIMIT_MAX).contains(n))
            .ok_or_else(|| ApiError::bad_request("limit must be from 1 to 50"))?,
    };
    let target = target(desktop, ax, window).await?;
    let matches = find_in(desktop, ax, caller, &target, body, limit).await?;
    Ok((json!({ "matches": matches }), target))
}

async fn find_in<D: Desktop>(
    desktop: &D,
    ax: &Ax,
    caller: &Caller,
    target: &Target,
    query: &Value,
    limit: u64,
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
    argv.push(format!("--limit={limit}"));
    let answer = data(&desktop.cli(argv, READ_TIMEOUT).await?).map_err(|e| map_error(&e, false, None))?;
    let raw: Vec<Value> = match answer.get("matches").and_then(Value::as_array) {
        Some(list) => list.clone(),
        None => answer.get("match").cloned().into_iter().filter(|m| !m.is_null()).collect(),
    };
    let displays: Vec<Value> = Vec::new();
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
        .map(|m| walk.node(m, false))
        .collect();
    ax.remember(std::mem::take(&mut walk.refs));
    Ok(matches)
}

/// `POST ax/get {ref, property: value|name|states|bounds}` → `{ref, property, value, secret?}`.
pub async fn get<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, RefEntry)> {
    let reference = text_field(body, "ref").ok_or_else(|| ApiError::bad_request("ref is required"))?;
    let property = text_field(body, "property").unwrap_or("value");
    let entry = ax.lookup(&caller.owner, reference)?;
    let value = get_property(desktop, &entry, reference, property).await?;
    let mut answer = json!({ "ref": reference, "property": property, "value": value });
    if entry.secret && property == "value" {
        answer["secret"] = json!(true);
    }
    Ok((answer, entry))
}

async fn get_property<D: Desktop>(desktop: &D, entry: &RefEntry, reference: &str, property: &str) -> ApiResult<Value> {
    let cli_property = match property {
        "value" => "value",
        "name" => "text",
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
    let answer = data(&desktop.cli(argv, READ_TIMEOUT).await?).map_err(|e| map_error(&e, false, None))?;
    let value = answer.get("value").cloned().unwrap_or(Value::Null);
    Ok(match property {
        "bounds" => {
            let displays = desktop.displays().await.unwrap_or_default();
            bounds_of(&json!({ "bounds": value }), &displays).unwrap_or(Value::Null)
        }
        "value" | "name" => match value.as_str() {
            Some(text) => json!(truncated(text).0),
            None => value,
        },
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

/// The type fallback chain and every other action (§2.4.3, §2.4.5).
pub async fn act_on<D: Desktop>(
    desktop: &D,
    entry: &RefEntry,
    reference: &str,
    action: &str,
    value: Option<&str>,
    pixels_allowed: bool,
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
    let needs_pixels = || {
        let mut error = ApiError::coded(422, "needs-pixels", format!("This element can't {action} through accessibility"));
        if let Some(bounds) = &entry.bounds {
            error = error.with_details(json!({ "bounds": bounds, "display": bounds.get("display") }));
        }
        error.with_delivery(Delivery::None)
    };
    let advertised = |name: &str| entry.actions.as_ref().is_none_or(|list| list.iter().any(|a| a == name));
    if action == "type" {
        return type_chain(desktop, entry, reference, value.unwrap_or(""), pixels_allowed).await;
    }
    // Not something this element does: the agent clicks its centre instead.
    if !advertised(action) {
        return Err(needs_pixels());
    }
    let mut argv = vec![action.to_string(), "--".to_string(), reference.to_string()];
    if let Some(value) = value {
        argv.push(value.to_string());
    }
    let envelope = desktop.cli(argv, ACT_TIMEOUT).await?;
    match data(&envelope) {
        Ok(answer) => Ok(Acted {
            // How text went in; any other action went through accessibility as itself.
            via: if action == "set-value" { "set-value" } else { "accessibility" },
            delivery: landed(&answer),
            value: read_back(&answer, entry),
        }),
        Err(error) => Err(map_error(&error, true, entry.bounds.as_ref())),
    }
}

fn read_back(answer: &Value, entry: &RefEntry) -> Option<Value> {
    if entry.secret {
        return None;
    }
    answer["post_state"]["value"]
        .as_str()
        .map(|v| json!(truncated(v).0))
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
) -> ApiResult<Acted> {
    let envelope = desktop
        .cli(vec!["type".into(), "--".into(), reference.into(), text.into()], ACT_TIMEOUT)
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
    // 2. set-value with the text appended (at the end, not the caret).
    let settable = entry.actions.as_ref().is_some_and(|a| a.iter().any(|x| x == "set-value"));
    if settable && !entry.secret {
        if let Ok(Value::String(old)) = get_property(desktop, entry, reference, "value").await {
            let wanted = format!("{old}{text}");
            let set = desktop
                .cli(vec!["set-value".into(), "--".into(), reference.into(), wanted.clone()], ACT_TIMEOUT)
                .await?;
            match data(&set) {
                Ok(_) => {
                    let now = get_property(desktop, entry, reference, "value").await;
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
    if pixels_allowed {
        if let Some(bounds) = &entry.bounds {
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
    if let Some(bounds) = &entry.bounds {
        error = error.with_details(json!({ "bounds": bounds, "display": bounds.get("display") }));
    }
    Err(error)
}

/// `POST ax/act {ref, action, value?}` → `{ref, action, via, delivery, value?}`.
pub async fn act<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, RefEntry, &'static str)> {
    let reference = text_field(body, "ref").ok_or_else(|| ApiError::bad_request("ref is required"))?;
    let action = text_field(body, "action").ok_or_else(|| ApiError::bad_request("action is required"))?;
    let value = body.get("value").and_then(Value::as_str);
    let entry = ax.lookup(&caller.owner, reference)?;
    let acted = act_on(desktop, &entry, reference, action, value, !caller.local).await?;
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

/// One step's activity: an act's row, or a read of the window.
pub enum Note {
    Read { window: String, app: String },
    Act { entry: RefEntry, action: String, via: &'static str, error: Option<ApiError> },
}

/// `POST ax/batch {window, steps:[…≤10], stopOnError=true}` → `{results:[{ok, data|error}]}`,
/// within 20 s in all. The rows to write come back with it.
pub async fn batch<D: Desktop>(desktop: &D, ax: &Ax, caller: &Caller, body: &Value) -> ApiResult<(Value, Vec<Note>)> {
    let window = text_field(body, "window").ok_or_else(|| ApiError::bad_request("window is required"))?;
    let steps = body
        .get("steps")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty() && s.len() <= MAX_STEPS)
        .ok_or_else(|| ApiError::bad_request("steps must be 1 to 10 steps"))?;
    let parsed: Vec<Step> = steps.iter().map(step_of).collect::<ApiResult<_>>()?;
    let stop_on_error = body.get("stopOnError") != Some(&Value::Bool(false));
    let target = target(desktop, ax, window).await?;
    let deadline = Instant::now() + BATCH_TIMEOUT;
    let mut results = Vec::new();
    let mut notes = Vec::new();
    let mut stopped = false;
    for step in parsed {
        if stopped {
            break;
        }
        if Instant::now() >= deadline {
            results.push(json!({ "ok": false, "error": error_json(&ApiError::coded(504, "timeout", "The batch ran out of its 20 s")) }));
            break;
        }
        let outcome: ApiResult<Value> = match step {
            Step::Get(get_step) => {
                let reference = text_field(get_step, "ref").unwrap_or("");
                let property = text_field(get_step, "property").unwrap_or("value");
                match ax.lookup(&caller.owner, reference) {
                    Ok(entry) if entry.window != target.window => Err(ApiError::coded(409, "stale-ref", "This ref is from another window")),
                    Ok(entry) => {
                        notes.push(Note::Read { window: target.window.clone(), app: target.app.clone() });
                        get_property(desktop, &entry, reference, property).await.map(|value| {
                            let mut answer = json!({ "ref": reference, "property": property, "value": value });
                            if entry.secret && property == "value" {
                                answer["secret"] = json!(true);
                            }
                            answer
                        })
                    }
                    Err(error) => Err(error),
                }
            }
            Step::Act(act_step) => {
                let reference = text_field(act_step, "ref").unwrap_or("");
                let action = text_field(act_step, "action").unwrap_or("");
                let value = act_step.get("value").and_then(Value::as_str);
                match ax.lookup(&caller.owner, reference) {
                    Ok(entry) if entry.window != target.window => Err(ApiError::coded(409, "stale-ref", "This ref is from another window")),
                    Ok(entry) => {
                        let acted = act_on(desktop, &entry, reference, action, value, !caller.local).await;
                        notes.push(Note::Act {
                            entry: entry.clone(),
                            action: action.to_string(),
                            via: acted.as_ref().map_or("type", |a| a.via),
                            error: acted.as_ref().err().cloned(),
                        });
                        acted.map(|acted| acted_json(reference, action, acted))
                    }
                    Err(error) => Err(error),
                }
            }
            Step::Find { query, then } => {
                notes.push(Note::Read { window: target.window.clone(), app: target.app.clone() });
                match find_in(desktop, ax, caller, &target, query, CANDIDATES as u64 + 1).await {
                    Err(error) => Err(error),
                    Ok(matches) => match then.filter(|t| !t.is_null()) {
                        None => Ok(json!({ "matches": matches })),
                        Some(then) => match matches.as_slice() {
                            [] => Err(ApiError::coded(409, "stale-ref", "Nothing matched to act on")),
                            [one] => {
                                let reference = one["ref"].as_str().unwrap_or("").to_string();
                                let action = text_field(then, "action").unwrap_or("");
                                let value = then.get("value").and_then(Value::as_str);
                                match ax.lookup(&caller.owner, &reference) {
                                    Err(error) => Err(error),
                                    Ok(entry) => {
                                        let acted = act_on(desktop, &entry, &reference, action, value, !caller.local).await;
                                        notes.push(Note::Act {
                                            entry: entry.clone(),
                                            action: action.to_string(),
                                            via: acted.as_ref().map_or("type", |a| a.via),
                                            error: acted.as_ref().err().cloned(),
                                        });
                                        acted.map(|acted| acted_json(&reference, action, acted))
                                    }
                                }
                            }
                            many => Err(ApiError::coded(409, "ambiguous-target", "More than one element matched")
                                .with_details(json!({ "candidates": many.iter().take(CANDIDATES).cloned().collect::<Vec<_>>() }))),
                        },
                    },
                }
            }
        };
        match outcome {
            Ok(data) => results.push(json!({ "ok": true, "data": data })),
            Err(error) => {
                results.push(json!({ "ok": false, "error": error_json(&error) }));
                stopped = stop_on_error;
            }
        }
    }
    Ok((json!({ "results": results }), notes))
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
            let (via, error) = match &answer {
                Ok((_, _, via)) => (*via, None),
                Err(error) => ("type", Some(error.clone())),
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
                                note_act(audit, caller, Some(entry), action, via, error.as_ref())
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
fn note_act(audit: &Audit, caller: &Caller, entry: Option<&RefEntry>, action: &str, via: &str, error: Option<&ApiError>) {
    let mut detail = json!({ "action": action, "via": via });
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
        pasted: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answers: Vec<Value>) -> Self {
            Self {
                windows: json!({ "windows": [
                    { "id": "m:42", "pid": 7, "app": "com.apple.calculator", "appName": "Calculator", "appKey": "bundle:com.apple.calculator", "title": "Calculator", "display": "1", "minimized": false },
                    { "id": "w:2a", "pid": 9, "app": "notepad.exe", "appName": "Notepad", "title": "Untitled", "display": "1", "minimized": true },
                ]}),
                answers: Mutex::new(answers.into()),
                calls: Mutex::new(Vec::new()),
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
        async fn cli(&self, argv: Vec<String>, _timeout: Duration) -> ApiResult<Value> {
            self.calls.lock().unwrap().push(argv);
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ApiError::coded(503, "unavailable", "no more answers"))
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
        assert!(call.contains(&"--max-depth=30".to_string()));
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
        let acted = block(act_on(&fake, &entry(&["type"], false, false), "@s1:e2", "type", Some("abc"), true)).unwrap();
        assert_eq!((acted.via, acted.delivery), ("type", Delivery::Done));
        // 2. refused, nothing delivered: get, set-value(old + text), get to check.
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": "Hello " })),
            ok(json!({ "disposition": { "delivery": "delivered_verified" } })),
            ok(json!({ "value": "Hello world" })),
        ]);
        let acted = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("world"), true)).unwrap();
        assert_eq!(acted.via, "set-value");
        assert_eq!(fake.calls()[2], vec!["set-value", "--", "@s1:e2", "Hello world"]);
        // 3. not settable: a click at its centre and a paste.
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let acted = block(act_on(&fake, &entry(&["type"], false, true), "@s1:e2", "type", Some("world"), true)).unwrap();
        assert_eq!(acted.via, "paste");
        assert_eq!(*fake.pasted.lock().unwrap(), vec!["world".to_string()]);
        // ...never for an agent on this computer (the person's own pointer), nor without bounds.
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let error = block(act_on(&fake, &entry(&["type"], false, true), "@s1:e2", "type", Some("x"), false)).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("needs-pixels"));
        assert!(fake.pasted.lock().unwrap().is_empty());
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let error = block(act_on(&fake, &entry(&["type"], false, false), "@s1:e2", "type", Some("x"), true)).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("needs-pixels"));
        // A password is never read back for the set-value path: straight to paste.
        let fake = Fake::new(vec![failed("POLICY_DENIED", "not_delivered")]);
        let acted = block(act_on(&fake, &entry(&["set-value"], true, true), "@s1:e3", "type", Some("pw"), true)).unwrap();
        assert_eq!(acted.via, "paste");
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn type_stops_when_it_may_have_landed() {
        // Typed but unverified (Electron on macOS): stop, no fallback, delivery unknown.
        let fake = Fake::new(vec![ok(json!({ "disposition": { "delivery": "delivered_unverified" } }))]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("abc"), true)).unwrap_err();
        let body = error.body();
        assert_eq!((body["code"].as_str(), body["delivery"].as_str(), body["retry"].as_str()), (Some("ax-failed"), Some("unknown"), Some("no")));
        assert!(body["suggestion"].as_str().unwrap().contains("set-value"));
        assert_eq!(fake.calls().len(), 1);
        assert!(fake.pasted.lock().unwrap().is_empty());
        // A failure that may have landed stops too.
        let fake = Fake::new(vec![failed("ACTION_FAILED", "delivered_unverified")]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("abc"), true)).unwrap_err();
        assert_eq!(error.delivery, Some(Delivery::Unknown));
        assert_eq!(fake.calls().len(), 1);
        // set-value may have landed half way: stop before pasting.
        let fake = Fake::new(vec![
            failed("ACTION_NOT_SUPPORTED", "not_delivered"),
            ok(json!({ "value": "a" })),
            failed("ACTION_FAILED", "delivery_uncertain"),
        ]);
        let error = block(act_on(&fake, &entry(&["set-value", "type"], false, true), "@s1:e2", "type", Some("b"), true)).unwrap_err();
        assert_eq!(error.delivery, Some(Delivery::Unknown));
        assert!(fake.pasted.lock().unwrap().is_empty());
    }

    #[test]
    fn an_action_the_element_lacks_needs_pixels() {
        let fake = Fake::new(vec![]);
        let error = block(act_on(&fake, &entry(&["click"], false, true), "@s1:e1", "toggle", None, true)).unwrap_err();
        let body = error_json(&error);
        assert_eq!(body["code"], "needs-pixels");
        assert_eq!(body["display"], "1");
        assert_eq!(body["bounds"]["width"], 100);
        assert!(fake.calls().is_empty());
        // Never anything outside the allow-list.
        assert!(block(act_on(&fake, &entry(&["click"], false, true), "@s1:e1", "press", None, true)).is_err());
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
        let target = block(target(&fake, &ax, "m:42")).unwrap();
        assert_eq!(target.cli_window, "w-42");
        assert_eq!(block(super::target(&fake, &ax, "w:2a")).unwrap_err().code.as_deref(), Some("window-not-found"));
        assert_eq!(block(super::target(&fake, &ax, "m:1")).unwrap_err().code.as_deref(), Some("window-not-found"));
        assert_eq!(block(super::target(&fake, &ax, "nope")).unwrap_err().status, 400);
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
}
