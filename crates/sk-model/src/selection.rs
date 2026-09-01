//! One selection, with the same semantics in every view — rule 8.
//!
//! Selection is over [`EntryId`], not over display rows, so re-sorting or
//! toggling hidden files keeps what the user had selected. Range operations take
//! the rows from the caller, because "everything between here and there" is a
//! question about what is on screen and only the view knows that.

use crate::bitset::BitSet;

use crate::listing::EntryId;

/// What is selected, plus the two positions a keyboard needs.
#[derive(Clone, Default, Debug)]
pub struct Selection {
    bits: BitSet,
    /// The entry a range extends from. Set by a plain click or an arrow key,
    /// kept by a shift-click, which is what makes shift-click-then-shift-click
    /// grow one range instead of two.
    anchor: Option<EntryId>,
    /// The entry the focus ring is on. Distinct from the selection because
    /// `Ctrl`+arrow moves it without changing what is selected.
    cursor: Option<EntryId>,
}

impl Selection {
    /// Resizes to a listing of `len` entries, clearing anything out of range.
    ///
    /// Called when a new listing is published. A cursor that no longer exists is
    /// dropped rather than clamped, because clamping silently moves the focus
    /// ring to an unrelated file.
    pub fn fit(&mut self, len: usize) {
        self.bits.resize(len);
        self.anchor = self.anchor.filter(|id| id.index() < len);
        self.cursor = self.cursor.filter(|id| id.index() < len);
    }

    /// Whether `id` is selected.
    #[must_use]
    pub fn contains(&self, id: EntryId) -> bool {
        self.bits.contains(id.index())
    }

    /// How many entries are selected.
    #[must_use]
    pub fn count(&self) -> usize {
        self.bits.count()
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bits.count() == 0
    }

    /// Every selected entry, in **`EntryId` order** — the order the directory
    /// was read in, which is nobody's idea of an order.
    ///
    /// For anything a person sees or receives, use [`Selection::in_order`]. This
    /// one is for counting and for set arithmetic.
    pub fn iter(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.bits.iter().map(|index| EntryId(index as u32))
    }

    /// Every selected entry, in the order it appears on screen.
    ///
    /// `rows` is the display order. Sorting by size and copying used to hand the
    /// peer `getdents64` order, so the paths arrived shuffled with respect to the
    /// list they were taken from — and nothing said so.
    pub fn in_order<'a>(&'a self, rows: &'a [EntryId]) -> impl Iterator<Item = EntryId> + 'a {
        rows.iter().copied().filter(|id| self.contains(*id))
    }

    /// Adds `id` without moving the anchor or the cursor.
    ///
    /// For restoring a selection across a re-read, where the anchor and the
    /// cursor are settled separately and by name.
    pub fn select(&mut self, id: EntryId) {
        self.bits.insert(id.index());
    }

    /// The entry the focus ring is on.
    #[must_use]
    pub fn cursor(&self) -> Option<EntryId> {
        self.cursor
    }

    /// Moves the focus ring without changing the selection.
    pub fn set_cursor(&mut self, id: Option<EntryId>) {
        self.cursor = id;
        self.anchor = id.or(self.anchor);
    }

    /// Selects only `id`, and makes it the anchor and the cursor.
    pub fn replace_with(&mut self, id: EntryId) {
        self.bits.clear();
        self.bits.insert(id.index());
        self.anchor = Some(id);
        self.cursor = Some(id);
    }

    /// Adds or removes `id`, moving the anchor and the cursor to it.
    pub fn toggle(&mut self, id: EntryId) {
        self.bits.toggle(id.index());
        self.anchor = Some(id);
        self.cursor = Some(id);
    }

    /// Takes `id` out of the selection, leaving the anchor and the cursor alone.
    ///
    /// **Not [`Self::toggle`], which moves both.** This is for entries that stopped
    /// being visible rather than entries somebody chose — a collapsed band's, whose
    /// rows have to leave a selection they can no longer be seen in. Moving the
    /// cursor onto one of them would be putting it where nothing is drawn.
    pub fn deselect(&mut self, id: EntryId) {
        self.bits.remove(id.index());
    }

    /// Selects everything between the anchor and `id`, given the display order.
    ///
    /// `rows` maps a display position to an entry. With no anchor this behaves
    /// as a plain click, which is what a shift-click into an empty selection
    /// should do.
    pub fn extend_to(&mut self, id: EntryId, rows: &[EntryId]) {
        let Some(anchor) = self.anchor else {
            self.replace_with(id);
            return;
        };
        let (Some(from), Some(to)) = (position(rows, anchor), position(rows, id)) else {
            self.replace_with(id);
            return;
        };
        self.bits.clear();
        for row in from.min(to)..=from.max(to) {
            if let Some(entry) = rows.get(row) {
                self.bits.insert(entry.index());
            }
        }
        self.cursor = Some(id);
    }

    /// Selects everything the view is showing.
    ///
    /// `rows` is the display order, and it is not optional: filling the whole
    /// bitset instead selects entries the listing holds but does not show — with
    /// hidden files off, `Ctrl+A` in a folder of nine visible files reported ten
    /// selected, and the tenth was a dotfile nobody could see. A selection that
    /// contains something invisible is a selection that a later operation acts
    /// on by surprise.
    pub fn select_all(&mut self, rows: &[EntryId]) {
        self.bits.clear();
        for id in rows {
            self.bits.insert(id.index());
        }
        self.anchor = rows.first().copied();
    }

    /// Selects nothing, keeping the cursor where it is.
    pub fn clear(&mut self) {
        self.bits.clear();
        self.anchor = None;
    }
}

fn position(rows: &[EntryId], id: EntryId) -> Option<usize> {
    rows.iter().position(|row| *row == id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Selection;
    use crate::listing::EntryId;

    fn rows(count: u32) -> Vec<EntryId> {
        (0..count).map(EntryId).collect()
    }

    #[test]
    fn a_click_replaces_and_a_ctrl_click_accumulates() {
        let mut selection = Selection::default();
        selection.fit(10);
        selection.replace_with(EntryId(3));
        assert_eq!(selection.count(), 1);
        selection.toggle(EntryId(5));
        assert_eq!(selection.count(), 2);
        selection.toggle(EntryId(5));
        assert_eq!(selection.count(), 1);
        assert_eq!(selection.cursor(), Some(EntryId(5)));
    }

    #[test]
    fn a_shift_click_grows_one_range_rather_than_two() {
        let mut selection = Selection::default();
        selection.fit(10);
        let rows = rows(10);
        selection.replace_with(EntryId(2));
        selection.extend_to(EntryId(5), &rows);
        assert_eq!(selection.iter().collect::<Vec<_>>(), (2..=5).map(EntryId).collect::<Vec<_>>());
        selection.extend_to(EntryId(7), &rows);
        assert_eq!(selection.iter().collect::<Vec<_>>(), (2..=7).map(EntryId).collect::<Vec<_>>());
        selection.extend_to(EntryId(0), &rows);
        assert_eq!(selection.iter().collect::<Vec<_>>(), (0..=2).map(EntryId).collect::<Vec<_>>());
    }

    #[test]
    fn a_shift_click_with_no_anchor_behaves_as_a_click() {
        let mut selection = Selection::default();
        selection.fit(10);
        selection.extend_to(EntryId(4), &rows(10));
        assert_eq!(selection.count(), 1);
        assert!(selection.contains(EntryId(4)));
    }

    #[test]
    fn select_all_then_deselect_is_the_operation_the_bitset_exists_for() {
        let mut selection = Selection::default();
        selection.fit(100_000);
        let rows = rows(100_000);
        selection.select_all(&rows);
        for index in 0..200 {
            selection.toggle(EntryId(index));
        }
        assert_eq!(selection.count(), 100_000 - 200);
    }

    /// The defect a test driving the real window found: with hidden files off,
    /// `Ctrl+A` reported one more selected than the list was showing.
    #[test]
    fn select_all_selects_what_is_shown_and_not_what_is_hidden() {
        let mut selection = Selection::default();
        selection.fit(10);
        // Nine of ten entries are in the display order; the tenth is a dotfile.
        let shown: Vec<EntryId> = (0..9).map(EntryId).collect();
        selection.select_all(&shown);
        assert_eq!(selection.count(), 9);
        assert!(!selection.contains(EntryId(9)), "something invisible was selected");
    }

    #[test]
    fn refitting_to_a_shorter_listing_drops_a_cursor_that_no_longer_exists() {
        let mut selection = Selection::default();
        selection.fit(10);
        selection.replace_with(EntryId(9));
        selection.fit(5);
        assert_eq!(selection.cursor(), None, "not clamped to an unrelated file");
        assert_eq!(selection.count(), 0);
    }
}
