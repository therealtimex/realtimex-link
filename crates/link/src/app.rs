//! The window: This computer and Computers, a request from a new computer, who is in control,
//! and Open RealTimeX / Get RealTimeX. State comes from rtx-linkd every 1.5 s; each change is one
//! request to it, made off the UI thread.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke, ViewportCommand};
use serde_json::{json, Value};

use crate::api::Api;
use crate::choices::{Choices, Field};
use crate::desktop;
use crate::i18n::{t, tf};
use crate::theme::{self, primary_button, semibold, tone_colors, Tone};

const POLL: Duration = Duration::from_millis(1500);
/// The window's height follows its content between these; past the maximum the content scrolls.
const MIN_HEIGHT: f32 = 360.0;
const MAX_HEIGHT: f32 = 720.0;
const NOTICE_FOR: Duration = Duration::from_secs(4);
/// macOS: the height of the hidden title bar, where the window's buttons stay.
const TRAFFIC_LIGHTS: f32 = 28.0;

#[derive(Default)]
pub struct Snapshot {
    pub api: Option<Api>,
    pub overview: Option<Value>,
    pub requests: Vec<Value>,
    pub password: String,
    /// The account Link is signed in to (GET /v1/account).
    pub account: Option<Value>,
    /// Link's own updates (GET /v1/update).
    pub update: Option<Value>,
    pub error: Option<String>,
    pub notice: Option<(String, bool, Instant)>,
    /// The daemon's showWindow count when last read: another rtx-link asked this one to show.
    pub show_window: Option<u64>,
    /// The theme ("light", "dark", "system") and the language (a tag or "system") the window
    /// and Settings show, and Settings' changes on their way to the daemon.
    pub choices: Choices,
}

pub type Shared = Arc<Mutex<Snapshot>>;

pub fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Snapshot> {
    shared.lock().unwrap_or_else(|p| p.into_inner())
}

/// One change, sent to rtx-linkd off the UI thread; the outcome shows as a notice.
pub fn act(
    shared: &Shared,
    refresh: &Sender<()>,
    ctx: &egui::Context,
    path: &str,
    body: Value,
    done: &str,
) {
    let (shared, refresh, ctx, path, done) = (
        shared.clone(),
        refresh.clone(),
        ctx.clone(),
        path.to_string(),
        done.to_string(),
    );
    std::thread::spawn(move || {
        let api = lock(&shared).api.clone();
        let result = match api {
            Some(api) => api.post(&path, body),
            None => Err(anyhow::anyhow!(t("errors.serviceNotRunning"))),
        };
        let notice = match result {
            Ok(_) => (done, true),
            Err(error) => (error.to_string(), false),
        };
        if !notice.0.is_empty() {
            lock(&shared).notice = Some((notice.0, notice.1, Instant::now()));
        }
        let _ = refresh.send(());
        ctx.request_repaint();
    });
}

/// Update to the latest release: the daemon installs it and the new Link starts itself, or (Linux)
/// the release page opens for its .deb / .rpm.
pub fn start_update(shared: &Shared, refresh: &Sender<()>, ctx: &egui::Context) {
    let (shared, refresh, ctx) = (shared.clone(), refresh.clone(), ctx.clone());
    std::thread::spawn(move || {
        let api = lock(&shared).api.clone();
        let answer = match api {
            Some(api) => api.post("/v1/update/install", json!({})),
            None => Err(anyhow::anyhow!(t("errors.serviceNotRunning"))),
        };
        match answer {
            Ok(answer) => {
                if let Some(page) = answer.get("open").and_then(Value::as_str) {
                    desktop::open_url(page);
                }
            }
            Err(error) => {
                lock(&shared).notice = Some((error.to_string(), false, Instant::now()));
            }
        }
        let _ = refresh.send(());
        ctx.request_repaint();
    });
}

/// A Settings change, shown at once and sent after any still on its way (one at a time, in
/// order). When the daemon doesn't take it, Settings goes back and offers Try again.
pub fn save_setting(
    shared: &Shared,
    refresh: &Sender<()>,
    ctx: &egui::Context,
    field: Field,
    value: &str,
) {
    let start = {
        let mut state = lock(shared);
        state.choices.choose(field, value);
        !std::mem::replace(&mut state.choices.writing, true)
    };
    // The poll applies it to the window now, without waiting for the daemon.
    let _ = refresh.send(());
    if !start {
        return;
    }
    let (shared, refresh, ctx) = (shared.clone(), refresh.clone(), ctx.clone());
    std::thread::spawn(move || loop {
        let (api, next) = {
            let mut state = lock(&shared);
            let next = state.choices.next_write();
            if next.is_none() {
                state.choices.writing = false;
            }
            (state.api.clone(), next)
        };
        let Some((field, value)) = next else { break };
        let mut body = serde_json::Map::new();
        body.insert(field.key().to_string(), json!(value));
        let taken = api.is_some_and(|api| api.post("/v1/appearance", Value::Object(body)).is_ok());
        lock(&shared).choices.finished(field, taken);
        let _ = refresh.send(());
        ctx.request_repaint();
    });
}

/// What this process is to the daemon (POST /v1/window/open): "window", or on Linux "tray" for
/// the icon on its own (icon.rs).
pub static ROLE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Register with the daemon as ROLE; false when another process already holds that role.
pub fn register(api: &Api) -> bool {
    let role = ROLE.get().copied().unwrap_or("window");
    let answer = api.post(
        "/v1/window/open",
        json!({ "pid": std::process::id(), "role": role }),
    );
    !answer.is_ok_and(|answer| answer["owner"] == false)
}

/// The theme the window follows: "light", "dark", or anything else the system's (an older daemon
/// has none).
fn theme_preference(theme: &str) -> egui::ThemePreference {
    match theme {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    }
}

pub fn poll_loop(shared: Shared, wake: Receiver<()>, ctx: egui::Context) {
    // Which CJK languages this computer can show: found off this thread, picked up by a later
    // poll (the window shows English until then).
    std::thread::spawn(crate::cjk::scan);
    let mut appearance = None;
    loop {
        let api = lock(&shared).api.clone();
        let api = match api {
            Some(api) => Some(api),
            None => match Api::ensure() {
                Ok(api) => {
                    // A daemon that restarted knows no window or icon yet: register again, and
                    // leave when another process already holds this role.
                    if !register(&api) {
                        std::process::exit(0);
                    }
                    lock(&shared).api = Some(api.clone());
                    Some(api)
                }
                Err(error) => {
                    lock(&shared).error = Some(error.to_string());
                    None
                }
            },
        };
        if let Some(api) = api {
            let edits = lock(&shared).choices.edits();
            let status = api.get("/v1/status").ok();
            // The stored theme and language (RealTimeX's, or chosen in Settings), unless a change
            // was made since this read started; an older daemon has no language. A failed read
            // changes nothing: the window keeps what it shows.
            let (theme, language) = {
                let mut state = lock(&shared);
                if let Some(status) = &status {
                    let field = |name: &str| status[name].as_str().unwrap_or("system");
                    state
                        .choices
                        .status(edits, field("appearance"), field("language"));
                }
                (
                    state.choices.shown(Field::Theme).to_string(),
                    state.choices.shown(Field::Language).to_string(),
                )
            };
            // The fonts first (a CJK language's), so its text never shows without them.
            let next = crate::i18n::effective_locale(&language);
            if !std::ptr::eq(next, crate::i18n::current()) {
                theme::apply_fonts(&ctx, next);
            }
            if crate::i18n::set_language(&language) {
                ctx.request_repaint();
            }
            let preference = match theme_preference(&theme) {
                egui::ThemePreference::System => theme::system_theme()
                    .map_or(egui::ThemePreference::System, egui::ThemePreference::from),
                preference => preference,
            };
            if appearance != Some(preference) {
                ctx.set_theme(preference);
                // The title bar too (the Linux one is drawn by winit, Windows' by the system).
                ctx.send_viewport_cmd(ViewportCommand::SetTheme(match preference {
                    egui::ThemePreference::Light => egui::SystemTheme::Light,
                    egui::ThemePreference::Dark => egui::SystemTheme::Dark,
                    egui::ThemePreference::System => egui::SystemTheme::SystemDefault,
                }));
                appearance = Some(preference);
            }
            let shown = status.and_then(|status| status["showWindow"].as_u64());
            let seen = std::mem::replace(&mut lock(&shared).show_window, shown);
            if let (Some(seen), Some(shown)) = (seen, shown) {
                if shown > seen {
                    crate::dock::set_open(true);
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
            }
            let overview = api.get("/v1/host/overview");
            let requests = api.get("/v1/host/connection-requests");
            let password = api.get("/v1/host/password");
            let account = api.get("/v1/account").ok();
            let update = api.get("/v1/update").ok();
            let mut state = lock(&shared);
            state.account = account;
            state.update = update;
            match overview {
                Ok(overview) => {
                    state.overview = Some(overview);
                    state.error = None;
                }
                Err(error) => {
                    // The daemon went away (quit elsewhere, restarted): find or start it again.
                    state.api = None;
                    state.error = Some(error.to_string());
                }
            }
            if let Ok(requests) = requests {
                state.requests = requests
                    .get("requests")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            }
            if let Ok(password) = password {
                state.password = password
                    .get("password")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
        }
        ctx.request_repaint();
        match wake.recv_timeout(POLL) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Grow or shrink the window by `extra` (content height minus the visible height), so no empty
/// space stays below the content and nothing scrolls until MAX_HEIGHT.
fn fit_height(ctx: &egui::Context, extra: f32) {
    if extra.abs() < 2.0 {
        return;
    }
    let Some(inner) = ctx.input(|i| i.viewport().inner_rect) else {
        return;
    };
    let height = (inner.height() + extra).clamp(MIN_HEIGHT, MAX_HEIGHT);
    if (height - inner.height()).abs() >= 2.0 {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(
            inner.width(),
            height,
        )));
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    ThisComputer,
    Computers,
}

/// Where keyboard focus goes next frame: Back when Settings opens, the gear when it closes.
#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Back,
    Gear,
}

enum Confirm {
    TurnOff,
    NewPassword,
    StopSession(String),
    Remove { id: String, name: String },
    SignOut,
}

pub struct LinkApp {
    shared: Shared,
    refresh: Sender<()>,
    tab: Tab,
    /// Settings in place of the tabs (the gear in the header).
    settings: bool,
    /// The header's area last frame: on macOS it moves the window.
    header: egui::Rect,
    focus: Option<Focus>,
    /// The widget that had keyboard focus last frame: one that gets it out of view is scrolled
    /// to (egui doesn't, so Tab could move to a control below the fold).
    last_focus: Option<egui::Id>,
    /// The select whose list is open: it gets the focus back when the list closes.
    open_combo: Option<egui::Id>,
    /// The header's and the footer's controls (the gear, Open/Get RealTimeX): never scrolled
    /// to, they are always in view.
    chrome: [Option<egui::Id>; 2],
    show_password: bool,
    editing_password: Option<String>,
    confirm: Option<Confirm>,
    tray: Option<crate::tray::Tray>,
    /// The app icon in the header.
    icon: egui::TextureHandle,
}

impl LinkApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let shared: Shared = Arc::new(Mutex::new(Snapshot::default()));
        let (refresh, wake) = channel();
        {
            let (shared, ctx) = (shared.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || poll_loop(shared, wake, ctx));
        }
        // On Linux the icon is its own process (icon.rs): a hidden window cannot be shown again
        // on Wayland, so this window is only ever open, and closing it ends this process.
        let tray = if cfg!(target_os = "linux") {
            None
        } else {
            let actions = crate::tray::Actions::window(&cc.egui_ctx);
            crate::tray::Tray::new(
                shared.clone(),
                refresh.clone(),
                cc.egui_ctx.clone(),
                actions,
            )
            .ok()
        };
        theme::apply(&cc.egui_ctx);
        Self {
            icon: header_icon(&cc.egui_ctx),
            tray,
            shared,
            refresh,
            tab: Tab::ThisComputer,
            settings: false,
            header: egui::Rect::NOTHING,
            focus: None,
            last_focus: None,
            open_combo: None,
            chrome: [None; 2],
            show_password: false,
            editing_password: None,
            confirm: None,
        }
    }

    fn act(&self, ctx: &egui::Context, path: &str, body: Value, done: &str) {
        act(&self.shared, &self.refresh, ctx, path, body, done);
    }

    fn has_tray(&self) -> bool {
        self.tray.is_some()
    }
}

// --- small pieces -----------------------------------------------------------------------

/// The app icon (the window's, main.rs) at 64 px, shown at 24 pt in the header: averaged down
/// 4 x 4 here, as the texture has no mipmaps to scale it down smoothly.
fn header_icon(ctx: &egui::Context) -> egui::TextureHandle {
    const FROM: usize = 256;
    const TO: usize = 64;
    const STEP: usize = FROM / TO;
    // The rounded icon outside Windows, like the platform's own (macOS has no window icon).
    #[cfg(windows)]
    let rgba = include_bytes!("../icons/window-256.rgba");
    #[cfg(not(windows))]
    let rgba = include_bytes!("../icons/window-linux-256.rgba");
    let full = egui::ColorImage::from_rgba_unmultiplied([FROM, FROM], rgba);
    let mut pixels = Vec::with_capacity(TO * TO);
    for y in 0..TO {
        for x in 0..TO {
            let mut sum = [0u32; 4];
            for dy in 0..STEP {
                for dx in 0..STEP {
                    let pixel = full.pixels[(y * STEP + dy) * FROM + x * STEP + dx];
                    for (total, channel) in sum.iter_mut().zip(pixel.to_array()) {
                        *total += u32::from(channel);
                    }
                }
            }
            let [r, g, b, a] = sum.map(|total| (total / (STEP * STEP) as u32) as u8);
            pixels.push(Color32::from_rgba_premultiplied(r, g, b, a));
        }
    }
    ctx.load_texture(
        "app-icon",
        egui::ColorImage::new([TO, TO], pixels),
        egui::TextureOptions::LINEAR,
    )
}

/// A pill: the tone's text on its surface.
fn badge(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let (fill, _, fg) = tone_colors(ui, tone);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(semibold(text).small().color(fg));
        });
}

/// The computer's name in the header: it takes only the room the title and the gear leave, and
/// a long name ends in "…" with the whole name on hover.
fn machine_badge(ui: &mut egui::Ui, machine: &str) {
    let (fill, _, fg) = tone_colors(ui, Tone::Neutral);
    let room = (ui.available_width() - ui.spacing().item_spacing.x - 16.0).max(24.0);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.set_max_width(room);
            ui.add(egui::Label::new(semibold(machine).small().color(fg)).truncate());
        });
}

/// No click in this frame. egui takes the focus away from a widget when a click lands elsewhere,
/// and a frame can run twice with the same input: focus asked for in the frame of the click that
/// opened or closed Settings is asked for again until a frame without one.
fn settled(ctx: &egui::Context) -> bool {
    let clicked = ctx.input(|i| i.pointer.any_click());
    if clicked {
        ctx.request_repaint();
    }
    !clicked
}

/// A select gets the keyboard focus back when its list closes (Esc, a choice, a click away), as a
/// native select does: egui leaves the focus nowhere, so Tab started over.
fn keep_focus(open: &mut Option<egui::Id>, ctx: &egui::Context, combo: &egui::Response) {
    if egui::ComboBox::is_open(ctx, combo.id) {
        *open = Some(combo.id);
    } else if *open == Some(combo.id) {
        *open = None;
        combo.request_focus();
    }
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    theme::card_frame(ui)
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn alert<R>(ui: &mut egui::Ui, tone: Tone, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let (fill, border, fg) = tone_colors(ui, tone);
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(semibold(title).color(fg));
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

fn caption(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().weak());
}

/// The switch: a pill with a knob (theme.rs paints it).
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(2.2, 1.2) * ui.spacing().interact_size.y;
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::Role::CheckBox,
            ui.is_enabled(),
            *on,
            t("host.allowRemoteControl"),
        )
    });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        theme::paint_switch(ui, &response, rect, t, *on);
    }
    response
}

/// "Full access" or "View only".
fn access_label(control: bool) -> String {
    t(if control {
        "access.full"
    } else {
        "access.view"
    })
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// "Expires in 0:42" from an ISO time.
fn expires_in(iso: &str) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return String::new();
    };
    let left = (at.timestamp() - chrono::Utc::now().timestamp()).max(0);
    let time = format!("{}:{:02}", left / 60, left % 60);
    tf("request.expiresIn", &[("time", time.as_str())])
}

fn ago(iso: &str) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return t("time.never");
    };
    let seconds = (chrono::Utc::now().timestamp() - at.timestamp()).max(0);
    let (key, count) = match seconds {
        0..=59 => return t("time.justNow"),
        60..=3599 => ("time.minutesAgo", seconds / 60),
        3600..=86_399 => ("time.hoursAgo", seconds / 3600),
        _ => ("time.daysAgo", seconds / 86_400),
    };
    tf(key, &[("count", count.to_string().as_str())])
}

// --- the window -------------------------------------------------------------------------

impl eframe::App for LinkApp {
    /// Also while the window is hidden: closing it and the tray's status.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window keeps hosting: it goes to the menu bar / tray. Quit is there.
        if ctx.input(|i| i.viewport().close_requested()) && self.has_tray() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            crate::dock::set_open(false);
        } else if cfg!(target_os = "linux") && ctx.input(|i| i.viewport().close_requested()) {
            // Linux: closing ends this window's process (its icon is another, icon.rs). It
            // ends here rather than when eframe is done: on GNOME Wayland the process outlived
            // its closed window, still registered as the window, so Open and a new launch asked
            // it to show itself and nothing appeared. A Settings change on its way goes first.
            let until = Instant::now() + Duration::from_secs(2);
            while lock(&self.shared).choices.writing && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            std::process::exit(0);
        }
        crate::dock::apply();
        if let Some(tray) = &self.tray {
            let state = lock(&self.shared);
            tray.update(state.overview.as_ref(), state.update.as_ref());
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        // Esc leaves Settings, unless a list or a confirmation is open (Esc closes that first).
        // Read before the selects run: they close their list on the same Esc.
        if self.settings
            && self.confirm.is_none()
            && !egui::Popup::is_any_open(ctx)
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.settings = false;
            self.focus = Some(Focus::Gear);
        }
        let (overview, requests, password, account, update, error, notice, stored) = {
            let mut state = lock(&self.shared);
            if state
                .notice
                .as_ref()
                .is_some_and(|(_, _, at)| at.elapsed() > NOTICE_FOR)
            {
                state.notice = None;
            }
            (
                state.overview.clone(),
                state.requests.clone(),
                state.password.clone(),
                state.account.clone(),
                state.update.clone(),
                state.error.clone(),
                state.notice.clone(),
                (
                    state.choices.shown(Field::Theme).to_string(),
                    state.choices.shown(Field::Language).to_string(),
                    state.choices.failed(),
                ),
            )
        };
        // Countdowns and notices move on their own.
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Panel::top("header").show(ui, |ui| {
            // Its text isn't selectable, so dragging the title moves the window (macOS).
            ui.style_mut().interaction.selectable_labels = false;
            // macOS: the title bar is hidden (main.rs) and its buttons sit over this space.
            if cfg!(target_os = "macos") {
                ui.add_space(TRAFFIC_LIGHTS);
            }
            // macOS: the whole header (as drawn last frame) moves the window, as the title bar
            // would; the gear, registered after it, keeps its clicks. A double-click minimizes
            // when the system says so ("Double-click a window's title bar to"); zooming would
            // fight the height that follows the content.
            if cfg!(target_os = "macos") {
                let header = ui.interact(
                    self.header,
                    ui.scope_id().with("move"),
                    egui::Sense::click_and_drag(),
                );
                if header.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                if header.double_clicked() && crate::dock::double_click_minimizes() {
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add(egui::Image::new((self.icon.id(), egui::vec2(24.0, 24.0))));
                ui.label(semibold("RealTimeX Link").size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let gear = theme::icon_button(ui, theme::Icon::Gear, &t("settings.title"));
                    self.chrome[0] = Some(gear.id);
                    if self.focus == Some(Focus::Gear) {
                        gear.request_focus();
                        if settled(ctx) {
                            self.focus = None;
                        }
                    }
                    if gear.clicked() {
                        self.settings = !self.settings;
                        self.focus = Some(if self.settings {
                            Focus::Back
                        } else {
                            Focus::Gear
                        });
                    }
                    if let Some(machine) = overview
                        .as_ref()
                        .map(|o| text(o, "machine"))
                        .filter(|m| !m.is_empty())
                    {
                        machine_badge(ui, &machine);
                    }
                });
            });
            ui.add_space(8.0);
            self.header = ui.min_rect();
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(8.0);
            let installed = desktop::realtimex_app().is_some();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Link on its own is the whole point here: RealTimeX is a quiet link.
                    let open = if installed {
                        primary_button(ui, &t("footer.openRealtimex"))
                    } else {
                        ui.link(t("footer.getRealtimex"))
                    };
                    self.chrome[1] = Some(open.id);
                    if open.clicked() {
                        desktop::open_realtimex();
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(if installed {
                                t("footer.installedHint")
                            } else {
                                t("footer.notInstalledHint")
                            })
                            .small()
                            .weak(),
                        )
                        .wrap(),
                    );
                });
            });
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            // Settings and the tabs scroll apart, and Settings opens at its top (Back showing).
            let mut scroll = egui::ScrollArea::vertical()
                .id_salt(if self.settings { "settings" } else { "tabs" })
                .auto_shrink([false, false]);
            if self.settings && self.focus == Some(Focus::Back) {
                scroll = scroll.vertical_scroll_offset(0.0);
            }
            let scroll = scroll.show(ui, |ui| {
                // The views end early (no daemon, Settings); the focus scroll below still runs.
                'content: {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    if let Some((message, ok, _)) = &notice {
                        alert(
                            ui,
                            if *ok { Tone::Success } else { Tone::Danger },
                            message,
                            |_| {},
                        );
                    }
                    let Some(overview) = overview.as_ref() else {
                        match &error {
                            Some(error) => {
                                alert(ui, Tone::Warning, &t("service.notRunningTitle"), |ui| {
                                    ui.label(error);
                                    caption(ui, t("service.retrying"));
                                })
                            }
                            None => {
                                ui.spinner();
                            }
                        }
                        break 'content;
                    };
                    self.update_ui(ui, ctx, update.as_ref());
                    self.requests_ui(ui, ctx, &requests);
                    self.session_ui(ui, overview);
                    if self.settings {
                        self.settings_ui(ui, ctx, &stored);
                        break 'content;
                    }
                    ui.horizontal(|ui| {
                        theme::selectable_value(
                            ui,
                            &mut self.tab,
                            Tab::ThisComputer,
                            &t("tabs.thisComputer"),
                        );
                        let count = overview
                            .get("clients")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        theme::selectable_value(
                            ui,
                            &mut self.tab,
                            Tab::Computers,
                            &tf("tabs.computers", &[("count", count.to_string().as_str())]),
                        );
                    });
                    ui.separator();
                    match self.tab {
                        Tab::ThisComputer => {
                            // On its own and not signed in yet, signing in comes first.
                            let signed_in = account.as_ref().and_then(|a| a.get("signedIn"))
                                == Some(&Value::Bool(true));
                            if !signed_in && desktop::realtimex_app().is_none() {
                                self.account_ui(ui, ctx, account.as_ref());
                                self.this_computer_ui(ui, ctx, overview, &password);
                            } else {
                                self.this_computer_ui(ui, ctx, overview, &password);
                                self.account_ui(ui, ctx, account.as_ref());
                            }
                        }
                        Tab::Computers => self.computers_ui(ui, ctx, overview),
                    }
                }
                let focused = ctx.memory(|memory| memory.focused());
                // A click moves focus too: only the keyboard's moves scroll.
                let clicked = ctx.input(|i| i.pointer.any_pressed() || i.pointer.any_released());
                if focused != self.last_focus {
                    self.last_focus = focused;
                    // Only this area's own controls: not the gear or the footer's button, not a
                    // popup's items.
                    let rect = focused
                        .filter(|id| !self.chrome.contains(&Some(*id)))
                        .and_then(|id| ctx.read_response(id))
                        .filter(|r| r.layer_id == ui.layer_id())
                        .map(|r| r.rect);
                    if let (Some(rect), false) = (rect, clicked) {
                        ui.scroll_to_rect(rect, None);
                    }
                }
            });
            fit_height(ctx, scroll.content_size.y - scroll.inner_rect.height());
        });

        self.confirm_ui(ctx);
    }
}

impl LinkApp {
    /// A newer RealTimeX Link: Update, and how it is going.
    fn update_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, update: Option<&Value>) {
        let Some(update) = update else { return };
        let available = update.get("available") == Some(&Value::Bool(true));
        let error = update.get("error").and_then(Value::as_str);
        if !available {
            return;
        }
        let latest = text(update, "latest");
        alert(
            ui,
            Tone::Info,
            &tf("update.available", &[("version", latest.as_str())]),
            |ui| {
                ui.horizontal(|ui| {
                    caption(
                        ui,
                        tf(
                            "update.current",
                            &[("version", text(update, "current").as_str())],
                        ),
                    );
                    let page = text(update, "page");
                    if !page.is_empty() && ui.link(t("update.whatsNew")).clicked() {
                        desktop::open_url(&page);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match update.get("busy").and_then(Value::as_str) {
                            Some(busy) => {
                                ui.label(if busy == "downloading" {
                                    t("update.downloading")
                                } else {
                                    t("update.installing")
                                });
                                ui.spinner();
                            }
                            None => {
                                if primary_button(ui, &t("update.update")).clicked() {
                                    start_update(&self.shared, &self.refresh, ctx);
                                }
                            }
                        }
                    });
                });
                if let Some(error) = error {
                    ui.label(RichText::new(error).color(tone_colors(ui, Tone::Danger).2));
                }
            },
        );
    }

    /// The RealTimeX account: Link signs in itself in the browser, or takes RealTimeX's.
    fn account_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, account: Option<&Value>) {
        let Some(account) = account else { return };
        let signed_in = account.get("signedIn") == Some(&Value::Bool(true));
        let signing_in = account.get("signingIn") == Some(&Value::Bool(true));
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(semibold(t("account.title")).size(15.0));
                if signed_in {
                    badge(ui, &t("account.signedIn"), Tone::Success);
                }
            });
            if let Some(error) = account.get("error").and_then(Value::as_str) {
                ui.label(RichText::new(error).color(tone_colors(ui, Tone::Danger).2));
            }
            if signed_in {
                ui.horizontal(|ui| {
                    let email = text(account, "email");
                    ui.label(&email);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(t("account.signOut")).clicked() {
                            self.confirm = Some(Confirm::SignOut);
                        }
                    });
                });
                if text(account, "via") == "realtimex" {
                    caption(ui, t("account.viaRealtimex"));
                }
                return;
            }
            caption(ui, t("account.pitch"));
            if signing_in {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t("account.finishInBrowser"));
                });
            }
            ui.horizontal_wrapped(|ui| {
                let label = if signing_in {
                    t("account.openBrowserAgain")
                } else {
                    t("account.signIn")
                };
                if primary_button(ui, &label).clicked() {
                    self.sign_in(ctx);
                }
                if let Some(offer) = account.get("offer").and_then(Value::as_str) {
                    let who = if offer.is_empty() {
                        t("account.realtimexAccount")
                    } else {
                        offer.to_string()
                    };
                    if ui
                        .button(tf("account.continueAs", &[("name", who.as_str())]))
                        .clicked()
                    {
                        self.act(
                            ctx,
                            "/v1/account/use-realtimex",
                            json!({}),
                            &t("account.signedInWithRealtimex"),
                        );
                    }
                }
            });
        });
    }

    /// Start the sign-in in the daemon, then open its page in the browser.
    fn sign_in(&self, ctx: &egui::Context) {
        let (shared, refresh, ctx) = (self.shared.clone(), self.refresh.clone(), ctx.clone());
        std::thread::spawn(move || {
            let api = lock(&shared).api.clone();
            let answer = match api {
                Some(api) => api.post("/v1/account/sign-in", json!({})),
                None => Err(anyhow::anyhow!(t("errors.serviceNotRunning"))),
            };
            match answer.map(|answer| text(&answer, "url")) {
                Ok(url) if !url.is_empty() => desktop::open_url(&url),
                Ok(_) => {}
                Err(error) => {
                    lock(&shared).notice = Some((error.to_string(), false, Instant::now()));
                }
            }
            let _ = refresh.send(());
            ctx.request_repaint();
        });
    }

    /// A new computer asks: Allow control, View only or Deny, before it expires.
    fn requests_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, requests: &[Value]) {
        for request in requests {
            let name = text(request, "clientName");
            let reference = text(request, "ref");
            let same = request.pointer("/account/sameAccount") == Some(&Value::Bool(true));
            let title = tf("request.wantsToConnect", &[("name", name.as_str())]);
            alert(ui, Tone::Info, &title, |ui| {
                let mut detail = vec![t("request.newComputer")];
                let machine = text(request, "machine");
                if !machine.is_empty() {
                    detail.push(machine);
                }
                detail.push(if same {
                    t("request.yourAccount")
                } else {
                    t("request.notYourAccount")
                });
                if let Some(login) = request.get("tailnetLogin").and_then(Value::as_str) {
                    detail.push(tf("request.tailscaleUser", &[("login", login)]));
                }
                ui.label(detail.join(" · "));
                if !same {
                    ui.label(
                        RichText::new(t("request.someoneElse"))
                            .color(tone_colors(ui, Tone::Warning).2),
                    );
                }
                let expires = expires_in(&text(request, "expiresAt"));
                caption(
                    ui,
                    tf("request.remembered", &[("expires", expires.as_str())]),
                );
                ui.horizontal_wrapped(|ui| {
                    let decide = |decision: &str, done: String| {
                        act(
                            &self.shared,
                            &self.refresh,
                            ctx,
                            "/v1/host/connection-requests/decide",
                            json!({ "requestRef": reference, "decision": decision }),
                            &done,
                        );
                    };
                    let named = [("name", name.as_str())];
                    if primary_button(ui, &t("request.allowControl")).clicked() {
                        decide("allow-control", tf("request.allowedControl", &named));
                    }
                    if ui.button(t("request.viewOnly")).clicked() {
                        decide("allow-view", tf("request.allowedView", &named));
                    }
                    if ui.button(t("request.deny")).clicked() {
                        decide("deny", tf("request.denied", &named));
                    }
                });
            });
        }
    }

    /// Who is in control right now, with Stop.
    fn session_ui(&mut self, ui: &mut egui::Ui, overview: &Value) {
        let session = overview.get("session").cloned().unwrap_or(Value::Null);
        if session.get("active") != Some(&Value::Bool(true)) {
            return;
        }
        let name = text(&session, "clientName");
        alert(
            ui,
            Tone::Warning,
            &tf("session.controlling", &[("name", name.as_str())]),
            |ui| {
                ui.horizontal(|ui| {
                    let since = ago(&text(&session, "since"));
                    caption(ui, tf("session.since", &[("time", since.as_str())]));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(t("session.stop")).clicked() {
                            self.confirm = Some(Confirm::StopSession(name.clone()));
                        }
                    });
                });
            },
        );
    }

    fn this_computer_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        overview: &Value,
        password: &str,
    ) {
        let state = text(overview, "state");
        let mut allow = overview.get("allowIncoming") == Some(&Value::Bool(true));
        let permissions = overview.get("permissions").cloned().unwrap_or(Value::Null);
        let missing: Vec<&str> = ["screenRecording", "accessibility"]
            .into_iter()
            .filter(|name| permissions.get(*name) == Some(&Value::Bool(false)))
            .collect();
        // Reachable through this computer's own Public Exposure (rtx-linkd's relay.rs) too.
        let relayed = overview
            .pointer("/publicExposure/state")
            .and_then(Value::as_str)
            == Some("ready");
        // The badge (host.state.*) and the line under it (host.description.*).
        let (label, tone, description) = match state.as_str() {
            "off" => ("off", Tone::Neutral, ""),
            "unsupported" => ("unsupported", Tone::Neutral, "unsupported"),
            "error" => ("error", Tone::Danger, "error"),
            "waiting-tailscale" if relayed && missing.is_empty() => {
                ("ready", Tone::Success, "relayOnly")
            }
            "waiting-tailscale" => ("waiting", Tone::Warning, "waitingTailscale"),
            "starting" => ("starting", Tone::Neutral, "starting"),
            _ if !missing.is_empty() => ("needsSetup", Tone::Warning, "needsPermissions"),
            _ if relayed => ("ready", Tone::Success, "tailscaleOrRelay"),
            _ => ("ready", Tone::Success, "tailscale"),
        };
        card(ui, |ui| {
            // The switch and its label sit beside the title, or on a line of their own when the
            // window is too narrow for both (the minimum width, longer languages).
            let allow_label = t("host.allowRemoteControl");
            let mut changed = false;
            let mut switch = |ui: &mut egui::Ui, allow: &mut bool| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    changed |= toggle(ui, allow).changed();
                    ui.label(allow_label.as_str());
                });
            };
            let mut beside = true;
            ui.horizontal(|ui| {
                ui.label(semibold(t("host.title")).size(15.0));
                badge(ui, &t(&format!("host.state.{label}")), tone);
                let needed = egui::WidgetText::from(allow_label.as_str())
                    .into_galley(
                        ui,
                        Some(egui::TextWrapMode::Extend),
                        f32::INFINITY,
                        egui::TextStyle::Body,
                    )
                    .size()
                    .x
                    + 2.2 * ui.spacing().interact_size.y
                    + 2.0 * ui.spacing().item_spacing.x;
                beside = ui.available_width() >= needed;
                if beside {
                    switch(ui, &mut allow);
                }
            });
            if !beside {
                ui.horizontal(|ui| switch(ui, &mut allow));
            }
            if changed {
                if allow {
                    self.act(
                        ctx,
                        "/v1/host/settings",
                        json!({ "allowIncoming": true }),
                        &t("host.turnedOn"),
                    );
                } else {
                    self.confirm = Some(Confirm::TurnOff);
                }
            }
            if state == "off" {
                ui.label(t("host.offExplained"));
                return;
            }
            caption(ui, t(&format!("host.description.{description}")));
            if state == "unsupported" || state == "error" {
                let detail = text(overview, "stateDetail");
                if !detail.is_empty() {
                    ui.label(detail);
                }
            }
            if !missing.is_empty() {
                alert(ui, Tone::Warning, &t("permissions.title"), |ui| {
                    ui.label(t("permissions.explain"));
                    for (name, title) in [
                        ("screenRecording", t("permissions.screenRecording")),
                        ("accessibility", t("permissions.accessibility")),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(title);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if missing.contains(&name) {
                                        if ui.button(t("permissions.openSettings")).clicked() {
                                            self.act(
                                                ctx,
                                                "/v1/host/permissions/request",
                                                json!({ "name": name }),
                                                "",
                                            );
                                            desktop::open_privacy_settings(name);
                                        }
                                    } else {
                                        badge(ui, &t("permissions.granted"), Tone::Success);
                                    }
                                },
                            );
                        });
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if ui.button(t("permissions.checkAgain")).clicked() {
                            let _ = self.refresh.send(());
                        }
                    });
                });
            }
            if state == "waiting-tailscale" {
                alert(ui, Tone::Info, &t("tailscale.notRunningTitle"), |ui| {
                    ui.label(t("tailscale.notRunning"));
                });
            }
            ui.add_space(4.0);
            caption(ui, t("host.appId"));
            ui.horizontal(|ui| {
                let app_id = text(overview, "appId");
                ui.label(RichText::new(&app_id).monospace().size(17.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(t("common.copy")).clicked() {
                        ctx.copy_text(app_id.clone());
                        lock(&self.shared).notice =
                            Some((t("host.appIdCopied"), true, Instant::now()));
                    }
                });
            });
            caption(ui, t("host.password"));
            match self.editing_password.as_mut() {
                Some(draft) => {
                    ui.add(
                        egui::TextEdit::singleline(draft)
                            .hint_text(t("password.newHint"))
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(
                        RichText::new(t("password.signsOutAll"))
                            .small()
                            .color(tone_colors(ui, Tone::Warning).2),
                    );
                    let short = draft.chars().count() < 8;
                    let mut done = false;
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!short, egui::Button::new(t("common.save")))
                            .clicked()
                        {
                            act(
                                &self.shared,
                                &self.refresh,
                                ctx,
                                "/v1/host/password/set",
                                json!({ "password": draft.clone() }),
                                &t("password.changed"),
                            );
                            done = true;
                        }
                        if ui.button(t("common.cancel")).clicked() {
                            done = true;
                        }
                    });
                    if done {
                        self.editing_password = None;
                    }
                }
                None => {
                    ui.horizontal(|ui| {
                        let shown = if self.show_password {
                            password.to_string()
                        } else {
                            "••••-••••-••••".into()
                        };
                        ui.label(RichText::new(shown).monospace().size(17.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(t("common.copy")).clicked() {
                                ctx.copy_text(password.to_string());
                                lock(&self.shared).notice =
                                    Some((t("password.copied"), true, Instant::now()));
                            }
                            if ui
                                .button(if self.show_password {
                                    t("common.hide")
                                } else {
                                    t("common.show")
                                })
                                .clicked()
                            {
                                self.show_password = !self.show_password;
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        if ui.link(t("password.new")).clicked() {
                            self.confirm = Some(Confirm::NewPassword);
                        }
                        if ui.link(t("password.setOwn")).clicked() {
                            self.editing_password = Some(String::new());
                        }
                        let changed = text(overview, "passwordChangedAt");
                        if !changed.is_empty() {
                            caption(
                                ui,
                                tf("password.changedAgo", &[("time", ago(&changed).as_str())]),
                            );
                        }
                    });
                }
            }
            ui.separator();
            ui.label(semibold(t("policy.title")));
            let policy = text(overview, "newComputerPolicy");
            let policies = [
                ("ask", t("policy.ask"), t("policy.askHelp")),
                (
                    "own-account",
                    t("policy.ownAccount"),
                    t("policy.ownAccountHelp"),
                ),
                (
                    "password-only",
                    t("policy.passwordOnly"),
                    t("policy.passwordOnlyHelp"),
                ),
            ];
            let mut chosen = policy.clone();
            let current = policies
                .iter()
                .find(|(id, ..)| *id == policy)
                .map_or_else(|| t("policy.ask"), |(_, label, _)| label.clone());
            let combo = egui::ComboBox::from_id_salt("policy")
                .width(ui.available_width())
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (id, label, help) in policies {
                        theme::selectable_value(ui, &mut chosen, id.to_string(), &label)
                            .on_hover_text(help);
                    }
                });
            keep_focus(&mut self.open_combo, ctx, &combo.response);
            if chosen != policy {
                self.act(
                    ctx,
                    "/v1/host/settings",
                    json!({ "newComputerPolicy": chosen }),
                    &t("common.saved"),
                );
            }
            ui.label(semibold(t("access.title")));
            let control =
                overview.pointer("/defaultPermissions/control") == Some(&Value::Bool(true));
            let mut access = control;
            let combo = egui::ComboBox::from_id_salt("access")
                .width(ui.available_width())
                .selected_text(access_label(control))
                .show_ui(ui, |ui| {
                    theme::selectable_value(ui, &mut access, true, &access_label(true))
                        .on_hover_text(t("access.fullHelp"));
                    theme::selectable_value(ui, &mut access, false, &access_label(false))
                        .on_hover_text(t("access.viewHelp"));
                });
            keep_focus(&mut self.open_combo, ctx, &combo.response);
            if access != control {
                self.act(
                    ctx,
                    "/v1/host/settings",
                    json!({ "defaultPermissions": { "view": true, "control": access } }),
                    &t("access.defaultsSaved"),
                );
            }
        });
    }

    fn computers_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, overview: &Value) {
        caption(ui, t("computers.explain"));
        let clients = overview
            .get("clients")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if clients.is_empty() {
            card(ui, |ui| {
                ui.label(t("computers.none"));
            });
        }
        for client in clients {
            let id = text(&client, "id");
            let name = text(&client, "name");
            let revoked = client.get("revoked") == Some(&Value::Bool(true));
            let control = client.pointer("/permissions/control") == Some(&Value::Bool(true));
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(semibold(&name));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if revoked {
                            badge(ui, &t("computers.blocked"), Tone::Danger);
                        } else if control {
                            badge(ui, &access_label(true), Tone::Info);
                        } else {
                            badge(ui, &access_label(false), Tone::Neutral);
                        }
                    });
                });
                let mut detail = Vec::new();
                let machine = text(&client, "machine");
                if !machine.is_empty() {
                    detail.push(machine);
                }
                let used = ago(&text(&client, "lastUsedAt"));
                detail.push(tf("computers.lastUsed", &[("time", used.as_str())]));
                match text(&client, "admittedBy").as_str() {
                    "password" => detail.push(t("computers.addedWithPassword")),
                    "request" => detail.push(t("computers.youAllowed")),
                    "account" => detail.push(t("request.yourAccount")),
                    _ => {}
                }
                caption(ui, detail.join(" · "));
                let mut access = control;
                ui.add_enabled_ui(!revoked, |ui| {
                    let combo = egui::ComboBox::from_id_salt(("access", &id))
                        .width(ui.available_width())
                        .selected_text(access_label(control))
                        .show_ui(ui, |ui| {
                            theme::selectable_value(ui, &mut access, true, &access_label(true));
                            theme::selectable_value(ui, &mut access, false, &access_label(false));
                        });
                    keep_focus(&mut self.open_combo, ctx, &combo.response);
                });
                if access != control {
                    let key = if access {
                        "computers.nowFull"
                    } else {
                        "computers.nowView"
                    };
                    let done = tf(key, &[("name", name.as_str())]);
                    self.act(
                        ctx,
                        "/v1/host/clients/permissions",
                        json!({ "id": id, "permissions": { "view": true, "control": access } }),
                        &done,
                    );
                }
                let named = [("name", name.as_str())];
                ui.horizontal(|ui| {
                    if !revoked && ui.button(t("computers.disconnect")).clicked() {
                        self.act(
                            ctx,
                            "/v1/host/clients/disconnect",
                            json!({ "id": id }),
                            &tf("computers.disconnected", &named),
                        );
                    }
                    if revoked {
                        if ui.button(t("computers.allowAgain")).clicked() {
                            self.act(
                                ctx,
                                "/v1/host/clients/allow",
                                json!({ "id": id }),
                                &tf("computers.allowedAgain", &named),
                            );
                        }
                    } else if ui.button(t("computers.block")).clicked() {
                        self.act(
                            ctx,
                            "/v1/host/clients/revoke",
                            json!({ "id": id }),
                            &tf("computers.blockedDone", &named),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(t("computers.remove"))
                            .on_hover_text(tf("computers.removeHover", &named))
                            .clicked()
                        {
                            self.confirm = Some(Confirm::Remove {
                                id: id.clone(),
                                name: name.clone(),
                            });
                        }
                    });
                });
            });
        }
    }

    /// Settings, in place of the tabs: Back, then the theme and the language. A change is saved
    /// at once (POST /v1/appearance); RealTimeX's own changes show on the next poll.
    fn settings_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        (stored_theme, language, failed): &(String, String, Vec<(Field, String)>),
    ) {
        ui.horizontal(|ui| {
            let back = theme::icon_button(ui, theme::Icon::ArrowLeft, &t("settings.back"));
            if self.focus == Some(Focus::Back) {
                back.request_focus();
                if settled(ctx) {
                    self.focus = None;
                }
            }
            if back.clicked() {
                self.settings = false;
                self.focus = Some(Focus::Gear);
            }
            ui.label(semibold(t("settings.title")).size(15.0));
        });
        ui.separator();
        if !failed.is_empty() {
            alert(ui, Tone::Danger, &t("settings.saveFailedTitle"), |ui| {
                ui.label(t("settings.saveFailed"));
                if ui.button(t("settings.tryAgain")).clicked() {
                    for (field, value) in failed {
                        save_setting(&self.shared, &self.refresh, ctx, *field, value);
                    }
                }
            });
        }
        card(ui, |ui| {
            ui.label(semibold(t("settings.theme")));
            caption(ui, t("settings.themeHelp"));
            let theme_label = |value: &str| {
                t(match value {
                    "light" => "settings.themeLight",
                    "dark" => "settings.themeDark",
                    _ => "settings.themeSystem",
                })
            };
            let mut chosen = stored_theme.clone();
            let combo = egui::ComboBox::from_id_salt("theme")
                .width(ui.available_width())
                .selected_text(theme_label(stored_theme.as_str()))
                .show_ui(ui, |ui| {
                    for value in ["system", "light", "dark"] {
                        theme::selectable_value(
                            ui,
                            &mut chosen,
                            value.to_string(),
                            &theme_label(value),
                        );
                    }
                });
            keep_focus(&mut self.open_combo, ctx, &combo.response);
            if chosen != *stored_theme {
                save_setting(&self.shared, &self.refresh, ctx, Field::Theme, &chosen);
            }
            ui.add_space(8.0);
            ui.label(semibold(t("settings.language")));
            caption(ui, t("settings.languageHelp"));
            // The stored language, as the list names it: "system", a locale shown here, or one
            // that can't be (the select then says English, and the note why).
            let unavailable = crate::i18n::unavailable(language);
            let current = if language == "system" || language.is_empty() {
                "system".to_string()
            } else if unavailable.is_some() {
                language.clone()
            } else {
                crate::i18n::effective_locale(language).code.to_string()
            };
            let system_default = tf(
                "settings.systemDefault",
                &[("language", crate::i18n::effective_locale("system").endonym)],
            );
            let selected = match current.as_str() {
                "system" => system_default.clone(),
                _ => crate::i18n::effective_locale(language).endonym.to_string(),
            };
            let mut chosen = current.clone();
            let combo = egui::ComboBox::from_id_salt("language")
                .width(ui.available_width())
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    theme::selectable_value(ui, &mut chosen, "system".to_string(), &system_default);
                    for locale in crate::i18n::offered_languages() {
                        theme::selectable_value_in(
                            ui,
                            &mut chosen,
                            locale.code.to_string(),
                            locale.endonym,
                            crate::cjk::endonym_family(locale.code),
                        );
                    }
                });
            keep_focus(&mut self.open_combo, ctx, &combo.response);
            if let Some(name) = &unavailable {
                caption(ui, tf("settings.cantShow", &[("language", name.as_str())]));
            }
            if chosen != current {
                save_setting(&self.shared, &self.refresh, ctx, Field::Language, &chosen);
            }
            if desktop::realtimex_app().is_some() {
                ui.add_space(4.0);
                caption(ui, t("settings.realtimexNote"));
            }
        });
    }

    /// Changes that end someone's access ask first.
    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        // confirm.<which>.title, .message and .action; {name} where there is one.
        let (which, name) = match confirm {
            Confirm::TurnOff => ("turnOff", ""),
            Confirm::NewPassword => ("newPassword", ""),
            Confirm::StopSession(name) => ("stop", name.as_str()),
            Confirm::Remove { name, .. } => ("remove", name.as_str()),
            Confirm::SignOut => ("signOut", ""),
        };
        let part = |part: &str| tf(&format!("confirm.{which}.{part}"), &[("name", name)]);
        let (title, message, action) = (part("title"), part("message"), part("action"));
        let mut close = false;
        let mut confirmed = false;
        egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(340.0);
            ui.label(semibold(title).size(15.0));
            ui.label(message);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(semibold(action).color(tone_colors(ui, Tone::Danger).2))
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.button(t("common.cancel")).clicked() {
                    close = true;
                }
            });
        });
        if confirmed {
            match self.confirm.take() {
                Some(Confirm::TurnOff) => self.act(
                    ctx,
                    "/v1/host/settings",
                    json!({ "allowIncoming": false }),
                    &t("host.turnedOff"),
                ),
                Some(Confirm::NewPassword) => self.act(
                    ctx,
                    "/v1/host/password/regenerate",
                    json!({}),
                    &t("password.regenerated"),
                ),
                Some(Confirm::StopSession(name)) => self.act(
                    ctx,
                    "/v1/host/stop-session",
                    json!({}),
                    &tf("session.ended", &[("name", name.as_str())]),
                ),
                Some(Confirm::Remove { id, name }) => self.act(
                    ctx,
                    "/v1/host/clients/remove",
                    json!({ "id": id }),
                    &tf("computers.removed", &[("name", name.as_str())]),
                ),
                Some(Confirm::SignOut) => self.act(
                    ctx,
                    "/v1/account/sign-out",
                    json!({}),
                    &t("account.signedOut"),
                ),
                None => {}
            }
        } else if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm = None;
        }
    }
}
