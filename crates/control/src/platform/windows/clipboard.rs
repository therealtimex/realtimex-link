//! Pasting through the clipboard without losing what was on it: the steps, over a small
//! [`Clipboard`] trait, so they are tested without Windows.
//!
//! The text goes on the clipboard, the paste keys are pressed, and the previous text comes
//! back afterwards, unless something else (the person, another app) put newer content there in
//! between. Pastes in a row (typing) bring it back once, after the last one: each one passes the
//! text the first one took off to the next.
//!
//! How "unchanged" is decided: Windows' clipboard sequence number changes with every new
//! content. The paste reads it while it still holds the clipboard lock, right after its own
//! write, so the saved number can only be its own. Windows may count that write once more when
//! the clipboard closes, so `restore` (again under the lock) accepts exactly the saved number,
//! or the saved number + 1 while the paste's window still owns the clipboard. Any other writer
//! empties the clipboard first, which makes it the owner; so a newer content always fails both
//! checks and is kept.

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
    /// Windows' clipboard sequence number, which changes with every new content.
    fn sequence(&mut self) -> u32;
    /// Whether the paste's own window owns the clipboard (the last one to empty it).
    fn owned(&mut self) -> bool;
}

/// What a paste took off the clipboard, to put back with [`restore`].
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub previous: Option<String>,
    /// The sequence number of the paste's own write, read under the same lock.
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
    // Before closing: once the lock is gone another writer could get in first.
    let sequence = clipboard.sequence();
    clipboard.close();
    Ok(Saved { previous, sequence })
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
    let current = clipboard.sequence();
    current == saved.sequence || (current == saved.sequence.wrapping_add(1) && clipboard.owned())
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
        /// Count a write when the clipboard closes instead of when it is written.
        bump_on_close: bool,
        /// Writing this text fails after emptying, as Windows can.
        fail_writes_of: Option<String>,
        /// Another app writes this as soon as the clipboard is first closed.
        written_after_first_close: Option<String>,
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
    }

    impl Clipboard for Fake {
        fn open(&mut self) -> Result<()> {
            assert!(!self.open, "opened twice");
            self.open = true;
            self.log.push("open".into());
            Ok(())
        }
        fn close(&mut self) {
            assert!(self.open, "closed while not open");
            self.open = false;
            self.log.push("close".into());
            if std::mem::take(&mut self.dirty) {
                self.sequence += 1;
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
            if self.bump_on_close {
                self.dirty = true;
            } else {
                self.sequence += 1;
            }
            Ok(())
        }
        fn sequence(&mut self) -> u32 {
            assert!(self.open, "sequence read without the lock");
            self.log.push("sequence".into());
            self.sequence
        }
        fn owned(&mut self) -> bool {
            assert!(self.open);
            self.log.push("owned".into());
            self.ours
        }
    }

    #[test]
    fn puts_the_text_on_then_restores_the_previous_text() {
        let mut clipboard = Fake::holding("before");
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        assert_eq!(clipboard.content.as_deref(), Some("pasted"));
        assert_eq!(saved.previous.as_deref(), Some("before"));
        // The number is read while the lock is still held.
        assert_eq!(
            clipboard.log,
            ["open", "text", "replace pasted", "sequence", "close"]
        );

        clipboard.log.clear();
        assert!(restore(&mut clipboard, &saved).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("before"));
        assert_eq!(
            clipboard.log,
            ["open", "sequence", "replace before", "close"]
        );
    }

    #[test]
    fn restores_when_windows_counts_the_write_at_close() {
        let mut clipboard = Fake {
            bump_on_close: true,
            ..Fake::holding("before")
        };
        let saved = put_text(&mut clipboard, "pasted", None).unwrap();
        assert_eq!(clipboard.sequence, saved.sequence + 1);
        assert!(restore(&mut clipboard, &saved).unwrap());
        assert_eq!(clipboard.content.as_deref(), Some("before"));
    }

    #[test]
    fn keeps_what_another_app_wrote_the_moment_the_paste_closed_the_clipboard() {
        for bump_on_close in [false, true] {
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
        for bump_on_close in [false, true] {
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
