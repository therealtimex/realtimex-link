//! What realtimex-ai-app#2836 adds to the window (prototype v3): "RealTimeX Link" with its
//! On/Off switch above the tabs, the one-time "This computer now has one App ID", and the
//! read-only "Shared from this computer" card, written here from what RealTimeX pushed
//! (GET /v1/shared) and whether RealTimeX runs now.

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
/// `open`: RealTimeX runs (an origin is registered).
fn rows(items: &Value, open: bool) -> Vec<Row> {
    let up = || (t("shared.up"), Tone::Success);
    let offline = || (t("shared.offline"), Tone::Neutral);
    let mut rows = Vec::new();
    for item in items.as_array().into_iter().flatten() {
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
        let row = match kind {
            // Served by RealTimeX until Link serves their folders itself (realtimex-link#6 M2).
            "artifacts" => Row {
                name: t("shared.artifacts"),
                detail: counted("shared.published", count(item)),
                badge: if open { up() } else { offline() },
            },
            "webhooks" => Row {
                name: t("shared.webhooks"),
                detail: counted("shared.endpoints", count(item)),
                badge: if open { up() } else { offline() },
            },
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
                                &t("link.turnedOn"),
                            );
                        } else {
                            self.confirm = Some(Confirm::TurnOffLink);
                        }
                    }
                });
            });
            caption(
                ui,
                t(if on {
                    "link.onDescription"
                } else {
                    "link.offDescription"
                }),
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
        alert(ui, Tone::Info, &t("moved.title"), |ui| {
            ui.label(tf(
                "moved.message",
                &[("appId", app_id.as_str()), ("old", from.as_str())],
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if primary_button(ui, &t("moved.ok")).clicked() {
                    self.act(ctx, "/v1/exposure/notice-seen", json!({}), "");
                }
            });
        });
    }

    /// "Shared from this computer": only when RealTimeX pushed what it shares. Read-only; the
    /// footer says where to change it.
    pub(super) fn shared_ui(
        &mut self,
        ui: &mut egui::Ui,
        shared: Option<&Value>,
        account: Option<&Value>,
        exposure: Option<&Value>,
    ) {
        let Some(answer) = shared else { return };
        let Some(items) = answer
            .get("items")
            .filter(|items| items.as_array().is_some_and(|list| !list.is_empty()))
        else {
            return;
        };
        let open = answer.get("origin") == Some(&Value::Bool(true));
        // RealTimeX handed over another account than Link's: nothing of RealTimeX's goes through.
        let other_account = exposure.map(|e| text(e, "state")).as_deref() == Some("other-account");
        let theirs = account.map(|a| text(a, "offer")).unwrap_or_default();
        let ours = account.map(|a| text(a, "email")).unwrap_or_default();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(semibold(t("shared.title")).size(15.0));
                if other_account {
                    badge(ui, &t("shared.notShared"), Tone::Warning);
                } else if open {
                    badge(ui, &t("shared.online"), Tone::Success);
                } else {
                    badge(ui, &t("shared.realtimexClosed"), Tone::Info);
                }
            });
            caption(ui, t("shared.explain"));
            if other_account {
                alert(ui, Tone::Warning, &t("shared.otherAccountTitle"), |ui| {
                    ui.label(tf(
                        "shared.otherAccount",
                        &[("realtimex", theirs.as_str()), ("link", ours.as_str())],
                    ));
                    if !theirs.is_empty()
                        && ui
                            .button(tf("shared.useAccount", &[("name", theirs.as_str())]))
                            .clicked()
                    {
                        self.confirm = Some(Confirm::UseAccount(theirs.clone()));
                    }
                });
                return;
            }
            if !open {
                alert(ui, Tone::Info, &t("shared.closedTitle"), |ui| {
                    ui.label(t("shared.closed"));
                });
            }
            for row in rows(items, open) {
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
        let open = rows(&items, true);
        assert_eq!(open.len(), 4);
        assert!(open.iter().all(|row| row.badge.1 == Tone::Success));
        assert!(open[2].detail.contains("support.example.com"));
        assert_eq!(open[3].detail, "Office Mac, Studio PC");
        let closed = rows(&items, false);
        assert!(closed.iter().all(|row| row.badge.1 == Tone::Neutral));
        assert!(rows(&json!([]), true).is_empty());
    }
}
