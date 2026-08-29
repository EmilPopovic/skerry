//! Searching everything under the folder you are in.
//!
//! **Not the find popup.** `Ctrl+F` narrows the listing that is already on
//! screen — one directory, instantly, because the names are already in memory.
//! This walks the tree underneath it, which is a job and a wait and a list of
//! results in other folders. Two things that both start with typing and are not
//! the same thing, so they are two surfaces.
//!
//! **Not a flattened view either**, which is M4's and is a bigger change: the
//! results here are navigated to, not acted on in place. `Enter` takes the pane
//! to the file's folder and puts the cursor on it, which is one keystroke from
//! everything the program can do to a file — and the honest version of what can
//! be built on a `Listing` that a directory read owns.
//!
//! The matching rule is [`sk_model::contains_folded`], the find popup's, because
//! [rule 8](../../../docs/03-principles.md) says the filter grammar is the same
//! in every view.

use std::path::PathBuf;

use sk_index::find::Found;

use crate::field::Field;

/// How many results are drawn before the list scrolls.
pub(crate) const VISIBLE: usize = 12;

/// A search over one subtree, as it stands.
pub(crate) struct Search {
    /// The directory the walk started from.
    pub(crate) root: PathBuf,
    /// What has been typed, and where the caret is.
    pub(crate) query: Field,
    /// The query the walk in flight is for, so a stale reply is discarded.
    ///
    /// Typing restarts the walk; a reply from the previous one is on its way and
    /// would otherwise replace a newer list with an older one.
    pub(crate) asked: String,
    /// What has been found so far.
    pub(crate) found: Found,
    /// Which result the keyboard or the pointer is on.
    pub(crate) active: usize,
    /// The first result drawn.
    pub(crate) scroll: usize,
    /// The token for the walk in flight, so a new query stops the old one.
    pub(crate) cancel: sk_util::Cancel,
}

impl Search {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            query: Field::new(String::new()),
            asked: String::new(),
            found: Found::default(),
            active: 0,
            scroll: 0,
            cancel: sk_util::Cancel::new(),
        }
    }

    /// Takes a report, unless it is from a query nobody is asking any more.
    pub(crate) fn arrived(&mut self, query: &str, found: Found) -> bool {
        if query != self.asked {
            return false;
        }
        // The highlight stays on the row it is on rather than following the list
        // down: results arrive underneath, and a highlight that jumped to the
        // newest one would make the list unusable while it is still filling.
        self.found = found;
        self.active = self.active.min(self.found.hits.len().saturating_sub(1));
        self.reveal();
        true
    }

    /// Moves the highlight, wrapping at both ends.
    pub(crate) fn step(&mut self, delta: isize) {
        let Ok(count) = isize::try_from(self.found.hits.len()) else { return };
        if count == 0 {
            return;
        }
        let Ok(from) = isize::try_from(self.active) else { return };
        self.active = usize::try_from(from.wrapping_add(delta).rem_euclid(count)).unwrap_or(0);
        self.reveal();
    }

    fn reveal(&mut self) {
        if self.active < self.scroll {
            self.scroll = self.active;
        } else if self.active >= self.scroll + VISIBLE {
            self.scroll = self.active + 1 - VISIBLE;
        }
        self.scroll = self.scroll.min(self.found.hits.len().saturating_sub(1));
    }

    /// The results that are drawn, and the index of the first.
    pub(crate) fn window(&self) -> (usize, &[sk_index::Hit]) {
        let first = self.scroll.min(self.found.hits.len());
        let last = (first + VISIBLE).min(self.found.hits.len());
        (first, &self.found.hits[first..last])
    }

    /// Puts the highlight on a drawn row, by its offset from the first drawn.
    pub(crate) fn hover(&mut self, offset: usize) -> bool {
        let row = self.scroll + offset;
        if row >= self.found.hits.len() || self.active == row {
            return false;
        }
        self.active = row;
        true
    }

    /// The result the highlight is on.
    pub(crate) fn chosen(&self) -> Option<&sk_index::Hit> {
        self.found.hits.get(self.active)
    }

    /// What to say above the list: how many, how far the walk got, and whether
    /// it is still going.
    pub(crate) fn summary(&self) -> String {
        if self.asked.is_empty() {
            return sk_model::i18n::lookup("search.type").to_owned();
        }
        let key = if self.found.capped {
            "search.capped"
        } else if self.found.complete {
            "search.done"
        } else {
            "search.running"
        };
        sk_model::i18n::format(
            key,
            &[&self.found.hits.len(), &sk_ui::format::grouped(self.found.scanned as usize)],
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Search, VISIBLE};
    use sk_index::find::{Found, Hit};
    use std::path::PathBuf;

    fn hit(name: &str) -> Hit {
        Hit { path: PathBuf::from("/tmp").join(name), size: 1, modified: 0, directory: false }
    }

    fn searching(count: usize) -> Search {
        let mut search = Search::new(PathBuf::from("/tmp"));
        search.asked = "x".to_owned();
        let hits = (0..count).map(|index| hit(&format!("x-{index}"))).collect();
        search.arrived("x", Found { hits, scanned: 100, ..Found::default() });
        search
    }

    #[test]
    fn the_highlight_wraps_and_the_window_follows_it() {
        let mut search = searching(VISIBLE + 5);
        assert_eq!(search.window().1.len(), VISIBLE);
        search.step(-1);
        let (first, drawn) = search.window();
        assert_eq!(first + drawn.len(), VISIBLE + 5, "the last row is not drawn");
        assert!(search.active >= first);
        search.step(1);
        assert_eq!(search.active, 0, "it did not wrap");
        assert_eq!(search.window().0, 0, "the window did not come back with it");
    }

    /// A reply for a query nobody is typing any more must not replace the list.
    ///
    /// Every keystroke starts a walk, and the one before it is still running: a
    /// reply taken on arrival rather than on its query would put the results for
    /// `re` over the results for `rep`, at whatever moment the older walk
    /// happened to report.
    #[test]
    fn a_reply_for_an_older_query_is_dropped() {
        let mut search = searching(3);
        assert!(!search.arrived("xy", Found { hits: vec![hit("no")], ..Found::default() }));
        assert_eq!(search.found.hits.len(), 3, "a stale reply replaced the list");
        assert!(search.arrived("x", Found { hits: vec![hit("yes")], ..Found::default() }));
        assert_eq!(search.found.hits.len(), 1);
    }

    /// A list that shrinks under the highlight must not leave it past the end.
    #[test]
    fn a_shorter_list_brings_the_highlight_back_inside_it() {
        let mut search = searching(20);
        search.step(-1);
        assert_eq!(search.active, 19);
        search.arrived("x", Found { hits: vec![hit("one")], ..Found::default() });
        assert_eq!(search.active, 0);
        assert_eq!(search.window().1.len(), 1);
        assert!(search.chosen().is_some());
    }

    /// Nothing found is a state the panel is in for as long as a walk takes.
    #[test]
    fn no_results_is_not_a_panic() {
        let mut search = searching(0);
        assert!(search.chosen().is_none());
        search.step(1);
        assert!(search.chosen().is_none());
        assert_eq!(search.window().1.len(), 0);
        assert!(!search.hover(0));
    }
}
