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

fn number(section: &Value, name: &str) -> u64 {
    section.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn names(section: &Value, name: &str) -> Vec<String> {
    section
        .get(name)
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

/// The card's rows, in the prototype's order. `open`: RealTimeX runs (an origin is registered).
fn rows(shared: &Value, open: bool) -> Vec<Row> {
    let up = || (t("shared.up"), Tone::Success);
    let offline = || (t("shared.offline"), Tone::Neutral);
    let mut rows = Vec::new();
    if let Some(section) = shared.get("artifacts") {
        rows.push(Row {
            name: t("shared.artifacts"),
            detail: counted("shared.published", number(section, "published")),
            // Served by RealTimeX until Link serves their folders itself (realtimex-link#6 M2).
            badge: if open { up() } else { offline() },
        });
    }
    if let Some(section) = shared.get("webhooks") {
        rows.push(Row {
            name: t("shared.webhooks"),
            detail: counted("shared.endpoints", number(section, "endpoints")),
            badge: if open { up() } else { offline() },
        });
    }
    if let Some(section) = shared.get("embed") {
        let widgets = counted("shared.widgets", number(section, "widgets"));
        let sites = names(section, "sites");
        rows.push(Row {
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
        });
    }
    if let Some(section) = shared.get("mobile") {
        let phones = counted("shared.phones", number(section, "phones"));
        rows.push(Row {
            name: t("shared.mobile"),
            detail: if open {
                phones
            } else {
                format!("{phones} · {}", t("shared.reconnectWhenOpen"))
            },
            badge: if open { up() } else { offline() },
        });
    }
    if let Some(section) = shared.get("peers") {
        let desktops = names(section, "desktops").join(", ");
        rows.push(Row {
            name: t("shared.peers"),
            detail: if open {
                desktops
            } else {
                format!("{desktops} · {}", t("shared.whileOpen"))
            },
            badge: if open { up() } else { offline() },
        });
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
                                "/v1/keep-running",
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
    ) {
        let Some(answer) = shared else { return };
        let Some(list) = answer.get("shared").filter(|s| {
            s.as_object()
                .is_some_and(|map| map.keys().any(|k| k != "account"))
        }) else {
            return;
        };
        let open = answer.get("origin") == Some(&Value::Bool(true));
        // RealTimeX signed in to another account than Link: nothing of RealTimeX's goes through.
        let theirs = text(list, "account");
        let ours = account.map(|a| text(a, "email")).unwrap_or_default();
        let other_account =
            !theirs.is_empty() && !ours.is_empty() && !theirs.eq_ignore_ascii_case(&ours);
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
                    let offered = account
                        .and_then(|a| a.get("offer"))
                        .and_then(Value::as_str)
                        .is_some();
                    if offered
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
            for row in rows(list, open) {
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
        let shared = json!({
            "artifacts": { "published": 3 },
            "webhooks": { "endpoints": 2 },
            "embed": { "widgets": 1, "sites": ["support.example.com"] },
            "peers": { "desktops": ["Office Mac", "Studio PC"] },
        });
        let open = rows(&shared, true);
        assert_eq!(open.len(), 4);
        assert!(open.iter().all(|row| row.badge.1 == Tone::Success));
        assert!(open[2].detail.contains("support.example.com"));
        assert_eq!(open[3].detail, "Office Mac, Studio PC");
        let closed = rows(&shared, false);
        assert!(closed.iter().all(|row| row.badge.1 == Tone::Neutral));
        assert!(rows(&json!({}), true).is_empty());
    }
}
