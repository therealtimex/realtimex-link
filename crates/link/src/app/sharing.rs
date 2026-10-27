//! What realtimex-ai-app#2836 adds to the window (prototype v3): "RealTimeX Link" with its
//! On/Off switch above the tabs, the one-time "This computer now has one App ID" (or, after
//! RealTimeX switched accounts, that the previous account's links are offline), and the
//! read-only "Shared from this computer" card, written here from what RealTimeX pushed
//! (GET /v1/shared), whether RealTimeX runs now, and what Link does itself while it is closed:
//! published artifacts stay up and webhooks are kept for RealTimeX.

use super::*;

/// One row of the Shared card: its name, what it shares now, and a badge.
struct Row {
    name: String,
    detail: String,
    badge: (String, Tone),
}

fn count(item: &Value) -> u64 {
    item.get("count").and_then(Value::as_u64).unwrap_or(0)
}

fn names(item: &Value) -> Vec<String> {
    item.get("names")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// "{count} …" in its singular or plural form (shared.<key>.one / .other).
fn counted(key: &str, count: u64) -> String {
    let form = if count == 1 { "one" } else { "other" };
    tf(
        &format!("{key}.{form}"),
        &[("count", count.to_string().as_str())],
    )
}

/// The card's rows from RealTimeX's items (already in the card's order, rtx-linkd's shared.rs).
/// `open`: RealTimeX runs (an origin is registered); `waiting`: webhook deliveries Link keeps
/// for RealTimeX.
fn rows(items: &Value, open: bool, waiting: u64) -> Vec<Row> {
    let up = || (t("shared.up"), Tone::Success);
    let offline = || (t("shared.offline"), Tone::Neutral);
    let mut rows = Vec::new();
    for item in items.as_array().into_iter().flatten() {
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
        let row = match kind {
            // Link serves them from their folders itself, RealTimeX open or not.
            "artifacts" => Row {
                name: t("shared.artifacts"),
                detail: format!(
                    "{} · {}",
                    counted("shared.published", count(item)),
                    t("shared.artifactsStayUp")
                ),
                badge: up(),
            },
            // Taken live, or kept by Link while RealTimeX is closed. Deliveries still waiting show
            // whatever the origin says: a stale origin (RealTimeX crashed) must not hide them.
            "webhooks" => {
                let endpoints = counted("shared.endpoints", count(item));
                if waiting == 0 {
                    Row {
                        name: t("shared.webhooks"),
                        detail: format!("{endpoints} · {}", t("shared.webhooksKept")),
                        badge: up(),
                    }
                } else {
                    Row {
                        name: t("shared.webhooks"),
                        detail: format!(
                            "{endpoints} · {}",
                            counted("shared.deliveriesKept", waiting)
                        ),
                        badge: (counted("shared.waiting", waiting), Tone::Info),
                    }
                }
            }
            "widget" => {
                let widgets = counted("shared.widgets", count(item));
                let sites = names(item);
                Row {
                    name: t("shared.embed"),
                    detail: if !open {
                        format!("{widgets} · {}", t("shared.chatWhenOpen"))
                    } else if sites.is_empty() {
                        widgets
                    } else {
                        tf(
                            "shared.widgetsOn",
                            &[
                                ("widgets", widgets.as_str()),
                                ("sites", sites.join(", ").as_str()),
                            ],
                        )
                    },
                    badge: if open {
                        up()
                    } else {
                        (t("shared.chatOffline"), Tone::Neutral)
                    },
                }
            }
            "mobile" => {
                let phones = counted("shared.phones", count(item));
                Row {
                    name: t("shared.mobile"),
                    detail: if open {
                        phones
                    } else {
                        format!("{phones} · {}", t("shared.reconnectWhenOpen"))
                    },
                    badge: if open { up() } else { offline() },
                }
            }
            "messaging" => {
                let desktops = names(item).join(", ");
                Row {
                    name: t("shared.peers"),
                    detail: match (open, desktops.is_empty()) {
                        (true, _) => desktops,
                        (false, true) => t("shared.whileOpen"),
                        (false, false) => format!("{desktops} · {}", t("shared.whileOpen")),
                    },
                    badge: if open { up() } else { offline() },
                }
            }
            _ => continue,
        };
        rows.push(row);
    }
    rows
}

impl LinkApp {
    /// "RealTimeX Link", On or Off, and its switch: always at the top. Off, it says what can't
    /// reach this computer, and the tabs are gone (the caller stops after it).
    pub(super) fn power_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, on: bool) {
        card(ui, |ui| {
            let mut wanted = on;
            ui.horizontal(|ui| {
                ui.label(semibold("RealTimeX Link").size(15.0));
                if on {
                    badge(ui, &t("link.on"), Tone::Success);
                } else {
                    badge(ui, &t("link.off"), Tone::Warning);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if toggle(ui, &mut wanted, &t("link.keepRunning")).changed() {
                        if wanted {
                            self.act(
                                ctx,
                                "/v1/running",
                                json!({ "on": true }),
                                &t(&keep_running_copy("link.turnedOn")),
                            );
                        } else {
                            self.confirm = Some(Confirm::TurnOffLink);
                        }
                    }
                });
            });
            caption(
                ui,
                t(&keep_running_copy(if on {
                    "link.onDescription"
                } else {
                    "link.offDescription"
                })),
            );
        });
    }

    /// Shown once after Link adopted RealTimeX's exposure: one App ID now, the old one followed.
    pub(super) fn moved_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, exposure: &Value) {
        let Some(notice) = exposure.get("notice").filter(|n| n.is_object()) else {
            return;
        };
        let app_id = text(notice, "appId");
        let from = text(notice, "from");
        // RealTimeX switched accounts (§6.3 A4): the previous account's links went offline.
        let switched = text(notice, "kind") == "switched";
        let (title, tone) = if switched {
            (t("switched.title"), Tone::Warning)
        } else {
            (t("moved.title"), Tone::Info)
        };
        alert(ui, tone, &title, |ui| {
            if switched {
                ui.label(tf("switched.message", &[("old", from.as_str())]));
            } else {
                ui.label(tf(
                    "moved.message",
                    &[("appId", app_id.as_str()), ("old", from.as_str())],
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if primary_button(ui, &t("moved.ok")).clicked() {
                    self.act(ctx, "/v1/exposure/notice-seen", json!({}), "");
                }
            });
        });
    }

    /// "Shared from this computer": only when RealTimeX pushed what it shares. Read-only; the
    /// footer says where to change it. Whether Link serves comes first (GET /v1/exposure's state),
    /// whether RealTimeX runs only then.
    pub(super) fn shared_ui(
        &mut self,
        ui: &mut egui::Ui,
        shared: Option<&Value>,
        exposure: Option<&Value>,
    ) {
        let Some(answer) = shared else { return };
        let Some(items) = answer
            .get("items")
            .filter(|items| items.as_array().is_some_and(|list| !list.is_empty()))
        else {
            return;
        };
        let reach = Reach::of(exposure, answer.get("origin") == Some(&Value::Bool(true)));
        let waiting = answer.get("waiting").and_then(Value::as_u64).unwrap_or(0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(semibold(t("shared.title")).size(15.0));
                let (label, tone) = reach.badge();
                badge(ui, &label, tone);
            });
            caption(ui, t("shared.explain"));
            match &reach {
                Reach::SignedOut => {
                    alert(ui, Tone::Warning, &t("shared.signedOutTitle"), |ui| {
                        ui.label(t("shared.signedOut"));
                    });
                }
                Reach::Error(message) => {
                    alert(ui, Tone::Danger, &t("shared.errorTitle"), |ui| {
                        if !message.is_empty() {
                            ui.label(message);
                        }
                        ui.label(t("shared.keepsTrying"));
                    });
                }
                Reach::Closed => {
                    alert(ui, Tone::Info, &t("shared.closedTitle"), |ui| {
                        ui.label(t("shared.closed"));
                    });
                }
                Reach::Online | Reach::Connecting => {}
            }
            for row in card_rows(items, &reach, waiting) {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(semibold(&row.name));
                        if !row.detail.is_empty() {
                            caption(ui, &row.detail);
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        badge(ui, &row.badge.0, row.badge.1);
                    });
                });
            }
        });
    }
}

/// What the Shared card says about reaching this computer: Link's exposure state first
/// (GET /v1/exposure), and only when it is online whether RealTimeX runs (an origin).
#[derive(Debug, PartialEq)]
enum Reach {
    Online,
    Closed,
    Connecting,
    SignedOut,
    Error(String),
}

impl Reach {
    fn of(exposure: Option<&Value>, origin: bool) -> Self {
        let state = exposure.map(|e| text(e, "state"));
        match state.as_deref() {
            Some("signed-out") => Reach::SignedOut,
            Some("error") => Reach::Error(
                exposure
                    .and_then(|e| e.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            Some("connecting") => Reach::Connecting,
            // Online, or a daemon that doesn't say (before #6): RealTimeX decides.
            _ if origin => Reach::Online,
            _ => Reach::Closed,
        }
    }

    fn badge(&self) -> (String, Tone) {
        match self {
            Reach::Online => (t("shared.online"), Tone::Success),
            Reach::Closed => (t("shared.realtimexClosed"), Tone::Info),
            Reach::Connecting => (t("shared.connecting"), Tone::Info),
            Reach::SignedOut => (t("shared.notShared"), Tone::Warning),
            Reach::Error(_) => (t("shared.notReachable"), Tone::Danger),
        }
    }
}

/// The rows as `reach` shows them: hidden when nothing of RealTimeX's can go through (signed
/// out); "Connecting" or "Offline" while Link doesn't serve.
fn card_rows(items: &Value, reach: &Reach, waiting: u64) -> Vec<Row> {
    let mut rows = rows(items, *reach == Reach::Online, waiting);
    let badge = match reach {
        Reach::SignedOut => return Vec::new(),
        Reach::Connecting => (t("shared.connecting"), Tone::Neutral),
        Reach::Error(_) => (t("shared.offline"), Tone::Neutral),
        Reach::Online | Reach::Closed => return rows,
    };
    for row in &mut rows {
        row.badge = badge.clone();
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_what_realtimex_pushed_and_whether_it_runs() {
        let items = json!([
            { "kind": "artifacts", "count": 3, "names": [] },
            { "kind": "webhooks", "count": 2, "names": [] },
            { "kind": "widget", "count": 1, "names": ["support.example.com"] },
            { "kind": "messaging", "count": 2, "names": ["Office Mac", "Studio PC"] },
            { "kind": "later", "count": 1 },
        ]);
        let open = rows(&items, true, 0);
        assert_eq!(open.len(), 4);
        assert!(open.iter().all(|row| row.badge.1 == Tone::Success));
        // Deliveries waiting while an origin is registered still show.
        assert_eq!(
            rows(&items, true, 4)[1].badge,
            ("4 waiting".to_string(), Tone::Info)
        );
        assert!(open[2].detail.contains("support.example.com"));
        assert_eq!(open[3].detail, "Office Mac, Studio PC");
        // RealTimeX closed: artifacts stay up (Link serves them), webhooks wait for it, the
        // widget's chat and desktop messaging are offline.
        let closed = rows(&items, false, 4);
        assert_eq!(closed[0].badge.1, Tone::Success);
        assert_eq!(closed[1].badge, ("4 waiting".to_string(), Tone::Info));
        assert!(
            closed[1].detail.contains("4 deliveries"),
            "{}",
            closed[1].detail
        );
        assert!(closed[2..].iter().all(|row| row.badge.1 == Tone::Neutral));
        assert_eq!(rows(&items, false, 0)[1].badge.1, Tone::Success);
        assert!(rows(&json!([]), true, 0).is_empty());
    }

    #[test]
    fn whether_link_serves_comes_before_whether_realtimex_runs() {
        let items = json!([
            { "kind": "mobile", "count": 2, "names": [] },
            { "kind": "messaging", "count": 1, "names": ["Office Mac"] },
        ]);
        let exposure = |state: &str| json!({ "state": state, "error": { "code": "recovery-capped", "message": "taken back" } });
        let cases = [
            ("online", true, Reach::Online, Some(Tone::Success)),
            ("online", false, Reach::Closed, Some(Tone::Neutral)),
            ("connecting", true, Reach::Connecting, Some(Tone::Neutral)),
            ("connecting", false, Reach::Connecting, Some(Tone::Neutral)),
            ("signed-out", true, Reach::SignedOut, None),
            ("signed-out", false, Reach::SignedOut, None),
            (
                "error",
                true,
                Reach::Error("taken back".into()),
                Some(Tone::Neutral),
            ),
        ];
        for (state, origin, reach, row_tone) in cases {
            let got = Reach::of(Some(&exposure(state)), origin);
            assert_eq!(got, reach, "{state} {origin}");
            let shown = card_rows(&items, &got, 0);
            match row_tone {
                None => assert!(shown.is_empty(), "{state}"),
                Some(tone) => {
                    assert_eq!(shown.len(), 2, "{state}");
                    assert!(
                        shown.iter().all(|row| row.badge.1 == tone),
                        "{state} {origin}"
                    );
                }
            }
        }
        // Not "Online" while connecting with RealTimeX open, nor "RealTimeX closed" when signed out.
        assert_eq!(
            Reach::of(Some(&exposure("connecting")), true).badge().1,
            Tone::Info
        );
        assert_eq!(
            Reach::of(Some(&exposure("signed-out")), false).badge().1,
            Tone::Warning
        );
        assert_eq!(
            Reach::of(Some(&exposure("error")), true).badge().1,
            Tone::Danger
        );
        // A daemon from before #6 says nothing: RealTimeX decides.
        assert_eq!(Reach::of(None, true), Reach::Online);
    }
}
