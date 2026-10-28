//! Pasting through the clipboard without losing what was on it: the steps, over a small
//! [`Clipboard`] trait, so they are tested without Windows.
//!
//! The text goes on the clipboard, the paste keys are pressed, and the previous text comes
//! back afterwards, unless something else (the person, another app) put newer content there in
//! between. Pastes in a row (typing) bring it back once, after the last one: each one passes the
//! text the first one took off to the next.
//!
//! How "unchanged" is decided, under the clipboard lock: the sequence number is still the one read
//! right after the paste closed the clipboard, the paste's own window still owns it, and it still
//! holds the pasted text. One write moves the number by several (emptying, the text, and the
//! formats Windows synthesizes from it when the clipboard closes: 806 → 811 on Windows 11 in
//! Link 0.2.4 QA, where 0.2.3 saved the number before closing and accepted only one more, so
//! the old text never came back). Reading it after the close counts all of those as the
//! paste's own. Any later write moves it again, including one that keeps the owner and the
//! text (another app may SetClipboardData without EmptyClipboard, review of !6), so newer
//! content is kept. Owner and text also cover a write in the moment between closing and
//! reading the number, unless it keeps both.

use anyhow::Result;

/// The clipboard operations a paste needs, all called between `open` and `close`.
pub trait Clipboard {
    /// Open and lock it, retrying for a moment while another app holds it.
    fn open(&mut self) -> Result<()>;
    fn close(&mut self);
    /// Its text, or None when it holds something else.
    fn text(&mut self) -> Option<String>;
    /// Empty it (taking ownership) and put this text on it.
    fn replace(&mut self, text: &str) -> Result<()>;
    /// Whether the paste's own window owns the clipboard (the last one to empty it).
    fn owned(&mut self) -> bool;
    /// Windows' clipboard sequence number, which changes with every write; it can be read
    /// without opening the clipboard.
    fn sequence(&mut self) -> u32;
}

/// What a paste took off the clipboard, to put back with [`restore`].
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub previous: Option<String>,
    /// The text the paste put on, to recognise it later.
    pub pasted: String,
    /// The sequence number right after the paste closed the clipboard.
    pub sequence: u32,
}

/// Put the text on the clipboard, keeping its previous text. `pending` is an earlier paste not
/// restored yet: while the clipboard still holds its text, what comes back later is what that
/// paste took off. When the write fails the previous text goes back before the error is
/// returned, so a failed paste doesn't erase the clipboard.
pub fn put_text(
    clipboard: &mut impl Clipboard,
    text: &str,
    pending: Option<Saved>,
) -> Result<Saved> {
    clipboard.open()?;
    let previous = match pending {
        Some(pending) if unchanged(clipboard, &pending) => pending.previous,
        _ => clipboard.text(),
    };
    if let Err(error) = clipboard.replace(text) {
        if let Some(previous) = &previous {
            let _ = clipboard.replace(previous);
        }
        clipboard.close();
        return Err(error);
    }
    clipboard.close();
    Ok(Saved {
        previous,
        pasted: text.to_string(),
        sequence: clipboard.sequence(),
    })
}

/// Put the previous text back if the clipboard still holds the paste's text (see the module
/// comment for the rule). Returns whether it did: false when there was no text to restore or
/// newer content arrived meanwhile.
pub fn restore(clipboard: &mut impl Clipboard, saved: &Saved) -> Result<bool> {
    let Some(previous) = &saved.previous else {
        return Ok(false);
    };
    clipboard.open()?;
    // Checked under the clipboard lock, so nothing can change it between check and write.
    let ours = unchanged(clipboard, saved);
    let result = if ours {
        clipboard.replace(previous)
    } else {
        Ok(())
    };
    clipboard.close();
    result.map(|()| ours)
}

/// Whether the open clipboard still holds the paste's own write (see the module comment).
fn unchanged(clipboard: &mut impl Clipboard, saved: &Saved) -> bool {
    clipboard.sequence() == saved.sequence
        && clipboard.owned()
        && clipboard.text().as_deref() == Some(saved.pasted.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    /// A clipboard in memory that logs each step.
    #[derive(Default)]
    struct Fake {
        content: Option<String>,
        sequence: u32,
        open: bool,
        /// Whether the paste's window is the owner (it emptied the clipboard last).
        ours: bool,
        /// Content changed during this open, not counted yet (with `bump_on_close`).
        dirty: bool,
        /// Count a write when the clipboard closes instead of when it is written, this many
        /// times (Windows adds formats synthesized from the text then).
        bump_on_close: u32,
        /// Writing this text fails after emptying, as Windows can.
        fail_writes_of: Option<String>,
        /// Another app writes this as soon as the clipboard is first closed.
        written_after_first_close: Option<String>,
        /// Opening fails this many times (another app holds the clipboard).
        busy_opens: u32,
        log: Vec<String>,
    }

    impl Fake {
        fn holding(text: &str) -> Self {
            Self {
                content: Some(text.into()),
                sequence: 1,
                ..Self::default()
            }
        }

        /// Another app writing: it empties the clipboard (taking ownership) and writes.
        fn written_by_someone_else(&mut self, text: &str) {
            self.content = Some(text.into());
            self.sequence += 1;
            self.ours = false;
        }

        /// Another app setting data without emptying first: the owner stays the same.
        fn written_without_emptying(&mut self, text: &str) {
            self.content = Some(text.into());
            self.sequence += 1;
        }
    }

    impl Clipboard for Fake {
        fn open(&mut self) -> Result<()> {
            assert!(!self.open, "opened twice");
            if self.busy_opens > 0 {
                self.busy_opens -= 1;
                self.log.push("busy".into());
                bail!("could not open the clipboard");
            }
            self.open = true;
            self.log.push("open".into());
            Ok(())
        }
        fn close(&mut self) {
            assert!(self.open, "closed while not open");
            self.open = false;
            self.log.push("close".into());
            if std::mem::take(&mut self.dirty) {
                self.sequence += self.bump_on_close;
            }
            if let Some(newer) = self.written_after_first_close.take() {
                self.written_by_someone_else(&newer);
            }
        }
        fn text(&mut self) -> Option<String> {
            assert!(self.open);
            self.log.push("text".into());
            self.content.clone()
        }
        fn replace(&mut self, text: &str) -> Result<()> {
            assert!(self.open);
            self.log.push(format!("replace {text}"));
            self.content = None;
            self.ours = true;
            self.sequence += 1;
            if self.fail_writes_of.as_deref() == Some(text) {
                bail!("SetClipboardData failed");
            }
            self.content = Some(text.into());
            if self.bump_on_close > 0 {
                self.dirty = true;
            } else {
                self.sequence += 1;
            }
            Ok(())
        }
        fn owned(&mut self) -> bool {
            assert!(self.open);
            self.log.push("owned".into());
            self.ours
        }
        fn sequence(&mut self) -> u32 {
            self.log.push("sequence".into());
            self.sequence
        }
    }

    #[test]
    fn puts_the_text_on_then_restores_the_previous_text() {
        let mut clipboard = Fake::holding("before");
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        assert_eq!(clipboard.content.as_deref(), Some("pasted"));
        assert_eq!(saved.previous.as_deref(), Some("before"));
        // The number is read once the clipboard is closed, with every count of the write in.
        assert_eq!(
            clipboard.log,
            ["open", "text", "replace pasted", "close", "sequence"]
        );

        clipboard.log.clear();
        assert!(restore(&mut clipboard, &saved).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("before"));
        // Checked under the lock: the same number, still the owner, still the pasted text.
        assert_eq!(
            clipboard.log,
            [
                "open",
                "sequence",
                "owned",
                "text",
                "replace before",
                "close"
            ]
        );
    }

    /// Link 0.2.4 QA (Windows 11): the paste's own write moved the sequence number by 5, and
    /// 0.2.3's check (the saved number or one more) left the pasted text on for good.
    #[test]
    fn restores_however_much_windows_counts_the_write() {
        for bump_on_close in [1, 2, 5] {
            let mut clipboard = Fake {
                bump_on_close,
                ..Fake::holding("before")
            };
            let start = clipboard.sequence;
            let saved = put_text(&mut clipboard, "pasted", None).unwrap();
            assert_eq!(saved.sequence, start + 1 + bump_on_close);
            assert!(restore(&mut clipboard, &saved).unwrap(), "{bump_on_close}");
            assert_eq!(clipboard.content.as_deref(), Some("before"));
        }
    }

    /// Another app copying the same text is a newer copy: it is kept.
    #[test]
    fn keeps_the_same_text_copied_by_another_app() {
        let mut clipboard = Fake::holding("before");
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        clipboard.written_by_someone_else("pasted");
        assert!(!restore(&mut clipboard, &saved).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("pasted"));
    }

    /// Review of !6: a write without EmptyClipboard keeps the paste's window the owner and may
    /// keep its text; the sequence number still tells it apart, and it is kept.
    #[test]
    fn keeps_a_write_that_kept_the_owner_and_the_text() {
        for bump_on_close in [0, 5] {
            let mut clipboard = Fake {
                bump_on_close,
                ..Fake::holding("before")
            };
            let saved = put_text(&mut clipboard, "pasted", None).unwrap();
            clipboard.written_without_emptying("pasted");
            assert!(clipboard.ours);
            assert!(!restore(&mut clipboard, &saved).unwrap());
            assert_eq!(clipboard.content.as_deref(), Some("pasted"));
            assert!(!clipboard.open);
        }
    }

    /// Another app holding the clipboard: restoring fails without changing anything, and a
    /// later try (the control loop's retry) puts the text back.
    #[test]
    fn a_busy_clipboard_is_restored_on_a_later_try() {
        let mut clipboard = Fake::holding("before");
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        clipboard.busy_opens = 2;
        assert!(restore(&mut clipboard, &saved).is_err());
        assert!(restore(&mut clipboard, &saved).is_err());
        assert_eq!(clipboard.content.as_deref(), Some("pasted"));
        assert!(!clipboard.open);
        assert!(restore(&mut clipboard, &saved).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("before"));
    }

    #[test]
    fn keeps_what_another_app_wrote_the_moment_the_paste_closed_the_clipboard() {
        for bump_on_close in [0, 5] {
            let mut clipboard = Fake {
                bump_on_close,
                written_after_first_close: Some("newer".into()),
                ..Fake::holding("before")
            };
            let saved = put_text(&mut clipboard, "pasted", None).unwrap();
            assert_eq!(clipboard.content.as_deref(), Some("newer"));
            assert!(!restore(&mut clipboard, &saved).unwrap());
            assert_eq!(clipboard.content.as_deref(), Some("newer"));
            assert!(!clipboard.open);
        }
    }

    #[test]
    fn a_failed_write_puts_the_previous_text_back_and_closes() {
        let mut clipboard = Fake::holding("before");
        clipboard.fail_writes_of = Some("pasted".into());
        assert!(put_text(&mut clipboard, "pasted", None).is_err());
        assert_eq!(clipboard.content.as_deref(), Some("before"));
        assert!(!clipboard.open);
        assert_eq!(
            clipboard.log,
            ["open", "text", "replace pasted", "replace before", "close"]
        );
    }

    #[test]
    fn keeps_newer_content_written_while_the_paste_waited() {
        let mut clipboard = Fake::holding("before");
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        clipboard.written_by_someone_else("newer");
        assert!(!restore(&mut clipboard, &saved).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("newer"));
        assert!(!clipboard.open);
    }

    #[test]
    fn pastes_in_a_row_bring_back_what_was_there_before_the_first() {
        for bump_on_close in [0, 5] {
            let mut clipboard = Fake {
                bump_on_close,
                ..Fake::holding("before")
            };
            let first = put_text(&mut clipboard, "one", None).unwrap();
            let second = put_text(&mut clipboard, "two", Some(first)).unwrap();
            assert_eq!(clipboard.content.as_deref(), Some("two"));
            assert_eq!(second.previous.as_deref(), Some("before"));
            assert!(restore(&mut clipboard, &second).unwrap());
            assert_eq!(clipboard.content.as_deref(), Some("before"));
        }
    }

    #[test]
    fn a_paste_after_someone_elses_copy_brings_that_copy_back() {
        let mut clipboard = Fake::holding("before");
        let first = put_text(&mut clipboard, "one", None).unwrap();
        clipboard.written_by_someone_else("newer");
        let second = put_text(&mut clipboard, "two", Some(first)).unwrap();
        assert_eq!(second.previous.as_deref(), Some("newer"));
        assert!(restore(&mut clipboard, &second).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("newer"));
    }

    #[test]
    fn leaves_the_paste_text_when_the_clipboard_held_no_text() {
        let mut clipboard = Fake::default();
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        assert_eq!(saved.previous, None);
        clipboard.log.clear();
        assert!(!restore(&mut clipboard, &saved).unwrap());
        assert!(clipboard.log.is_empty());
        assert_eq!(clipboard.content.as_deref(), Some("pasted"));
    }
}
