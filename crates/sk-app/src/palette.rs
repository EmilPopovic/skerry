// SPDX-License-Identifier: Apache-2.0
//! The command palette: the one registry, searchable.
//!
//! [Rule 4](../../../docs/03-principles.md) is that discoverability outranks
//! configurability — a feature nobody can find has no users, however well it is
//! implemented. The keyboard overlay already lists every command and its chord,
//! but a list of fifty rows is a reference, not a way in. This is the way in.
//!
//! It is cheap because it arrives last. `sk-cmd`'s registry has been the single
//! list since before there was a window, [`crate::field::Field`] is the same one
//! line of editable text the path bar and the rename field use, and the ranking
//! is `sk_cmd::fuzzy`. Nothing here is new machinery; it is a filter over a list
//! that was already being drawn.

use sk_cmd::Registry;

use crate::field::Field;

/// One command as the palette shows it.
pub struct Row {
    /// The command it runs.
    pub id: &'static str,
    /// What it is called, through the catalogue.
    pub name: String,
    /// Its chord, so the palette teaches the keyboard rather than replacing it.
    pub keys: String,
    /// Whether it can run right now. Shown and dimmed rather than hidden: a
    /// palette that hides what it cannot do is a palette that cannot answer
    /// "can this program do X".
    pub enabled: bool,
}

/// An open palette.
pub struct Palette {
    /// What has been typed.
    pub query: Field,
    /// Every command, in registry order, built once when it opens.
    rows: Vec<Row>,
    /// Indices into `rows`, best match first.
    pub shown: Vec<usize>,
    /// Which of `shown` the keyboard or the pointer is on.
    pub active: usize,
    /// The first row drawn, for a result list longer than the panel.
    pub scroll: usize,
}

/// Builds a palette over every command in the registry.
///
/// `enabled` is passed rather than the context a command runs against, for the
/// same reason [`crate::menu::build`] takes it: the program asks the registry
/// about its `App`, and a capture has no `App` and states the answers it wants a
/// golden frame of.
#[must_use]
pub fn open<C>(registry: &Registry<C>, enabled: impl Fn(&'static str) -> bool) -> Palette {
    let rows: Vec<Row> = registry
        .iter()
        .map(|command| Row {
            id: command.id,
            name: command.name.to_owned(),
            keys: command.keys.clone(),
            enabled: enabled(command.id),
        })
        .collect();
    let mut palette =
        Palette { query: Field::new(String::new()), rows, shown: Vec::new(), active: 0, scroll: 0 };
    palette.refilter();
    palette
}

impl Palette {
    /// The row at a position in the result list.
    #[must_use]
    pub fn row(&self, position: usize) -> Option<&Row> {
        self.rows.get(*self.shown.get(position)?)
    }

    /// Re-ranks the results for the current query.
    ///
    /// An empty query shows everything in registry order, which is the order the
    /// keyboard overlay uses — so opening the palette and pressing nothing is the
    /// same list a person has already seen, rather than an alphabetical one they
    /// have not.
    pub fn refilter(&mut self) {
        let query = self.query.text.clone();
        let mut hits: Vec<(i32, usize)> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| sk_cmd::fuzzy::score(&query, &row.name).map(|s| (s, index)))
            .collect();
        // Registry order breaks a tie, so two commands scoring the same appear in
        // the order everything else in the program lists them.
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.shown = hits.into_iter().map(|(_, index)| index).collect();
        self.active = 0;
        self.scroll = 0;
        self.settle_active(true);
    }

    /// Moves the highlight, skipping what cannot run and wrapping at both ends.
    ///
    /// Skipping for the same reason the menu skips: a highlight that lands on a
    /// dimmed row is a key press that looks like the list is stuck, and Undo is
    /// dimmed for the whole first minute a person uses the program.
    pub fn step(&mut self, forward: bool) {
        let count = self.shown.len();
        if count == 0 {
            return;
        }
        let mut index = self.active.min(count - 1);
        for _ in 0..count {
            index = if forward { (index + 1) % count } else { (index + count - 1) % count };
            if self.row(index).is_some_and(|row| row.enabled) {
                self.active = index;
                self.settle_scroll();
                return;
            }
        }
    }

    /// Puts the highlight on a result the pointer found.
    ///
    /// Returns whether anything changed, because a frame is only worth drawing if
    /// it does. A dimmed row does not take the highlight, so the pointer and the
    /// arrows agree about what is selectable.
    pub fn hover(&mut self, position: Option<usize>) -> bool {
        let Some(position) = position.filter(|at| self.row(*at).is_some_and(|row| row.enabled))
        else {
            return false;
        };
        let changed = self.active != position;
        self.active = position;
        changed
    }

    /// The command the highlighted result runs, if it can run.
    #[must_use]
    pub fn active_id(&self) -> Option<&'static str> {
        self.row(self.active).filter(|row| row.enabled).map(|row| row.id)
    }

    /// Moves the highlight off a dimmed row, in `forward`'s direction.
    fn settle_active(&mut self, forward: bool) {
        if self.row(self.active).is_some_and(|row| row.enabled) {
            return;
        }
        // `step` from here lands on the first enabled row in that direction, and
        // leaves the highlight alone when there is none at all.
        if self.active == 0 && forward {
            self.active = self.shown.len().saturating_sub(1);
        }
        self.step(forward);
    }

    /// Keeps the highlight inside the drawn window.
    fn settle_scroll(&mut self) {
        if self.active < self.scroll {
            self.scroll = self.active;
        } else if self.active >= self.scroll + VISIBLE {
            self.scroll = self.active + 1 - VISIBLE;
        }
    }

    /// Scrolls the result list, in wheel units.
    ///
    /// A pointer has to be able to reach the fiftieth result without typing:
    /// [rule 5](../../../docs/03-principles.md) is that the pointer and the
    /// keyboard are peers, and the arrows can already walk the whole list.
    pub fn wheel(&mut self, delta: f32) {
        let rows = (delta / WHEEL).round();
        let steps = rows.abs() as usize;
        let last = self.shown.len().saturating_sub(VISIBLE);
        self.scroll = if rows > 0.0 {
            (self.scroll + steps.max(1)).min(last)
        } else if rows < 0.0 {
            self.scroll.saturating_sub(steps.max(1))
        } else {
            self.scroll
        };
        // The highlight follows the window rather than staying on a row nobody
        // can see: `Enter` after scrolling should run something on screen.
        self.active = self.active.clamp(self.scroll, self.scroll + VISIBLE - 1);
        self.settle_active(true);
    }

    /// The results that fit, and where the first of them sits in `shown`.
    #[must_use]
    pub fn window(&self) -> (usize, usize) {
        let first = self.scroll.min(self.shown.len().saturating_sub(1));
        (first, (first + VISIBLE).min(self.shown.len()))
    }
}

/// How much wheel movement is one row.
///
/// A notch on a discrete wheel; a touchpad sends fractions of it and the rounding
/// turns those into whole rows, because a list scrolled half a row is a list with
/// a row cut in half at both ends.
const WHEEL: f32 = 30.0;

/// How many results are drawn at once.
///
/// Not "as many as fit": the panel is a fixed size so that the list does not jump
/// as a query narrows, and eleven is what fits the shortest window the golden
/// frames cover at 200%.
pub const VISIBLE: usize = 11;
/// The space above and below the rows.
pub const PAD: f32 = 8.0;
/// One result row.
pub const STEP: f32 = 28.0;
/// The query field's height.
pub const FIELD: f32 = 40.0;
/// The text inset at each side.
pub const INSET: f32 = 14.0;
/// The least space between a name and its chord.
pub const SPACING: f32 = 28.0;
/// How far below the top of the window the panel sits.
pub const TOP: f32 = 64.0;
/// The narrowest and widest the panel may be.
pub const WIDTH: (f32, f32) = (320.0, 620.0);

/// How tall a panel showing `count` results is.
#[must_use]
pub fn height(count: usize) -> f32 {
    FIELD + PAD * 2.0 + count as f32 * STEP
}

/// The top of each drawn row inside a placed panel.
///
/// One function, read by the draw and by the hit test, because two copies of this
/// arithmetic is how a click runs the command below the one the pointer was on.
pub fn row_tops(count: usize, panel: crate::layout::Area) -> impl Iterator<Item = f32> {
    let first = panel.y + FIELD + PAD;
    (0..count).map(move |index| first + index as f32 * STEP)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Palette, Row, VISIBLE, open};
    use crate::field::Field;

    fn palette(names: &[(&str, bool)]) -> Palette {
        let rows = names
            .iter()
            .map(|(name, enabled)| Row {
                id: "file.copy",
                name: (*name).to_owned(),
                keys: String::new(),
                enabled: *enabled,
            })
            .collect();
        let mut palette = Palette {
            query: Field::new(String::new()),
            rows,
            shown: Vec::new(),
            active: 0,
            scroll: 0,
        };
        palette.refilter();
        palette
    }

    fn shown(palette: &Palette) -> Vec<&str> {
        (0..palette.shown.len()).filter_map(|at| palette.row(at)).map(|row| &*row.name).collect()
    }

    #[test]
    fn an_empty_query_shows_everything_in_registry_order() {
        let palette = palette(&[("New folder", true), ("Copy", true), ("Undo", true)]);
        assert_eq!(shown(&palette), vec!["New folder", "Copy", "Undo"]);
    }

    #[test]
    fn typing_narrows_and_ranks() {
        let mut palette =
            palette(&[("Undo the last change", true), ("New folder", true), ("Copy", true)]);
        palette.query = Field::new("nf".to_owned());
        palette.refilter();
        assert_eq!(shown(&palette), vec!["New folder"]);
    }

    /// A command that cannot run right now is still an answer to "can this
    /// program do X", so it is shown and dimmed rather than hidden.
    #[test]
    fn a_command_that_cannot_run_is_still_listed() {
        let palette = palette(&[("Undo", false), ("Copy", true)]);
        assert_eq!(shown(&palette), vec!["Undo", "Copy"]);
    }

    /// And the highlight starts on something that can run, rather than on a row
    /// where Enter would do nothing.
    #[test]
    fn the_highlight_starts_on_a_row_that_can_run() {
        let palette = palette(&[("Undo", false), ("Redo", false), ("Copy", true)]);
        assert_eq!(palette.active, 2);
        assert!(palette.active_id().is_some());
    }

    #[test]
    fn the_arrows_skip_what_cannot_run_and_wrap() {
        let mut palette = palette(&[("Copy", true), ("Undo", false), ("Paste", true)]);
        assert_eq!(palette.active, 0);
        palette.step(true);
        assert_eq!(palette.active, 2, "Undo was not skipped");
        palette.step(true);
        assert_eq!(palette.active, 0, "it did not wrap");
        palette.step(false);
        assert_eq!(palette.active, 2, "it did not wrap backwards");
    }

    #[test]
    fn nothing_matches_and_nothing_breaks() {
        let mut palette = palette(&[("Copy", true)]);
        palette.query = Field::new("zzzz".to_owned());
        palette.refilter();
        assert!(shown(&palette).is_empty());
        assert_eq!(palette.active_id(), None);
        palette.step(true);
        palette.step(false);
        assert_eq!(palette.window(), (0, 0));
    }

    /// Every row disabled is a real state — a palette opened over a listing that
    /// has not loaded yet — and the arrows have to leave the highlight alone
    /// rather than spin.
    #[test]
    fn every_row_dimmed_leaves_the_highlight_where_it_is() {
        let mut palette = palette(&[("Undo", false), ("Redo", false)]);
        let before = palette.active;
        palette.step(true);
        assert_eq!(palette.active, before);
        assert_eq!(palette.active_id(), None);
    }

    #[test]
    fn the_window_follows_the_highlight_down_a_long_list() {
        let names: Vec<(String, bool)> =
            (0..40).map(|index| (format!("command {index}"), true)).collect();
        let borrowed: Vec<(&str, bool)> =
            names.iter().map(|(name, enabled)| (name.as_str(), *enabled)).collect();
        let mut palette = palette(&borrowed);
        assert_eq!(palette.window(), (0, VISIBLE));
        for _ in 0..VISIBLE {
            palette.step(true);
        }
        let (first, last) = palette.window();
        assert!(palette.active >= first && palette.active < last, "{first}..{last}");
        assert_eq!(last - first, VISIBLE);
    }

    /// A pointer with no keyboard has to be able to reach the fiftieth result.
    #[test]
    fn the_wheel_walks_the_whole_list_and_stops_at_both_ends() {
        let names: Vec<(String, bool)> =
            (0..40).map(|index| (format!("command {index}"), true)).collect();
        let borrowed: Vec<(&str, bool)> =
            names.iter().map(|(name, enabled)| (name.as_str(), *enabled)).collect();
        let mut palette = palette(&borrowed);
        palette.wheel(-120.0);
        assert_eq!(palette.scroll, 0, "it scrolled above the first result");
        for _ in 0..40 {
            palette.wheel(120.0);
        }
        assert_eq!(palette.scroll, 40 - VISIBLE, "it scrolled past the last result");
        let (first, last) = palette.window();
        assert!(palette.active >= first && palette.active < last, "the highlight left the window");
    }

    /// Fewer results than fit is the common case once a query is typed, and the
    /// wheel must not move a list that has nowhere to go.
    #[test]
    fn the_wheel_does_nothing_to_a_list_that_fits() {
        let mut palette = palette(&[("Copy", true), ("Paste", true)]);
        palette.wheel(600.0);
        assert_eq!(palette.scroll, 0);
        assert_eq!(palette.window(), (0, 2));
        assert!(palette.active_id().is_some());
    }

    /// The palette is built from the registry rather than from a table beside it,
    /// which is the whole of rule 2 — so opening it over the real registry has to
    /// produce every command the registry holds.
    #[test]
    fn opening_over_the_real_registry_lists_every_command() {
        let registry = crate::commands::registry();
        let palette = open(&registry, |_| true);
        // Every one of them, because an empty query shows everything: the palette
        // is a view of the registry and not a table beside it, which is the whole
        // of rule 2.
        assert_eq!(palette.shown.len(), registry.len());
        assert!(palette.shown.len() > 30, "the registry has {} commands", registry.len());
    }
}
