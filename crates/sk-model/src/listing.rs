//! One directory's worth of entries, stored as parallel arrays.
//!
//! A directory entry is not a struct in a `Vec`. This layout is what makes
//! sorting and filtering a million entries a memory-bandwidth problem instead of
//! a pointer-chasing one, and it is the most important decision in the project
//! after "do not block the UI thread" —
//! [docs/04-architecture.md](../../../docs/04-architecture.md#data-layout).
//!
//! The listing is also the unit of the versioned snapshot the UI thread reads:
//! it is built on a job, published whole, and never mutated while a frame is
//! reading it.

use crate::arena::{StrArena, StrRef};

use crate::collate::natural_cmp;
use crate::sort::{Sort, SortColumn};

/// An index into one listing's parallel arrays.
///
/// Stable for the life of the listing, and meaningless outside it — the same
/// reasoning as [`StrRef`]. A `u32` because a directory with four billion
/// entries is not a case, and halving the index width halves the permutation
/// that a sort walks.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EntryId(pub u32);

impl EntryId {
    /// The index into the parallel arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// What is known about an entry without reading its contents.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Flags(u16);

impl Flags {
    /// Nothing known.
    pub const NONE: Self = Self(0);
    /// A directory, or a symlink that resolves to one.
    pub const DIR: Self = Self(1 << 0);
    /// A symbolic link, whatever it points at.
    pub const SYMLINK: Self = Self(1 << 1);
    /// A name beginning with a dot.
    pub const HIDDEN: Self = Self(1 << 2);
    /// Executable by somebody.
    pub const EXEC: Self = Self(1 << 3);
    /// A symlink whose target could not be resolved.
    pub const BROKEN: Self = Self(1 << 4);
    /// Neither a file nor a directory: a socket, fifo, or device node.
    pub const SPECIAL: Self = Self(1 << 5);
    /// `statx` has not landed yet, so size and mtime are not yet real.
    ///
    /// The view draws these rows immediately with the name it has, because
    /// [Spike B](../../../spikes/b-enumeration/README.md) measured 200 rows on
    /// screen in **0.143 ms** by deferring exactly this. A row that waits for
    /// its metadata is a row that waits for a disk.
    pub const META_PENDING: Self = Self(1 << 6);

    /// Whether every bit in `other` is set.
    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Both sets of bits.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// This set without the bits in `other`.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl std::ops::BitOr for Flags {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        self.with(other)
    }
}

/// One directory, columnar.
///
/// `Clone` because a re-sort runs on a job and a job cannot borrow the pane's
/// copy: the order is computed from a clone while metadata keeps landing on the
/// original, and the resulting permutation is still valid because an `EntryId`
/// never moves. A 200,000-entry listing is about 10 MB and clones in 2 ms, which
/// is the price of not sorting on the UI thread.
#[derive(Clone, Debug)]
pub struct Listing {
    /// Which listing this is — see [`Listing::version`].
    version: u32,
    names: StrArena,
    name: Vec<StrRef>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<Flags>,
    /// The rows a view walks, in display order.
    order: Vec<EntryId>,
    /// Entries still waiting for `statx`, counted rather than searched for.
    pending: usize,
}

/// Hands out a distinct version to every listing ever built in this process.
///
/// This exists to make one class of bug impossible rather than remembered. An
/// `EntryId` is an index, so entry 3 of one directory and entry 3 of the next are
/// different files with the same id — and anything that caches per entry, which
/// is the whole point of the text layout cache, will serve the first when asked
/// for the second. It happened: navigating into a subdirectory drew two of its
/// four rows with the parent's names and the parent's sizes, and the cache had a
/// doc comment saying exactly when to invalidate it and no caller that did.
///
/// A version in the cache key removes the choice. It wraps after four billion
/// listings, by which point a stale entry would have had to survive four billion
/// of them, which a two-generation cache of a few thousand runs cannot.
static NEXT_VERSION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl Default for Listing {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl Listing {
    /// An empty listing with room for `capacity` entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            version: NEXT_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            // 24 bytes per name is the mean over /usr on this machine; the arena
            // grows if that is wrong and the guess costs one reallocation.
            names: StrArena::with_capacity(capacity * 24),
            name: Vec::with_capacity(capacity),
            size: Vec::with_capacity(capacity),
            mtime: Vec::with_capacity(capacity),
            flags: Vec::with_capacity(capacity),
            order: Vec::with_capacity(capacity),
            pending: 0,
        }
    }

    /// Appends an entry known only by name and directory-entry type.
    ///
    /// This is what `getdents64` gives us for free. `size` and `mtime` arrive
    /// later through [`Listing::set_meta`], and until they do the entry carries
    /// [`Flags::META_PENDING`].
    pub fn push(&mut self, name: &str, flags: Flags) -> EntryId {
        let id = EntryId(self.name.len() as u32);
        let hidden = if name.starts_with('.') { Flags::HIDDEN } else { Flags::NONE };
        self.name.push(self.names.push(name));
        self.size.push(0);
        self.mtime.push(0);
        self.flags.push(flags | hidden | Flags::META_PENDING);
        self.order.push(id);
        self.pending += 1;
        id
    }

    /// Fills in what `statx` returned, clearing [`Flags::META_PENDING`].
    pub fn set_meta(&mut self, id: EntryId, size: u64, mtime: i64, extra: Flags) {
        let index = id.index();
        if index >= self.name.len() {
            return;
        }
        self.size[index] = size;
        self.mtime[index] = mtime;
        if self.flags[index].has(Flags::META_PENDING) {
            self.pending -= 1;
        }
        self.flags[index] = self.flags[index].without(Flags::META_PENDING) | extra;
    }

    /// Which listing this is.
    ///
    /// Distinct for every listing built in this process, and part of the text
    /// layout cache's key — see [`NEXT_VERSION`].
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// How many entries the directory held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.name.len()
    }

    /// Whether the directory held nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// How many entries are still waiting for their metadata.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// The entries a view walks, in display order.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.order.len()
    }

    /// The entry at display position `row`.
    #[must_use]
    pub fn at(&self, row: usize) -> Option<EntryId> {
        self.order.get(row).copied()
    }

    /// Every visible entry, in display order.
    ///
    /// A slice rather than an iterator because range selection needs random
    /// access into it: "everything between the anchor and here" is two positions
    /// in this order and every index between them.
    #[must_use]
    pub fn order(&self) -> &[EntryId] {
        &self.order
    }

    /// The display position of `id`, if it is visible. Linear: for the one place
    /// that needs it — restoring a cursor after a re-sort — over a listing that
    /// is already in cache.
    #[must_use]
    pub fn row_of(&self, id: EntryId) -> Option<usize> {
        self.order.iter().position(|row| *row == id)
    }

    /// The entry's name.
    #[must_use]
    pub fn name(&self, id: EntryId) -> &str {
        self.name.get(id.index()).map_or("", |reference| self.names.get(*reference))
    }

    /// The entry's size in bytes, or 0 while its metadata is pending.
    #[must_use]
    pub fn size(&self, id: EntryId) -> u64 {
        self.size.get(id.index()).copied().unwrap_or(0)
    }

    /// Seconds since the epoch, or 0 while its metadata is pending.
    #[must_use]
    pub fn mtime(&self, id: EntryId) -> i64 {
        self.mtime.get(id.index()).copied().unwrap_or(0)
    }

    /// What is known about the entry.
    #[must_use]
    pub fn flags(&self, id: EntryId) -> Flags {
        self.flags.get(id.index()).copied().unwrap_or(Flags::NONE)
    }

    /// Bytes held by the name arena, which is the listing's dominant cost.
    #[must_use]
    pub fn name_bytes(&self) -> usize {
        self.names.bytes()
    }

    /// Rebuilds the display order for `sort`, keeping or dropping dotfiles.
    ///
    /// A comparison sort over the permutation, with no precomputed key: see
    /// [`crate::collate`] for the counterexample that ruled the key out. The
    /// secondary key is always the name, so every order is total and a listing
    /// cannot reshuffle between two frames.
    pub fn reorder(&mut self, sort: Sort, show_hidden: bool) {
        self.order = self.compute_order(sort, show_hidden);
    }

    /// Replaces the display order with one computed elsewhere.
    ///
    /// Elsewhere means a job. Every id in `order` still names the same entry it
    /// did when the order was computed, because an `EntryId` is an index into
    /// arrays that only ever grow at the end — so a permutation computed while
    /// `statx` results were still landing is still correct when it arrives.
    pub fn set_order(&mut self, order: Vec<EntryId>) {
        self.order = order;
    }

    /// The display order for `sort`, without applying it.
    ///
    /// Takes `&self` so it can run on a job over a clone. This is the function
    /// that costs 272 ms at 200,000 entries, and the reason it is not called from
    /// the frame loop.
    #[must_use]
    pub fn compute_order(&self, sort: Sort, show_hidden: bool) -> Vec<EntryId> {
        let mut order: Vec<EntryId> = (0..self.name.len() as u32)
            .map(EntryId)
            .filter(|id| show_hidden || !self.flags[id.index()].has(Flags::HIDDEN))
            .collect();

        let (names, name_refs, size, mtime, flags) =
            (&self.names, &self.name, &self.size, &self.mtime, &self.flags);
        let name_of = |index: usize| names.get(name_refs[index]);

        order.sort_unstable_by(|a, b| {
            let (a, b) = (a.index(), b.index());
            if sort.dirs_first {
                let (left, right) = (flags[a].has(Flags::DIR), flags[b].has(Flags::DIR));
                if left != right {
                    return right.cmp(&left);
                }
            }
            let primary = match sort.column {
                SortColumn::Name => natural_cmp(name_of(a), name_of(b)),
                SortColumn::Size => {
                    size[a].cmp(&size[b]).then_with(|| natural_cmp(name_of(a), name_of(b)))
                }
                SortColumn::Modified => {
                    mtime[a].cmp(&mtime[b]).then_with(|| natural_cmp(name_of(a), name_of(b)))
                }
            };
            if sort.descending { primary.reverse() } else { primary }
        });
        order
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{EntryId, Flags, Listing};
    use crate::sort::{Sort, SortColumn};

    fn fixture() -> Listing {
        let mut listing = Listing::with_capacity(8);
        for (name, flags) in [
            ("img10.png", Flags::NONE),
            ("src", Flags::DIR),
            ("img2.png", Flags::NONE),
            (".hidden", Flags::NONE),
            ("Makefile", Flags::NONE),
        ] {
            listing.push(name, flags);
        }
        listing
    }

    fn names(listing: &Listing) -> Vec<&str> {
        (0..listing.rows()).filter_map(|row| listing.at(row)).map(|id| listing.name(id)).collect()
    }

    #[test]
    fn a_pushed_entry_is_pending_until_its_metadata_lands() {
        let mut listing = fixture();
        assert_eq!(listing.pending(), 5);
        assert!(listing.flags(EntryId(0)).has(Flags::META_PENDING));
        listing.set_meta(EntryId(0), 4096, 17, Flags::NONE);
        assert_eq!(listing.pending(), 4);
        assert!(!listing.flags(EntryId(0)).has(Flags::META_PENDING));
        assert_eq!(listing.size(EntryId(0)), 4096);
    }

    #[test]
    fn setting_metadata_twice_does_not_underflow_the_pending_count() {
        let mut listing = fixture();
        listing.set_meta(EntryId(1), 1, 1, Flags::NONE);
        listing.set_meta(EntryId(1), 2, 2, Flags::NONE);
        assert_eq!(listing.pending(), 4);
        assert_eq!(listing.size(EntryId(1)), 2);
    }

    #[test]
    fn a_dot_name_is_hidden_without_being_told() {
        let listing = fixture();
        let dotfile = (0..listing.len())
            .map(|i| EntryId(i as u32))
            .find(|id| listing.name(*id) == ".hidden")
            .unwrap();
        assert!(listing.flags(dotfile).has(Flags::HIDDEN));
    }

    #[test]
    fn directories_lead_and_names_are_natural() {
        let mut listing = fixture();
        listing.reorder(Sort::default(), false);
        assert_eq!(names(&listing), vec!["src", "img2.png", "img10.png", "Makefile"]);
    }

    #[test]
    fn hidden_entries_appear_only_when_asked_for() {
        let mut listing = fixture();
        listing.reorder(Sort::default(), false);
        assert_eq!(listing.rows(), 4);
        listing.reorder(Sort::default(), true);
        assert_eq!(listing.rows(), 5);
        assert!(names(&listing).contains(&".hidden"));
    }

    #[test]
    fn descending_reverses_the_names_but_not_the_directory_grouping() {
        let mut listing = fixture();
        listing.reorder(Sort { descending: true, ..Sort::default() }, false);
        assert_eq!(names(&listing), vec!["src", "Makefile", "img10.png", "img2.png"]);
    }

    #[test]
    fn a_size_sort_falls_back_to_the_name_so_it_is_reproducible() {
        let mut listing = fixture();
        listing.reorder(
            Sort { column: SortColumn::Size, dirs_first: false, descending: false },
            false,
        );
        // Every size is still 0, so the tiebreak is the whole order.
        assert_eq!(names(&listing), vec!["img2.png", "img10.png", "Makefile", "src"]);
    }

    #[test]
    fn row_of_finds_what_at_returns() {
        let mut listing = fixture();
        listing.reorder(Sort::default(), true);
        for row in 0..listing.rows() {
            let id = listing.at(row).unwrap();
            assert_eq!(listing.row_of(id), Some(row));
        }
    }
}
