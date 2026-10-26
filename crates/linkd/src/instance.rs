//! Which Link instance this daemon is. Each RealTimeX (the packaged app, dev:all, each worktree)
//! runs its own Link on a data folder of its own; the folder is the instance. The default folder
//! (paths::is_standard) is the computer's Link: no label, and the only one that starts at login
//! and updates itself. Any other folder carries a label RealTimeX gives it (`--label`), kept in
//! the folder so a restart keeps it, and shown as "RealTimeX Link (<label>)".

use std::path::Path;
use std::sync::OnceLock;

use serde_json::{json, Value};

pub const FILE: &str = "instance.json";
/// Long enough for "worktree <name>"; the window and tray shorten it where they must.
pub const MAX_LABEL: usize = 64;

static LABEL: OnceLock<Option<String>> = OnceLock::new();

/// The label as shown: trimmed, control characters dropped, whitespace runs made one space, at
/// most MAX_LABEL characters. None when nothing is left.
pub fn sanitize(text: &str) -> Option<String> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect();
    let label: String = words.join(" ").chars().take(MAX_LABEL).collect();
    let label = label.trim_end().to_string();
    (!label.is_empty()).then_some(label)
}

/// The label stored in `dir`, if any.
pub fn stored(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    sanitize(value.get("label")?.as_str()?)
}

/// This instance's label at start, written by the daemon only (after it took the folder's lock).
/// `flag` is `--label`: None keeps the stored label, Some("") clears it. The default folder never
/// has one and its folder is never written.
pub fn settle(
    dir: &Path,
    flag: Option<&str>,
    standard: bool,
    log: &dyn Fn(&str),
) -> Option<String> {
    if standard {
        if flag.is_some_and(|text| !text.is_empty()) {
            log("[instance] --label is ignored: this is the default RealTimeX Link");
        }
        return None;
    }
    let Some(flag) = flag else {
        return stored(dir);
    };
    let label = sanitize(flag);
    if label != stored(dir) {
        let written =
            crate::files::write_atomically(&dir.join(FILE), &json!({ "label": label }).to_string());
        if let Err(error) = written {
            log(&format!("[instance] couldn't keep the label: {error}"));
        }
    }
    label
}

pub fn remember(label: Option<String>) {
    let _ = LABEL.set(label);
}

/// This instance's label; None for the default instance (or one RealTimeX gave no label).
pub fn label() -> Option<String> {
    LABEL.get().cloned().flatten()
}

/// `instance` in /v1/status: { default, label }.
pub fn status() -> Value {
    json!({ "default": crate::paths::is_standard(), "label": label() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_one_line_and_bounded() {
        assert_eq!(sanitize("dev").as_deref(), Some("dev"));
        assert_eq!(
            sanitize("  worktree   my-branch \n").as_deref(),
            Some("worktree my-branch")
        );
        assert_eq!(sanitize("a\u{7}b\tc").as_deref(), Some("a b c"));
        assert_eq!(sanitize(" \n\t"), None);
        assert_eq!(sanitize(""), None);
        let long = "é".repeat(80);
        assert_eq!(sanitize(&long).unwrap().chars().count(), MAX_LABEL);
        // Cut at the limit, no trailing space left.
        let spaced = format!("{} b", "a".repeat(MAX_LABEL - 1));
        assert_eq!(sanitize(&spaced).unwrap(), "a".repeat(MAX_LABEL - 1));
    }

    #[test]
    fn the_label_is_kept_across_restarts_and_cleared_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let log = |_: &str| {};
        assert_eq!(settle(dir.path(), None, false, &log), None);
        assert_eq!(
            settle(dir.path(), Some("worktree x"), false, &log).as_deref(),
            Some("worktree x")
        );
        // A restart without --label keeps it.
        assert_eq!(
            settle(dir.path(), None, false, &log).as_deref(),
            Some("worktree x")
        );
        assert_eq!(settle(dir.path(), Some(""), false, &log), None);
        assert_eq!(settle(dir.path(), None, false, &log), None);
    }

    #[test]
    fn the_default_folder_has_no_label_and_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let logged = std::sync::Mutex::new(Vec::new());
        let log = |line: &str| logged.lock().unwrap().push(line.to_string());
        assert_eq!(settle(dir.path(), Some("dev"), true, &log), None);
        assert!(!dir.path().join(FILE).exists());
        assert_eq!(logged.lock().unwrap().len(), 1);
    }
}
