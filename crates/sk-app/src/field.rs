//! One line of editable text, a caret in it, and a selection.
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
//!
//! The selection exists for one case and pays for itself there: a rename opens
//! with the name selected and the extension not, so typing replaces the part being
//! changed. Before it, renaming `holiday-2024-portugal.jpeg` was twenty-six
//! presses of Backspace, or a `Ctrl+U` that took the extension with it.

use sk_plat::{Key, KeyEvent, Named};

/// Editable text with a caret, and possibly a selection.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Field {
    /// What has been typed.
    pub text: String,
    /// Where the caret is, as a byte offset into [`Field::text`].
    pub caret: usize,
    /// The other end of the selection, when there is one.
    ///
    /// An anchor rather than a range, because the caret is one end of it by
    /// definition: every edit collapses to the caret, and the anchor is the thing
    /// that moved away from it.
    pub anchor: Option<usize>,
}

impl Field {
    /// A field holding `text`, with the caret at the end and nothing selected.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { caret: text.len(), text, anchor: None }
    }

    /// A field holding a filename, with everything before the extension selected.
    ///
    /// What every rename in an editor or a file manager does: `email2.txt` opens
    /// with `email2` selected, so typing replaces the name and keeps the
    /// extension — which is the edit a rename almost always is. The whole name for
    /// a directory, which has no extension to keep, and for a dotfile, whose
    /// leading dot is part of its name rather than a separator.
    ///
    /// The **last** dot, so `archive.tar.gz` keeps `.gz` and offers `archive.tar`.
    ///
    /// A name with no extension is all stem, so all of it is selected — `Makefile`
    /// and `.bashrc` behave like a directory does, which is the consistency the
    /// rule is for rather than an exception to it.
    #[must_use]
    pub fn naming(text: String) -> Self {
        let end = stem_end(&text);
        Self { caret: end, anchor: Some(0), text }
    }

    /// A field holding a name with all of it selected.
    ///
    /// For a directory: there is no extension to keep, so the whole name is what a
    /// rename offers — the same gesture as a file's stem, over a name that has no
    /// parts to it.
    #[must_use]
    pub fn naming_all(text: String) -> Self {
        Self { caret: text.len(), anchor: Some(0), text }
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.text.len();
    }

    /// Moves the caret one character, keeping or starting a selection.
    ///
    /// The anchor stays where the caret was when the first `Shift` arrow arrived,
    /// which is what makes a run of them grow one selection rather than a new one
    /// per press.
    pub fn extend(&mut self, forward: bool) {
        let anchor = self.anchor.unwrap_or(self.caret);
        if forward {
            if let Some(next) = self.text[self.caret..].chars().next() {
                self.caret += next.len_utf8();
            }
        } else if let Some(before) = self.text[..self.caret].chars().next_back() {
            self.caret -= before.len_utf8();
        }
        self.anchor = Some(anchor);
    }

    /// Moves the caret to one end, keeping or starting a selection.
    pub fn extend_to_end(&mut self, to_end: bool) {
        let anchor = self.anchor.unwrap_or(self.caret);
        self.caret = if to_end { self.text.len() } else { 0 };
        self.anchor = Some(anchor);
    }

    /// Moves the caret to `offset`, keeping or starting a selection.
    ///
    /// What a pointer drag does: the anchor is where the press landed and the
    /// caret follows the pointer.
    pub fn extend_to(&mut self, offset: usize) {
        let anchor = self.anchor.unwrap_or(self.caret);
        self.caret = offset.min(self.text.len());
        self.anchor = Some(anchor);
    }

    /// The selected text, or `""`.
    #[must_use]
    pub fn selected(&self) -> &str {
        match self.selection() {
            Some((start, end)) => self.text.get(start..end).unwrap_or(""),
            None => "",
        }
    }

    /// The selected range in byte offsets, if anything is selected.
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.caret).then(|| (anchor.min(self.caret), anchor.max(self.caret)))
    }

    /// Drops the selection without moving the caret.
    pub fn deselect(&mut self) {
        self.anchor = None;
    }

    /// Removes the selection if there is one, and says whether there was.
    fn take_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.text.drain(start..end);
        self.caret = start;
        self.anchor = None;
        true
    }

    /// Inserts text at the caret, replacing the selection if there is one.
    pub fn insert(&mut self, text: &str) {
        self.take_selection();
        self.text.insert_str(self.caret, text);
        self.caret += text.len();
    }

    /// Removes the selection, or the character before the caret.
    pub fn backspace(&mut self) {
        if self.take_selection() {
            return;
        }
        let Some(before) = self.text[..self.caret].chars().next_back() else { return };
        self.caret -= before.len_utf8();
        self.text.remove(self.caret);
    }

    /// Removes the selection, or the character after the caret.
    pub fn delete(&mut self) {
        if self.take_selection() {
            return;
        }
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }

    /// Moves the caret one character, or collapses a selection to one end.
    ///
    /// A selection collapses **to the end the arrow points at** and moves no
    /// further, which is what every text field does: `Right` out of a selection
    /// puts the caret after it rather than one character past it.
    pub fn step(&mut self, forward: bool) {
        if let Some((start, end)) = self.selection() {
            self.caret = if forward { end } else { start };
            self.anchor = None;
            return;
        }
        self.anchor = None;
        if forward {
            if let Some(next) = self.text[self.caret..].chars().next() {
                self.caret += next.len_utf8();
            }
        } else if let Some(before) = self.text[..self.caret].chars().next_back() {
            self.caret -= before.len_utf8();
        }
    }

    /// Puts the caret at one end, dropping any selection.
    pub fn jump(&mut self, to_end: bool) {
        self.anchor = None;
        self.caret = if to_end { self.text.len() } else { 0 };
    }

    /// Removes everything before the caret.
    pub fn clear_before_caret(&mut self) {
        self.take_selection();
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
            // Shift and an arrow. Not in the registry, unlike `Ctrl+A`: these
            // chords mean nothing over a listing, so there is nothing for a scope
            // to disambiguate and they are text editing rather than actions.
            Key::Named(Named::Left) if event.mods.only_shift() => self.extend(false),
            Key::Named(Named::Right) if event.mods.only_shift() => self.extend(true),
            Key::Named(Named::Home) if event.mods.only_shift() => self.extend_to_end(false),
            Key::Named(Named::End) if event.mods.only_shift() => self.extend_to_end(true),
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

/// Where a filename's stem ends: the last dot that is not the first byte.
///
/// Not the first dot, so `archive.tar.gz` offers `archive.tar` rather than
/// `archive`. Not a leading dot, because `.bashrc` is a name and not an extension
/// on an empty one.
fn stem_end(name: &str) -> usize {
    name.char_indices()
        .filter(|(at, character)| *at > 0 && *character == '.')
        .map(|(at, _)| at)
        .next_back()
        .unwrap_or(name.len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Field;

    /// The rule, over every shape a filename comes in.
    #[test]
    fn a_rename_offers_the_name_and_keeps_the_extension() {
        for (name, wanted) in [
            ("email2.txt", Some("email2")),
            // The last dot, not the first: `.gz` is the extension here.
            ("archive.tar.gz", Some("archive.tar")),
            // A leading dot is part of the name, not a separator on an empty one —
            // so there is no extension and the whole thing is the stem.
            (".bashrc", Some(".bashrc")),
            (".hidden.txt", Some(".hidden")),
            // No extension either, and all of it selected for the same reason a
            // directory's whole name is.
            ("Makefile", Some("Makefile")),
            ("", None),
            // A name that is only dots: the last one still separates, so `..` is
            // offered and the final `.` kept. Odd, and it is what the rule says.
            ("...", Some("..")),
            ("café.txt", Some("café")),
        ] {
            let field = Field::naming(name.to_owned());
            let selected = field.selection().map(|(start, end)| &field.text[start..end]);
            assert_eq!(selected, wanted, "{name:?}");
            // The caret sits at the end of what is offered, so typing replaces it
            // and `Right` steps into the extension.
            assert_eq!(field.caret, wanted.map_or(name.len(), str::len), "{name:?}");
        }
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut field = Field::naming("email2.txt".to_owned());
        field.insert("notes");
        assert_eq!(field.text, "notes.txt");
        assert_eq!(field.selection(), None);
        assert_eq!(field.caret, 5);
    }

    #[test]
    fn backspace_and_delete_take_the_whole_selection() {
        for take in [Field::backspace, Field::delete] {
            let mut field = Field::naming("email2.txt".to_owned());
            take(&mut field);
            assert_eq!(field.text, ".txt");
            assert_eq!(field.caret, 0);
            assert_eq!(field.selection(), None);
        }
    }

    /// An arrow collapses to the end it points at and goes no further, which is
    /// what every text field does.
    #[test]
    fn an_arrow_collapses_the_selection_to_the_end_it_points_at() {
        let mut field = Field::naming("email2.txt".to_owned());
        field.step(true);
        assert_eq!((field.caret, field.selection()), (6, None));

        let mut field = Field::naming("email2.txt".to_owned());
        field.step(false);
        assert_eq!((field.caret, field.selection()), (0, None));
    }

    #[test]
    fn home_and_end_drop_the_selection() {
        let mut field = Field::naming("email2.txt".to_owned());
        field.jump(true);
        assert_eq!((field.caret, field.selection()), (10, None));

        let mut field = Field::naming("email2.txt".to_owned());
        field.jump(false);
        assert_eq!((field.caret, field.selection()), (0, None));
    }

    /// A name with no extension is all stem, so all of it is offered — the same
    /// gesture as a file's stem, over a name with no parts to it.
    #[test]
    fn a_name_with_no_extension_is_all_stem() {
        let mut field = Field::naming("Makefile".to_owned());
        assert_eq!(field.selected(), "Makefile");
        field.insert("Justfile");
        assert_eq!(field.text, "Justfile");
    }

    /// A directory has no extension to keep, so a rename offers its whole name
    /// however many dots are in it.
    #[test]
    fn a_directory_offers_its_whole_name() {
        for name in ["photos", "my.folder", "site.v2.backup"] {
            let field = Field::naming_all(name.to_owned());
            assert_eq!(field.selected(), name, "{name:?}");
        }
    }

    #[test]
    fn shift_and_an_arrow_grow_one_selection() {
        let mut field = Field::new("abcdef".to_owned());
        field.jump(false);
        field.extend(true);
        field.extend(true);
        assert_eq!(field.selected(), "ab");
        // Back one: the anchor has not moved, so the selection shrinks rather than
        // starting again from here.
        field.extend(false);
        assert_eq!(field.selected(), "a");
    }

    #[test]
    fn shift_home_and_shift_end_reach_the_ends() {
        let mut field = Field::new("abcdef".to_owned());
        field.step(false);
        field.extend_to_end(false);
        assert_eq!(field.selected(), "abcde");

        let mut field = Field::new("abcdef".to_owned());
        field.jump(false);
        field.extend_to_end(true);
        assert_eq!(field.selected(), "abcdef");
    }

    /// What a pointer drag does: the anchor stays where the press landed.
    #[test]
    fn dragging_extends_from_where_it_started() {
        let mut field = Field::new("abcdef".to_owned());
        field.caret = 1;
        field.extend_to(4);
        assert_eq!(field.selected(), "bcd");
        field.extend_to(2);
        assert_eq!(field.selected(), "b");
        // Back past the anchor, and the selection is on the other side of it.
        field.extend_to(0);
        assert_eq!(field.selected(), "a");
    }

    #[test]
    fn select_all_takes_everything_and_typing_replaces_it() {
        let mut field = Field::new("abcdef".to_owned());
        field.caret = 3;
        field.select_all();
        assert_eq!(field.selected(), "abcdef");
        field.insert("x");
        assert_eq!(field.text, "x");
    }

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
