//! Which Link instance this window or icon belongs to (rtx-linkd's instance.rs): this computer's
//! own Link, or one a RealTimeX runs with a data folder of its own (dev:all, a worktree). Read
//! from GET /v1/status's instance, never from argv: a running daemon keeps the label it has. The
//! others are named everywhere this one says "RealTimeX Link" (the window title, the header, the
//! tray), and leave out what would act on this computer's Link or RealTimeX (Update, Open
//! RealTimeX). Layout and copy: the Link 0.2.1 UX contract (prototype issue-link-6-instances-v1).

use std::sync::Mutex;

use serde_json::Value;

use crate::i18n::t;

const NAME: &str = "RealTimeX Link";
/// The tray menu's first line and the macOS process name show this much of the label: a native
/// menu is as wide as its longest item.
const SHORT: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    /// This computer's Link (and an older daemon, which knows no other).
    pub default: bool,
    /// The label RealTimeX gave the instance (sanitized by the daemon, at most 64 characters).
    pub label: Option<String>,
}

impl Default for Instance {
    fn default() -> Self {
        Self {
            default: true,
            label: None,
        }
    }
}

static CURRENT: Mutex<Option<Instance>> = Mutex::new(None);

impl Instance {
    /// GET /v1/status's instance; None when the status has none (an older daemon).
    pub fn of(status: &Value) -> Option<Self> {
        let instance = status.get("instance").filter(|i| i.is_object())?;
        Some(Self {
            default: instance.get("default").and_then(Value::as_bool) != Some(false),
            label: instance
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(str::to_string),
        })
    }

    /// The label, or the word for none ("unnamed"): an unnamed instance still isn't this
    /// computer's Link. None for the default instance.
    pub fn label(&self) -> Option<String> {
        (!self.default).then(|| self.label.clone().unwrap_or_else(|| t("instance.unnamed")))
    }

    /// "RealTimeX Link", or "RealTimeX Link (<label>)": the window title and the tray tooltip.
    pub fn name(&self) -> String {
        self.label()
            .map_or_else(|| NAME.to_string(), |label| format!("{NAME} ({label})"))
    }

    /// The name with at most the label's first 32 characters: the tray menu's first line and the
    /// macOS process name.
    pub fn short_name(&self) -> String {
        self.label().map_or_else(
            || NAME.to_string(),
            |label| format!("{NAME} ({})", shortened(&label, SHORT)),
        )
    }
}

/// `text`'s first `most` characters, then "…", when it is longer.
fn shortened(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    let mut short: String = text.chars().take(most).collect();
    short.push('…');
    short
}

/// The instance as last read (main.rs before the window, then each poll); the default until then.
pub fn current() -> Instance {
    CURRENT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Remember what GET /v1/status says; a status without an instance (an older daemon) is the
/// default one.
pub fn remember(status: &Value) {
    *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(Instance::of(status).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn of(status: Value) -> Instance {
        Instance::of(&status).unwrap_or_default()
    }

    #[test]
    fn only_the_other_instances_are_named() {
        let default = of(json!({ "instance": { "default": true, "label": null } }));
        assert_eq!(default.name(), "RealTimeX Link");
        assert_eq!(default.short_name(), "RealTimeX Link");
        assert_eq!(default.label(), None);
        // An older daemon knows only this computer's Link.
        assert!(of(json!({ "running": true })).default);

        let dev = of(json!({ "instance": { "default": false, "label": "dev" } }));
        assert_eq!(dev.name(), "RealTimeX Link (dev)");
        assert_eq!(dev.short_name(), "RealTimeX Link (dev)");

        // No label: still not this computer's Link.
        let unnamed = of(json!({ "instance": { "default": false, "label": null } }));
        assert_eq!(unnamed.label().as_deref(), Some("unnamed"));
        assert_eq!(unnamed.name(), "RealTimeX Link (unnamed)");
        let blank = of(json!({ "instance": { "default": false, "label": "  " } }));
        assert_eq!(blank.name(), "RealTimeX Link (unnamed)");
    }

    #[test]
    fn the_menu_line_shows_the_first_32_characters() {
        let label = "worktree feat-2836-link-instances-and-tray-badge";
        let long = of(json!({ "instance": { "default": false, "label": label } }));
        assert_eq!(long.name(), format!("RealTimeX Link ({label})"));
        assert_eq!(
            long.short_name(),
            "RealTimeX Link (worktree feat-2836-link-instance…)"
        );
        let exact = "x".repeat(32);
        let fits = of(json!({ "instance": { "default": false, "label": exact } }));
        assert_eq!(fits.short_name(), format!("RealTimeX Link ({exact})"));
        // Characters, not bytes.
        assert_eq!(
            shortened(&"é".repeat(33), 32),
            format!("{}…", "é".repeat(32))
        );
    }
}
