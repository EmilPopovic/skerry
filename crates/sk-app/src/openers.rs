// SPDX-License-Identifier: MIT OR Apache-2.0
//! Open With: the list of applications that say they handle a file.
//!
//! **Not a menu of commands.** The context menu and the palette are views of the
//! command registry, and every row in them is a `CommandSpec`. These rows are
//! *data* — whatever `mimeapps.list` and the `.desktop` files on this machine
//! happen to say — so they are their own overlay, the way a breadcrumb's sibling
//! menu is. Rule 2 is kept by the one registry entry that opens this.
//!
//! **The list is read on a job.** Resolving a type opens the file when the name
//! says nothing, and resolving handlers reads `.desktop` files across every XDG
//! data directory.

use sk_desktop::DesktopEntry;

/// How many applications the list shows before it scrolls.
///
/// A machine with a dozen text editors installed is a real machine, and a list
/// as tall as the window is a list whose bottom nobody can reach.
pub(crate) const VISIBLE: usize = 10;

/// The open list, once the job that built it has answered.
pub(crate) struct Openers {
    /// What was asked about.
    pub(crate) path: std::path::PathBuf,
    /// The type it turned out to be, shown as the list's subtitle so a person
    /// can see *why* these are the choices.
    pub(crate) mime: String,
    /// The applications, best first — which is the order the desktop's own
    /// association files put them in.
    pub(crate) rows: Vec<DesktopEntry>,
    /// Which row the keyboard or the pointer is on.
    pub(crate) active: usize,
    /// The first row drawn.
    pub(crate) scroll: usize,
}

impl Openers {
    pub(crate) fn new(path: std::path::PathBuf, mime: String, rows: Vec<DesktopEntry>) -> Self {
        Self { path, mime, rows, active: 0, scroll: 0 }
    }

    /// Moves the highlight, wrapping at both ends.
    pub(crate) fn step(&mut self, delta: isize) {
        let Ok(count) = isize::try_from(self.rows.len()) else { return };
        if count == 0 {
            return;
        }
        let Ok(from) = isize::try_from(self.active) else { return };
        let next = from.wrapping_add(delta).rem_euclid(count);
        self.active = usize::try_from(next).unwrap_or(0);
        self.reveal();
    }

    fn reveal(&mut self) {
        if self.active < self.scroll {
            self.scroll = self.active;
        } else if self.active >= self.scroll + VISIBLE {
            self.scroll = self.active + 1 - VISIBLE;
        }
    }

    /// The rows that are drawn, and the index of the first.
    pub(crate) fn window(&self) -> (usize, &[DesktopEntry]) {
        let first = self.scroll.min(self.rows.len());
        let last = (first + VISIBLE).min(self.rows.len());
        (first, &self.rows[first..last])
    }

    /// Puts the highlight on a drawn row, by its offset from the first drawn.
    pub(crate) fn hover(&mut self, offset: usize) -> bool {
        let row = self.scroll + offset;
        if row >= self.rows.len() || self.active == row {
            return false;
        }
        self.active = row;
        true
    }

    /// The application the highlighted row names.
    pub(crate) fn chosen(&self) -> Option<&DesktopEntry> {
        self.rows.get(self.active)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Openers, VISIBLE};
    use sk_desktop::DesktopEntry;
    use std::path::PathBuf;

    fn entry(name: &str) -> DesktopEntry {
        DesktopEntry {
            id: format!("{name}.desktop"),
            path: PathBuf::from("/usr/share/applications"),
            name: name.to_owned(),
            exec: format!("{name} %f"),
            terminal: false,
            mime_types: Vec::new(),
            no_display: false,
            try_exec: None,
        }
    }

    fn list(count: usize) -> Openers {
        let rows = (0..count).map(|index| entry(&format!("app-{index}"))).collect();
        Openers::new(PathBuf::from("/tmp/thing.txt"), "text/plain".to_owned(), rows)
    }

    #[test]
    fn the_highlight_wraps_at_both_ends() {
        let mut openers = list(3);
        assert_eq!(openers.chosen().map(|it| it.name.clone()), Some("app-0".to_owned()));
        openers.step(-1);
        assert_eq!(openers.chosen().map(|it| it.name.clone()), Some("app-2".to_owned()));
        openers.step(1);
        assert_eq!(openers.chosen().map(|it| it.name.clone()), Some("app-0".to_owned()));
    }

    #[test]
    fn a_long_list_scrolls_to_the_highlight() {
        let mut openers = list(VISIBLE + 5);
        assert_eq!(openers.window().1.len(), VISIBLE);
        openers.step(-1);
        let (first, drawn) = openers.window();
        assert_eq!(first + drawn.len(), VISIBLE + 5, "the last row is not drawn");
        assert!(openers.active >= first, "the highlight is above the window");
    }

    /// An empty list is what a file with no handler installed produces, and it
    /// must not panic or select something that is not there.
    #[test]
    fn nothing_to_open_it_with_is_a_list_of_nothing() {
        let mut openers = list(0);
        assert!(openers.chosen().is_none());
        openers.step(1);
        assert!(openers.chosen().is_none());
        assert_eq!(openers.window().1.len(), 0);
        assert!(!openers.hover(0));
    }
}
