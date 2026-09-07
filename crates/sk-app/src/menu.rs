// SPDX-License-Identifier: Apache-2.0
//! The context menu: the pointer's way into the command registry.
//!
//! **Rule 2 from the other end.** The keyboard reaches every command through the
//! registry's chords; this is how a pointer reaches the ones that are not a
//! button — and it reaches them through the same registry, so a menu row cannot
//! run something the palette will not find or describe a key that is not bound.
//!
//! The tables below are the contract
//! `every_command_can_be_reached_without_a_keyboard` checks: a command that is
//! neither a button, nor a column header, nor on one of these menus, nor in the
//! exemption table with a reason, fails the build.

use sk_cmd::Registry;
use sk_plat::{Point, Size};

use crate::layout::Area;

/// What a secondary click on a row offers.
///
/// Grouped, and the groups are the separators: the ways of seeing the file
/// first — Open because it is what a double click already does, and the quick
/// look because it is the one that does not leave — then the two clipboard
/// entries together, and the two that change the file last, where a slip costs
/// the most and the pointer is least likely to be.
pub const ON_ROW: &[&[&str]] = &[
    &[
        "go.open",
        "go.open_new_tab",
        "go.open_beside",
        "go.open_below",
        "file.open_with",
        "view.quick_look",
    ],
    &["file.copy", "file.cut", "file.paste"],
    &["file.copy_path", "file.copy_content"],
    &["file.extract"],
    &["file.duplicate", "file.rename", "file.trash", "file.delete"],
    // **Pin, on a row, is about the row.** A folder somebody has found is the
    // commonest thing they want on the list of places, and reaching it used to
    // mean entering the folder first — the command's target is the entry the menu
    // opened over, so this row and the background's mean two different folders
    // and both read as "this folder".
    &["place.pin", "file.properties"],
    // **The colours, last and in spectrum order.** A row of seven is what Finder
    // and Drive both put on this menu, and each one draws its own swatch — a list
    // of colour names with no colours on it is a menu a person has to try rather
    // than read. Dimmed on anything that is not a folder, because that is what
    // they act on.
    &[
        "view.colour_red",
        "view.colour_orange",
        "view.colour_yellow",
        "view.colour_green",
        "view.colour_blue",
        "view.colour_purple",
        "view.colour_none",
    ],
];

/// What a secondary click inside a text preview offers.
///
/// **Two rows, and they are the pointer's way to the look's own scope.** `Ctrl+A`
/// and `Ctrl+C` mean the text in the panel rather than the files behind it —
/// [`sk_cmd::Scope::Look`] — and [rule
/// 5](../../../docs/03-principles.md) wants both of them reachable without a
/// keyboard. A menu is what every text view offers for exactly this, and it costs
/// no new furniture: the rows come out of the same registry the chords do.
pub const IN_A_LOOK: &[&[&str]] = &[
    &["look.select_all", "look.copy"],
    &["look.edit", "look.save", "look.discard"],
    &["look.play", "look.find", "look.source"],
];

/// What a secondary click on a sidebar row offers.
///
/// **Three rows, and all three are about that row.** The background menu's
/// entries are about the directory the list is showing, so offering them over a
/// place would make New folder mean somewhere else — and the place's own two
/// verbs have nowhere else to live. Which row it is comes from
/// [`crate::app::App`]'s own target rather than from a command argument, because
/// a command runs against `&mut App` and takes none:
/// [ADR 0021](../../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md).
///
/// Pin is deliberately absent: every row of the sidebar is already a place, so
/// the row would be dimmed every time the menu opened.
pub const ON_A_PLACE: &[&[&str]] = &[
    &["place.expand"],
    &["place.unpin", "place.raise", "place.lower"],
    // **The three a disk has** — [ADR
    // 0030](../../../docs/adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).
    // Dimmed over a row they mean nothing for, which is every place that is not
    // a volume, rather than absent: a menu that changes shape by row is a menu
    // whose rows move under a hand.
    &["volume.mount", "volume.unmount", "volume.eject"],
];

/// What a secondary click on anything else offers.
pub const ON_BACKGROUND: &[&[&str]] = &[
    &["file.new_folder", "file.paste"],
    &["edit.undo", "edit.redo", "edit.pause", "edit.cancel", "edit.resume"],
    &[
        "select.all",
        "view.filter",
        "go.search",
        "go.search_everywhere",
        "index.build",
        "index.build_all",
        "index.forget",
    ],
    &[
        "view.details",
        "view.grid",
        "view.columns",
        "view.group_none",
        "view.group_time",
        "view.group_size",
        "view.group_kind",
        "view.bands_show",
    ],
    &["view.columns_reset", "view.hidden", "view.theme", "view.sidebar"],
    &["tab.new", "tab.close", "tab.reopen"],
    &["pane.split_beside", "pane.split_below", "pane.close"],
    &["place.pin", "file.terminal", "view.path_entry", "file.paste_location", "go.reload"],
    &["view.palette", "help.keys", "settings.open"],
];

/// One row of an open menu.
pub struct Item {
    /// The command it runs.
    pub id: &'static str,
    /// What it is called, through the catalogue.
    pub name: String,
    /// Its chord, so the menu teaches the keyboard — rule 4.
    pub keys: String,
    /// Whether it can run right now. A row that cannot is dimmed and not
    /// removed, so the menu is the same shape every time it opens.
    pub enabled: bool,
    /// Whether a divider goes above this row.
    pub starts_group: bool,
}

/// An open menu.
pub struct Menu {
    /// Where the pointer was, or where the cursor row is when a key opened it.
    pub at: Point,
    /// The rows, in order.
    pub items: Vec<Item>,
    /// The row the keyboard or the pointer is on.
    pub active: Option<usize>,
    /// The first row drawn, for a menu taller than the window it is in.
    pub scroll: usize,
}

/// Builds a menu from a table of command identifiers.
///
/// An identifier the registry does not have is skipped rather than shown broken;
/// a test asserts that never happens, which is the difference between a typo
/// failing the build and a typo becoming an empty row.
///
/// `enabled` is passed rather than the context a command runs against, because
/// the two callers have different ones: the program asks the registry about its
/// `App`, and a capture has no `App` and states the answers it wants a golden
/// frame of.
#[must_use]
pub fn build<C>(
    groups: &[&[&str]],
    registry: &Registry<C>,
    at: Point,
    enabled: impl Fn(&'static str) -> bool,
) -> Option<Menu> {
    let mut items: Vec<Item> = Vec::new();
    for group in groups {
        let mut first = true;
        for id in *group {
            let Some(command) = registry.find(id) else { continue };
            let Some(described) = registry.describe(command) else { continue };
            items.push(Item {
                id: described.id,
                name: described.name.to_owned(),
                keys: described.keys.clone(),
                enabled: enabled(described.id),
                starts_group: first && !items.is_empty(),
            });
            first = false;
        }
    }
    (!items.is_empty()).then_some(Menu { at, items, active: None, scroll: 0 })
}

impl Menu {
    /// Moves the highlight, skipping rows that cannot run and wrapping at both
    /// ends.
    ///
    /// Skipping rather than stopping: a dimmed row the arrows land on is a key
    /// press that looks like the menu is stuck, and Undo is dimmed for the whole
    /// first minute a person uses the program.
    pub fn step(&mut self, forward: bool) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        let mut index = match self.active {
            Some(active) => active,
            // One step from beyond the end lands on the first row, and from
            // beyond the start on the last.
            None if forward => count - 1,
            None => 0,
        };
        for _ in 0..count {
            index = if forward { (index + 1) % count } else { (index + count - 1) % count };
            if self.items[index].enabled {
                self.active = Some(index);
                return;
            }
        }
    }

    /// Puts the highlight on a row the pointer found, or takes it off.
    ///
    /// Returns whether anything changed, because a frame is only worth drawing
    /// if it does.
    pub fn hover(&mut self, index: Option<usize>) -> bool {
        let wanted = index.filter(|row| self.items.get(*row).is_some_and(|item| item.enabled));
        let changed = self.active != wanted;
        self.active = wanted;
        changed
    }

    /// The rows that fit in `room` starting at the scroll, and the first's index.
    ///
    /// **A menu can be taller than the window.** The background menu is the
    /// pointer's way to every command that is not a button, so it grows with the
    /// registry — and at 900×520 it is already taller than the window it opens
    /// in. `place` flips it rather than clipping it, which cannot help when
    /// there is no side it fits on.
    #[must_use]
    pub fn window(&self, room: f32) -> (usize, &[Item]) {
        let first = self.scroll.min(self.items.len().saturating_sub(1));
        let mut used = PAD * 2.0;
        let mut last = first;
        for (offset, item) in self.items[first..].iter().enumerate() {
            // The divider above the first drawn row would be a line under
            // nothing.
            let step = STEP + if item.starts_group && offset > 0 { DIVIDER } else { 0.0 };
            if used + step > room && last > first {
                break;
            }
            used += step;
            last += 1;
        }
        (first, &self.items[first..last.min(self.items.len())])
    }

    /// Puts the highlighted row inside the window of `room`.
    pub fn reveal(&mut self, room: f32) {
        let Some(active) = self.active else { return };
        if active < self.scroll {
            self.scroll = active;
            return;
        }
        // Walk the scroll forward until the active row is one of the drawn ones,
        // which is one step per row rather than arithmetic over a mix of row
        // heights and divider heights.
        while !self.window(room).1.is_empty() {
            let (first, drawn) = self.window(room);
            if active < first + drawn.len() {
                return;
            }
            self.scroll += 1;
        }
    }

    /// Scrolls by a wheel notch. Whether anything moved.
    pub fn wheel(&mut self, delta: f32, room: f32) -> bool {
        let was = self.scroll;
        // **The same direction the list scrolls.** A positive delta is the wheel
        // turned down and it brings *later* rows into view, here as everywhere
        // else. These two arms were the other way round, so a tall menu answered
        // the wheel backwards — and the scenario that covered it was written
        // against the wrong direction, which is why nobody noticed.
        if delta > 0.0 {
            let (first, drawn) = self.window(room);
            if first + drawn.len() < self.items.len() {
                self.scroll += 1;
            }
        } else if delta < 0.0 {
            self.scroll = self.scroll.saturating_sub(1);
        }
        self.scroll != was
    }

    /// The command the highlighted row runs.
    #[must_use]
    pub fn active_id(&self) -> Option<&'static str> {
        self.items.get(self.active?).map(|item| item.id)
    }
}

/// How close to the window's edge a menu may come.
pub const MARGIN: f32 = 4.0;
/// The vertical space around the rows.
pub const PAD: f32 = 6.0;
/// One row.
pub const STEP: f32 = 26.0;
/// The extra height a divider costs.
pub const DIVIDER: f32 = 9.0;
/// The text inset at each side.
pub const INSET: f32 = 12.0;
/// The least space between a name and its chord.
pub const SPACING: f32 = 28.0;
/// The narrowest and widest a menu may be.
pub const WIDTH: (f32, f32) = (200.0, 460.0);

/// How tall a menu showing exactly these rows is.
///
/// The first row's divider does not count: a line above the top row is a line
/// under nothing, which is what a scrolled menu would otherwise draw.
#[must_use]
pub fn height(items: &[Item]) -> f32 {
    let dividers = items.iter().skip(1).filter(|item| item.starts_group).count();
    PAD * 2.0 + items.len() as f32 * STEP + dividers as f32 * DIVIDER
}

/// Where the panel goes, given the size the view measured for it.
///
/// **Flipped, never clipped.** A menu whose bottom rows are off the window is a
/// menu whose last two entries do not exist, and the last two entries of the row
/// menu are Rename and Move to Trash. The flip puts the corner the pointer is at
/// on the other side of the pointer, which is what every menu on the desktop
/// does and therefore the only behaviour that will not surprise.
#[must_use]
pub fn place(at: Point, width: f32, height: f32, window: Size) -> Area {
    let margin = MARGIN;
    let mut x = at.x;
    if x + width > window.width - margin {
        x = at.x - width;
    }
    let mut y = at.y;
    if y + height > window.height - margin {
        y = at.y - height;
    }
    Area {
        x: x.max(margin).min((window.width - width - margin).max(margin)),
        y: y.max(margin).min((window.height - height - margin).max(margin)),
        width,
        height,
    }
}

/// The top of each row inside a placed panel, in the order of [`Menu::items`].
///
/// One function, read by the draw and by the hit test, because two copies of
/// this arithmetic is how a click lands on Move to Trash when the pointer was on
/// Rename.
pub fn row_tops(items: &[Item], panel: Area) -> impl Iterator<Item = f32> + '_ {
    let mut y = panel.y + PAD;
    items.iter().enumerate().map(move |(index, item)| {
        if item.starts_group && index > 0 {
            y += DIVIDER;
        }
        let top = y;
        y += STEP;
        top
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Item, Menu, height, place, row_tops};
    use sk_plat::{Point, Size};

    fn menu(enabled: &[bool]) -> Menu {
        let items = enabled
            .iter()
            .enumerate()
            .map(|(index, enabled)| Item {
                id: "file.copy",
                name: format!("row {index}"),
                keys: String::new(),
                enabled: *enabled,
                starts_group: false,
            })
            .collect();
        Menu { at: Point::new(0.0, 0.0), items, active: None, scroll: 0 }
    }

    /// Undo is dimmed for the whole first minute a person uses the program, and
    /// an arrow key that lands on it looks like the menu is stuck.
    #[test]
    fn the_arrows_skip_what_cannot_run_and_wrap_at_both_ends() {
        let mut menu = menu(&[true, false, false, true]);
        menu.step(true);
        assert_eq!(menu.active, Some(0));
        menu.step(true);
        assert_eq!(menu.active, Some(3), "two dimmed rows were not skipped");
        menu.step(true);
        assert_eq!(menu.active, Some(0), "it did not wrap");
        menu.step(false);
        assert_eq!(menu.active, Some(3), "backwards did not wrap");
    }

    #[test]
    fn a_menu_with_nothing_enabled_never_highlights_a_row() {
        let mut menu = menu(&[false, false]);
        menu.step(true);
        assert_eq!(menu.active, None);
        assert!(!menu.hover(Some(1)), "a dimmed row took the highlight");
        assert_eq!(menu.active_id(), None);
    }

    #[test]
    fn the_pointer_and_the_keyboard_share_one_highlight() {
        let mut menu = menu(&[true, true]);
        assert!(menu.hover(Some(1)));
        assert_eq!(menu.active_id(), Some("file.copy"));
        assert!(!menu.hover(Some(1)), "the same row twice asked for a frame");
        assert!(menu.hover(None));
        assert_eq!(menu.active, None);
    }

    /// The last two entries of the row menu are Rename and Move to Trash, and a
    /// menu opened near the bottom edge used to put them off the window.
    #[test]
    fn a_menu_near_an_edge_flips_instead_of_falling_off_the_window() {
        let window = Size::new(800.0, 600.0);
        let near = place(Point::new(780.0, 580.0), 240.0, 200.0, window);
        assert!(near.x < 780.0 && near.right() <= 800.0, "{near:?}");
        assert!(near.y < 580.0 && near.bottom() <= 600.0, "{near:?}");

        let room = place(Point::new(100.0, 100.0), 240.0, 200.0, window);
        assert!((room.x - 100.0).abs() < f32::EPSILON, "{room:?}");
        assert!((room.y - 100.0).abs() < f32::EPSILON, "{room:?}");
    }

    /// A menu taller than the window has nowhere to flip to, and the arithmetic
    /// that clamps it must not put it off the top instead.
    #[test]
    fn a_menu_taller_than_the_window_still_starts_on_screen() {
        let window = Size::new(320.0, 200.0);
        let area = place(Point::new(10.0, 190.0), 480.0, 400.0, window);
        assert!(area.x >= 4.0 && area.y >= 4.0, "{area:?}");
    }

    #[test]
    fn a_divider_moves_every_row_below_it_down_by_its_own_height() {
        let mut menu = menu(&[true, true, true]);
        menu.items[1].starts_group = true;
        let panel =
            place(Point::new(0.0, 0.0), 240.0, height(&menu.items), Size::new(800.0, 600.0));
        let tops: Vec<f32> = row_tops(&menu.items, panel).collect();
        assert_eq!(tops.len(), 3);
        assert!((tops[1] - tops[0] - super::STEP - super::DIVIDER).abs() < 0.01, "{tops:?}");
        assert!((tops[2] - tops[1] - super::STEP).abs() < 0.01, "{tops:?}");
        // The panel is exactly as tall as the rows it holds, plus its padding.
        assert!((panel.bottom() - super::PAD - tops[2] - super::STEP).abs() < 0.01, "{tops:?}");
    }

    /// **A menu answers the wheel the way the list does.** A positive delta is the
    /// wheel turned down and it shows what is further along; the two arms were
    /// swapped, and a menu taller than the window scrolled backwards.
    #[test]
    fn the_wheel_scrolls_a_menu_the_way_it_scrolls_a_list() {
        let mut tall = menu(&[true; 20]);
        // Room for a handful of its rows, so there is somewhere to scroll to.
        let room = 120.0;
        assert_eq!(tall.scroll, 0);
        assert!(tall.wheel(66.0, room), "a wheel down moved nothing");
        assert_eq!(tall.scroll, 1, "a wheel down shows what is further along");
        assert!(tall.wheel(-66.0, room));
        assert_eq!(tall.scroll, 0, "and a wheel up comes back");
        assert!(!tall.wheel(-66.0, room), "the top clamps rather than wrapping");
    }
}
