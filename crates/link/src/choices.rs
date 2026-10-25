//! The theme and the language as Settings shows them, and the changes still on their way to
//! rtx-linkd. Changes are sent one at a time in the order they were made, so the last one chosen
//! is the one kept; a change the daemon didn't take goes back only when nothing newer is waiting,
//! and a status read from before a change never replaces it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Theme,
    Language,
}

impl Field {
    const ALL: [Field; 2] = [Field::Theme, Field::Language];

    /// Its name in POST /v1/appearance.
    pub fn key(self) -> &'static str {
        match self {
            Field::Theme => "theme",
            Field::Language => "language",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Choice {
    /// What the select and the window show.
    shown: String,
    /// What the daemon has: from GET /v1/status, or a change it took.
    confirmed: String,
    /// The latest change, not sent yet.
    wanted: Option<String>,
    /// The change being sent.
    sending: Option<String>,
    /// A change the daemon didn't take: Try again sends it.
    failed: Option<String>,
    /// Counts changes and the daemon's answers to them: a status read started before the latest
    /// one is stale for this field.
    edits: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Choices {
    fields: [Choice; 2],
    /// A thread is sending changes.
    pub writing: bool,
}

/// Which changes had been made when a status read started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edits([u64; 2]);

impl Choices {
    fn field(&self, field: Field) -> &Choice {
        &self.fields[field as usize]
    }

    fn field_mut(&mut self, field: Field) -> &mut Choice {
        &mut self.fields[field as usize]
    }

    /// What the window shows ("" before the first status read: the system's).
    pub fn shown(&self, field: Field) -> &str {
        &self.field(field).shown
    }

    /// The changes the daemon didn't take, for Try again.
    pub fn failed(&self) -> Vec<(Field, String)> {
        Field::ALL
            .into_iter()
            .filter_map(|field| Some((field, self.field(field).failed.clone()?)))
            .collect()
    }

    /// Chosen in Settings: shown at once, sent after anything already on its way.
    pub fn choose(&mut self, field: Field, value: &str) {
        let choice = self.field_mut(field);
        choice.shown = value.to_string();
        choice.wanted = Some(value.to_string());
        choice.failed = None;
        choice.edits += 1;
    }

    /// The next change to send, one at a time.
    pub fn next_write(&mut self) -> Option<(Field, String)> {
        if self.fields.iter().any(|choice| choice.sending.is_some()) {
            return None;
        }
        Field::ALL.into_iter().find_map(|field| {
            let choice = self.field_mut(field);
            let value = choice.wanted.take()?;
            choice.sending = Some(value.clone());
            Some((field, value))
        })
    }

    /// The daemon's answer to the change being sent. A refusal puts back what the daemon has,
    /// unless a newer change is waiting (that one is shown, and sent next).
    pub fn finished(&mut self, field: Field, taken: bool) {
        let choice = self.field_mut(field);
        let Some(value) = choice.sending.take() else {
            return;
        };
        // A status read that started before this answer may predate the change.
        choice.edits += 1;
        if taken {
            choice.confirmed = value;
        } else if choice.wanted.is_none() {
            choice.shown = choice.confirmed.clone();
            choice.failed = Some(value);
        }
    }

    pub fn edits(&self) -> Edits {
        Edits(self.fields.clone().map(|choice| choice.edits))
    }

    /// A successful GET /v1/status, started when `edits` were the changes made. It sets a field
    /// only when nothing was chosen for it since and nothing is on its way.
    pub fn status(&mut self, edits: Edits, theme: &str, language: &str) {
        for (field, value) in [(Field::Theme, theme), (Field::Language, language)] {
            let choice = self.field_mut(field);
            if choice.edits == edits.0[field as usize]
                && choice.wanted.is_none()
                && choice.sending.is_none()
            {
                choice.confirmed = value.to_string();
                choice.shown = value.to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends every waiting change, answering each with `taken`.
    fn send_all(choices: &mut Choices, taken: bool) {
        while let Some((field, _)) = choices.next_write() {
            choices.finished(field, taken);
        }
    }

    #[test]
    fn quick_choices_reach_the_daemon_in_order_and_the_last_one_stays() {
        let mut choices = Choices::default();
        choices.status(choices.edits(), "system", "system");
        let before = choices.edits();
        choices.choose(Field::Theme, "light");
        let first = choices.next_write();
        assert_eq!(first, Some((Field::Theme, "light".into())));
        // Dark is chosen while Light is on its way: it waits for Light's answer.
        choices.choose(Field::Theme, "dark");
        assert_eq!(choices.next_write(), None);
        // A status read from before both changes (still "system") replaces neither.
        choices.status(before, "system", "system");
        assert_eq!(choices.shown(Field::Theme), "dark");
        choices.finished(Field::Theme, true);
        assert_eq!(choices.shown(Field::Theme), "dark");
        assert_eq!(choices.next_write(), Some((Field::Theme, "dark".into())));
        // Read while Dark is on its way: "light", now stale.
        choices.status(choices.edits(), "light", "system");
        assert_eq!(choices.shown(Field::Theme), "dark");
        choices.finished(Field::Theme, true);
        assert_eq!(choices.next_write(), None);
        choices.status(choices.edits(), "dark", "system");
        assert_eq!(choices.shown(Field::Theme), "dark");
        assert!(choices.failed().is_empty());
    }

    #[test]
    fn a_status_read_started_during_a_write_does_not_replace_it() {
        let mut choices = Choices::default();
        choices.status(choices.edits(), "light", "system");
        choices.choose(Field::Theme, "dark");
        let _ = choices.next_write();
        // GET /v1/status starts while Dark is on its way, and answers after it was taken.
        let read = choices.edits();
        choices.finished(Field::Theme, true);
        choices.status(read, "light", "system");
        assert_eq!(choices.shown(Field::Theme), "dark");
        // Dark is what the daemon has: a later refusal goes back to it.
        choices.choose(Field::Theme, "system");
        send_all(&mut choices, false);
        assert_eq!(choices.shown(Field::Theme), "dark");
        // The next read, started after the answer, is taken as it is.
        choices.status(choices.edits(), "dark", "system");
        assert_eq!(choices.shown(Field::Theme), "dark");
    }

    #[test]
    fn an_older_refusal_does_not_undo_a_newer_choice() {
        let mut choices = Choices::default();
        choices.status(choices.edits(), "system", "system");
        choices.choose(Field::Theme, "light");
        let _ = choices.next_write();
        choices.choose(Field::Theme, "dark");
        choices.finished(Field::Theme, false);
        assert_eq!(choices.shown(Field::Theme), "dark");
        assert!(choices.failed().is_empty());
        send_all(&mut choices, true);
        assert_eq!(choices.shown(Field::Theme), "dark");
    }

    #[test]
    fn a_refused_language_keeps_its_retry_when_a_theme_change_is_taken() {
        let mut choices = Choices::default();
        choices.status(choices.edits(), "light", "system");
        choices.choose(Field::Language, "vi");
        send_all(&mut choices, false);
        assert_eq!(choices.shown(Field::Language), "system");
        assert_eq!(choices.failed(), vec![(Field::Language, "vi".to_string())]);
        choices.choose(Field::Theme, "dark");
        send_all(&mut choices, true);
        assert_eq!(choices.shown(Field::Theme), "dark");
        assert_eq!(choices.failed(), vec![(Field::Language, "vi".to_string())]);
        // Try again: chosen again, and taken this time.
        choices.choose(Field::Language, "vi");
        send_all(&mut choices, true);
        assert_eq!(choices.shown(Field::Language), "vi");
        assert!(choices.failed().is_empty());
    }

    #[test]
    fn realtimex_changes_show_when_nothing_is_pending() {
        let mut choices = Choices::default();
        choices.status(choices.edits(), "system", "system");
        choices.status(choices.edits(), "dark", "de");
        assert_eq!(choices.shown(Field::Theme), "dark");
        assert_eq!(choices.shown(Field::Language), "de");
        // A refusal goes back to what RealTimeX set.
        choices.choose(Field::Theme, "light");
        send_all(&mut choices, false);
        assert_eq!(choices.shown(Field::Theme), "dark");
    }
}
