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
//!
//! **A word here is a part of a name.** `holiday-2024.jpeg` is three of them,
//! because the parts are what somebody editing a filename edits — a word rule that
//! breaks only on spaces makes `Ctrl ←` jump the whole name, which is what `Home`
//! already does.
//!
//! And the history is the field's own. `Ctrl+Z` while typing a name means "undo
//! what I just typed"; the same chord outside a field walks back the last operation
//! on disk, and letting it through from inside one is how a rename undid somebody's
//! Move to Trash.

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
    /// States to go back to, oldest first.
    history: Vec<Snapshot>,
    /// States to go forward to, most recently undone last.
    redo: Vec<Snapshot>,
    /// What the last edit was, so a run of typing groups into one undo entry.
    last: Edit,
    /// Text an input method is still composing, and where its caret is inside
    /// it.
    ///
    /// **Not part of [`Field::text`].** It is not typed yet: the method may
    /// replace all of it with something else, and a rename that committed it
    /// would name a file after a half-finished sequence. It is drawn at the
    /// caret, underlined, and every edit that is not the method's own clears
    /// it.
    pub preedit: Option<(String, Option<(usize, usize)>)>,
}

/// One state of a field, for its own undo.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Snapshot {
    text: String,
    caret: usize,
}

/// What kind of edit an operation is, for grouping the undo history.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Edit {
    /// Characters arriving one at a time. A run of them is one entry.
    Typing,
    /// Anything else: a deletion, a paste, a replacement.
    #[default]
    Structural,
}

/// How many states a field remembers.
///
/// A field holds one line and lives for a few seconds. Sixty-four is more edits
/// than a filename receives and a bounded amount of memory either way.
const HISTORY: usize = 64;

/// Whether a character separates two words of a name or a path.
///
/// **A filename's own punctuation, not prose's.** `holiday-2024.jpeg` is three
/// words to somebody editing it, and a word rule that only breaks on spaces makes
/// `Ctrl ←` jump the whole name — which is what `Home` already does.
///
/// **`/` among them**, because the same one line of text is the path bar: a word
/// there is a path component, so `Ctrl+Backspace` walks back up a path one
/// directory at a time. A component with a hyphen in it is still two words, which
/// is the same rule rather than an exception to it.
fn is_separator(character: char) -> bool {
    character.is_whitespace()
        || matches!(character, '/' | '-' | '_' | '.' | ',' | '(' | ')' | '[' | ']')
}

impl Field {
    /// The parts every constructor shares.
    fn empty() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: None,
            history: Vec::new(),
            redo: Vec::new(),
            last: Edit::Structural,
            preedit: None,
        }
    }

    /// A field holding `text`, with the caret at the end and nothing selected.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { caret: text.len(), text, anchor: None, ..Self::empty() }
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
        Self { caret: end, anchor: Some(0), text, ..Self::empty() }
    }

    /// A field holding a name with all of it selected.
    ///
    /// For a directory: there is no extension to keep, so the whole name is what a
    /// rename offers — the same gesture as a file's stem, over a name that has no
    /// parts to it.
    #[must_use]
    pub fn naming_all(text: String) -> Self {
        Self { caret: text.len(), anchor: Some(0), text, ..Self::empty() }
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.text.len();
    }

    /// Selects the run at `offset` — a double click — or all of it, for a triple.
    ///
    /// **A triple click takes the whole field**, because one line of text is one
    /// paragraph: there is nothing else for the third click to mean, and taking
    /// the line is what it takes everywhere text is selected.
    ///
    /// The run is [`crate::word::around`]'s and not [`is_separator`]'s: a double
    /// click on `holiday_2024.jpeg` takes `holiday_2024`, where `Ctrl ←` stops at
    /// the `_`. The two answer different questions, and the module says which.
    pub fn select_run(&mut self, offset: usize, whole: bool) {
        if whole {
            self.select_all();
            return;
        }
        let (from, to) = crate::word::around(&self.text, offset);
        self.anchor = Some(from);
        self.caret = to;
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

    /// Applies an input method's change: delete, then commit, then preedit.
    ///
    /// The order is the protocol's and the type carries it, so this cannot get
    /// it wrong — see [`sk_plat::TextChange`].
    pub fn compose(&mut self, change: &sk_plat::TextChange) {
        let (before, after) = change.delete;
        for _ in 0..before {
            self.backspace();
        }
        for _ in 0..after {
            self.delete();
        }
        if !change.commit.is_empty() {
            self.insert(&change.commit);
        }
        self.preedit = (!change.preedit.is_empty()).then(|| {
            let cursor =
                change.preedit_cursor.map(|(begin, end)| (begin as usize, end as usize)).filter(
                    |(begin, end)| *begin <= change.preedit.len() && *end <= change.preedit.len(),
                );
            (change.preedit.clone(), cursor)
        });
    }

    /// Inserts text at the caret, replacing the selection if there is one.
    pub fn insert(&mut self, text: &str) {
        // One character at a time is typing and groups; a paste is not.
        self.remember(if text.chars().nth(1).is_none() { Edit::Typing } else { Edit::Structural });
        self.take_selection();
        self.text.insert_str(self.caret, text);
        self.caret += text.len();
    }

    /// Removes the selection, or the character before the caret.
    pub fn backspace(&mut self) {
        self.remember(Edit::Structural);
        if self.take_selection() {
            return;
        }
        let Some(before) = self.text[..self.caret].chars().next_back() else { return };
        self.caret -= before.len_utf8();
        self.text.remove(self.caret);
    }

    /// Removes the selection, or the character after the caret.
    pub fn delete(&mut self) {
        self.remember(Edit::Structural);
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
        self.remember(Edit::Structural);
        self.take_selection();
        self.text.drain(..self.caret);
        self.caret = 0;
    }

    /// Moves the caret one word, or collapses a selection and then moves.
    pub fn step_word(&mut self, forward: bool) {
        self.anchor = None;
        self.caret = self.word_boundary(forward);
    }

    /// Moves the caret one word, keeping or starting a selection.
    pub fn extend_word(&mut self, forward: bool) {
        let anchor = self.anchor.unwrap_or(self.caret);
        self.caret = self.word_boundary(forward);
        self.anchor = Some(anchor);
    }

    /// Removes the selection, or the word on one side of the caret.
    pub fn delete_word(&mut self, forward: bool) {
        self.remember(Edit::Structural);
        if self.take_selection() {
            return;
        }
        let to = self.word_boundary(forward);
        let (start, end) = (to.min(self.caret), to.max(self.caret));
        self.text.drain(start..end);
        self.caret = start;
    }

    /// Where the caret lands after one word in `forward`'s direction.
    ///
    /// **Skip the separators, then the word.** Going right from inside
    /// `holiday-2024.jpeg` stops at each `-` and `.` boundary rather than at the
    /// end, which is what makes a word operation useful on a filename — the parts
    /// of a name are what a person edits.
    fn word_boundary(&self, forward: bool) -> usize {
        let bytes: Vec<(usize, char)> = self.text.char_indices().collect();
        if forward {
            let mut at =
                bytes.iter().position(|(index, _)| *index >= self.caret).unwrap_or(bytes.len());
            while at < bytes.len() && is_separator(bytes[at].1) {
                at += 1;
            }
            while at < bytes.len() && !is_separator(bytes[at].1) {
                at += 1;
            }
            bytes.get(at).map_or(self.text.len(), |(index, _)| *index)
        } else {
            let mut at =
                bytes.iter().position(|(index, _)| *index >= self.caret).unwrap_or(bytes.len());
            while at > 0 && is_separator(bytes[at - 1].1) {
                at -= 1;
            }
            while at > 0 && !is_separator(bytes[at - 1].1) {
                at -= 1;
            }
            bytes.get(at).map_or(0, |(index, _)| *index)
        }
    }

    /// Takes back the last edit, if there is one to take back.
    ///
    /// A field's own history, unrelated to the one over files: `Ctrl+Z` while
    /// typing a name means "undo what I just typed", and the alternative — the
    /// chord reaching the file undo stack — is how a rename walked back somebody's
    /// last Move to Trash.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.history.pop() else { return false };
        self.redo.push(Snapshot { text: self.text.clone(), caret: self.caret });
        self.text = previous.text;
        self.caret = previous.caret.min(self.text.len());
        self.anchor = None;
        self.last = Edit::Structural;
        true
    }

    /// Does the last undone edit again.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.history.push(Snapshot { text: self.text.clone(), caret: self.caret });
        self.text = next.text;
        self.caret = next.caret.min(self.text.len());
        self.anchor = None;
        self.last = Edit::Structural;
        true
    }

    /// Records the state before an edit, grouping a run of typing into one entry.
    ///
    /// A person who typed ten characters and pressed `Ctrl+Z` expects the ten to
    /// go, not the last one — so consecutive inserts share a snapshot and anything
    /// else starts a new one.
    fn remember(&mut self, kind: Edit) {
        let grouped = kind == Edit::Typing && self.last == Edit::Typing;
        self.last = kind;
        if grouped {
            return;
        }
        self.redo.clear();
        if self.history.len() == HISTORY {
            self.history.remove(0);
        }
        self.history.push(Snapshot { text: self.text.clone(), caret: self.caret });
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
            // A word at a time, with and without Shift, and a word deleted from
            // either side. `Ctrl+Backspace` is the one a person reaches for most
            // and the one whose absence sent the keystroke to the listing.
            Key::Named(Named::Left) if event.mods.only_ctrl() => self.step_word(false),
            Key::Named(Named::Right) if event.mods.only_ctrl() => self.step_word(true),
            Key::Named(Named::Left) if event.mods.ctrl_shift() => self.extend_word(false),
            Key::Named(Named::Right) if event.mods.ctrl_shift() => self.extend_word(true),
            Key::Named(Named::Backspace) if event.mods.only_ctrl() => self.delete_word(false),
            Key::Named(Named::Delete) if event.mods.only_ctrl() => self.delete_word(true),
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

    /// A double click takes the run under it; a triple takes the whole line,
    /// because one line of text is one paragraph.
    #[test]
    fn a_double_click_takes_a_word_and_a_triple_takes_the_field() {
        let mut field = Field::new("holiday-2024.jpeg".to_owned());
        field.select_run(3, false);
        assert_eq!(field.selected(), "holiday");
        field.select_run(9, false);
        assert_eq!(field.selected(), "2024");
        field.select_run(9, true);
        assert_eq!(field.selected(), "holiday-2024.jpeg");
    }

    /// **`_` is part of a word to a double click, and a separator to `Ctrl ←`.**
    /// The two are different questions and the difference is deliberate: one is
    /// what the pointer is on, the other is how many presses cross a name.
    #[test]
    fn a_double_click_takes_a_whole_underscored_name() {
        let mut field = Field::new("my_file_name.txt".to_owned());
        field.select_run(4, false);
        assert_eq!(field.selected(), "my_file_name");
        field.caret = 12;
        field.step_word(false);
        assert_eq!(field.caret, 8, "Ctrl+Left still stops at the underscore");
    }

    /// Typing replaces what a double click took, which is the point of taking it.
    #[test]
    fn typing_replaces_what_a_double_click_took() {
        let mut field = Field::new("one two three".to_owned());
        field.select_run(5, false);
        field.insert("2");
        assert_eq!(field.text, "one 2 three");
    }

    /// A filename's parts are its words, so a word operation stops at each of
    /// them rather than jumping the whole name — which is what `Home` does.
    #[test]
    fn a_word_is_a_part_of_a_name() {
        let mut field = Field::new("holiday-2024.jpeg".to_owned());
        field.jump(false);
        let mut stops = vec![field.caret];
        for _ in 0..5 {
            field.step_word(true);
            stops.push(field.caret);
        }
        // `holiday` `-2024` `.jpeg`, and then the end, twice over.
        assert_eq!(stops, vec![0, 7, 12, 17, 17, 17]);

        let mut back = vec![field.caret];
        for _ in 0..4 {
            field.step_word(false);
            back.push(field.caret);
        }
        assert_eq!(back, vec![17, 13, 8, 0, 0]);
    }

    /// The same one line of text is the path bar, so a word there is a path
    /// component and `Ctrl+Backspace` walks back up a directory at a time.
    #[test]
    fn a_word_in_a_path_is_a_component() {
        let mut field = Field::new("/home/emil/repos/skerry".to_owned());
        field.delete_word(false);
        assert_eq!(field.text, "/home/emil/repos/");
        field.delete_word(false);
        assert_eq!(field.text, "/home/emil/");
        field.delete_word(false);
        assert_eq!(field.text, "/home/");
        // The leading `/` is a component boundary of its own, so the last press
        // leaves the root rather than an empty field.
        field.delete_word(false);
        assert_eq!(field.text, "/");
        field.delete_word(false);
        assert_eq!(field.text, "");
    }

    /// A component with punctuation in it is more than one word, which is the
    /// same rule and not an exception to it.
    #[test]
    fn a_hyphenated_component_is_still_two_words() {
        let mut field = Field::new("/tmp/skerry-golden/fixture".to_owned());
        field.step_word(false);
        assert_eq!(field.caret, 19, "the caret is before `fixture`");
        field.step_word(false);
        assert_eq!(&field.text[field.caret..], "golden/fixture");
    }

    #[test]
    fn ctrl_backspace_takes_the_word_before_the_caret() {
        let mut field = Field::new("holiday-2024.jpeg".to_owned());
        field.delete_word(false);
        assert_eq!(field.text, "holiday-2024.");
        field.delete_word(false);
        assert_eq!(field.text, "holiday-");
        field.delete_word(false);
        assert_eq!(field.text, "");
    }

    #[test]
    fn ctrl_delete_takes_the_word_after_the_caret() {
        let mut field = Field::new("holiday-2024.jpeg".to_owned());
        field.jump(false);
        field.delete_word(true);
        assert_eq!(field.text, "-2024.jpeg");
        assert_eq!(field.caret, 0);
    }

    #[test]
    fn ctrl_shift_and_an_arrow_selects_a_word() {
        let mut field = Field::new("holiday-2024.jpeg".to_owned());
        field.jump(false);
        field.extend_word(true);
        assert_eq!(field.selected(), "holiday");
        field.extend_word(true);
        assert_eq!(field.selected(), "holiday-2024");
    }

    /// A run of typing is one entry, because somebody who typed ten characters
    /// and pressed `Ctrl+Z` means the ten.
    #[test]
    fn a_run_of_typing_undoes_as_one_edit() {
        let mut field = Field::new(String::new());
        for character in ["n", "o", "t", "e", "s"] {
            field.insert(character);
        }
        assert_eq!(field.text, "notes");
        assert!(field.undo());
        assert_eq!(field.text, "");
        assert!(!field.undo(), "there was nothing else to take back");
    }

    #[test]
    fn a_deletion_is_its_own_entry_and_redo_puts_it_back() {
        let mut field = Field::naming("notes.txt".to_owned());
        field.insert("plan");
        assert_eq!(field.text, "plan.txt");
        field.delete_word(false);
        assert_eq!(field.text, ".txt");

        assert!(field.undo());
        assert_eq!(field.text, "plan.txt");
        assert!(field.undo());
        assert_eq!(field.text, "notes.txt", "the selected stem came back");

        assert!(field.redo());
        assert_eq!(field.text, "plan.txt");
        assert!(field.redo());
        assert_eq!(field.text, ".txt");
        assert!(!field.redo());
    }

    /// Typing after an undo drops the redo branch, which is what every editor
    /// does — a redo onto text that has moved on is a redo onto nothing.
    #[test]
    fn typing_after_an_undo_drops_the_redo_branch() {
        let mut field = Field::new("abc".to_owned());
        field.insert("d");
        assert!(field.undo());
        assert_eq!(field.text, "abc");
        field.insert("z");
        assert!(!field.redo());
        assert_eq!(field.text, "abcz");
    }

    /// The history is bounded, because a field that remembers everything is a
    /// field that grows without one.
    #[test]
    fn the_history_is_bounded() {
        let mut field = Field::new(String::new());
        for index in 0..(super::HISTORY * 3) {
            // Structural each time, so every one is its own entry.
            field.insert(&format!("{index} "));
        }
        let mut taken = 0;
        while field.undo() {
            taken += 1;
            assert!(taken <= super::HISTORY, "the history is not bounded");
        }
        assert_eq!(taken, super::HISTORY);
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
