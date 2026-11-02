//! The Activity tab (realtimex-ai-app#3055 §1.4; Product/UX contract
//! issue-3055-ux-contract.md, prototype v1): what other computers, agents on this computer and
//! the owner did here, from rtx-linkd's activity log (GET /v1/host/audit), newest first, in plain
//! sentences that start with the computer's name. Routine steps by one computer fold into a run;
//! connections, access changes, views, the clipboard, transfers and anything refused or failed
//! always stand alone. A filter by computer, Show older (the `next` cursor), and Clear.
//!
//! Names and agent/person are what each computer says about itself: the caption says so once,
//! and nothing here claims they are verified.

use std::collections::HashSet;

use super::*;

/// While the tab is shown, the newest page is read again this often.
const LIVE: Duration = Duration::from_secs(5);
const PAGE: usize = 100;
/// Routine steps of one computer this close together fold into one run.
const GAP_MS: i64 = 5 * 60 * 1000;
const ROUTINE: [&str; 7] = [
    "screenshot",
    "actions",
    "windows",
    "focus",
    "wait",
    "ax-read",
    "ax-act",
];

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Load {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed,
}

/// The tab's data, read off the UI thread.
#[derive(Default)]
pub struct Activity {
    pub load: Load,
    /// Newest first, as loaded so far.
    pub entries: Vec<Value>,
    pub next: Option<i64>,
    pub dropped: u64,
    /// The computer the entries are for (None: all computers).
    pub client: Option<String>,
    /// Show older: Idle, Loading or Failed.
    pub older: Load,
    /// Each new load moves it on: an answer for an earlier one is dropped.
    pub generation: u64,
    pub polled: Option<Instant>,
    pub polling: bool,
    /// The filter takes the focus (after a clear).
    pub focus_filter: bool,
    /// Agents on this computer seen in the log (`via:"local"`): (client id, thread name), for the
    /// filter (contract Addendum 1).
    pub local: Vec<(String, String)>,
}

/// A fresh first page: the list starts again from it and its `next`. For all computers and
/// agents, the agents on this computer in the filter are rebuilt from it, so those whose entries
/// were cleared or aged out go (review C5); for one computer they are only added to.
fn fresh_page(activity: &mut Activity, page: &Value) {
    let entries = entries_of(page);
    if activity.client.is_none() {
        activity.local.clear();
    }
    remember_local(activity, &entries);
    activity.entries = entries;
    activity.next = next_of(page);
    activity.dropped = page.get("dropped").and_then(Value::as_u64).unwrap_or(0);
}

/// Remember the local callers among `entries` (the newest name wins).
fn remember_local(activity: &mut Activity, entries: &[Value]) {
    for entry in entries {
        if str_of(entry, "via") != "local" {
            continue;
        }
        let id = str_of(entry, "clientId");
        if id.is_empty() || activity.local.iter().any(|(known, _)| known == id) {
            continue;
        }
        activity
            .local
            .push((id.to_string(), str_of(entry, "clientName").to_string()));
    }
}

/// The tab's own state on the UI thread.
#[derive(Default)]
pub struct ActivityView {
    /// Runs shown step by step: a run is open when any of its steps is here, so it stays open
    /// while new steps join it at either end.
    open: HashSet<i64>,
    /// Clear… gets the focus back (its confirm was cancelled).
    pub focus_clear: bool,
    /// The frame the tab was last drawn in: a gap means it is shown again.
    seen: Option<u64>,
}

fn id_of(entry: &Value) -> i64 {
    entry.get("id").and_then(Value::as_i64).unwrap_or(0)
}

fn at_of(entry: &Value) -> i64 {
    entry.get("at").and_then(Value::as_i64).unwrap_or(0)
}

fn str_of<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or("")
}

fn detail<'a>(entry: &'a Value, key: &str) -> Option<&'a Value> {
    entry.get("detail").and_then(|d| d.get(key))
}

fn detail_str<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    detail(entry, key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn detail_u64(entry: &Value, key: &str) -> u64 {
    detail(entry, key).and_then(Value::as_u64).unwrap_or(0)
}

/// A query value, percent-encoded.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn path(client: Option<&str>, before: Option<i64>) -> String {
    let mut path = format!("/v1/host/audit?limit={PAGE}");
    if let Some(before) = before {
        path.push_str(&format!("&before={before}"));
    }
    if let Some(client) = client {
        path.push_str(&format!("&client={}", encode(client)));
    }
    path
}

fn entries_of(page: &Value) -> Vec<Value> {
    page.get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn next_of(page: &Value) -> Option<i64> {
    page.get("next").and_then(Value::as_i64)
}

fn get(shared: &Shared, path: &str) -> anyhow::Result<Value> {
    let api = lock(shared).api.clone();
    match api {
        Some(api) => api.get(path),
        None => Err(anyhow::anyhow!(t("errors.serviceNotRunning"))),
    }
}

/// Load the newest page for `client` (None: all computers): the spinner until it answers.
pub fn load(shared: &Shared, ctx: &egui::Context, client: Option<String>) {
    let generation = {
        let mut state = lock(shared);
        let activity = &mut state.activity;
        activity.generation += 1;
        activity.load = Load::Loading;
        activity.client = client.clone();
        activity.entries.clear();
        activity.next = None;
        activity.older = Load::Idle;
        activity.polled = Some(Instant::now());
        activity.generation
    };
    let (shared, ctx) = (shared.clone(), ctx.clone());
    std::thread::spawn(move || {
        let answer = get(&shared, &path(client.as_deref(), None));
        {
            let mut state = lock(&shared);
            let activity = &mut state.activity;
            if activity.generation != generation {
                return;
            }
            match answer {
                Ok(page) => {
                    fresh_page(activity, &page);
                    activity.load = Load::Ready;
                }
                Err(_) => activity.load = Load::Failed,
            }
            activity.polled = Some(Instant::now());
        }
        ctx.request_repaint();
    });
}

/// The newest page again, merged on top: the scroll position and open runs stay as they are.
fn poll(shared: &Shared, ctx: &egui::Context) {
    let (generation, client) = {
        let mut state = lock(shared);
        let activity = &mut state.activity;
        if activity.polling || activity.load != Load::Ready {
            return;
        }
        activity.polling = true;
        activity.polled = Some(Instant::now());
        (activity.generation, activity.client.clone())
    };
    let (shared, ctx) = (shared.clone(), ctx.clone());
    std::thread::spawn(move || {
        let answer = get(&shared, &path(client.as_deref(), None));
        {
            let mut state = lock(&shared);
            let activity = &mut state.activity;
            activity.polling = false;
            activity.polled = Some(Instant::now());
            if activity.generation != generation {
                return;
            }
            if let Ok(page) = answer {
                merge(activity, &page);
            }
        }
        ctx.request_repaint();
    });
}

/// The newest page again: new entries go on top. The list starts over from the page instead
/// (keeping the filter; open runs stay open where their ids remain) when
/// - the log was cleared elsewhere (its ids start again too, or it is empty), or
/// - more than a page arrived since: the page doesn't reach the loaded newest and has more
///   behind it, so putting it on top would hide the entries in between (review C2).
fn merge(activity: &mut Activity, page: &Value) {
    let fresh = entries_of(page);
    let Some(newest) = activity.entries.first().map(id_of) else {
        fresh_page(activity, page);
        return;
    };
    let starts_over = match (fresh.first().map(id_of), fresh.last().map(id_of)) {
        (None, _) | (_, None) => true,
        (Some(page_newest), _) if page_newest < newest => true,
        (_, Some(page_oldest)) => page_oldest > newest && next_of(page).is_some(),
    };
    if starts_over {
        fresh_page(activity, page);
        // A Show older still on its way was for the list that went (older() drops its page).
        activity.older = Load::Idle;
        return;
    }
    remember_local(activity, &fresh);
    activity.dropped = page.get("dropped").and_then(Value::as_u64).unwrap_or(activity.dropped);
    let mut added: Vec<Value> = fresh.into_iter().filter(|e| id_of(e) > newest).collect();
    added.append(&mut activity.entries);
    activity.entries = added;
}

/// Read the newest page again now: every 5 s while the tab shows, and at once when it is shown
/// again after another tab (or the window was away), quietly, without the spinner (review C2).
fn refresh_due(load: Load, polled: Option<Instant>, shown_again: bool) -> bool {
    load == Load::Ready && (shown_again || polled.is_none_or(|at| at.elapsed() >= LIVE))
}

/// The tab wasn't drawn in the frame before this one (`seen`: the frame it last was).
fn shown_again(seen: Option<u64>, frame: u64) -> bool {
    seen.is_some_and(|seen| frame > seen + 1)
}

/// Show older pressed: the cursor to load from, or None while a page is still on its way (the
/// button stays enabled to keep its focus, and the press is ignored: review C4).
fn start_older(activity: &mut Activity) -> Option<i64> {
    let before = activity.next?;
    if activity.older == Load::Loading {
        return None;
    }
    activity.older = Load::Loading;
    Some(before)
}

/// Show older's page, appended below: the list stays where it is and nothing takes the focus,
/// on the last page too, where the end caption replaces the button (contract Addendum 2).
/// False when the list started over meanwhile (a new load or refresh): the page isn't for it.
fn took_older(activity: &mut Activity, before: i64, page: &Value) -> bool {
    if activity.next != Some(before) {
        return false;
    }
    let more = entries_of(page);
    remember_local(activity, &more);
    activity.entries.extend(more);
    activity.next = next_of(page);
    activity.older = Load::Idle;
    true
}

/// Show older: the next page after `next`, appended (runs fold again across the boundary).
fn older(shared: &Shared, ctx: &egui::Context) {
    let (generation, client, before) = {
        let mut state = lock(shared);
        let activity = &mut state.activity;
        let Some(before) = start_older(activity) else { return };
        (activity.generation, activity.client.clone(), before)
    };
    let (shared, ctx) = (shared.clone(), ctx.clone());
    std::thread::spawn(move || {
        let answer = get(&shared, &path(client.as_deref(), Some(before)));
        {
            let mut state = lock(&shared);
            let activity = &mut state.activity;
            // A new load, or a refresh that started the list over meanwhile: not for this list.
            if activity.generation != generation || activity.next != Some(before) {
                return;
            }
            match answer {
                Ok(page) => {
                    took_older(activity, before, &page);
                }
                Err(_) => activity.older = Load::Failed,
            }
        }
        ctx.request_repaint();
    });
}

/// Clear: DELETE everything, then the list again for all computers (one note is left).
pub fn clear(shared: &Shared, ctx: &egui::Context) {
    let (shared, ctx) = (shared.clone(), ctx.clone());
    std::thread::spawn(move || {
        let api = lock(&shared).api.clone();
        let answer = match api {
            Some(api) => api.delete("/v1/host/audit"),
            None => Err(anyhow::anyhow!(t("errors.serviceNotRunning"))),
        };
        match answer {
            Ok(_) => {
                lock(&shared).notice = Some((t("activity.cleared"), Tone::Success, Instant::now()));
                load(&shared, &ctx, None);
                lock(&shared).activity.focus_filter = true;
            }
            Err(_) => {
                lock(&shared).notice =
                    Some((t("activity.clearFailed"), Tone::Danger, Instant::now()));
            }
        }
        ctx.request_repaint();
    });
}

// --- wording -------------------------------------------------------------------------------

/// A sentence: its key, the values of its placeholders, and which of them are semibold (the
/// computer's name, "You").
#[derive(Debug, Clone, PartialEq)]
struct Sentence {
    key: String,
    args: Vec<(&'static str, String)>,
}

impl Sentence {
    fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            args: Vec::new(),
        }
    }

    fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.args.push((name, value.into()));
        self
    }

    /// As plain text, every placeholder filled.
    fn text(&self) -> String {
        let args: Vec<(&str, &str)> = self.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        tf(&self.key, &args)
    }

    /// As one wrapping label: `{name}` and `{you}` semibold, the rest as it is.
    fn job(&self, ui: &egui::Ui) -> egui::text::LayoutJob {
        let template = t(&self.key);
        let mut job = egui::text::LayoutJob::default();
        let style = ui.style();
        let plain = |job: &mut egui::text::LayoutJob, text: &str| {
            if !text.is_empty() {
                RichText::new(text).append_to(
                    job,
                    style,
                    egui::FontSelection::Default,
                    egui::Align::Center,
                );
            }
        };
        let mut rest = template.as_str();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            plain(&mut job, &rest[..open]);
            let name = &rest[open + 1..open + close];
            match self.args.iter().find(|(k, _)| *k == name) {
                Some((k, value)) if *k == "name" || *k == "you" => {
                    semibold(value.as_str()).append_to(
                        &mut job,
                        style,
                        egui::FontSelection::Default,
                        egui::Align::Center,
                    );
                }
                Some((_, value)) => plain(&mut job, value),
                None => plain(&mut job, &rest[open..open + close + 1]),
            }
            rest = &rest[open + close + 1..];
        }
        plain(&mut job, rest);
        job
    }
}

/// One shown row: a single entry, or a run of routine steps.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    /// The newest step's time; `from` the oldest's (runs and views).
    at: i64,
    from: Option<i64>,
    sentence: Sentence,
    actor: Option<String>,
    /// Refused (warning) or failed (danger).
    badge: Option<(String, Tone)>,
    captions: Vec<String>,
    files: Option<String>,
    /// A run's steps, newest first.
    steps: Vec<Value>,
}

/// Names for the rows: a client's current name from the overview, else what the entry says.
struct Names<'a> {
    clients: &'a [Value],
}

impl Names<'_> {
    fn of(&self, entry: &Value) -> String {
        let said = str_of(entry, "clientName");
        if str_of(entry, "via") == "local" {
            return tf("activity.localName", &[("name", said)]);
        }
        let id = str_of(entry, "clientId");
        let current = (!id.is_empty())
            .then(|| self.clients.iter().find(|c| text(c, "id") == id))
            .flatten()
            .map(|c| text(c, "name"))
            .filter(|n| !n.is_empty());
        current
            .or_else(|| Some(said.to_string()).filter(|n| !n.is_empty()))
            .or_else(|| Some(str_of(entry, "machine").to_string()).filter(|n| !n.is_empty()))
            .unwrap_or_else(|| t("activity.someComputer"))
    }
}

fn reason(code: &str) -> String {
    let key = match code {
        "wrong-password" => "wrongPassword",
        "password-required" => "passwordRequired",
        "access-revoked" => "accessRevoked",
        "credential-reset" => "credentialReset",
        "not-accepting" => "notAccepting",
        "rate-limited" => "rateLimited",
        "request-expired" => "requestExpired",
        "view-only" => "viewOnly",
        "input-blocked" => "inputBlocked",
        "screen-locked" => "screenLocked",
        "permission-missing" => "permissionMissing",
        "focus-refused" => "focusRefused",
        _ => "other",
    };
    t(&format!("activity.reason.{key}"))
}

fn actor(entry: &Value) -> Option<String> {
    match str_of(entry, "actor") {
        "agent" => Some(t("activity.agent")),
        "person" => Some(t("activity.person")),
        _ => None,
    }
}

/// "Ctrl+S", "Cmd+Shift+T", "Enter" from the log's "ctrl+s".
pub fn key_name(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| match part {
            "ctrl" | "control" => "Ctrl".to_string(),
            "cmd" | "command" | "meta" => "Cmd".to_string(),
            "alt" | "option" => "Alt".to_string(),
            "win" | "windows" | "super" => "Win".to_string(),
            "shift" => "Shift".to_string(),
            "esc" | "escape" => "Esc".to_string(),
            "pageup" | "page_up" => "PageUp".to_string(),
            "pagedown" | "page_down" => "PageDown".to_string(),
            "arrowup" => "Up".to_string(),
            "arrowdown" => "Down".to_string(),
            "arrowleft" => "Left".to_string(),
            "arrowright" => "Right".to_string(),
            other if other.chars().count() <= 1 => other.to_uppercase(),
            other => {
                let mut chars = other.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// B, KB, MB, GB; one decimal from MB up.
pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes < 1000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{:.0} KB", b / 1e3)
    } else if bytes < 1_000_000_000 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.1} GB", b / 1e9)
    }
}

fn duration(ms: u64) -> String {
    let minutes = (ms as f64 / 60_000.0).round() as u64;
    if minutes < 1 {
        t("activity.underMinute")
    } else if minutes < 60 {
        tf("activity.minutes", &[("count", minutes.to_string().as_str())])
    } else {
        tf(
            "activity.hoursMinutes",
            &[
                ("hours", (minutes / 60).to_string().as_str()),
                ("minutes", (minutes % 60).to_string().as_str()),
            ],
        )
    }
}

fn local(ms: i64) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms).single()
}

fn hm(ms: i64) -> String {
    local(ms).map_or_else(String::new, |at| at.format("%H:%M").to_string())
}

fn hms(ms: i64) -> String {
    local(ms).map_or_else(String::new, |at| at.format("%H:%M:%S").to_string())
}

/// "Today", "Yesterday", else the date (YYYY-MM-DD: chrono has no locale formats without
/// `unstable-locales`, the contract's fallback).
fn day_label(ms: i64, today: chrono::NaiveDate) -> String {
    let Some(day) = local(ms).map(|at| at.date_naive()) else {
        return String::new();
    };
    match (today - day).num_days() {
        0 => t("activity.today"),
        1 => t("activity.yesterday"),
        _ => day.format("%Y-%m-%d").to_string(),
    }
}

fn app_of(entry: &Value) -> String {
    detail_str(entry, "app").map_or_else(|| t("activity.anApp"), str::to_string)
}

fn verb_of(action: &str) -> &'static str {
    match action {
        "click" => "click",
        "set-value" => "setValue",
        "type" => "type",
        "toggle" => "toggle",
        "select" => "select",
        "expand" => "expand",
        "collapse" => "collapse",
        "scroll-to" => "scrollTo",
        _ => "other",
    }
}

fn role_of(role: &str) -> String {
    let plain: String = role
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let key = match plain.as_str() {
        "button" | "pushbutton" => "button",
        "textfield" | "text" | "edit" | "textarea" | "entry" | "searchfield" => "textField",
        "checkbox" | "togglebutton" | "switch" => "checkbox",
        "menuitem" => "menuItem",
        "link" | "hyperlink" => "link",
        "tab" | "pagetab" => "tab",
        "listitem" | "row" | "cell" => "listItem",
        "combobox" | "popupbutton" | "list" => "comboBox",
        _ => "other",
    };
    t(&format!("activity.role.{key}"))
}

#[derive(Default)]
struct Totals {
    shots: u64,
    clicks: u64,
    scrolls: u64,
    typing: bool,
    keys: Vec<String>,
    controls: u64,
    reads: u64,
    apps: Vec<String>,
}

impl Totals {
    fn add_actions(&mut self, entry: &Value) {
        let kinds = detail(entry, "kinds");
        let n = |name: &str| kinds.and_then(|k| k.get(name)).and_then(Value::as_u64).unwrap_or(0);
        self.clicks += n("click") + n("double_click") + n("right_click");
        self.scrolls += n("scroll");
        self.typing |= n("type") > 0 || n("printable") > 0 || n("paste") > 0;
        for key in detail(entry, "keys").and_then(Value::as_array).into_iter().flatten() {
            if let Some(key) = key.as_str().map(key_name) {
                if !self.keys.contains(&key) {
                    self.keys.push(key);
                }
            }
        }
    }

    fn add_app(&mut self, entry: &Value) {
        if let Some(app) = detail_str(entry, "app") {
            if !self.apps.iter().any(|a| a == app) {
                self.apps.push(app.to_string());
            }
        }
    }

    fn text(&self) -> String {
        let count = |key: &str, n: u64| tf(key, &[("count", n.to_string().as_str())]);
        let mut parts = Vec::new();
        if self.shots > 0 {
            parts.push(count("activity.total.screenshots", self.shots));
        }
        if self.clicks > 0 {
            parts.push(count("activity.total.clicks", self.clicks));
        }
        if self.scrolls > 0 {
            parts.push(count("activity.total.scrolls", self.scrolls));
        }
        if self.typing {
            parts.push(t("activity.total.typing"));
        }
        if !self.keys.is_empty() {
            parts.push(tf("activity.total.keys", &[("keys", self.keys.join(", ").as_str())]));
        }
        if self.controls > 0 {
            parts.push(count("activity.total.controls", self.controls));
        }
        if self.reads > 0 {
            parts.push(count("activity.total.reads", self.reads));
        }
        if !self.apps.is_empty() {
            parts.push(tf("activity.total.apps", &[("apps", self.apps.join(", ").as_str())]));
        }
        parts.join(" · ")
    }
}

/// A routine step: its sentence after the name (`{name} …`), its step line, and its caption.
fn step(entry: &Value) -> (Sentence, Sentence, Option<String>) {
    let kind = str_of(entry, "kind");
    let both = |key: &str| (Sentence::new(format!("activity.{key}")), Sentence::new(format!("activity.step.{key}")));
    let (row, line, caption) = match kind {
        "screenshot" => {
            let (row, line) = both("screenshot");
            (row, line, None)
        }
        "actions" => {
            let (row, line) = both("actions");
            let mut totals = Totals::default();
            totals.add_actions(entry);
            (row, line, Some(totals.text()).filter(|t| !t.is_empty()))
        }
        "windows" => {
            let (row, line) = both("windows");
            (row, line, None)
        }
        "focus" => {
            let (row, line) = both("focus");
            let app = app_of(entry);
            (row.with("app", app.clone()), line.with("app", app), None)
        }
        "wait" => {
            let (row, line) = both("wait");
            let changed = detail(entry, "changed").and_then(Value::as_bool);
            let caption = changed.map(|c| t(if c { "activity.changed" } else { "activity.noChange" }));
            (row, line, caption)
        }
        "ax-read" => {
            let (row, line) = both("axRead");
            let app = app_of(entry);
            let reads = detail_u64(entry, "count").max(1);
            let caption = tf("activity.total.reads", &[("count", reads.to_string().as_str())]);
            (row.with("app", app.clone()), line.with("app", app), Some(caption))
        }
        "ax-act" => {
            let verb = verb_of(detail_str(entry, "action").unwrap_or(""));
            let (row, line) = both(&format!("axAct.{verb}"));
            let app = app_of(entry);
            let role = role_of(detail_str(entry, "role").unwrap_or(""));
            (
                row.with("role", role.clone()).with("app", app.clone()),
                line.with("role", role).with("app", app),
                None,
            )
        }
        _ => (
            Sentence::new("activity.tried.other"),
            Sentence::new("activity.tried.other"),
            None,
        ),
    };
    (row, line, caption)
}

/// One entry as a row (contract §4).
fn describe(entry: &Value, names: &Names) -> Shown {
    let name = names.of(entry);
    let kind = str_of(entry, "kind");
    let outcome = str_of(entry, "outcome");
    let code = str_of(entry, "code");
    let mut shown = Shown {
        at: at_of(entry),
        from: None,
        sentence: Sentence::new("activity.tried.other"),
        actor: actor(entry),
        badge: None,
        captions: Vec::new(),
        files: None,
        steps: Vec::new(),
    };
    let you = t("activity.you");
    match kind {
        "connect" => match outcome {
            "pending" => shown.sentence = Sentence::new("activity.askedToConnect").with("name", name),
            "ok" => {
                shown.sentence = Sentence::new("activity.connected").with("name", name);
                let control = detail_str(entry, "permission") == Some("control");
                let mut caption = vec![access_label(control)];
                match detail_str(entry, "admittedBy") {
                    Some("password") => caption.push(t("activity.withPassword")),
                    Some("request") => caption.push(t("clients.youAllowed")),
                    Some("account") => caption.push(t("request.yourAccount")),
                    _ => {}
                }
                shown.captions.push(caption.join(" · "));
            }
            _ => {
                // Never admitted: only what it called itself, quoted.
                let said = Some(str_of(entry, "clientName"))
                    .filter(|n| !n.is_empty())
                    .map_or(name, str::to_string);
                shown.sentence = Sentence::new("activity.couldNotConnect").with("name", said);
                shown.badge = Some((t("activity.refused"), Tone::Warning));
                shown.captions.push(reason(code));
            }
        },
        "access" => {
            shown.actor = None;
            let change = detail_str(entry, "change").unwrap_or("");
            let control = detail_str(entry, "permission") == Some("control");
            let key = match change {
                "allowed" => "allowed",
                "denied" => "denied",
                "permission" if control => "full",
                "permission" => "view",
                "blocked" => "blocked",
                "allowed-again" => "allowedAgain",
                "removed" => "removed",
                "disconnected" => "disconnected",
                "new-password" => "newPassword",
                _ => "other",
            };
            shown.sentence = Sentence::new(format!("activity.access.{key}"))
                .with("you", you)
                .with("name", name);
            if change == "allowed" {
                shown.captions.push(access_label(control));
            }
        }
        "view" => {
            let input = detail(entry, "input");
            let n = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_u64).unwrap_or(0);
            let any = input
                .and_then(Value::as_object)
                .is_some_and(|i| i.values().any(|v| v.as_u64().unwrap_or(0) > 0));
            let key = if any { "activity.controlled" } else { "activity.viewed" };
            shown.sentence = Sentence::new(key).with("name", name);
            let length = detail_u64(entry, "durationMs");
            let mut caption = vec![duration(length)];
            if n("click") > 0 {
                caption.push(tf("activity.total.clicks", &[("count", n("click").to_string().as_str())]));
            }
            if n("key") > 0 {
                caption.push(t("activity.keyboard"));
            }
            shown.captions.push(caption.join(" · "));
            shown.from = Some(shown.at - length as i64);
        }
        "clipboard" => {
            let read = detail_str(entry, "direction") != Some("write");
            let what = detail_str(entry, "kind").unwrap_or("text");
            let what = match what {
                "image" | "files" => what,
                _ => "text",
            };
            let (key, caption) = if read {
                ("activity.clipboardRead", format!("activity.held.{what}"))
            } else {
                ("activity.clipboardWrite", format!("activity.was.{what}"))
            };
            shown.sentence = Sentence::new(key).with("name", name);
            shown.captions.push(t(&caption));
        }
        "transfer" => {
            let folder = detail_str(entry, "folder").map_or_else(|| t("activity.aFolder"), str::to_string);
            let count = detail_u64(entry, "count");
            let files = tf(
                "activity.files",
                &[
                    ("count", count.to_string().as_str()),
                    ("size", size(detail_u64(entry, "bytes")).as_str()),
                ],
            );
            let (key, caption) = match detail_str(entry, "op").unwrap_or("") {
                "get" => ("activity.transferGet", Some(files)),
                "send" => ("activity.transferSend", Some(files)),
                "list" => ("activity.transferList", None),
                "paste-image" => ("activity.transferPasteImage", None),
                "paste-files" => ("activity.transferPasteFiles", Some(files)),
                _ => ("activity.transferOther", Some(files)),
            };
            shown.sentence = Sentence::new(key).with("name", name).with("folder", folder);
            shown.captions.extend(caption);
            let names: Vec<&str> = detail(entry, "files")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !names.is_empty() {
                let joined = names.join(", ");
                let more = count.saturating_sub(names.len() as u64);
                shown.files = Some(if more > 0 {
                    tf(
                        "activity.filesMore",
                        &[("names", joined.as_str()), ("count", more.to_string().as_str())],
                    )
                } else {
                    joined
                });
            }
        }
        "audit-cleared" => {
            shown.actor = None;
            shown.sentence = Sentence::new("activity.auditCleared").with("you", you);
            let rows = detail_u64(entry, "rows");
            shown
                .captions
                .push(tf("activity.deleted", &[("count", rows.to_string().as_str())]));
        }
        _ => {
            let (row, _, caption) = step(entry);
            shown.sentence = row.with("name", name.clone());
            shown.captions.extend(caption);
        }
    }
    if outcome != "ok" && outcome != "pending" && kind != "connect" {
        shown.badge = Some(if outcome == "refused" {
            (t("activity.refused"), Tone::Warning)
        } else {
            (t("activity.failed"), Tone::Danger)
        });
        let tried = match kind {
            "screenshot" => "screenshot",
            "actions" => "actions",
            "windows" => "windows",
            "focus" => "focus",
            "wait" => "wait",
            "ax-read" => "axRead",
            "ax-act" => "axAct",
            "clipboard" => "clipboard",
            "transfer" => "transfer",
            "view" => "view",
            _ => "other",
        };
        // An agent on this computer: "couldn't use apps" (contract Addendum 1).
        let key = if str_of(entry, "via") == "local" {
            "activity.couldNotUseApps".to_string()
        } else {
            format!("activity.tried.{tried}")
        };
        shown.sentence = Sentence::new(key)
            .with("name", names.of(entry))
            .with("app", app_of(entry));
        shown.captions = vec![reason(code)];
        shown.files = None;
        shown.from = None;
    }
    shown
}

/// A run of two or more routine steps (newest first).
fn summarize(run: Vec<Value>, names: &Names) -> Shown {
    let mut totals = Totals::default();
    let mut acted = false;
    for entry in &run {
        match str_of(entry, "kind") {
            "screenshot" => totals.shots += 1,
            "actions" => {
                totals.add_actions(entry);
                acted = true;
            }
            "focus" => {
                totals.add_app(entry);
                acted = true;
            }
            "ax-read" => {
                totals.reads += detail_u64(entry, "count").max(1);
                totals.add_app(entry);
            }
            "ax-act" => {
                totals.controls += 1;
                totals.add_app(entry);
                acted = true;
            }
            _ => {}
        }
    }
    let first = &run[0];
    let local = str_of(first, "via") == "local";
    let key = match (acted, local) {
        (true, false) => "activity.usedComputer",
        (false, false) => "activity.lookedAt",
        // An agent on this computer used or read apps (contract Addendum 1).
        (true, true) => "activity.usedApps",
        (false, true) => "activity.readApps",
    };
    Shown {
        at: at_of(first),
        from: run.last().map(at_of),
        sentence: Sentence::new(key).with("name", names.of(first)),
        actor: actor(first),
        badge: None,
        captions: vec![totals.text()].into_iter().filter(|t| !t.is_empty()).collect(),
        files: None,
        steps: run,
    }
}

/// Fold the loaded entries (newest first) into rows: consecutive routine steps that went well,
/// of one computer and actor, each within 5 minutes of the next, become one run.
fn fold(entries: &[Value], names: &Names) -> Vec<Shown> {
    let mut out = Vec::new();
    let mut run: Vec<Value> = Vec::new();
    let flush = |run: &mut Vec<Value>, out: &mut Vec<Shown>| match run.len() {
        0 => {}
        1 => out.push(describe(&run.remove(0), names)),
        _ => out.push(summarize(std::mem::take(run), names)),
    };
    for entry in entries {
        let routine = ROUTINE.contains(&str_of(entry, "kind")) && str_of(entry, "outcome") == "ok";
        let joins = routine
            && run.last().is_some_and(|last| {
                str_of(last, "clientId") == str_of(entry, "clientId")
                    && str_of(last, "actor") == str_of(entry, "actor")
                    && at_of(last) - at_of(entry) <= GAP_MS
            });
        if joins {
            run.push(entry.clone());
            continue;
        }
        flush(&mut run, &mut out);
        if routine {
            run.push(entry.clone());
        } else {
            out.push(describe(entry, names));
        }
    }
    flush(&mut run, &mut out);
    out
}

// --- the tab -------------------------------------------------------------------------------

impl LinkApp {
    /// The Activity tab (contract §3–§9).
    pub(super) fn activity_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, overview: &Value) {
        caption(ui, t("activity.description"));
        let clients = overview
            .get("clients")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (load_state, entries, next, dropped, client, older_state, polled, focus_filter, local_callers) = {
            let mut state = lock(&self.shared);
            let activity = &mut state.activity;
            (
                activity.load,
                activity.entries.clone(),
                activity.next,
                activity.dropped,
                activity.client.clone(),
                activity.older,
                activity.polled,
                std::mem::take(&mut activity.focus_filter),
                activity.local.clone(),
            )
        };
        // The filter's choices: each client by name (its machine after it), then each agent on
        // this computer (contract §6, Addendum 1): (id, label, name).
        let mut options: Vec<(String, String, String)> = clients
            .iter()
            .map(|c| {
                let (name, machine) = (text(c, "name"), text(c, "machine"));
                let label = if machine.is_empty() {
                    name.clone()
                } else {
                    format!("{name} · {machine}")
                };
                (text(c, "id"), label, name)
            })
            .collect();
        for (id, thread) in &local_callers {
            let name = tf("activity.localName", &[("name", thread.as_str())]);
            options.push((id.clone(), name.clone(), name));
        }
        let name_of_choice = |id: &str| {
            options
                .iter()
                .find(|(option, _, _)| option == id)
                .map(|(_, _, name)| name.clone())
        };
        // The first look, and a chosen computer that was removed: all computers again.
        let removed = client.as_ref().is_some_and(|id| {
            !id.starts_with("local:") && !clients.iter().any(|c| text(c, "id") == *id)
        });
        let frame = ctx.cumulative_frame_nr();
        let again = shown_again(self.activity.seen.replace(frame), frame);
        if load_state == Load::Idle || (removed && load_state != Load::Loading) {
            load(&self.shared, ctx, None);
        } else if refresh_due(load_state, polled, again) {
            poll(&self.shared, ctx);
        }
        ctx.request_repaint_after(LIVE);
        let ready = load_state == Load::Ready;
        let mut focus_filter = focus_filter;

        // The filter, then Clear….
        ui.horizontal(|ui| {
            let clear_label = t("activity.clear");
            let clear_width = theme::icon_text_button_width(ui, &clear_label);
            let spacing = ui.spacing().item_spacing.x;
            let mut chosen = client.clone();
            let all = t("activity.allComputers");
            let selected = client
                .as_deref()
                .and_then(name_of_choice)
                .unwrap_or_else(|| all.clone());
            ui.add_enabled_ui(ready, |ui| {
                let combo = egui::ComboBox::from_id_salt("activity-computer")
                    .width((ui.available_width() - clear_width - spacing).max(120.0))
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        theme::selectable_value(ui, &mut chosen, None, &all);
                        for (id, label, _) in &options {
                            theme::selectable_value(ui, &mut chosen, Some(id.clone()), label);
                        }
                    });
                keep_focus(&mut self.open_combo, ctx, &combo.response);
                if focus_filter && ready {
                    combo.response.request_focus();
                    focus_filter = false;
                }
            });
            if chosen != client {
                self.activity.open.clear();
                load(&self.shared, ctx, chosen);
            }
            let clearable = ready && entries.iter().any(|e| str_of(e, "kind") != "audit-cleared");
            let clear = ui
                .add_enabled_ui(clearable, |ui| theme::icon_text_button(ui, theme::Icon::Trash, &clear_label))
                .inner;
            if std::mem::take(&mut self.activity.focus_clear) {
                clear.request_focus();
            }
            if clear.clicked() {
                self.confirm = Some(Confirm::ClearActivity);
            }
        });
        // Not ready yet to take it (still loading): asked again next frame.
        if focus_filter {
            lock(&self.shared).activity.focus_filter = true;
        }

        match load_state {
            Load::Idle | Load::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t("activity.loading"));
                });
                return;
            }
            Load::Failed => {
                alert(ui, Tone::Danger, &t("activity.errorTitle"), |ui| {
                    ui.label(t("activity.errorText"));
                    ui.add_space(4.0);
                    if ui.button(t("activity.tryAgain")).clicked() {
                        load(&self.shared, ctx, client.clone());
                    }
                });
                return;
            }
            Load::Ready => {}
        }
        if dropped > 0 {
            alert(ui, Tone::Warning, &t("activity.droppedTitle"), |ui| {
                ui.label(tf("activity.droppedText", &[("count", dropped.to_string().as_str())]));
            });
        }
        if entries.is_empty() && next.is_none() {
            let chosen = client.as_deref().and_then(name_of_choice);
            card(ui, |ui| match chosen {
                Some(name) => {
                    ui.label(semibold(tf("activity.filteredTitle", &[("name", name.as_str())])));
                    caption(ui, t("activity.filteredText"));
                }
                None => {
                    ui.label(semibold(t("activity.emptyTitle")));
                    caption(ui, t("activity.emptyText"));
                }
            });
            return;
        }
        let names = Names { clients: &clients };
        let rows = fold(&entries, &names);
        let today = chrono::Local::now().date_naive();
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let mut day = String::new();
            for row in &rows {
                let label = day_label(row.at, today);
                if label != day {
                    ui.add_space(6.0);
                    ui.label(semibold(label.as_str()).small().weak());
                    day = label;
                }
                self.row_ui(ui, row);
                ui.separator();
            }
        });
        if next.is_some() {
            // Enabled while it loads: egui drops a disabled widget's focus, and Space would then
            // do nothing (review C4). older() ignores a press until the page is in. One id in
            // both states, so the focus stays with it.
            let label = if older_state == Load::Loading {
                t("activity.loadingOlder")
            } else {
                t("activity.showOlder")
            };
            let more = ui.push_id("activity-show-older", |ui| ui.button(label)).inner;
            if more.clicked() {
                older(&self.shared, ctx);
            }
            if older_state == Load::Failed {
                caption(ui, t("activity.olderFailed"));
            }
        } else {
            caption(ui, t("activity.end"));
        }
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, row: &Shown) {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(40.0, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(40.0);
                    caption(ui, hm(row.at));
                },
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let job = row.sentence.job(ui);
                match &row.badge {
                    Some((badge_text, tone)) => {
                        ui.horizontal_top(|ui| {
                            let room = (ui.available_width() - badge_width(ui, badge_text) - 8.0).max(80.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(room, 0.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_max_width(room);
                                    ui.add(egui::Label::new(job).wrap());
                                },
                            );
                            badge(ui, badge_text, *tone);
                        });
                    }
                    None => {
                        ui.add(egui::Label::new(job).wrap());
                    }
                }
                let mut meta = Vec::new();
                if let Some(actor) = &row.actor {
                    meta.push(actor.clone());
                }
                if let Some(from) = row.from {
                    meta.push(format!("{}–{}", hm(from), hm(row.at)));
                }
                if !meta.is_empty() {
                    caption(ui, meta.join(" · "));
                }
                for line in &row.captions {
                    caption(ui, line.as_str());
                }
                if let Some(files) = &row.files {
                    ui.add(egui::Label::new(RichText::new(files.as_str()).small()).wrap());
                }
                if row.steps.len() > 1 {
                    self.steps_ui(ui, &row.steps);
                }
            });
        });
    }

    /// A run's disclosure: "Show each step (N)", then each step with its time.
    fn steps_ui(&mut self, ui: &mut egui::Ui, steps: &[Value]) {
        let ids: Vec<i64> = steps.iter().map(id_of).collect();
        let open = ids.iter().any(|id| self.activity.open.contains(id));
        let key = ids.iter().copied().min().unwrap_or(0);
        let label = if open {
            t("activity.hideSteps")
        } else {
            tf("activity.showSteps", &[("count", steps.len().to_string().as_str())])
        };
        let toggle = ui.push_id(("run", key), |ui| ui.link(label)).inner;
        if toggle.clicked() {
            if open {
                for id in &ids {
                    self.activity.open.remove(id);
                }
            } else {
                self.activity.open.extend(ids.iter().copied());
            }
            // It keeps the focus: the same widget, the same id.
            toggle.request_focus();
        }
        if !open {
            return;
        }
        egui::Frame::new()
            .inner_margin(Margin {
                left: 10,
                ..Margin::ZERO
            })
            .stroke(Stroke::NONE)
            .show(ui, |ui| {
                for entry in steps {
                    let (_, line, caption_text) = step(entry);
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(56.0, 0.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.set_width(56.0);
                                caption(ui, hms(at_of(entry)));
                            },
                        );
                        ui.vertical(|ui| {
                            ui.add(egui::Label::new(RichText::new(line.text()).small()).wrap());
                            if let Some(text) = caption_text {
                                caption(ui, text);
                            }
                        });
                    });
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn entry(id: i64, at: i64, client: &str, kind: &str, detail: Value) -> Value {
        json!({ "id": id, "at": at, "clientId": client, "clientName": "Office PC", "machine": "DESKTOP-4KQ7",
            "actor": "agent", "via": "public", "kind": kind, "outcome": "ok", "code": null, "detail": detail })
    }

    const MIN: i64 = 60 * 1000;

    #[test]
    fn routine_steps_fold_into_runs_and_the_rest_stands_alone() {
        let clients = vec![json!({ "id": "c1", "name": "Office PC (renamed)" })];
        let names = Names { clients: &clients };
        let t0 = 100 * MIN;
        let entries = vec![
            entry(9, t0 + 30 * MIN, "c1", "transfer", json!({ "op": "get", "count": 1, "files": ["a.pdf"], "bytes": 10 })),
            entry(8, t0 + 3 * MIN, "c1", "actions", json!({ "kinds": { "click": 2, "printable": 3 }, "keys": ["ctrl+s"] })),
            entry(7, t0 + 2 * MIN, "c1", "ax-act", json!({ "app": "EXCEL.EXE", "action": "set-value", "role": "text field" })),
            entry(6, t0 + MIN, "c1", "screenshot", json!({})),
            entry(5, t0, "c1", "screenshot", json!({})),
            // 20 minutes earlier: a new run, of one step.
            entry(4, t0 - 20 * MIN, "c1", "screenshot", json!({})),
        ];
        let rows = fold(&entries, &names);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].sentence.key, "activity.transferGet");
        assert_eq!(rows[0].files.as_deref(), Some("a.pdf"));
        let run = &rows[1];
        assert_eq!(run.steps.len(), 4);
        assert_eq!(run.sentence.key, "activity.usedComputer");
        // The name is the client's current one.
        assert_eq!(run.sentence.args[0].1, "Office PC (renamed)");
        assert_eq!((run.at, run.from), (t0 + 3 * MIN, Some(t0)));
        assert_eq!(rows[2].sentence.key, "activity.screenshot");
        assert!(rows[2].steps.is_empty());
    }

    #[test]
    fn a_refused_step_breaks_a_run() {
        let names = Names { clients: &[] };
        let mut refused = entry(2, 2 * MIN, "c1", "actions", json!({}));
        refused["outcome"] = json!("refused");
        refused["code"] = json!("input-blocked");
        let entries = vec![
            entry(3, 3 * MIN, "c1", "screenshot", json!({})),
            refused,
            entry(1, MIN, "c1", "screenshot", json!({})),
        ];
        let rows = fold(&entries, &names);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].sentence.key, "activity.tried.actions");
        assert_eq!(rows[1].badge.as_ref().map(|b| b.1), Some(Tone::Warning));
        assert_eq!(rows[1].captions, vec![t("activity.reason.inputBlocked")]);
    }

    #[test]
    fn owner_and_connection_rows_are_worded_by_change_and_outcome() {
        let names = Names { clients: &[] };
        let mut access = entry(1, 0, "c2", "access", json!({ "change": "permission", "permission": "view" }));
        access["actor"] = json!("owner");
        let shown = describe(&access, &names);
        assert_eq!(shown.sentence.key, "activity.access.view");
        assert_eq!(shown.actor, None);
        let mut refused = entry(2, 0, "", "connect", json!({}));
        refused["outcome"] = json!("refused");
        refused["code"] = json!("request-expired");
        refused["clientName"] = json!("DESKTOP-JH21");
        let shown = describe(&refused, &names);
        assert_eq!(shown.sentence.key, "activity.couldNotConnect");
        assert_eq!(shown.sentence.args, vec![("name", "DESKTOP-JH21".to_string())]);
        assert_eq!(shown.captions, vec![t("activity.reason.requestExpired")]);
        let mut pending = entry(3, 0, "", "connect", json!({}));
        pending["outcome"] = json!("pending");
        assert_eq!(describe(&pending, &names).sentence.key, "activity.askedToConnect");
        let mut unknown = access.clone();
        unknown["detail"] = json!({ "change": "something-new" });
        assert_eq!(describe(&unknown, &names).sentence.key, "activity.access.other");
        // An agent on this computer.
        let mut local = entry(4, 0, "local:w1:t1", "ax-read", json!({ "app": "Calculator", "count": 3 }));
        local["via"] = json!("local");
        local["clientName"] = json!("Fix invoices");
        assert_eq!(names.of(&local), tf("activity.localName", &[("name", "Fix invoices")]));
    }

    #[test]
    fn agents_on_this_computer_used_or_read_apps() {
        let names = Names { clients: &[] };
        let local = |id: i64, at: i64, kind: &str, outcome: &str| {
            let mut e = entry(id, at, "local:w1:t1", kind, json!({ "app": "Calculator", "count": 1, "action": "click", "role": "button" }));
            e["via"] = json!("local");
            e["clientName"] = json!("Fix invoices");
            e["outcome"] = json!(outcome);
            e
        };
        let reads = fold(&[local(2, MIN, "ax-read", "ok"), local(1, 0, "ax-read", "ok")], &names);
        assert_eq!(reads[0].sentence.key, "activity.readApps");
        let used = fold(&[local(2, MIN, "ax-act", "ok"), local(1, 0, "ax-read", "ok")], &names);
        assert_eq!(used[0].sentence.key, "activity.usedApps");
        let mut locked = local(3, 0, "ax-act", "refused");
        locked["code"] = json!("screen-locked");
        let shown = describe(&locked, &names);
        assert_eq!(shown.sentence.key, "activity.couldNotUseApps");
        assert_eq!(shown.captions, vec![t("activity.reason.screenLocked")]);
        // The filter learns each caller once.
        let mut activity = Activity::default();
        remember_local(&mut activity, &[local(5, 0, "ax-read", "ok"), local(4, 0, "ax-act", "ok")]);
        assert_eq!(activity.local, vec![("local:w1:t1".to_string(), "Fix invoices".to_string())]);
    }

    #[test]
    fn a_view_says_how_long_and_whether_it_controlled() {
        let names = Names { clients: &[] };
        let view = entry(1, 60 * MIN, "c2", "view", json!({ "durationMs": 21 * MIN, "input": { "click": 48, "key": 210 } }));
        let shown = describe(&view, &names);
        assert_eq!(shown.sentence.key, "activity.controlled");
        assert_eq!(shown.from, Some(39 * MIN));
        assert_eq!(
            shown.captions,
            vec![format!(
                "{} · {} · {}",
                tf("activity.minutes", &[("count", "21")]),
                tf("activity.total.clicks", &[("count", "48")]),
                t("activity.keyboard")
            )]
        );
        let watched = entry(2, 0, "c2", "view", json!({ "durationMs": 1000 }));
        assert_eq!(describe(&watched, &names).sentence.key, "activity.viewed");
    }

    #[test]
    fn keys_sizes_and_merging() {
        assert_eq!(key_name("ctrl+s"), "Ctrl+S");
        assert_eq!(key_name("cmd+shift+t"), "Cmd+Shift+T");
        assert_eq!(key_name("enter"), "Enter");
        assert_eq!(key_name("f12"), "F12");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_480_000), "1.5 MB");
        assert_eq!(size(212_000_000), "212.0 MB");
        assert_eq!(size(48_200), "48 KB");
        assert_eq!(encode("local:w 1"), "local%3Aw%201");
        let mut activity = Activity {
            entries: vec![json!({ "id": 5 }), json!({ "id": 4 })],
            next: Some(4),
            ..Activity::default()
        };
        merge(&mut activity, &json!({ "entries": [{ "id": 7 }, { "id": 6 }, { "id": 5 }], "next": 5, "dropped": 2 }));
        let ids: Vec<i64> = activity.entries.iter().map(id_of).collect();
        assert_eq!(ids, [7, 6, 5, 4]);
        assert_eq!((activity.next, activity.dropped), (Some(4), 2));
        // Cleared elsewhere: the ids start again, and so does the list.
        merge(&mut activity, &json!({ "entries": [{ "id": 1 }], "next": null }));
        assert_eq!(activity.entries.iter().map(id_of).collect::<Vec<_>>(), [1]);
        assert_eq!(activity.next, None);
    }

    fn ids(activity: &Activity) -> Vec<i64> {
        activity.entries.iter().map(id_of).collect()
    }

    /// A page of `ids` (newest first), with `next` after its oldest when there is more.
    fn page(ids: std::ops::RangeInclusive<i64>, more: bool) -> Value {
        let entries: Vec<Value> = ids.clone().rev().map(|id| json!({ "id": id })).collect();
        json!({ "entries": entries, "next": more.then_some(*ids.start()), "dropped": 0 })
    }

    #[test]
    fn more_than_a_page_since_the_last_read_starts_the_list_over() {
        // Loaded: 1..=100, the whole log. 150 entries arrive (101..=250): the newest page is
        // 151..=250 with more behind it. Putting it on top would hide 101..=150 (review C2).
        let mut activity = Activity {
            client: Some("c-lan".into()),
            ..Activity::default()
        };
        fresh_page(&mut activity, &page(1..=100, false));
        activity.older = Load::Loading;
        merge(&mut activity, &page(151..=250, true));
        assert_eq!(ids(&activity), (151..=250).rev().collect::<Vec<_>>());
        assert_eq!(activity.next, Some(151));
        // The filter stays; Show older goes on from the new page.
        assert_eq!(activity.client.as_deref(), Some("c-lan"));
        assert_eq!(activity.older, Load::Idle);
        // A page that reaches the loaded newest still goes on top, the rest kept.
        merge(&mut activity, &page(161..=260, true));
        assert_eq!(ids(&activity).len(), 110);
        assert_eq!((ids(&activity)[0], activity.next), (260, Some(151)));
        // New entries that don't overlap but are all there are (no `next`): on top as before.
        let mut small = Activity::default();
        fresh_page(&mut small, &page(1..=3, false));
        merge(&mut small, &page(5..=6, false));
        assert_eq!(ids(&small), [6, 5, 3, 2, 1]);
        // Emptied elsewhere: empty.
        merge(&mut small, &json!({ "entries": [], "next": null }));
        assert!(small.entries.is_empty());
    }

    #[test]
    fn the_tab_shown_again_reads_the_newest_page_at_once() {
        assert!(shown_again(Some(10), 12));
        assert!(!shown_again(Some(10), 11));
        assert!(!shown_again(Some(10), 10));
        assert!(!shown_again(None, 1));
        let just = Some(Instant::now());
        assert!(refresh_due(Load::Ready, just, true));
        assert!(!refresh_due(Load::Ready, just, false));
        assert!(refresh_due(Load::Ready, None, false));
        // Not while loading or failed: those have their own spinner and Try again.
        assert!(!refresh_due(Load::Loading, None, true));
        assert!(!refresh_due(Load::Failed, None, true));
        // And what it reads is merged by the same rule: more than a page since starts over,
        // keeping the filter (the open runs live in ActivityView and are not touched).
        let mut activity = Activity {
            client: Some("local:w1:t2".into()),
            ..Activity::default()
        };
        fresh_page(&mut activity, &page(1..=100, true));
        merge(&mut activity, &page(201..=300, true));
        assert_eq!((ids(&activity)[99], activity.next), (201, Some(201)));
        assert_eq!(activity.client.as_deref(), Some("local:w1:t2"));
    }

    #[test]
    fn a_fresh_load_of_everything_rebuilds_the_agents_in_the_filter() {
        let local = |id: i64, client: &str, name: &str| {
            json!({ "id": id, "clientId": client, "clientName": name, "via": "local" })
        };
        let mut activity = Activity::default();
        fresh_page(&mut activity, &json!({ "entries": [local(2, "local:w1:t1", "Q3 report"), local(1, "local:w1:t2", "Inbox triage")] }));
        assert_eq!(activity.local.len(), 2);
        // After Clear: only the note is left, and the agents go from the filter (review C5).
        fresh_page(&mut activity, &json!({ "entries": [{ "id": 1, "kind": "audit-cleared", "via": "public" }] }));
        assert!(activity.local.is_empty());
        // Filtered to one computer, a load only adds.
        fresh_page(&mut activity, &json!({ "entries": [local(3, "local:w1:t1", "Q3 report")] }));
        activity.client = Some("c-office".into());
        fresh_page(&mut activity, &json!({ "entries": [{ "id": 4, "clientId": "c-office", "via": "public" }] }));
        assert_eq!(activity.local, vec![("local:w1:t1".to_string(), "Q3 report".to_string())]);
        // A starting-over refresh of everything rebuilds too.
        activity.client = None;
        merge(&mut activity, &json!({ "entries": [local(1, "local:w2:t9", "New run")], "next": null }));
        assert_eq!(activity.local, vec![("local:w2:t9".to_string(), "New run".to_string())]);
    }

    #[test]
    fn show_older_keeps_its_place_and_ignores_presses_while_loading() {
        let mut activity = Activity::default();
        fresh_page(&mut activity, &page(201..=300, true));
        // Pressed: loading from 201. Pressed again before the page is in: ignored (review C4).
        assert_eq!(start_older(&mut activity), Some(201));
        assert_eq!(activity.older, Load::Loading);
        assert_eq!(start_older(&mut activity), None);
        // The page goes below; nothing takes the focus (contract Addendum 2).
        assert!(took_older(&mut activity, 201, &page(101..=200, true)));
        assert_eq!((ids(&activity).len(), activity.next, activity.older), (200, Some(101), Load::Idle));
        assert!(!activity.focus_filter);
        // The last page: no `next`, so the end caption replaces the button; still no focus move.
        assert_eq!(start_older(&mut activity), Some(101));
        assert!(took_older(&mut activity, 101, &page(1..=100, false)));
        assert_eq!((activity.next, activity.focus_filter), (None, false));
        assert_eq!(start_older(&mut activity), None);
        // A page for a list that started over meanwhile is dropped.
        fresh_page(&mut activity, &page(301..=400, true));
        assert!(!took_older(&mut activity, 101, &page(1..=100, false)));
        assert_eq!(ids(&activity).len(), 100);
    }

    #[test]
    fn every_sentence_key_exists_in_english() {
        let english: HashMap<String, String> =
            serde_json::from_str(include_str!("../../locales/en.json")).unwrap();
        let mut keys: Vec<String> = vec![
            "activity.connected".into(),
            "activity.askedToConnect".into(),
            "activity.couldNotConnect".into(),
            "activity.usedComputer".into(),
            "activity.lookedAt".into(),
            "activity.auditCleared".into(),
            "activity.usedApps".into(),
            "activity.readApps".into(),
            "activity.couldNotUseApps".into(),
        ];
        for key in ["allowed", "denied", "full", "view", "blocked", "allowedAgain", "removed", "disconnected", "newPassword", "other"] {
            keys.push(format!("activity.access.{key}"));
        }
        for kind in ["screenshot", "actions", "windows", "focus", "wait", "axRead", "axAct", "clipboard", "transfer", "view", "other"] {
            keys.push(format!("activity.tried.{kind}"));
        }
        for verb in ["click", "set-value", "type", "toggle", "select", "expand", "collapse", "scroll-to", "?"] {
            let verb = verb_of(verb);
            keys.push(format!("activity.axAct.{verb}"));
            keys.push(format!("activity.step.axAct.{verb}"));
        }
        for code in ["wrong-password", "password-required", "access-revoked", "credential-reset", "not-accepting",
            "rate-limited", "request-expired", "view-only", "input-blocked", "screen-locked", "permission-missing", "focus-refused"] {
            assert_ne!(reason(code), t("activity.reason.other"), "{code}");
        }
        for key in keys {
            assert!(english.contains_key(&key), "{key}");
        }
    }
}
