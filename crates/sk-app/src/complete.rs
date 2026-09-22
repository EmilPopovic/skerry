// SPDX-License-Identifier: Apache-2.0
//! What the path field offers to finish the directory being typed.
//!
//! **One flag per question**, which is [rule
//! 2](../../../docs/03-principles.md#2-everything-is-a-command)'s own wording.
//! There are two questions here and they have two answers: whether a list exists
//! is `Option<Completion>`, and whether the keyboard is *in* it is
//! [`Completion::chosen`]. Behind one flag the list becomes a mode — arrowing
//! into it would have to close the field's own caret, `Escape` would have to
//! decide which of the two it was leaving, and every keystroke would be a
//! question about which thing has the keyboard. It is the shape the find field
//! was already caught in.
//!
//! So the field never stops having the keyboard. Characters are typed into it
//! while the list is up, and typing is what puts the keyboard back in it: the
//! list narrows and forgets its row. Only `Tab` and the two arrows are the
//! list's, and only `Enter` changes meaning — and only while a row is chosen,
//! which is the flag.
//!
//! **The names come from a job**, because rule 1 gives the UI thread no
//! filesystem syscalls at all and a half-typed path is exactly where an
//! unresponsive mount is met. The read is per directory rather than per
//! keystroke: [`Completion::narrow`] is what a growing name costs, and only a
//! new parent asks the pool again.

use std::path::{Path, PathBuf};

/// How many rows the list shows before it scrolls.
///
/// The list hangs below the path bar over the rows a person is navigating away
/// from, and a panel as tall as the window is one that covers the answer it is
/// helping them reach.
pub(crate) const VISIBLE: usize = 8;

/// The directories that could finish what the path field is holding.
pub(crate) struct Completion {
    /// The directory the names were read from.
    pub(crate) parent: PathBuf,
    /// Every directory in it, in display order.
    rows: Vec<String>,
    /// Those of [`Completion::rows`] the typed name matches, as indices into it.
    shown: Vec<usize>,
    /// Which of [`Completion::shown`] the keyboard is on.
    ///
    /// `None` while the keyboard is in the field, which is where it starts and
    /// where typing a character puts it back. The second of the two flags: a
    /// list that is up and a list that is being walked are different states, and
    /// this is the one `Enter` reads.
    pub(crate) chosen: Option<usize>,
    /// Which of [`Completion::shown`] the pointer is over.
    ///
    /// **Not [`Completion::chosen`].** Where the pointer is and where the
    /// keyboard is are two questions, and answering both with one field would
    /// make a pointer drifting across the list change what `Enter` means.
    pub(crate) hovered: Option<usize>,
    /// The first of [`Completion::shown`] that is drawn.
    pub(crate) scroll: usize,
    /// The name that produced [`Completion::shown`].
    ///
    /// Kept so that a key which does not change the text — an arrow — does not
    /// narrow again and throw away the row it just moved to.
    typed: String,
}

impl Completion {
    /// The list for a directory that has just been read, narrowed to `typed`.
    pub(crate) fn new(parent: PathBuf, mut rows: Vec<String>, typed: &str) -> Self {
        rows.sort_by(|left, right| sk_model::natural_cmp(left, right));
        let mut list = Self {
            parent,
            rows,
            shown: Vec::new(),
            chosen: None,
            hovered: None,
            scroll: 0,
            typed: typed.to_owned(),
        };
        list.refilter(typed);
        list
    }

    /// Narrows to a name that has changed, and puts the keyboard back in the
    /// field.
    ///
    /// Typing is the gesture that means "not that row, this text" — so a name
    /// that moved forgets the chosen row, and a key that left the name alone
    /// leaves the row alone too.
    pub(crate) fn narrow(&mut self, typed: &str) {
        if self.typed == typed {
            return;
        }
        typed.clone_into(&mut self.typed);
        self.refilter(typed);
        self.chosen = None;
        // The pointer has not moved, but what is under it has: the next move
        // says which row it is now over.
        self.hovered = None;
        self.scroll = 0;
    }

    /// Which rows a name admits.
    ///
    /// **Case-insensitively**, because somebody typing a path is reaching for a
    /// directory they have not looked at: `doc` finding `Documents` is the whole
    /// point, and accepting the row writes the real name into the field.
    ///
    /// **A dot is how you ask for a hidden one.** Every directory was read, so
    /// which of them to offer is a question about the name rather than another
    /// trip to the pool — and a list that opened with forty dot-directories on
    /// it would bury the four somebody was after.
    fn refilter(&mut self, typed: &str) {
        let wanted = typed.to_lowercase();
        let hidden = typed.starts_with('.');
        self.shown = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, name)| hidden || !name.starts_with('.'))
            .filter(|(_, name)| name.to_lowercase().starts_with(&wanted))
            .map(|(at, _)| at)
            .collect();
    }

    /// How many names are on offer.
    pub(crate) fn len(&self) -> usize {
        self.shown.len()
    }

    /// Whether nothing matches what has been typed, which is a list with no
    /// panel: a folder somebody is naming from memory has no rows to offer.
    pub(crate) fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }

    /// Whether the list has anything to say, which is what draws it.
    ///
    /// **One row that is exactly what has been typed says nothing** — and it is
    /// what `Ctrl+L` produces every time, because the field opens holding a
    /// complete path. A panel over the top of the listing to repeat the last
    /// component back is noise in the one place a person is about to click.
    pub(crate) fn offers_anything(&self) -> bool {
        !self.is_empty() && (self.len() > 1 || self.name(0) != Some(self.typed.as_str()))
    }

    /// The name at an offset into what is shown.
    pub(crate) fn name(&self, at: usize) -> Option<&str> {
        self.shown.get(at).and_then(|index| self.rows.get(*index)).map(String::as_str)
    }

    /// The name the keyboard is on, if it is in the list at all.
    pub(crate) fn chosen_name(&self) -> Option<&str> {
        self.chosen.and_then(|at| self.name(at))
    }

    /// Moves the keyboard into the list, and then along it.
    ///
    /// Wrapping at both ends, the way the sibling menu does: a list of names is
    /// a ring with no natural end, and `Down` on the last row doing nothing
    /// reads as broken rather than as finished.
    pub(crate) fn step(&mut self, delta: isize) {
        let Ok(count) = isize::try_from(self.shown.len()) else { return };
        if count == 0 {
            return;
        }
        let from = match self.chosen.and_then(|at| isize::try_from(at).ok()) {
            Some(at) => at.wrapping_add(delta),
            // The first press puts the keyboard on an end rather than one step
            // past where it was not: `Down` takes the first row and `Up` the
            // last, which is what a list under a field does everywhere.
            None if delta > 0 => 0,
            None => count - 1,
        };
        self.chosen = usize::try_from(from.rem_euclid(count)).ok();
        self.reveal();
    }

    /// Puts the chosen row inside the drawn window.
    fn reveal(&mut self) {
        let Some(chosen) = self.chosen else { return };
        if chosen < self.scroll {
            self.scroll = chosen;
        } else if chosen >= self.scroll + VISIBLE {
            self.scroll = chosen + 1 - VISIBLE;
        }
    }

    /// The rows that are drawn, and the offset of the first.
    pub(crate) fn window(&self) -> (usize, Vec<&str>) {
        let first = self.scroll.min(self.shown.len());
        let last = (first + VISIBLE).min(self.shown.len());
        (first, (first..last).filter_map(|at| self.name(at)).collect())
    }

    /// Marks the drawn row the pointer is over, by its offset from the first.
    pub(crate) fn hover(&mut self, offset: Option<usize>) -> bool {
        let row = offset.map(|at| self.scroll + at).filter(|row| *row < self.shown.len());
        if self.hovered == row {
            return false;
        }
        self.hovered = row;
        true
    }

    /// The row the pointer would take: whatever it is over.
    pub(crate) fn under_the_pointer(&self) -> Option<&str> {
        self.hovered.and_then(|at| self.name(at))
    }

    /// The longest beginning every offered name shares, if it is longer than
    /// what has been typed.
    ///
    /// What `Tab` inserts. `Doc` against `Documents` and `Downloads` gives
    /// nothing; against `Documents` alone it gives the rest of the word — which
    /// is the press that saves the most typing and the one a shell taught
    /// everybody to expect.
    ///
    /// Compared by character and not by byte, because a common prefix cut inside
    /// `é` is a `String` that cannot be built.
    pub(crate) fn common(&self) -> Option<String> {
        let mut names = self.shown.iter().filter_map(|at| self.rows.get(*at));
        let first = names.next()?;
        let mut shared = first.chars().count();
        for name in names {
            let same = first.chars().zip(name.chars()).take_while(|(a, b)| a == b).count();
            shared = shared.min(same);
        }
        let prefix: String = first.chars().take(shared).collect();
        (prefix.chars().count() > self.typed.chars().count()).then_some(prefix)
    }
}

/// The directory to read and the name being typed, out of one line of text.
///
/// Split at the last `/`: what is before it names a directory and what is after
/// it is the beginning of a name in it. A line with no `/` at all is a name in
/// the directory the pane is already showing, which is what makes a relative
/// path complete — the same rule [`crate::app`] applies when the field is
/// committed.
pub(crate) fn split(typed: &str, here: &Path) -> (PathBuf, String) {
    split_under(typed, here, home().as_deref())
}

/// [`split`], with the home directory handed in rather than read — so a test
/// can name one without changing the environment of a process whose other
/// threads are reading it.
fn split_under(typed: &str, here: &Path, home: Option<&Path>) -> (PathBuf, String) {
    let Some(at) = typed.rfind('/') else {
        return (here.to_path_buf(), typed.to_owned());
    };
    let (head, name) = (&typed[..=at], &typed[at + 1..]);
    let parent = expand_under(head, home).unwrap_or_else(|| {
        let path = Path::new(head);
        if path.is_absolute() { path.to_path_buf() } else { here.join(path) }
    });
    (parent, name.to_owned())
}

/// `~` and `~/…` as this user's home, or nothing for any other text.
///
/// The one expansion the path field does. A shell's `~other` is somebody else's
/// home and needs `getpwnam`, which is the network call
/// [`sk_fs::facts`](../../sk-fs/src/facts.rs) already refuses to make on the
/// frame loop.
pub(crate) fn expand_home(typed: &str) -> Option<PathBuf> {
    expand_under(typed, home().as_deref())
}

/// [`expand_home`] against a home that is handed in.
fn expand_under(typed: &str, home: Option<&Path>) -> Option<PathBuf> {
    let rest = typed.strip_prefix('~')?;
    let home = home?;
    match rest {
        "" | "/" => Some(home.to_path_buf()),
        _ => Some(home.join(rest.strip_prefix('/')?)),
    }
}

/// This user's home, which is what `~` means.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Completion, VISIBLE, split};
    use std::path::{Path, PathBuf};

    fn list(names: &[&str], typed: &str) -> Completion {
        let rows = names.iter().map(|name| (*name).to_owned()).collect();
        Completion::new(PathBuf::from("/home/emil"), rows, typed)
    }

    /// The rule the whole module exists for: the list showing and the list
    /// having the keyboard are two states, and typing is what moves between
    /// them.
    #[test]
    fn typing_puts_the_keyboard_back_in_the_field() {
        let mut names = list(&["Documents", "Downloads", "Desktop"], "D");
        assert_eq!(names.len(), 3);
        assert_eq!(names.chosen, None, "a list that opens does not take the keyboard");

        names.step(1);
        assert_eq!(names.chosen_name(), Some("Desktop"));
        // An arrow does not change the text, so it must not narrow: the row it
        // just moved to would be thrown away by its own keystroke.
        names.narrow("D");
        assert_eq!(names.chosen_name(), Some("Desktop"));

        names.narrow("Do");
        assert_eq!(names.chosen, None, "a character typed is the keyboard coming back");
        assert_eq!(names.len(), 2);
    }

    /// `Ctrl+L` opens the field holding a complete path, so the list it produces
    /// is the last component repeating itself — a panel over the top of the
    /// listing saying nothing.
    #[test]
    fn a_name_that_is_already_complete_is_not_a_list() {
        assert!(!list(&["fixture", "other"], "fixture").offers_anything());
        assert!(!list(&[], "fixture").offers_anything());
        // But the moment there is a choice, or anything left to add, there is.
        assert!(list(&["fixture", "fixtures"], "fixture").offers_anything());
        assert!(list(&["fixtures"], "fixture").offers_anything());
        assert!(list(&["alpha", "beta"], "").offers_anything());
    }

    /// A person typing a path is reaching for a directory they have not looked
    /// at, so the case they guessed is not the case on disk.
    #[test]
    fn a_name_matches_whatever_case_it_was_typed_in() {
        let names = list(&["Documents", "downloads"], "do");
        assert_eq!(names.len(), 2);
        let names = list(&["Documents", "downloads"], "DO");
        assert_eq!(names.len(), 2);
    }

    /// Every directory is read, and a dot is how a hidden one is asked for.
    #[test]
    fn a_dot_is_what_offers_the_hidden_directories() {
        let names = list(&[".config", ".local", "Documents"], "");
        assert_eq!(names.len(), 1, "a list of dot-directories buries the ones in view");
        let names = list(&[".config", ".local", "Documents"], ".");
        assert_eq!(names.len(), 2);
        let names = list(&[".config", ".local"], ".co");
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn tab_takes_the_longest_beginning_they_share() {
        let names = list(&["Documents", "Downloads"], "D");
        assert_eq!(names.common().as_deref(), Some("Do"));
        // Nothing left to share: two names that differ at the next character
        // give back what is already there, and `Tab` has nothing to insert.
        let names = list(&["Documents", "Downloads"], "Do");
        assert_eq!(names.common(), None);
        let names = list(&["Documents"], "Do");
        assert_eq!(names.common().as_deref(), Some("Documents"));
        let names = list(&[], "z");
        assert_eq!(names.common(), None);
    }

    /// A caret cut inside a multi-byte character is a `String` that cannot be
    /// built, which is the defect the character-wise comparison exists for.
    #[test]
    fn a_shared_beginning_is_cut_between_characters() {
        let names = list(&["été-2024", "été-2025"], "é");
        assert_eq!(names.common().as_deref(), Some("été-202"));
    }

    #[test]
    fn the_first_arrow_takes_an_end_and_the_list_wraps() {
        let mut names = list(&["a", "b", "c"], "");
        names.step(-1);
        assert_eq!(names.chosen_name(), Some("c"));
        names.step(1);
        assert_eq!(names.chosen_name(), Some("a"));
        names.step(-1);
        assert_eq!(names.chosen_name(), Some("c"));
    }

    #[test]
    fn an_empty_list_does_not_move_and_does_not_panic() {
        let mut names = list(&["a", "b"], "z");
        assert_eq!(names.len(), 0);
        names.step(1);
        assert_eq!(names.chosen, None);
        assert_eq!(names.window().1.len(), 0);
        assert!(!names.hover(Some(0)));
    }

    /// The pointer and the keyboard answer different questions, so a pointer
    /// moving across the list must not change what `Enter` does.
    #[test]
    fn hovering_a_row_is_not_the_keyboard_choosing_one() {
        let mut names = list(&["alpha", "beta", "gamma"], "");
        assert!(names.hover(Some(2)));
        assert_eq!(names.under_the_pointer(), Some("gamma"));
        assert_eq!(names.chosen, None, "the keyboard is still in the field");
        assert!(names.hover(Some(9)), "a row that is not there is nothing hovered");
        assert_eq!(names.under_the_pointer(), None);
        assert!(!names.hover(None), "and it does not change twice");
    }

    #[test]
    fn a_long_list_scrolls_to_the_chosen_row() {
        let names: Vec<String> = (0..40).map(|index| format!("d{index:02}")).collect();
        let mut list = Completion::new(PathBuf::from("/x"), names, "");
        for _ in 0..=VISIBLE {
            list.step(1);
        }
        let (first, drawn) = list.window();
        assert_eq!(drawn.len(), VISIBLE);
        assert!(
            list.chosen.is_some_and(|row| row >= first && row < first + VISIBLE),
            "the chosen row at {:?} is outside the drawn window at {first}",
            list.chosen
        );
    }

    /// The rows are what was narrowed, not what was read: a window over the
    /// whole list would name the wrong directory for a pointer's offset.
    #[test]
    fn the_drawn_window_is_over_the_matches() {
        let names = list(&["alpha", "beta", "brie"], "b");
        assert_eq!(names.window().1, ["beta", "brie"]);
        assert_eq!(names.name(0), Some("beta"));
    }

    #[test]
    fn a_line_splits_into_a_directory_and_the_name_in_it() {
        let here = Path::new("/srv/fixture");
        assert_eq!(split("/home/emil/Doc", here), (PathBuf::from("/home/emil"), "Doc".to_owned()));
        assert_eq!(split("/home/emil/", here), (PathBuf::from("/home/emil"), String::new()));
        assert_eq!(split("/", here), (PathBuf::from("/"), String::new()));
        // No separator at all: a name in the directory the pane is showing,
        // which is the same rule that commits a relative path.
        assert_eq!(split("alp", here), (PathBuf::from("/srv/fixture"), "alp".to_owned()));
        assert_eq!(split("", here), (PathBuf::from("/srv/fixture"), String::new()));
        assert_eq!(split("sub/al", here), (PathBuf::from("/srv/fixture/sub"), "al".to_owned()));
    }

    /// `~other` is a shell feature that needs `getpwnam`. Expanding it as this
    /// user's home would send a person somewhere they did not ask for, and
    /// treating it as a relative directory called `~other` at least fails with a
    /// message that names what it looked for.
    ///
    /// **The home is handed in**, not set: tests run on parallel threads, and
    /// changing `HOME` under the ones reading it is a data race.
    #[test]
    fn only_this_users_home_is_expanded() {
        use super::{expand_under, split_under};
        let home = Some(Path::new("/home/tester"));

        assert_eq!(expand_under("~", home).as_deref(), Some(Path::new("/home/tester")));
        assert_eq!(expand_under("~/", home).as_deref(), Some(Path::new("/home/tester")));
        assert_eq!(
            expand_under("~/Downloads/x", home).as_deref(),
            Some(Path::new("/home/tester/Downloads/x"))
        );
        assert_eq!(expand_under("~other", home), None);
        assert_eq!(expand_under("~other/files", home), None);
        assert_eq!(expand_under("/etc", home), None);
        assert_eq!(expand_under("relative", home), None);
        assert_eq!(expand_under("~/", None), None, "no home, so nothing to expand to");

        // And the split reads it the same way, so a completion under `~` lists
        // the home directory rather than one called `~`.
        let here = Path::new("/srv");
        assert_eq!(
            split_under("~/Doc", here, home),
            (PathBuf::from("/home/tester"), "Doc".to_owned())
        );
        assert_eq!(split_under("~/", here, home), (PathBuf::from("/home/tester"), String::new()));
    }
}
