//! The menu bar (macOS) / tray (Windows, Linux) icon: whether remote control is on and who is in
//! control, Copy App ID and password, turn it off or on, open the window or RealTimeX, update
//! Link, and Quit,
//! which stops hosting (closing the window does not). How the window opens and text is copied is
//! up to the process that shows the icon (Actions): the window's own on macOS and Windows, a
//! separate window process on Linux (icon.rs); copying is clipboard.rs's on both.

use std::sync::mpsc::Sender;
use std::time::Instant;

use eframe::egui::{self, ViewportCommand};
use serde_json::{json, Value};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app::{act, lock, start_update, Shared};
use crate::desktop;
use crate::i18n::{t, tf};

/// What the icon's menu does for the process showing it.
pub struct Actions {
    pub open: Box<dyn Fn() + Send + Sync>,
    pub copy: Box<dyn Fn(String) + Send + Sync>,
}

impl Actions {
    /// The icon's own window, hidden until opened (macOS, Windows).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn window(ctx: &egui::Context) -> Self {
        let (show, copy) = (ctx.clone(), ctx.clone());
        Self {
            open: Box::new(move || {
                crate::dock::set_open(true);
                show.send_viewport_cmd(ViewportCommand::Visible(true));
                show.send_viewport_cmd(ViewportCommand::Focus);
            }),
            // Straight to the clipboard: hidden, the window draws no frame to carry egui's copy.
            // Should that fail, egui's copy lands once the window draws again.
            copy: Box::new(move |text| {
                if crate::clipboard::copy(&text).is_err() {
                    copy.copy_text(text);
                }
            }),
        }
    }
}

pub struct Tray {
    _icon: TrayIcon,
    status: MenuItem,
    toggle: MenuItem,
    update: MenuItem,
    copy: MenuItem,
    open: MenuItem,
    realtimex: MenuItem,
    quit: MenuItem,
    last: std::cell::RefCell<(String, String, String)>,
    /// The locale the fixed items are in (they change only with the language).
    language: std::cell::Cell<&'static str>,
}

/// "Open RealTimeX", or "Get RealTimeX" when it is not on this computer.
fn realtimex_label() -> String {
    if desktop::realtimex_app().is_some() {
        t("footer.openRealtimex")
    } else {
        t("footer.getRealtimex")
    }
}

/// The menu bar / tray icon, the app icon without its navy square (made by scripts/icons.py): on
/// macOS a template (black on transparent, the badge's chain cut out, 44 px: 22 pt at 2x) the
/// system tints for the light and dark menu bar; elsewhere in colour at 32 px.
fn icon() -> Icon {
    #[cfg(target_os = "macos")]
    let (rgba, size) = (
        include_bytes!("../icons/tray-template-44.rgba").to_vec(),
        44,
    );
    #[cfg(not(target_os = "macos"))]
    let (rgba, size) = (include_bytes!("../icons/tray-32.rgba").to_vec(), 32);
    Icon::from_rgba(rgba, size, size).expect("icon")
}

impl Tray {
    pub fn new(
        shared: Shared,
        refresh: Sender<()>,
        ctx: egui::Context,
        actions: Actions,
    ) -> anyhow::Result<Self> {
        let status = MenuItem::new(t("tray.remoteControl"), false, None);
        let toggle = MenuItem::new(t("tray.turnOff"), true, None);
        let copy = MenuItem::new(t("tray.copy"), true, None);
        let open = MenuItem::new(t("tray.open"), true, None);
        let update = MenuItem::new(t("tray.checkUpdates"), true, None);
        let realtimex = MenuItem::new(realtimex_label(), true, None);
        let quit = MenuItem::new(t("tray.quit"), true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &status,
            &PredefinedMenuItem::separator(),
            &copy,
            &toggle,
            &PredefinedMenuItem::separator(),
            &open,
            &realtimex,
            &update,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("RealTimeX Link");
        // macOS tints a template icon for the light and dark menu bar.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon());
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon());
        let icon = builder.build()?;

        let ids = (
            copy.id().clone(),
            toggle.id().clone(),
            open.id().clone(),
            realtimex.id().clone(),
            update.id().clone(),
            quit.id().clone(),
        );
        // Handled here, not in the window: the window may be hidden and not drawing.
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let (copy, toggle, open, realtimex, update, quit) = &ids;
            let id = &event.id;
            if id == open {
                (actions.open)();
            } else if id == realtimex {
                desktop::open_realtimex();
            } else if id == update {
                let available = lock(&shared)
                    .update
                    .as_ref()
                    .and_then(|u| u.get("available"))
                    == Some(&Value::Bool(true));
                if available {
                    (actions.open)();
                    start_update(&shared, &refresh, &ctx);
                } else {
                    act(&shared, &refresh, &ctx, "/v1/update/check", json!({}), "");
                }
            } else if id == copy {
                let state = lock(&shared);
                let app_id = state
                    .overview
                    .as_ref()
                    .and_then(|o| o.get("appId"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let text = tf(
                    "tray.copyText",
                    &[
                        ("appId", app_id.as_str()),
                        ("password", state.password.as_str()),
                    ],
                );
                drop(state);
                (actions.copy)(text);
                lock(&shared).notice = Some((t("tray.copied"), true, Instant::now()));
            } else if id == toggle {
                let on = lock(&shared)
                    .overview
                    .as_ref()
                    .and_then(|o| o.get("allowIncoming"))
                    == Some(&Value::Bool(true));
                let done = t(if on {
                    "host.turnedOff"
                } else {
                    "host.turnedOn"
                });
                act(
                    &shared,
                    &refresh,
                    &ctx,
                    "/v1/host/settings",
                    json!({ "allowIncoming": !on }),
                    &done,
                );
            } else if id == quit {
                // Quit is all of RealTimeX Link: hosting stops too, and any other window.
                if let Some(api) = lock(&shared).api.clone() {
                    let _ = api.post(
                        "/v1/shutdown",
                        json!({ "app": true, "pid": std::process::id() }),
                    );
                }
                std::process::exit(0);
            }
            ctx.request_repaint();
        }));
        Ok(Self {
            _icon: icon,
            status,
            toggle,
            update,
            copy,
            open,
            realtimex,
            quit,
            last: Default::default(),
            language: std::cell::Cell::new(crate::i18n::current().code),
        })
    }

    /// The status line, the switch's and the update item's labels, from the latest state; every
    /// label when the language changed.
    pub fn update(&self, overview: Option<&Value>, update: Option<&Value>) {
        let language = crate::i18n::current().code;
        if self.language.replace(language) != language {
            self.copy.set_text(t("tray.copy"));
            self.open.set_text(t("tray.open"));
            self.realtimex.set_text(realtimex_label());
            self.quit.set_text(t("tray.quit"));
        }
        let on = overview.and_then(|o| o.get("allowIncoming")) == Some(&Value::Bool(true));
        let holder = overview
            .and_then(|o| o.get("session"))
            .filter(|s| s.get("active") == Some(&Value::Bool(true)))
            .and_then(|s| s.get("clientName"))
            .and_then(Value::as_str);
        let status = match (overview, on, holder) {
            (None, ..) => t("tray.starting"),
            (_, false, _) => t("tray.off"),
            (_, true, Some(name)) => tf("session.controlling", &[("name", name)]),
            (_, true, None) => t("tray.on"),
        };
        let toggle = t(if on { "tray.turnOff" } else { "tray.turnOn" });
        let latest = update
            .filter(|u| u.get("available") == Some(&Value::Bool(true)))
            .and_then(|u| u.get("latest"))
            .and_then(Value::as_str);
        let update_label = match latest {
            Some(version) => tf("tray.updateTo", &[("version", version)]),
            None => t("tray.checkUpdates"),
        };
        let mut last = self.last.borrow_mut();
        if last.2 != update_label {
            self.update.set_text(&update_label);
            last.2 = update_label;
        }
        if last.0 != status {
            self.status.set_text(&status);
            last.0 = status;
        }
        if last.1 != toggle {
            self.toggle.set_text(&toggle);
            last.1 = toggle;
        }
    }
}
