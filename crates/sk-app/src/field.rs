//! One line of editable text, and a caret in it.
//!
//! Two things in the window need this — the name being typed over a row, and the
//! path being typed over the breadcrumbs — and they need exactly the same keys
//! for everything except Enter and Escape, which mean different things in each.
//! So the keys are here and the two verbs are not.
//!
//! **Characters, never bytes.** `caret` is a byte offset because that is what
//! slicing a `String` takes, and every move steps by a whole character: a caret
//! that lands inside `é` panics on the next slice, and half the filenames on a
//! European desktop have one in them.

use sk_plat::{Key, KeyEvent, Named};

/// Editable text with a caret.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Field {
    /// What has been typed.
    pub text: String,
    /// Where the caret is, as a byte offset into [`Field::text`].
    pub caret: usize,
}

impl Field {
    /// A field holding `text`, with the caret at the end.
    ///
    /// The end rather than the whole of it selected: there is no selection in
    /// this field, and the end is the one position from which every edit is
    /// reachable with the keys that exist.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { caret: text.len(), text }
    }

    /// Inserts text at the caret.
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.caret, text);
        self.caret += text.len();
    }

    /// Removes the character before the caret.
    pub fn backspace(&mut self) {
        let Some(before) = self.text[..self.caret].chars().next_back() else { return };
        self.caret -= before.len_utf8();
        self.text.remove(self.caret);
    }

    /// Removes the character after the caret.
    pub fn delete(&mut self) {
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }

    /// Moves the caret one character.
    pub fn step(&mut self, forward: bool) {
        if forward {
            if let Some(next) = self.text[self.caret..].chars().next() {
                self.caret += next.len_utf8();
            }
        } else if let Some(before) = self.text[..self.caret].chars().next_back() {
            self.caret -= before.len_utf8();
        }
    }

    /// Puts the caret at one end.
    pub fn jump(&mut self, to_end: bool) {
        self.caret = if to_end { self.text.len() } else { 0 };
    }

    /// Removes everything before the caret.
    pub fn clear_before_caret(&mut self) {
        self.text.drain(..self.caret);
        self.caret = 0;
    }

    /// The text before the caret, for measuring where to draw it.
    #[must_use]
    pub fn before_caret(&self) -> &str {
        &self.text[..self.caret.min(self.text.len())]
    }

    /// Feeds one key to the field. Returns whether it was consumed.
    ///
    /// **Enter and Escape are not here.** Committing and abandoning are the
    /// caller's, and they are the only two keys whose meaning depends on what
    /// the field is for. Every other chord that is not handled falls through, so
    /// `Ctrl+Z` still undoes and `F1` still opens the keyboard list from inside
    /// a field.
    pub fn key(&mut self, event: &KeyEvent) -> bool {
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Backspace) if bare => self.backspace(),
            Key::Named(Named::Delete) if bare => self.delete(),
            Key::Named(Named::Left) if bare => self.step(false),
            Key::Named(Named::Right) if bare => self.step(true),
            Key::Named(Named::Home) if bare => self.jump(false),
            Key::Named(Named::End) if bare => self.jump(true),
            // Readline's, and every terminal's, and the only way to clear this
            // field in one key: there is no selection in it, so renaming
            // `holiday-2024-portugal.jpeg` was twenty-six presses of Backspace.
            Key::Character('u') if event.mods.ctrl => self.clear_before_caret(),
            _ => match crate::keys::typed_text(event) {
                Some(text) => self.insert(text),
                None => return false,
            },
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Field;

    /// The defect this shape exists to prevent: `café` is five bytes and four
    /// characters, and a caret that steps by one byte lands inside the last
    /// character and panics on the next slice.
    #[test]
    fn the_caret_moves_by_characters_and_not_by_bytes() {
        let mut field = Field::new("café".to_owned());
        assert_eq!(field.caret, 5, "é is two bytes");
        field.step(false);
        assert_eq!(field.caret, 3);
        field.step(false);
        assert_eq!(field.caret, 2);
        field.step(true);
        assert_eq!(field.caret, 3);

        field.jump(true);
        assert_eq!(field.caret, 5);
        field.jump(false);
        assert_eq!(field.caret, 0);
        // And neither end runs off.
        field.step(false);
        assert_eq!(field.caret, 0);
        field.jump(true);
        field.step(true);
        assert_eq!(field.caret, 5);
    }

    #[test]
    fn typing_and_deleting_work_at_the_caret() {
        let mut field = Field::new("report.pdf".to_owned());
        field.jump(false);
        field.insert("2024-");
        assert_eq!(field.text, "2024-report.pdf");
        assert_eq!(field.caret, 5);

        field.backspace();
        assert_eq!(field.text, "2024report.pdf");
        assert_eq!(field.caret, 4);

        field.delete();
        assert_eq!(field.text, "2024eport.pdf");
        assert_eq!(field.caret, 4);
    }

    #[test]
    fn deleting_at_either_end_does_nothing_rather_than_panicking() {
        let mut field = Field::new("a".to_owned());
        field.jump(false);
        field.backspace();
        assert_eq!(field.text, "a");
        field.jump(true);
        field.delete();
        assert_eq!(field.text, "a");
        field.backspace();
        field.backspace();
        assert_eq!(field.text, "");
        assert_eq!(field.caret, 0);
    }

    /// The field has no selection, so clearing it was one press of Backspace per
    /// character — twenty-six of them for a photograph's name.
    #[test]
    fn one_key_clears_the_field_and_leaves_what_is_after_the_caret() {
        let mut field = Field::new("holiday.jpeg".to_owned());
        field.clear_before_caret();
        assert_eq!(field.text, "");
        assert_eq!(field.caret, 0);

        let mut field = Field::new("holiday.jpeg".to_owned());
        field.jump(false);
        for _ in 0..7 {
            field.step(true);
        }
        field.clear_before_caret();
        assert_eq!(field.text, ".jpeg", "the extension went with it");
        assert_eq!(field.caret, 0);
    }

    #[test]
    fn a_multibyte_character_is_removed_whole() {
        let mut field = Field::new("aéb".to_owned());
        field.step(false);
        field.backspace();
        assert_eq!(field.text, "ab");

        let mut other = Field::new("aéb".to_owned());
        other.jump(false);
        other.step(true);
        other.delete();
        assert_eq!(other.text, "ab");
    }

    #[test]
    fn what_is_before_the_caret_is_always_a_character_boundary() {
        let mut field = Field::new("café".to_owned());
        assert_eq!(field.before_caret(), "café");
        field.step(false);
        assert_eq!(field.before_caret(), "caf");
        field.jump(false);
        assert_eq!(field.before_caret(), "");
    }
}
