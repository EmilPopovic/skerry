// SPDX-License-Identifier: Apache-2.0
//! Searching everything under the folder you are in, or everything there is.
//!
//! **Not the find popup.** `Ctrl+F` narrows the listing that is already on
//! screen — one directory, instantly, because the names are already in memory.
//! This walks the tree underneath it, which is a job and a wait and a list of
//! results in other folders. Two things that both start with typing and are not
//! the same thing, so they are two surfaces.
//!
//! **Two scopes, one panel.** `Ctrl+Shift+F` searches under the folder and
//! `Ctrl+Alt+F` searches every local filesystem;
//! [ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)
//! settles what "everywhere" is and what it costs. Either chord switches an open
//! panel without losing what has been typed, and the switcher in the panel is the
//! pointer's way to the same thing.
//!
//! **`Enter` navigates and `Ctrl+Enter` flattens.** The first takes the pane to
//! the file's folder and puts the cursor on it, which is what a person wants when
//! the search was a way of getting somewhere. The second puts every result *into*
//! the pane as rows, which is what they want when the search was a way of finding
//! a set of files to do something to — and rows are rows, so selecting, copying,
//! dragging and trashing them all work with no second implementation. That is
//! [rule 8](../../../docs/03-principles.md): the same selection semantics, the
//! same path model and the same filter grammar in every view.
//!
//! The matching rule is [`sk_model::contains_folded`], the find popup's, because
//! [rule 8](../../../docs/03-principles.md) says the filter grammar is the same
//! in every view.

use std::path::{Path, PathBuf};

use sk_index::find::Found;

use crate::field::Field;

/// Seconds since the epoch.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| i64::try_from(it.as_secs()).unwrap_or(0))
}

/// How many results are drawn before the list scrolls.
pub(crate) const VISIBLE: usize = 12;

/// What a search covers.
///
/// **Not a list of roots**, because the roots of `Everywhere` are a fact about
/// the machine that changes while a window is open — a disk is plugged in — and
/// reading the mount table is a syscall the frame loop may not make. The roots
/// are asked for where they are already in memory, which is the sidebar's own
/// Volumes section: [ADR
/// 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    /// Everything under one folder.
    Here(PathBuf),
    /// Every local filesystem that is mounted.
    Everywhere,
}

impl Scope {
    /// The folder a result's own directory is shown relative to.
    ///
    /// Nothing for `Everywhere`: a result three disks away has no useful prefix
    /// to strip, and stripping the wrong one is how `a/b/c` becomes `../../..`.
    pub(crate) fn under(&self) -> Option<&Path> {
        match self {
            Self::Here(path) => Some(path),
            Self::Everywhere => None,
        }
    }

    /// Whether this is the whole machine.
    pub(crate) const fn is_everywhere(&self) -> bool {
        matches!(self, Self::Everywhere)
    }

    /// What the empty field says it will do.
    pub(crate) const fn prompt(&self) -> &'static str {
        match self {
            Self::Here(_) => "search.title",
            Self::Everywhere => "search.title_everywhere",
        }
    }
}

/// A search over one subtree, as it stands.
pub(crate) struct Search {
    /// What it covers.
    pub(crate) scope: Scope,
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
    /// When the oldest index this search scanned was built, if **every** root of
    /// the scope was answered from one.
    ///
    /// **The freshness policy's whole user interface.** [ADR
    /// 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md)
    /// verifies every hit before it is drawn, so a file deleted since the build
    /// cannot appear — but one *created* since it cannot be found, and no check
    /// fixes that. Saying how old the index is, is what makes that honest rather
    /// than mysterious.
    ///
    /// **Every root or none**, which is [ADR
    /// 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md):
    /// an index's age is a promise about what could have been found, and two ages
    /// averaged is a promise about neither — so a scope where one disk was
    /// indexed and another was walked takes the plain line, which promises
    /// nothing false.
    pub(crate) indexed: Option<i64>,
}

impl Search {
    /// Every result as a listing the pane can show.
    ///
    /// Each row carries the directory it came from, which is the whole of what
    /// makes this a listing rather than a list: `Pane::path_of` then answers for
    /// a result exactly as it answers for a row of a folder, and nothing above it
    /// has to know which it is looking at.
    ///
    /// Sizes and times come from the walk, so no row is `META_PENDING` and no
    /// second `statx` pass is dispatched for entries somebody has already stated.
    pub(crate) fn listing(&self) -> sk_model::Listing {
        let mut listing = sk_model::Listing::with_capacity(self.found.hits.len());
        for hit in &self.found.hits {
            let (Some(parent), Some(name)) = (hit.path.parent(), hit.path.file_name()) else {
                continue;
            };
            let flags = if hit.directory { sk_model::Flags::DIR } else { sk_model::Flags::NONE };
            let id = listing.push_from(parent, &name.to_string_lossy(), flags);
            listing.set_raw_name(id, name);
            listing.set_meta(id, hit.size, hit.modified, sk_model::Flags::NONE);
        }
        listing
    }

    pub(crate) fn new(scope: Scope) -> Self {
        Self {
            scope,
            query: Field::new(String::new()),
            asked: String::new(),
            found: Found::default(),
            active: 0,
            scroll: 0,
            cancel: sk_util::Cancel::new(),
            indexed: None,
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
            let key =
                if self.scope.is_everywhere() { "search.type_everywhere" } else { "search.type" };
            return sk_model::i18n::lookup(key).to_owned();
        }
        let scanned = sk_ui::format::grouped(self.found.scanned as usize);
        if let Some(built) = self.indexed.filter(|_| self.found.complete) {
            let key = if self.found.capped { "search.indexed_capped" } else { "search.indexed" };
            let when = sk_ui::format::modified(built, now());
            // The entries walked, which is the noun in all four of these keys —
            // the hit count has none of its own.
            let walked = self.found.scanned;
            return sk_model::i18n::plural(key, walked, &[&self.found.hits.len(), &scanned, &when]);
        }
        let key = if self.found.capped {
            "search.capped"
        } else if self.found.complete {
            "search.done"
        } else {
            "search.running"
        };
        // `search.running` has no plural form and needs none — it says "so far",
        // with no noun to agree. `plural` falls back to the bare key for it.
        sk_model::i18n::plural(key, self.found.scanned, &[&self.found.hits.len(), &scanned])
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
        let mut search = Search::new(super::Scope::Here(PathBuf::from("/tmp")));
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
