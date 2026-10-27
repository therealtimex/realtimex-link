//! The menu bar (macOS) / tray (Windows, Linux) icon: whether remote control is on and who is in
//! control, Copy App ID and password, turn it off or on, open the window or RealTimeX, update
//! Link, and Quit,
//! which stops hosting (closing the window does not). How the window opens and text is copied is
//! up to the process that shows the icon (Actions): the window's own on macOS and Windows, a
//! separate window process on Linux (icon.rs); copying is clipboard.rs's on both. Another Link
//! instance (instance.rs) has a dot on its icon, its name as the menu's first line and tooltip, and
//! no Update or Open RealTimeX: those would act on this computer's Link and RealTimeX.

use std::sync::mpsc::Sender;
use std::time::Instant;

use eframe::egui::{self, ViewportCommand};
use serde_json::{json, Value};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app::{act, lock, start_update, Shared};
use crate::desktop;
use crate::i18n::{t, tf};
use crate::instance::Instance;

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
    icon: TrayIcon,
    menu: Menu,
    /// Another instance's name, the menu's first line (Linux desktops often show no tooltip).
    name: MenuItem,
    status: MenuItem,
    toggle: MenuItem,
    update: MenuItem,
    copy: MenuItem,
    open: MenuItem,
    realtimex: MenuItem,
    quit: MenuItem,
    /// "RealTimeX will start Link again", under Quit when it will.
    quit_note: MenuItem,
    /// The instance, Quit's case and the language the menu was laid out for.
    layout: std::cell::RefCell<Option<(Instance, Quit, &'static str)>>,
    last: std::cell::RefCell<(String, String, String)>,
    /// Whether the items that need Link running are enabled now.
    enabled: std::cell::Cell<Option<bool>>,
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
/// system tints for the light and dark menu bar; elsewhere in colour at 32 px. Another instance's
/// ("-other") has a dot at the top left, black in the template, violet elsewhere.
fn icon(default: bool) -> Icon {
    #[cfg(target_os = "macos")]
    let (rgba, size): (&[u8], u32) = if default {
        (include_bytes!("../icons/tray-template-44.rgba"), 44)
    } else {
        (include_bytes!("../icons/tray-template-44-other.rgba"), 44)
    };
    #[cfg(not(target_os = "macos"))]
    let (rgba, size): (&[u8], u32) = if default {
        (include_bytes!("../icons/tray-32.rgba"), 32)
    } else {
        (include_bytes!("../icons/tray-32-other.rgba"), 32)
    };
    Icon::from_rgba(rgba.to_vec(), size, size).expect("icon")
}

/// What Quit says (the Link 0.2.1 UX contract, §4): the first case that holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quit {
    /// RealTimeX owns Link and Keep running is on: it starts Link again ("Quit", and a note).
    Restarted,
    /// Link starts at login: "Quit until next login".
    UntilLogin,
    /// "Quit".
    Plain,
}

/// From GET /v1/status: realtimexPresent, running (Keep running) and autostartEnabled (false on
/// another instance). None yet: plain.
fn quit_case(status: Option<&Value>) -> Quit {
    let flag = |name: &str| status.and_then(|s| s.get(name)).and_then(Value::as_bool) == Some(true);
    if flag("realtimexPresent") && flag("running") {
        Quit::Restarted
    } else if flag("autostartEnabled") {
        Quit::UntilLogin
    } else {
        Quit::Plain
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Name,
    Status,
    Separator,
    Copy,
    Toggle,
    Open,
    Realtimex,
    Update,
    Quit,
    QuitNote,
}

/// The menu, top to bottom: another instance's begins with its name and has no Open RealTimeX or
/// Update; the note under Quit only when RealTimeX starts Link again.
fn entries(default: bool, quit: Quit) -> Vec<Entry> {
    let mut entries = Vec::new();
    if !default {
        entries.push(Entry::Name);
    }
    entries.extend([
        Entry::Status,
        Entry::Separator,
        Entry::Copy,
        Entry::Toggle,
        Entry::Separator,
        Entry::Open,
    ]);
    if default {
        entries.extend([Entry::Realtimex, Entry::Update]);
    }
    entries.extend([Entry::Separator, Entry::Quit]);
    if quit == Quit::Restarted {
        entries.push(Entry::QuitNote);
    }
    entries
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
        let quit = MenuItem::new(t("tray.quitPlain"), true, None);
        let name = MenuItem::new("", false, None);
        let quit_note = MenuItem::new(t("tray.quitRestartNote"), false, None);
        // Filled by lay_out, below.
        let menu = Menu::new();
        let instance = crate::instance::current();
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip(instance.name());
        // macOS tints a template icon for the light and dark menu bar.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon(instance.default));
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon(instance.default));
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
        let tray = Self {
            icon,
            menu,
            name,
            status,
            toggle,
            update,
            copy,
            open,
            realtimex,
            quit,
            quit_note,
            layout: Default::default(),
            last: Default::default(),
            enabled: std::cell::Cell::new(None),
            language: std::cell::Cell::new(crate::i18n::current().code),
        };
        tray.lay_out(None);
        Ok(tray)
    }

    /// The menu's items, Quit's wording, the tooltip and the icon, for the instance (as last
    /// read) and Quit's case: again only when one of them or the language changed.
    fn lay_out(&self, status: Option<&Value>) {
        let instance = crate::instance::current();
        let quit = quit_case(status);
        let layout = (instance.clone(), quit, crate::i18n::current().code);
        let previous = self.layout.replace(Some(layout.clone()));
        if previous.as_ref() == Some(&layout) {
            return;
        }
        if previous.as_ref().map(|(instance, ..)| instance) != Some(&instance) {
            let _ = self.icon.set_tooltip(Some(instance.name()));
            #[cfg(target_os = "macos")]
            let _ = self.icon.set_icon_templated(Some(icon(instance.default)));
            #[cfg(not(target_os = "macos"))]
            let _ = self.icon.set_icon(Some(icon(instance.default)));
        }
        self.name.set_text(instance.short_name());
        self.quit.set_text(t(if quit == Quit::UntilLogin {
            "tray.quit"
        } else {
            "tray.quitPlain"
        }));
        self.quit_note.set_text(t("tray.quitRestartNote"));
        while self.menu.remove_at(0).is_some() {}
        for entry in entries(instance.default, quit) {
            let _ = match entry {
                Entry::Name => self.menu.append(&self.name),
                Entry::Status => self.menu.append(&self.status),
                Entry::Separator => self.menu.append(&PredefinedMenuItem::separator()),
                Entry::Copy => self.menu.append(&self.copy),
                Entry::Toggle => self.menu.append(&self.toggle),
                Entry::Open => self.menu.append(&self.open),
                Entry::Realtimex => self.menu.append(&self.realtimex),
                Entry::Update => self.menu.append(&self.update),
                Entry::Quit => self.menu.append(&self.quit),
                Entry::QuitNote => self.menu.append(&self.quit_note),
            };
        }
    }

    /// The status line, the switch's and the update item's labels, from the latest state; every
    /// label when the language changed, and the layout when the instance or Quit's case did. `status` is GET /v1/status: with "Keep RealTimeX Link
    /// running" off, the status says so and copying and remote control are unavailable (turning
    /// Link on is the switch at the top of the window).
    pub fn update(&self, overview: Option<&Value>, update: Option<&Value>, status: Option<&Value>) {
        let running = status
            .and_then(|s| s.get("running"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if self.enabled.replace(Some(running)) != Some(running) {
            self.copy.set_enabled(running);
            self.toggle.set_enabled(running);
        }
        let language = crate::i18n::current().code;
        if self.language.replace(language) != language {
            self.copy.set_text(t("tray.copy"));
            self.open.set_text(t("tray.open"));
            self.realtimex.set_text(realtimex_label());
        }
        self.lay_out(status);
        let on = overview.and_then(|o| o.get("allowIncoming")) == Some(&Value::Bool(true));
        let holder = overview
            .and_then(|o| o.get("session"))
            .filter(|s| s.get("active") == Some(&Value::Bool(true)))
            .and_then(|s| s.get("clientName"))
            .and_then(Value::as_str);
        let status = match (overview, on, holder) {
            (None, ..) => t("tray.starting"),
            _ if !running => t("tray.linkOff"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_says_what_happens_next() {
        let case = |status: Value| quit_case(Some(&status));
        // RealTimeX starts Link again: that wins over the login item.
        assert_eq!(
            case(json!({ "realtimexPresent": true, "running": true, "autostartEnabled": true })),
            Quit::Restarted
        );
        assert_eq!(
            case(json!({ "realtimexPresent": true, "running": false, "autostartEnabled": true })),
            Quit::UntilLogin
        );
        assert_eq!(
            case(json!({ "realtimexPresent": false, "running": true, "autostartEnabled": true })),
            Quit::UntilLogin
        );
        // Another instance never starts at login.
        assert_eq!(
            case(json!({ "realtimexPresent": false, "running": true, "autostartEnabled": false })),
            Quit::Plain
        );
        // An older daemon, or none read yet.
        assert_eq!(case(json!({ "running": true })), Quit::Plain);
        assert_eq!(quit_case(None), Quit::Plain);
    }

    #[test]
    fn another_instance_is_named_and_leaves_this_computers_link_alone() {
        use Entry as E;
        assert_eq!(
            entries(true, Quit::UntilLogin),
            [
                E::Status,
                E::Separator,
                E::Copy,
                E::Toggle,
                E::Separator,
                E::Open,
                E::Realtimex,
                E::Update,
                E::Separator,
                E::Quit
            ]
        );
        assert_eq!(
            entries(false, Quit::Restarted),
            [
                E::Name,
                E::Status,
                E::Separator,
                E::Copy,
                E::Toggle,
                E::Separator,
                E::Open,
                E::Separator,
                E::Quit,
                E::QuitNote
            ]
        );
        assert_eq!(entries(true, Quit::Plain).last(), Some(&E::Quit));
    }
}
