// SPDX-License-Identifier: MIT OR Apache-2.0
//! The menu a breadcrumb separator opens.
//!
//! **What the chevron is for.** A separator sits between two components, so the
//! directories inside the one on its left are the *siblings* of the one on its
//! right — and that is the list a person wants when they are three levels deep
//! and meant to be in the folder next door. Windows 11 made this the standard
//! shape of a path bar and it is the one feature of that path bar worth copying:
//! it turns "up, up, down, down" into one press and one click, at any depth.
//!
//! The separator after the *last* component has no right-hand neighbour, so its
//! list is the children of where you already are. That is the one that saves the
//! most steps, because it is a way into a subdirectory without leaving.
//!
//! **The listing happens on a job.** Rule 1: the UI thread issues no filesystem
//! syscalls, and a directory of a hundred thousand entries on a hung mount is
//! exactly the case that rule exists for. The menu is asked for, and it opens
//! when the answer arrives.

use std::path::{Path, PathBuf};

/// How many rows the menu shows before it scrolls.
///
/// `/usr/lib` has hundreds of subdirectories and a menu as tall as the window is
/// a menu whose bottom is off the screen.
pub(crate) const VISIBLE: usize = 12;

/// A separator's menu, once its directory has been read.
pub(crate) struct CrumbMenu {
    /// Which separator it hangs from — the index of the component on its left.
    pub(crate) depth: usize,
    /// The directory whose children these are.
    pub(crate) parent: PathBuf,
    /// The subdirectory names, in display order.
    pub(crate) rows: Vec<String>,
    /// Which row the keyboard or pointer is on.
    pub(crate) active: Option<usize>,
    /// The first row drawn, for a list longer than [`VISIBLE`].
    pub(crate) scroll: usize,
    /// The child that is on the current path, so the way you came is marked.
    ///
    /// `None` for the separator after the last component: nothing below it is on
    /// the path, because the path ends there.
    pub(crate) current: Option<usize>,
    /// Where the separator is, so the menu opens under it.
    pub(crate) at: f32,
}

impl CrumbMenu {
    /// Builds the menu for a directory that has just been read.
    ///
    /// `along` is the component of the current path that follows `parent`, which
    /// is the row to mark and to start on — a menu that opens with nothing
    /// selected makes the keyboard start from the top of an alphabet rather than
    /// from where the person already is.
    pub(crate) fn new(
        depth: usize,
        parent: PathBuf,
        at: f32,
        mut rows: Vec<String>,
        along: Option<&str>,
    ) -> Self {
        rows.sort_by(|left, right| sk_model::natural_cmp(left, right));
        let current = along.and_then(|name| rows.iter().position(|row| row == name));
        let mut menu = Self { depth, parent, rows, active: current, scroll: 0, current, at };
        menu.reveal();
        menu
    }

    /// The path a row leads to.
    pub(crate) fn path_of(&self, row: usize) -> Option<PathBuf> {
        self.rows.get(row).map(|name| self.parent.join(name))
    }

    /// Moves the highlight, wrapping at both ends.
    ///
    /// Wrapping because the list is a ring of siblings with no natural end, and
    /// because a menu opened on the last row with `Down` doing nothing reads as
    /// broken rather than as finished.
    pub(crate) fn step(&mut self, delta: isize) {
        let Ok(count) = isize::try_from(self.rows.len()) else { return };
        if count == 0 {
            return;
        }
        let from = self.active.and_then(|row| isize::try_from(row).ok()).unwrap_or(0);
        let next = from.wrapping_add(delta).rem_euclid(count);
        self.active = usize::try_from(next).ok();
        self.reveal();
    }

    /// Puts the highlighted row on screen.
    fn reveal(&mut self) {
        let Some(active) = self.active else { return };
        if active < self.scroll {
            self.scroll = active;
        } else if active >= self.scroll + VISIBLE {
            self.scroll = active + 1 - VISIBLE;
        }
    }

    /// The rows that are drawn, and the index of the first.
    pub(crate) fn window(&self) -> (usize, &[String]) {
        let first = self.scroll.min(self.rows.len());
        let last = (first + VISIBLE).min(self.rows.len());
        (first, &self.rows[first..last])
    }

    /// Puts the highlight on a drawn row, by its offset from the first drawn.
    pub(crate) fn hover(&mut self, offset: usize) -> bool {
        let row = self.scroll + offset;
        if row >= self.rows.len() || self.active == Some(row) {
            return false;
        }
        self.active = Some(row);
        true
    }
}

/// The directories inside `parent`, as a menu would list them.
///
/// **Runs on a job.** Symbolic links to directories are included, because a link
/// into a directory is a way into that directory and a path bar that hid them
/// would be hiding the shortest route somebody made for themselves. Dotfiles
/// follow the pane's own setting, so the menu shows what the list would.
pub(crate) fn children(
    parent: &Path,
    show_hidden: bool,
    cancel: &sk_util::Cancel,
) -> Result<Vec<String>, sk_model::Problem> {
    let found = sk_fs::scan::names(parent, cancel)?;
    let listing = &found.listing;
    let mut rows = Vec::new();
    for row in 0..listing.rows() {
        let Some(id) = listing.at(row) else { continue };
        if !listing.flags(id).has(sk_model::Flags::DIR) {
            continue;
        }
        let name = listing.name(id);
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        rows.push(name.to_owned());
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(rows: &[&str], along: Option<&str>) -> CrumbMenu {
        let rows = rows.iter().map(|name| (*name).to_owned()).collect();
        CrumbMenu::new(1, PathBuf::from("/home"), 0.0, rows, along)
    }

    /// Natural order, the same as the list's: `img10` after `img2`.
    #[test]
    fn the_rows_are_in_the_order_the_list_would_show_them() {
        let menu = menu(&["img10", "img2", "Alpha", "beta"], None);
        assert_eq!(menu.rows, ["Alpha", "beta", "img2", "img10"]);
    }

    /// A menu that opens with nothing selected makes the keyboard start from the
    /// top of an alphabet rather than from where the person already is.
    #[test]
    fn it_opens_on_the_component_the_path_goes_through() {
        let menu = menu(&["one", "two", "three"], Some("two"));
        assert_eq!(menu.active, Some(2), "{:?}", menu.rows);
        assert_eq!(menu.current, Some(2));
    }

    #[test]
    fn a_name_that_is_not_there_leaves_it_unselected() {
        let menu = menu(&["one", "two"], Some("gone"));
        assert_eq!(menu.active, None);
        assert_eq!(menu.current, None);
    }

    #[test]
    fn the_highlight_wraps_at_both_ends() {
        let mut menu = menu(&["a", "b", "c"], None);
        menu.step(-1);
        assert_eq!(menu.active, Some(2));
        menu.step(1);
        assert_eq!(menu.active, Some(0));
    }

    #[test]
    fn a_row_leads_to_a_child_of_the_directory_it_lists() {
        let menu = menu(&["one", "two"], None);
        assert_eq!(menu.path_of(0), Some(PathBuf::from("/home/one")));
        assert_eq!(menu.path_of(9), None);
    }

    /// A list longer than the window scrolls to keep the highlight drawn, which
    /// is the whole reason `window` exists.
    #[test]
    fn a_long_list_scrolls_to_the_highlight() {
        let names: Vec<String> = (0..40).map(|index| format!("d{index:02}")).collect();
        let mut menu = CrumbMenu::new(1, PathBuf::from("/x"), 0.0, names, None);
        menu.step(1);
        assert_eq!(menu.window().0, 0);
        for _ in 0..VISIBLE {
            menu.step(1);
        }
        let (first, drawn) = menu.window();
        assert_eq!(drawn.len(), VISIBLE);
        assert!(
            menu.active.is_some_and(|row| row >= first && row < first + VISIBLE),
            "the highlight at {:?} is outside the drawn window at {first}",
            menu.active
        );
    }

    #[test]
    fn an_empty_menu_does_not_move_and_does_not_panic() {
        let mut menu = menu(&[], None);
        menu.step(1);
        assert_eq!(menu.active, None);
        assert_eq!(menu.window().1.len(), 0);
    }
}
