//! The menu bar (macOS) / tray (Windows, Linux) icon: whether remote control is on and who is in
//! control, Copy App ID and password, turn it off or on, open the window or RealTimeX, update
//! Link, and Quit,
//! which stops hosting (closing the window does not). How the window opens and text is copied is
//! up to the process that shows the icon (Actions): the window's own on macOS and Windows, a
//! separate window process on Linux (icon.rs).

use std::sync::mpsc::Sender;
use std::time::Instant;

use eframe::egui::{self, ViewportCommand};
use serde_json::{json, Value};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app::{act, lock, start_update, Shared};
use crate::desktop;

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
            copy: Box::new(move |text| copy.copy_text(text)),
        }
    }
}

pub struct Tray {
    _icon: TrayIcon,
    status: MenuItem,
    toggle: MenuItem,
    update: MenuItem,
    last: std::cell::RefCell<(String, String, String)>,
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
        let status = MenuItem::new("Remote control", false, None);
        let toggle = MenuItem::new("Turn off remote control", true, None);
        let copy = MenuItem::new("Copy App ID and password", true, None);
        let open = MenuItem::new("Open RealTimeX Link…", true, None);
        let update = MenuItem::new("Check for updates", true, None);
        let realtimex = MenuItem::new(
            if desktop::realtimex_app().is_some() {
                "Open RealTimeX"
            } else {
                "Get RealTimeX"
            },
            true,
            None,
        );
        let quit = MenuItem::new("Quit (other computers can't connect)", true, None);
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
                let text = format!("App ID: {app_id}\nPassword: {}", state.password);
                drop(state);
                (actions.copy)(text);
                lock(&shared).notice =
                    Some(("App ID and password copied.".into(), true, Instant::now()));
            } else if id == toggle {
                let on = lock(&shared)
                    .overview
                    .as_ref()
                    .and_then(|o| o.get("allowIncoming"))
                    == Some(&Value::Bool(true));
                let done = if on {
                    "Remote control is off. No other computer can connect."
                } else {
                    "Remote control is on."
                };
                act(
                    &shared,
                    &refresh,
                    &ctx,
                    "/v1/host/settings",
                    json!({ "allowIncoming": !on }),
                    done,
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
            last: Default::default(),
        })
    }

    /// The status line, the switch's and the update item's labels, from the latest state.
    pub fn update(&self, overview: Option<&Value>, update: Option<&Value>) {
        let on = overview.and_then(|o| o.get("allowIncoming")) == Some(&Value::Bool(true));
        let holder = overview
            .and_then(|o| o.get("session"))
            .filter(|s| s.get("active") == Some(&Value::Bool(true)))
            .and_then(|s| s.get("clientName"))
            .and_then(Value::as_str);
        let status = match (overview, on, holder) {
            (None, ..) => "RealTimeX Link is starting…".to_string(),
            (_, false, _) => "Remote control is off".to_string(),
            (_, true, Some(name)) => format!("{name} is controlling this computer"),
            (_, true, None) => "Remote control is on".to_string(),
        };
        let toggle = if on {
            "Turn off remote control"
        } else {
            "Turn on remote control"
        }
        .to_string();
        let latest = update
            .filter(|u| u.get("available") == Some(&Value::Bool(true)))
            .and_then(|u| u.get("latest"))
            .and_then(Value::as_str);
        let update_label = match latest {
            Some(version) => format!("Update to RealTimeX Link {version}…"),
            None => "Check for updates".to_string(),
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
