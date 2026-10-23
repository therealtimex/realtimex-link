//! The window: This computer and Computers, a request from a new computer, who is in control,
//! and Open RealTimeX / Get RealTimeX. State comes from rtx-linkd every 1.5 s; each change is one
//! request to it, made off the UI thread.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke, ViewportCommand};
use serde_json::{json, Value};

use crate::api::Api;
use crate::desktop;

const POLL: Duration = Duration::from_millis(1500);
/// The window's height follows its content between these; past the maximum the content scrolls.
const MIN_HEIGHT: f32 = 360.0;
const MAX_HEIGHT: f32 = 720.0;
const NOTICE_FOR: Duration = Duration::from_secs(4);

#[derive(Default)]
pub struct Snapshot {
    pub api: Option<Api>,
    pub overview: Option<Value>,
    pub requests: Vec<Value>,
    pub password: String,
    pub error: Option<String>,
    pub notice: Option<(String, bool, Instant)>,
    /// The daemon's showWindow count when last read: another rtx-link asked this one to show.
    pub show_window: Option<u64>,
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
            None => Err(anyhow::anyhow!("RealTimeX Link's service is not running")),
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

pub fn poll_loop(shared: Shared, wake: Receiver<()>, ctx: egui::Context) {
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
            let shown = api
                .get("/v1/status")
                .ok()
                .and_then(|status| status["showWindow"].as_u64());
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
            let mut state = lock(&shared);
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

enum Confirm {
    TurnOff,
    NewPassword,
    StopSession(String),
    Remove { id: String, name: String },
}

pub struct LinkApp {
    shared: Shared,
    refresh: Sender<()>,
    tab: Tab,
    show_password: bool,
    editing_password: Option<String>,
    confirm: Option<Confirm>,
    tray: Option<crate::tray::Tray>,
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
        Self {
            tray,
            shared,
            refresh,
            tab: Tab::ThisComputer,
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

#[derive(Clone, Copy)]
enum Tone {
    Success,
    Warning,
    Danger,
    Info,
    Neutral,
}

fn tone_colors(ui: &egui::Ui, tone: Tone) -> (Color32, Color32) {
    let dark = ui.visuals().dark_mode;
    match (tone, dark) {
        (Tone::Success, true) => (
            Color32::from_rgb(20, 61, 38),
            Color32::from_rgb(134, 239, 172),
        ),
        (Tone::Success, false) => (
            Color32::from_rgb(220, 252, 231),
            Color32::from_rgb(22, 101, 52),
        ),
        (Tone::Warning, true) => (
            Color32::from_rgb(66, 46, 12),
            Color32::from_rgb(253, 211, 77),
        ),
        (Tone::Warning, false) => (
            Color32::from_rgb(254, 243, 199),
            Color32::from_rgb(133, 77, 14),
        ),
        (Tone::Danger, true) => (
            Color32::from_rgb(69, 20, 20),
            Color32::from_rgb(252, 165, 165),
        ),
        (Tone::Danger, false) => (
            Color32::from_rgb(254, 226, 226),
            Color32::from_rgb(153, 27, 27),
        ),
        (Tone::Info, true) => (
            Color32::from_rgb(23, 37, 72),
            Color32::from_rgb(147, 197, 253),
        ),
        (Tone::Info, false) => (
            Color32::from_rgb(219, 234, 254),
            Color32::from_rgb(30, 64, 175),
        ),
        (Tone::Neutral, true) => (Color32::from_gray(48), Color32::from_gray(220)),
        (Tone::Neutral, false) => (Color32::from_gray(232), Color32::from_gray(60)),
    }
}

fn badge(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let (fill, fg) = tone_colors(ui, tone);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(fg));
        });
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let visuals = ui.visuals();
    egui::Frame::new()
        .fill(visuals.extreme_bg_color)
        .stroke(Stroke::new(
            1.0,
            visuals.widgets.noninteractive.bg_stroke.color,
        ))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn alert<R>(ui: &mut egui::Ui, tone: Tone, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let (fill, fg) = tone_colors(ui, tone);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().color(fg));
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

fn caption(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().weak());
}

/// The switch from egui's demo: a pill with a knob.
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
            "Allow remote control",
        )
    });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let visuals = ui.style().interact_selectable(&response, *on);
        let radius = 0.5 * rect.height();
        ui.painter().rect(
            rect,
            radius,
            visuals.bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter().circle(
            egui::pos2(x, rect.center().y),
            0.75 * radius,
            visuals.fg_stroke.color,
            visuals.fg_stroke,
        );
    }
    response
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
    format!("Expires in {}:{:02}", left / 60, left % 60)
}

fn ago(iso: &str) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return "never".into();
    };
    let seconds = (chrono::Utc::now().timestamp() - at.timestamp()).max(0);
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
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
        }
        crate::dock::apply();
        if let Some(tray) = &self.tray {
            tray.update(lock(&self.shared).overview.as_ref());
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        let (overview, requests, password, error, notice) = {
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
                state.error.clone(),
                state.notice.clone(),
            )
        };
        // Countdowns and notices move on their own.
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("RealTimeX Link").strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(machine) = overview
                        .as_ref()
                        .map(|o| text(o, "machine"))
                        .filter(|m| !m.is_empty())
                    {
                        badge(ui, &machine, Tone::Neutral);
                    }
                });
            });
            ui.add_space(8.0);
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(8.0);
            let installed = desktop::realtimex_app().is_some();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if installed {
                        if ui.button("Open RealTimeX").clicked() {
                            desktop::open_realtimex();
                        }
                    } else if ui
                        .add(egui::Button::new(RichText::new("Get RealTimeX").strong()))
                        .clicked()
                    {
                        desktop::open_realtimex();
                    }
                    ui.add(egui::Label::new(
                        RichText::new(if installed {
                            "Control your other computers from RealTimeX."
                        } else {
                            "To see and control your other computers from this one, get RealTimeX."
                        })
                        .small()
                        .weak(),
                    )
                    .wrap());
                });
            });
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let scroll = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
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
                            Some(error) => alert(
                                ui,
                                Tone::Warning,
                                "RealTimeX Link's service isn't running",
                                |ui| {
                                    ui.label(error);
                                    caption(ui, "Trying again…");
                                },
                            ),
                            None => {
                                ui.spinner();
                            }
                        }
                        return;
                    };
                    self.requests_ui(ui, ctx, &requests);
                    self.session_ui(ui, overview);
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.tab, Tab::ThisComputer, "This computer");
                        let count = overview
                            .get("clients")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        ui.selectable_value(
                            &mut self.tab,
                            Tab::Computers,
                            format!("Computers · {count}"),
                        );
                    });
                    ui.separator();
                    match self.tab {
                        Tab::ThisComputer => self.this_computer_ui(ui, ctx, overview, &password),
                        Tab::Computers => self.computers_ui(ui, ctx, overview),
                    }
                });
            fit_height(ctx, scroll.content_size.y - scroll.inner_rect.height());
        });

        self.confirm_ui(ctx);
    }
}

impl LinkApp {
    /// A new computer asks: Allow control, View only or Deny, before it expires.
    fn requests_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, requests: &[Value]) {
        for request in requests {
            let name = text(request, "clientName");
            let reference = text(request, "ref");
            let same = request.pointer("/account/sameAccount") == Some(&Value::Bool(true));
            alert(ui, Tone::Info, &format!("{name} wants to connect"), |ui| {
                let mut detail = vec!["New computer".to_string()];
                let machine = text(request, "machine");
                if !machine.is_empty() {
                    detail.push(machine);
                }
                detail.push(if same {
                    "your account".into()
                } else {
                    "not your account".into()
                });
                if let Some(login) = request.get("tailnetLogin").and_then(Value::as_str) {
                    detail.push(format!("Tailscale user {login}"));
                }
                ui.label(detail.join(" · "));
                if !same {
                    ui.label(RichText::new("This computer belongs to someone else. Only allow it if you expect it.").color(tone_colors(ui, Tone::Warning).1));
                }
                caption(
                    ui,
                    format!(
                        "{} · Once allowed, it connects without asking next time.",
                        expires_in(&text(request, "expiresAt"))
                    ),
                );
                ui.horizontal_wrapped(|ui| {
                    let decide = |decision: &str, done: String| {
                        act(&self.shared, &self.refresh, ctx, "/v1/host/connection-requests/decide", json!({ "requestRef": reference, "decision": decision }), &done);
                    };
                    if ui.button(RichText::new("Allow control").strong()).clicked() {
                        decide("allow-control", format!("{name} can now control this computer. It connects without asking next time."));
                    }
                    if ui.button("View only").clicked() {
                        decide("allow-view", format!("{name} can now view this computer. It connects without asking next time."));
                    }
                    if ui.button("Deny").clicked() {
                        decide("deny", format!("Denied. {name} was not let in."));
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
            &format!("{name} is controlling this computer"),
            |ui| {
                ui.horizontal(|ui| {
                    caption(ui, format!("Since {}", ago(&text(&session, "since"))));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Stop").clicked() {
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
        let (label, tone, description) = match state.as_str() {
            "off" => ("Off", Tone::Neutral, ""),
            "unsupported" => (
                "Not supported",
                Tone::Neutral,
                "Remote control isn't available on this desktop.",
            ),
            "error" => (
                "Error",
                Tone::Danger,
                "Something went wrong. Details below.",
            ),
            "waiting-tailscale" => (
                "Waiting",
                Tone::Warning,
                "No other computer can connect until Tailscale runs.",
            ),
            "starting" => ("Starting", Tone::Neutral, "Getting ready…"),
            _ if !missing.is_empty() => (
                "Needs setup",
                Tone::Warning,
                "Allow the macOS permissions below, then other computers can connect.",
            ),
            _ => (
                "Ready",
                Tone::Success,
                "Other computers on your Tailscale network can connect.",
            ),
        };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("This computer").strong().size(15.0));
                badge(ui, label, tone);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if toggle(ui, &mut allow).changed() {
                        if allow {
                            self.act(
                                ctx,
                                "/v1/host/settings",
                                json!({ "allowIncoming": true }),
                                "Remote control is on.",
                            );
                        } else {
                            self.confirm = Some(Confirm::TurnOff);
                        }
                    }
                    ui.label("Allow remote control");
                });
            });
            if state == "off" {
                ui.label("Remote control is off. No other computer can see or use this screen. Turn it on to give another computer this App ID and password.");
                return;
            }
            caption(ui, description);
            if state == "unsupported" || state == "error" {
                let detail = text(overview, "stateDetail");
                if !detail.is_empty() {
                    ui.label(detail);
                }
            }
            if !missing.is_empty() {
                alert(
                    ui,
                    Tone::Warning,
                    "Allow RealTimeX Link to see and control this Mac",
                    |ui| {
                        ui.label("macOS asks for this once. Open each setting, turn on RealTimeX Link, then check again.");
                        for (name, title) in [
                            ("screenRecording", "Screen Recording"),
                            ("accessibility", "Accessibility"),
                        ] {
                            ui.horizontal(|ui| {
                                ui.label(title);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if missing.contains(&name) {
                                            if ui.button("Open Settings").clicked() {
                                                self.act(
                                                    ctx,
                                                    "/v1/host/permissions/request",
                                                    json!({ "name": name }),
                                                    "",
                                                );
                                                desktop::open_privacy_settings(name);
                                            }
                                        } else {
                                            badge(ui, "Granted", Tone::Success);
                                        }
                                    },
                                );
                            });
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            if ui.button("Check again").clicked() {
                                let _ = self.refresh.send(());
                            }
                        });
                    },
                );
            }
            if state == "waiting-tailscale" {
                alert(ui, Tone::Info, "Tailscale isn't running", |ui| {
                    ui.label("Other computers reach this one over your Tailscale network. Start Tailscale and sign in.");
                });
            }
            ui.add_space(4.0);
            caption(ui, "App ID");
            ui.horizontal(|ui| {
                let app_id = text(overview, "appId");
                ui.label(RichText::new(&app_id).monospace().size(17.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Copy").clicked() {
                        ctx.copy_text(app_id.clone());
                        lock(&self.shared).notice =
                            Some(("App ID copied.".into(), true, Instant::now()));
                    }
                });
            });
            caption(ui, "Password");
            match self.editing_password.as_mut() {
                Some(draft) => {
                    ui.add(
                        egui::TextEdit::singleline(draft)
                            .hint_text("New password, at least 8 characters")
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(
                        RichText::new(
                            "Every computer is signed out and must use the new password.",
                        )
                        .small()
                        .color(tone_colors(ui, Tone::Warning).1),
                    );
                    let short = draft.chars().count() < 8;
                    let mut done = false;
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!short, egui::Button::new("Save")).clicked() {
                            act(
                                &self.shared,
                                &self.refresh,
                                ctx,
                                "/v1/host/password/set",
                                json!({ "password": draft.clone() }),
                                "Password changed. Every computer must sign in again.",
                            );
                            done = true;
                        }
                        if ui.button("Cancel").clicked() {
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
                            if ui.button("Copy").clicked() {
                                ctx.copy_text(password.to_string());
                                lock(&self.shared).notice =
                                    Some(("Password copied.".into(), true, Instant::now()));
                            }
                            if ui
                                .button(if self.show_password { "Hide" } else { "Show" })
                                .clicked()
                            {
                                self.show_password = !self.show_password;
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        if ui.link("New password").clicked() {
                            self.confirm = Some(Confirm::NewPassword);
                        }
                        if ui.link("Set my own").clicked() {
                            self.editing_password = Some(String::new());
                        }
                        let changed = text(overview, "passwordChangedAt");
                        if !changed.is_empty() {
                            caption(ui, format!("Changed {}", ago(&changed)));
                        }
                    });
                }
            }
            ui.separator();
            ui.label(RichText::new("When a new computer asks to connect").strong());
            let policy = text(overview, "newComputerPolicy");
            let policies = [
                (
                    "ask",
                    "Ask me",
                    "You allow or deny each one here; it expires after a minute",
                ),
                (
                    "own-account",
                    "Allow my own computers without asking",
                    "New computers on your account are let in at once; others ask",
                ),
                (
                    "password-only",
                    "Password only",
                    "New computers need the App ID and password; no requests",
                ),
            ];
            let mut chosen = policy.clone();
            let current = policies
                .iter()
                .find(|(id, ..)| *id == policy)
                .map_or("Ask me", |(_, label, _)| label);
            egui::ComboBox::from_id_salt("policy")
                .width(ui.available_width())
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (id, label, help) in policies {
                        ui.selectable_value(&mut chosen, id.to_string(), label)
                            .on_hover_text(help);
                    }
                });
            if chosen != policy {
                self.act(
                    ctx,
                    "/v1/host/settings",
                    json!({ "newComputerPolicy": chosen }),
                    "Saved.",
                );
            }
            ui.label(RichText::new("Access for new computers").strong());
            let control =
                overview.pointer("/defaultPermissions/control") == Some(&Value::Bool(true));
            let mut access = control;
            egui::ComboBox::from_id_salt("access")
                .width(ui.available_width())
                .selected_text(if control { "Full access" } else { "View only" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut access, true, "Full access")
                        .on_hover_text(
                            "See the screen, use the mouse, keyboard, clipboard and files",
                        );
                    ui.selectable_value(&mut access, false, "View only")
                        .on_hover_text("See the screen only");
                });
            if access != control {
                self.act(
                    ctx,
                    "/v1/host/settings",
                    json!({ "defaultPermissions": { "view": true, "control": access } }),
                    "New computers get this access. Computers already here keep theirs.",
                );
            }
        });
    }

    fn computers_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, overview: &Value) {
        caption(ui, "Each one signed in with the password, or you allowed it. Changing the password signs them all out.");
        let clients = overview
            .get("clients")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if clients.is_empty() {
            card(ui, |ui| {
                ui.label(
                    "No computers yet. Give another computer this App ID and password to connect.",
                );
            });
        }
        for client in clients {
            let id = text(&client, "id");
            let name = text(&client, "name");
            let revoked = client.get("revoked") == Some(&Value::Bool(true));
            let control = client.pointer("/permissions/control") == Some(&Value::Bool(true));
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&name).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if revoked {
                            badge(ui, "Blocked", Tone::Danger);
                        } else if control {
                            badge(ui, "Full access", Tone::Info);
                        } else {
                            badge(ui, "View only", Tone::Neutral);
                        }
                    });
                });
                let mut detail = Vec::new();
                let machine = text(&client, "machine");
                if !machine.is_empty() {
                    detail.push(machine);
                }
                detail.push(format!("last used {}", ago(&text(&client, "lastUsedAt"))));
                match text(&client, "admittedBy").as_str() {
                    "password" => detail.push("added with the password".into()),
                    "request" => detail.push("you allowed it".into()),
                    "account" => detail.push("your account".into()),
                    _ => {}
                }
                caption(ui, detail.join(" · "));
                let mut access = control;
                ui.add_enabled_ui(!revoked, |ui| {
                    egui::ComboBox::from_id_salt(("access", &id))
                        .width(ui.available_width())
                        .selected_text(if control { "Full access" } else { "View only" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut access, true, "Full access");
                            ui.selectable_value(&mut access, false, "View only");
                        });
                });
                if access != control {
                    let done = format!(
                        "{name} now has {}.",
                        if access { "full access" } else { "view only" }
                    );
                    self.act(
                        ctx,
                        "/v1/host/clients/permissions",
                        json!({ "id": id, "permissions": { "view": true, "control": access } }),
                        &done,
                    );
                }
                ui.horizontal(|ui| {
                    if !revoked && ui.button("Disconnect").clicked() {
                        self.act(ctx, "/v1/host/clients/disconnect", json!({ "id": id }), &format!("{name} was disconnected. It can connect again; block it to stop that."));
                    }
                    if revoked {
                        if ui.button("Allow again").clicked() {
                            self.act(ctx, "/v1/host/clients/allow", json!({ "id": id }), &format!("{name} can connect again."));
                        }
                    } else if ui.button("Block").clicked() {
                        self.act(ctx, "/v1/host/clients/revoke", json!({ "id": id }), &format!("{name} can't connect until you allow it again."));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Remove").on_hover_text(format!("Remove {name}")).clicked() {
                            self.confirm = Some(Confirm::Remove { id: id.clone(), name: name.clone() });
                        }
                    });
                });
            });
        }
    }

    /// Changes that end someone's access ask first.
    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let (title, message, action) = match confirm {
            Confirm::TurnOff => (
                "Turn off remote control?".to_string(),
                "No other computer can see or use this screen until you turn it on again. Any session ends now.".to_string(),
                "Turn off",
            ),
            Confirm::NewPassword => (
                "Make a new password?".to_string(),
                "Every computer is signed out and must use the new password.".to_string(),
                "New password",
            ),
            Confirm::StopSession(name) => (
                format!("Stop {name}'s session?"),
                "It stops seeing and controlling this computer now. It can connect again unless you block it.".to_string(),
                "Stop",
            ),
            Confirm::Remove { name, .. } => (
                "Remove computer".to_string(),
                format!("Remove {name}? Its session ends. With the password it can connect again as a new computer."),
                "Remove",
            ),
        };
        let mut close = false;
        let mut confirmed = false;
        egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(340.0);
            ui.label(RichText::new(title).strong().size(15.0));
            ui.label(message);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(
                        RichText::new(action)
                            .strong()
                            .color(tone_colors(ui, Tone::Danger).1),
                    )
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
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
                    "Remote control is off. No other computer can connect.",
                ),
                Some(Confirm::NewPassword) => self.act(
                    ctx,
                    "/v1/host/password/regenerate",
                    json!({}),
                    "New password. Every computer must sign in again.",
                ),
                Some(Confirm::StopSession(name)) => self.act(
                    ctx,
                    "/v1/host/stop-session",
                    json!({}),
                    &format!("{name}'s session ended."),
                ),
                Some(Confirm::Remove { id, name }) => self.act(
                    ctx,
                    "/v1/host/clients/remove",
                    json!({ "id": id }),
                    &format!("{name} was removed."),
                ),
                None => {}
            }
        } else if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm = None;
        }
    }
}
