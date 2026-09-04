// SPDX-License-Identifier: MIT OR Apache-2.0
//! One directory's worth of entries, stored as parallel arrays.
//!
//! A directory entry is not a struct in a `Vec`. This layout is what makes
//! sorting and filtering a million entries a memory-bandwidth problem instead of
//! a pointer-chasing one, and it is the most important decision in the project
//! after "do not block the UI thread" —
//! [docs/04-architecture.md](../../../docs/04-architecture.md#data-layout).
//!
//! **`version` identifies the directory, not the contents.** A listing is built
//! on a job and published whole; after that the UI thread owns it and does mutate
//! it in place — `set_meta` writes two thousand rows at a time as `statx`
//! returns, and `set_recursive_size` writes a folder total every time a walk
//! reports. Neither touches `version`, and that is deliberate: the version exists
//! so a cache key cannot confuse row 3 of this directory with row 3 of the last
//! one, which is a question about *which listing*, not about what is in it.
//!
//! The doc here used to say the listing was "never mutated while a frame is
//! reading it", which is not true and was load-bearing: it is what made
//! `sk_text::RunKey` look sufficient without the string in it, and the size
//! column consequently drew the first value it was ever given and never another.

use crate::arena::{StrArena, StrRef};

use crate::sort::{Banding, Sort, SortColumn};

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
    /// A recursive size has been computed for this directory, complete or not.
    pub const SIZE_MEASURED: Self = Self(1 << 7);
    /// The recursive size finished. Without this it is a lower bound that is
    /// still climbing, and the view draws it dimmed to say so.
    pub const SIZE_COMPLETE: Self = Self(1 << 8);
    /// Something under it could not be counted: an unreadable subdirectory, or
    /// one on another filesystem.
    pub const SIZE_PARTIAL: Self = Self(1 << 9);

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
    /// Recursive size, for directories that have been measured.
    ///
    /// Eight bytes an entry for a column only directories use. A parallel array
    /// rather than a map because the sort reads it per comparison, and a hash
    /// lookup per comparison is the access pattern
    /// [docs/12-measurement.md](../../../docs/12-measurement.md) says costs 30×.
    recursive: Vec<u64>,
    /// Which directory each entry is in, for a listing whose rows are not one
    /// directory's.
    ///
    /// **Empty for a directory read, which is nearly every listing.** A pane
    /// showing a folder knows the one directory behind every row and pays
    /// nothing for this; a pane showing search results across a subtree cannot,
    /// and one index per entry into [`Self::homes`] is what lets the two be the
    /// same `Listing` — which is what [rule 8](../../../docs/03-principles.md)
    /// asks for: the same selection, the same filter, the same everything, in
    /// every view.
    home: Vec<u32>,
    /// The directories [`Self::home`] indexes, each stored once.
    homes: Vec<std::path::PathBuf>,
    /// The rows a view walks, in display order.
    order: Vec<EntryId>,
    /// Entries still waiting for `statx`, counted rather than searched for.
    pending: usize,
}

/// Whether `haystack` contains `needle`, ignoring ASCII case.
///
/// `needle` is already lowercased by the caller. ASCII-folded only, the same
/// bargain [`crate::collate`] makes and the same one type-ahead makes: full
/// Unicode case folding is a table, and it belongs with the search work rather
/// than with a substring test. A Turkish `İ` therefore matches only itself.
///
/// **Public because [rule 8](../../../docs/03-principles.md) says the filter
/// grammar is the same in every view.** The find popup narrows one directory
/// with this and the subtree search in `sk-index` narrows a whole tree with it,
/// and two copies of a substring rule is two places for "why did that not
/// match" to have different answers.
#[must_use]
pub fn contains_folded(haystack: &str, needle: &str) -> bool {
    // Byte windows, and both sides folded by the comparison: a multi-byte
    // character is a sequence of bytes that `eq_ignore_ascii_case` leaves alone,
    // so it matches itself and nothing else.
    let (hay, needle) = (haystack.as_bytes(), needle.as_bytes());
    // `windows(0)` panics. The only caller short-circuits on an empty filter, so
    // this is a guard against the second caller rather than against today.
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(a, b)| a.eq_ignore_ascii_case(b)))
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
            recursive: Vec::new(),
            home: Vec::new(),
            homes: Vec::new(),
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

    /// Appends an entry that lives somewhere other than one directory.
    ///
    /// For a listing gathered from a whole subtree. `directory` is stored once
    /// however many entries name it, because a search over a deep tree finds
    /// many files in the same folder and a `PathBuf` per row is the listing's
    /// dominant cost repeated.
    pub fn push_from(&mut self, directory: &std::path::Path, name: &str, flags: Flags) -> EntryId {
        let home = self.homes.iter().position(|known| known == directory).unwrap_or_else(|| {
            self.homes.push(directory.to_path_buf());
            self.homes.len() - 1
        });
        let id = self.push(name, flags);
        // Filled lazily and to the full length, so an entry pushed by `push`
        // before this is ever called still indexes something.
        self.home.resize(self.name.len(), 0);
        self.home[id.index()] = u32::try_from(home).unwrap_or(0);
        id
    }

    /// The directory an entry is in, for a listing that spans several.
    ///
    /// `None` when every row came from one directory, which is what a pane
    /// showing a folder has — and the caller already knows which one.
    #[must_use]
    pub fn home(&self, id: EntryId) -> Option<&std::path::Path> {
        let at = *self.home.get(id.index())?;
        self.homes.get(at as usize).map(std::path::PathBuf::as_path)
    }

    /// Whether these rows came from more than one directory.
    #[must_use]
    pub fn is_flattened(&self) -> bool {
        !self.homes.is_empty()
    }

    /// Records how big a directory is, counting everything under it.
    ///
    /// Called repeatedly for one entry as the walk progresses, which is why it
    /// takes the total rather than an increment: a partial report that arrives
    /// after a later one must not make the number go backwards.
    pub fn set_recursive_size(&mut self, id: EntryId, bytes: u64, complete: bool, partial: bool) {
        let index = id.index();
        if index >= self.name.len() {
            return;
        }
        if self.recursive.len() < self.name.len() {
            // Allocated on first use: a listing whose directories are never
            // measured never pays for the column.
            self.recursive.resize(self.name.len(), 0);
        }
        self.recursive[index] = self.recursive[index].max(bytes);
        self.flags[index] = self.flags[index].with(Flags::SIZE_MEASURED);
        if complete {
            self.flags[index] = self.flags[index].with(Flags::SIZE_COMPLETE);
        }
        if partial {
            self.flags[index] = self.flags[index].with(Flags::SIZE_PARTIAL);
        }
    }

    /// The recursive size of a directory, and whether the walk finished.
    #[must_use]
    pub fn recursive_size(&self, id: EntryId) -> Option<(u64, bool)> {
        if !self.flags(id).has(Flags::SIZE_MEASURED) {
            return None;
        }
        let bytes = self.recursive.get(id.index()).copied().unwrap_or(0);
        Some((bytes, self.flags(id).has(Flags::SIZE_COMPLETE)))
    }

    /// What the size column sorts and displays for an entry.
    ///
    /// A directory's own `st_size` is the size of its inode — 4,096 on most
    /// filesystems, whatever it contains — so it is never the answer. An
    /// unmeasured directory sorts as zero, which keeps directories together at
    /// one end rather than scattering them through the files.
    #[must_use]
    pub fn sort_size(&self, id: EntryId) -> u64 {
        if self.flags(id).has(Flags::DIR) {
            self.recursive_size(id).map_or(0, |(bytes, _)| bytes)
        } else {
            self.size(id)
        }
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
    /// The secondary key is always the name, so every order is total and a listing
    /// cannot reshuffle between two frames.
    pub fn reorder(&mut self, sort: Sort, show_hidden: bool, filter: &str, kinds: &[u8]) {
        self.order = self.compute_order(sort, show_hidden, filter, kinds);
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
    /// Takes `&self` so it can run on a job over a clone.
    ///
    /// **A radix sort over collation keys, not a comparison sort.** One key per
    /// surviving entry — the bytes of the column ahead of the bytes of the name —
    /// and then [`crate::radix::order`] walks them a byte at a time and never
    /// compares two names. The comparison sort this replaced was 1.40 s at
    /// 974,000 entries against a 120 ms budget; the numbers and the method are in
    /// [09-performance-budgets.md](../../../docs/09-performance-budgets.md#what-m0-measured).
    ///
    /// `kinds` is one band byte per entry, indexed by [`EntryId`], and is read
    /// only under [`Banding::Kind`] — empty for every other banding, because
    /// nothing else needs a byte this level cannot derive.
    #[must_use]
    pub fn compute_order(
        &self,
        sort: Sort,
        show_hidden: bool,
        filter: &str,
        kinds: &[u8],
    ) -> Vec<EntryId> {
        // **Not lowercased.** `contains_folded` compares with
        // `eq_ignore_ascii_case`, which folds both sides — so folding one of them
        // first is folding it twice for ASCII and once for everything else. With
        // the needle lowercased, `Ä` matched `äpfel` and not `Äpfel`, and
        // `İstanbul` matched nothing at all.
        let needle = filter;
        let kept: Vec<EntryId> = (0..self.name.len() as u32)
            .map(EntryId)
            .filter(|id| show_hidden || !self.flags[id.index()].has(Flags::HIDDEN))
            .filter(|id| needle.is_empty() || contains_folded(self.name(*id), needle))
            .collect();

        let reserve: usize = kept
            .iter()
            .map(|id| crate::collate::key_capacity(self.name[id.index()].len()) + PREFIX_MAX)
            .sum();
        let mut keys = crate::radix::KeySet::with_capacity(kept.len(), reserve);
        let mut prefix = [0u8; PREFIX_MAX];
        for id in &kept {
            let width = self.write_prefix(*id, sort, kinds, &mut prefix);
            keys.push(&prefix[..width], self.names.raw(self.name[id.index()]));
        }

        let mut order: Vec<EntryId> =
            crate::radix::order(&keys).iter().map(|at| kept[*at as usize]).collect();
        if sort.descending {
            self.reverse_within_groups(&mut order, sort, kinds);
        }
        order
    }

    /// Which band an entry falls in, and `0` where there is no banding.
    ///
    /// **Directories are a band of their own under [`Banding::Size`]**, ahead of
    /// every size band. A folder's size is a recursive total that may never have
    /// been measured, so it reads as zero — and every folder in a directory
    /// filed under "Empty", moving to "Large" the next time anything re-sorts, is
    /// a heading that lies twice. The Size *column* says the same thing by
    /// drawing nothing at all.
    #[must_use]
    pub fn band_of(&self, id: EntryId, bands: Banding, kinds: &[u8]) -> u8 {
        match bands {
            Banding::None => 0,
            Banding::Time(cuts) => cuts.band_of(self.mtime(id)),
            Banding::Size(_) if self.flags(id).has(Flags::DIR) => 0,
            Banding::Size(cuts) => {
                1 + cuts.band_of(i64::try_from(self.sort_size(id)).unwrap_or(i64::MAX))
            }
            // A byte nobody supplied bands with whatever the level above calls
            // its last band, which is where "anything else" belongs.
            Banding::Kind => kinds.get(id.index()).copied().unwrap_or(u8::MAX),
        }
    }

    /// The bytes that sort ahead of the name, and how many there are.
    ///
    /// Directories first is one byte because it is a grouping rather than an
    /// order: it is not reversed by `descending`, which is why it goes ahead of
    /// everything and why the reversal below happens inside each group.
    fn write_prefix(
        &self,
        id: EntryId,
        sort: Sort,
        kinds: &[u8],
        prefix: &mut [u8; PREFIX_MAX],
    ) -> usize {
        let mut width = 0;
        // **Ahead of everything, including directories-first.** The bands are the
        // outermost grouping: a heading says "today" and everything under it is
        // today's, folders included. Like `dirs_first` it is not reversed by
        // `descending` — see `reverse_within_groups`.
        if !sort.bands.is_empty() {
            prefix[0] = self.band_of(id, sort.bands, kinds);
            width = 1;
        }
        if sort.dirs_first {
            prefix[width] = u8::from(!self.flags[id.index()].has(Flags::DIR));
            width += 1;
        }
        let column = match sort.column {
            SortColumn::Name => return width,
            SortColumn::Size => self.sort_size(id),
            // Sign flipped into the top bit, because a big-endian `u64` compares
            // as an unsigned number and a timestamp before 1970 is negative.
            SortColumn::Modified => (self.mtime(id) as u64) ^ (1 << 63),
        };
        prefix[width..width + 8].copy_from_slice(&column.to_be_bytes());
        width + 8
    }

    /// Reverses each group in place, where a group is a band and a kind.
    ///
    /// `descending` reverses the column and the name tiebreak together, so the
    /// descending order is the ascending one backwards — but only within a group,
    /// because neither grouping directories ahead of files nor cutting the
    /// listing into time bands is part of the order the user asked to reverse.
    /// Reversing across a band would put yesterday's files under today's heading.
    ///
    /// The groups are runs, because the order is already sorted by exactly this
    /// key: the prefix bytes `write_prefix` emits ahead of the column.
    fn reverse_within_groups(&self, order: &mut [EntryId], sort: Sort, kinds: &[u8]) {
        let key = |id: &EntryId| {
            let band = self.band_of(*id, sort.bands, kinds);
            let kind = sort.dirs_first && !self.flags[id.index()].has(Flags::DIR);
            (band, kind)
        };
        let mut from = 0;
        while from < order.len() {
            let here = key(&order[from]);
            let mut to = from + 1;
            while to < order.len() && key(&order[to]) == here {
                to += 1;
            }
            order[from..to].reverse();
            from = to;
        }
    }
}

/// Room for the widest sort prefix: two grouping bytes — the time band and
/// directories-first — and one eight-byte column.
const PREFIX_MAX: usize = 10;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    /// Bands come out in order, and each one is sorted on its own.
    ///
    /// **The claim the whole feature rests on.** Grouping by time is only useful
    /// if the entries under a heading are ordered by whatever the pane is sorted
    /// by — otherwise it is two orderings fighting. The band is a prefix byte
    /// ahead of the column, so this falls out of the radix sort rather than being
    /// a second pass.
    #[test]
    fn each_band_is_sorted_on_its_own_and_the_bands_keep_their_order() {
        let mut listing = Listing::with_capacity(6);
        // Two bands of three, deliberately interleaved and out of name order.
        for (name, mtime) in [
            ("charlie", 500),
            ("alpha", 1_500),
            ("bravo", 500),
            ("delta", 1_500),
            ("echo", 500),
            ("foxtrot", 1_500),
        ] {
            let id = listing.push(name, Flags::NONE);
            listing.set_meta(id, 0, mtime, Flags::NONE);
        }
        // One edge at 1,000: band 0 is everything at or after it.
        let bands = Banding::Time(crate::sort::Cuts::new(&[1_000]));
        let sort = Sort { dirs_first: false, bands, ..Sort::default() };
        listing.reorder(sort, true, "", &[]);
        let names: Vec<&str> = (0..listing.rows())
            .filter_map(|row| listing.at(row))
            .map(|id| listing.name(id))
            .collect();
        assert_eq!(
            names,
            ["alpha", "delta", "foxtrot", "bravo", "charlie", "echo"],
            "the newest band first, and each sorted by name"
        );

        // Descending reverses inside each band and never moves a band.
        let sort = Sort { descending: true, ..sort };
        listing.reorder(sort, true, "", &[]);
        let names: Vec<&str> = (0..listing.rows())
            .filter_map(|row| listing.at(row))
            .map(|id| listing.name(id))
            .collect();
        assert_eq!(
            names,
            ["foxtrot", "delta", "alpha", "echo", "charlie", "bravo"],
            "descending reversed across the bands rather than inside them"
        );
    }

    /// A band and a kind are two groupings, and neither is reversed.
    #[test]
    fn folders_stay_ahead_of_files_inside_every_band() {
        let mut listing = Listing::with_capacity(4);
        for (name, mtime, flags) in [
            ("new-file", 1_500, Flags::NONE),
            ("new-dir", 1_500, Flags::DIR),
            ("old-file", 500, Flags::NONE),
            ("old-dir", 500, Flags::DIR),
        ] {
            let id = listing.push(name, flags);
            listing.set_meta(id, 0, mtime, flags);
        }
        let bands = Banding::Time(crate::sort::Cuts::new(&[1_000]));
        listing.reorder(Sort { descending: true, bands, ..Sort::default() }, true, "", &[]);
        let names: Vec<&str> = (0..listing.rows())
            .filter_map(|row| listing.at(row))
            .map(|id| listing.name(id))
            .collect();
        // Band 0's folder, band 0's file, then band 1's folder and file — with
        // `descending` reversing neither grouping.
        assert_eq!(names, ["new-dir", "new-file", "old-dir", "old-file"]);
    }

    /// A folder's size is a recursive total that may never have been measured,
    /// so grouping by size gives folders a band of their own ahead of the sizes.
    ///
    /// **Without it every unmeasured folder files under "Empty"** — and moves to
    /// "Large" the next time anything re-sorts, which is a heading that lies
    /// twice. The Size column says the same thing by drawing nothing at all.
    #[test]
    fn a_size_banding_files_every_directory_ahead_of_every_size() {
        let mut listing = Listing::with_capacity(5);
        for (name, size, flags) in [
            ("small.txt", 2_000u64, Flags::NONE),
            ("measured-dir", 0, Flags::DIR),
            ("big.bin", 5_000_000, Flags::NONE),
            ("plain-dir", 0, Flags::DIR),
            ("empty.txt", 0, Flags::NONE),
        ] {
            let id = listing.push(name, flags);
            listing.set_meta(id, size, 0, flags);
        }
        // A measured folder is still a folder: it is the *kind* of quantity that
        // puts it here, not the number.
        let dir = (0..5).map(EntryId).find(|id| listing.name(*id) == "measured-dir").expect("dir");
        listing.set_recursive_size(dir, 9_000_000, true, false);

        let cuts = crate::sort::Cuts::new(&[1_000_000, 1_000, 1]);
        let sort = Sort { bands: Banding::Size(cuts), ..Sort::default() };
        listing.reorder(sort, true, "", &[]);
        let bands: Vec<u8> = (0..listing.rows())
            .filter_map(|row| listing.at(row))
            .map(|id| listing.band_of(id, sort.bands, &[]))
            .collect();
        // Three edges make four size bands; nothing here falls in the third, and
        // an empty band is simply never drawn.
        assert_eq!(bands, [0, 0, 1, 2, 4], "the bands are not in order: {bands:?}");
        assert_eq!(
            names(&listing),
            vec!["measured-dir", "plain-dir", "big.bin", "small.txt", "empty.txt"],
            "a folder's total put it in a size band, or a band came out of order"
        );
    }

    /// The kind bytes come from above and this level only ever compares them.
    ///
    /// A byte nobody supplied is the *last* band, which is where "anything we
    /// could not classify" belongs — and `label` clamps to the same key, so the
    /// heading over it says so rather than panicking.
    #[test]
    fn a_kind_banding_groups_by_the_bytes_it_is_given() {
        let mut listing = Listing::with_capacity(4);
        for name in ["delta", "alpha", "charlie", "bravo"] {
            listing.push(name, Flags::NONE);
        }
        // Indexed by `EntryId`, which is push order: delta and bravo share band
        // 2, alpha is band 5, charlie is band 7. So band 2 comes first with its
        // two members in name order, and the bytes decide the bands while the
        // column still decides the order inside one.
        let kinds = [2u8, 5, 7, 2];
        let sort = Sort { dirs_first: false, bands: Banding::Kind, ..Sort::default() };
        listing.reorder(sort, true, "", &kinds);
        assert_eq!(names(&listing), vec!["bravo", "delta", "alpha", "charlie"]);
        assert_eq!(listing.band_of(EntryId(3), sort.bands, &kinds), 2);
        assert_eq!(
            listing.band_of(EntryId(0), sort.bands, &[]),
            u8::MAX,
            "a listing with no kind bytes at all banded as something"
        );
    }

    /// No banding is one band, and costs the order nothing.
    #[test]
    fn an_empty_banding_orders_exactly_as_it_did_before_bands_existed() {
        let mut listing = Listing::with_capacity(3);
        for (name, mtime) in [("b", 500), ("a", 1_500), ("c", 900)] {
            let id = listing.push(name, Flags::NONE);
            listing.set_meta(id, 0, mtime, Flags::NONE);
        }
        assert!(Sort::default().bands.is_empty());
        listing.reorder(Sort { dirs_first: false, ..Sort::default() }, true, "", &[]);
        let names: Vec<&str> = (0..listing.rows())
            .filter_map(|row| listing.at(row))
            .map(|id| listing.name(id))
            .collect();
        assert_eq!(names, ["a", "b", "c"], "an unbanded sort is the plain one");
    }

    /// The whole of what makes a flattened view possible: rows that remember
    /// which directory they came from, in the same `Listing` a directory read
    /// produces.
    ///
    /// **Empty for a directory read, and it has to be.** Nearly every listing in
    /// the program is one directory's; if this cost four bytes an entry there,
    /// a million-row directory would pay 4 MB for a column it never reads.
    #[test]
    fn a_flattened_listing_remembers_where_each_row_came_from() {
        let mut plain = Listing::with_capacity(2);
        plain.push("one.txt", Flags::NONE);
        assert!(!plain.is_flattened());
        assert_eq!(plain.home(EntryId(0)), None, "a directory read pays for a home column");

        let mut results = Listing::with_capacity(3);
        let a = results.push_from(std::path::Path::new("/tmp/alpha"), "same-name.txt", Flags::NONE);
        let b = results.push_from(std::path::Path::new("/tmp/beta"), "same-name.txt", Flags::NONE);
        // The same directory twice: stored once, because a search over a deep
        // tree finds many files in one folder.
        let c = results.push_from(std::path::Path::new("/tmp/alpha"), "other.txt", Flags::NONE);
        assert!(results.is_flattened());
        assert_eq!(results.home(a), Some(std::path::Path::new("/tmp/alpha")));
        assert_eq!(results.home(b), Some(std::path::Path::new("/tmp/beta")));
        assert_eq!(results.home(c), Some(std::path::Path::new("/tmp/alpha")));
        // Two rows with the same name are two different files, which is the case
        // a list of names alone cannot represent at all.
        assert_eq!(results.name(a), results.name(b));
        assert_ne!(results.home(a), results.home(b));
    }

    use super::{EntryId, Flags, Listing};
    use crate::collate::natural_cmp;
    use crate::sort::{Banding, Sort, SortColumn};

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

    /// The comparison sort the radix replaced, kept as the reference it has to
    /// agree with.
    ///
    /// Not dead code and not a second implementation: it is the *definition* of
    /// the order, and the only way to know the accelerator produces it is to run
    /// both. [12-measurement.md](../../../docs/12-measurement.md) has this as its
    /// own rule, learned from an accelerator that disagreed with the order it was
    /// accelerating.
    fn by_comparison(
        listing: &Listing,
        sort: Sort,
        show_hidden: bool,
        filter: &str,
    ) -> Vec<EntryId> {
        let mut order: Vec<EntryId> = (0..listing.name.len())
            .map(|index| EntryId(index as u32))
            .filter(|id| show_hidden || !listing.flags[id.index()].has(Flags::HIDDEN))
            .filter(|id| filter.is_empty() || super::contains_folded(listing.name(*id), filter))
            .collect();
        order.sort_by(|a, b| {
            let (left, right) = (a.index(), b.index());
            if sort.dirs_first {
                let (a_dir, b_dir) =
                    (listing.flags[left].has(Flags::DIR), listing.flags[right].has(Flags::DIR));
                if a_dir != b_dir {
                    return b_dir.cmp(&a_dir);
                }
            }
            let primary = match sort.column {
                SortColumn::Name => natural_cmp(listing.name(*a), listing.name(*b)),
                SortColumn::Size => listing
                    .sort_size(*a)
                    .cmp(&listing.sort_size(*b))
                    .then_with(|| natural_cmp(listing.name(*a), listing.name(*b))),
                SortColumn::Modified => listing
                    .mtime(*a)
                    .cmp(&listing.mtime(*b))
                    .then_with(|| natural_cmp(listing.name(*a), listing.name(*b))),
            };
            if sort.descending { primary.reverse() } else { primary }
        });
        order
    }

    /// Names chosen for what breaks a key rather than for what looks like a
    /// directory: digits against punctuation, three depths of leading zero, case
    /// that only differs, a shared prefix longer than the packed head, and bytes
    /// above ASCII.
    fn adversarial() -> Listing {
        let mut listing = Listing::with_capacity(64);
        let names: &[(&str, Flags)] = &[
            ("img10.png", Flags::NONE),
            ("img2.png", Flags::NONE),
            ("img1.png", Flags::NONE),
            ("IMG2.PNG", Flags::NONE),
            ("src", Flags::DIR),
            ("Src", Flags::DIR),
            ("a.b", Flags::NONE),
            ("a1b", Flags::NONE),
            ("a-b", Flags::NONE),
            ("a~b", Flags::NONE),
            ("007", Flags::NONE),
            ("07", Flags::NONE),
            ("7", Flags::NONE),
            (".hidden", Flags::NONE),
            (".config", Flags::DIR),
            ("Makefile", Flags::NONE),
            ("makefile", Flags::NONE),
            ("main.c", Flags::NONE),
            ("résumé-2.py", Flags::NONE),
            ("résumé-10.py", Flags::NONE),
            ("회의록-2", Flags::NONE),
            ("회의록-10", Flags::NONE),
            ("shared-prefix-beyond-eight-0001", Flags::NONE),
            ("shared-prefix-beyond-eight-0002", Flags::NONE),
            ("shared-prefix-beyond-eight-0010", Flags::NONE),
            ("v08", Flags::NONE),
            ("v9", Flags::NONE),
            ("v10", Flags::NONE),
            ("2024-01-02", Flags::NONE),
            ("2024-1-2", Flags::NONE),
        ];
        for (index, (name, flags)) in names.iter().enumerate() {
            let id = listing.push(name, *flags);
            // Sizes and times that collide on purpose, so the name tiebreak is
            // exercised on both columns rather than only on the name column.
            let size = (index as u64 % 4) * 1024;
            let mtime = i64::try_from(index % 3).unwrap_or(0) - 1;
            listing.set_meta(id, size, mtime, Flags::NONE);
        }
        listing
    }

    /// The one assertion that makes the radix safe to have replaced a comparison
    /// sort: every sort a person can ask for, against the comparator that defines
    /// it, with and without dotfiles and under a filter.
    #[test]
    fn the_radix_order_is_the_comparison_order_for_every_sort() {
        let listing = adversarial();
        for column in [SortColumn::Name, SortColumn::Size, SortColumn::Modified] {
            for descending in [false, true] {
                for dirs_first in [false, true] {
                    let sort = Sort { column, descending, dirs_first, ..Sort::default() };
                    for show_hidden in [false, true] {
                        for filter in ["", "img", "a", "É"] {
                            let wanted = by_comparison(&listing, sort, show_hidden, filter);
                            let got = listing.compute_order(sort, show_hidden, filter, &[]);
                            let shown = |order: &[EntryId]| -> Vec<&str> {
                                order.iter().map(|id| listing.name(*id)).collect()
                            };
                            assert_eq!(
                                shown(&got),
                                shown(&wanted),
                                "{column:?} descending={descending} dirs_first={dirs_first} \
                                 hidden={show_hidden} filter={filter:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// At a size that forces the counting pass rather than the insertion cutoff,
    /// on all three columns.
    #[test]
    fn the_radix_agrees_at_a_size_that_takes_the_counting_path() {
        let mut listing = Listing::with_capacity(4_000);
        for index in 0..4_000u64 {
            let name = match index % 5 {
                0 => format!("report-{index}.txt"),
                1 => format!("{:06}-padded.log", index % 211),
                2 => format!("a-long-shared-prefix-{index}"),
                3 => format!("MiXeD-{index}"),
                _ => format!("résumé-{index}.pdf"),
            };
            let flags = if index % 7 == 0 { Flags::DIR } else { Flags::NONE };
            let id = listing.push(&name, flags);
            let mtime = i64::try_from(index % 13).unwrap_or(0) - 6;
            listing.set_meta(id, index % 97, mtime, Flags::NONE);
        }
        for column in [SortColumn::Name, SortColumn::Size, SortColumn::Modified] {
            let sort = Sort { column, descending: false, dirs_first: true, ..Sort::default() };
            let wanted: Vec<&str> = by_comparison(&listing, sort, true, "")
                .iter()
                .map(|id| listing.name(*id))
                .collect();
            let got: Vec<&str> = listing
                .compute_order(sort, true, "", &[])
                .iter()
                .map(|id| listing.name(*id))
                .collect();
            assert_eq!(got, wanted, "{column:?}");
        }
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
        listing.reorder(Sort::default(), false, "", &[]);
        assert_eq!(names(&listing), vec!["src", "img2.png", "img10.png", "Makefile"]);
    }

    #[test]
    fn hidden_entries_appear_only_when_asked_for() {
        let mut listing = fixture();
        listing.reorder(Sort::default(), false, "", &[]);
        assert_eq!(listing.rows(), 4);
        listing.reorder(Sort::default(), true, "", &[]);
        assert_eq!(listing.rows(), 5);
        assert!(names(&listing).contains(&".hidden"));
    }

    #[test]
    fn descending_reverses_the_names_but_not_the_directory_grouping() {
        let mut listing = fixture();
        listing.reorder(Sort { descending: true, ..Sort::default() }, false, "", &[]);
        assert_eq!(names(&listing), vec!["src", "Makefile", "img10.png", "img2.png"]);
    }

    /// The filter narrows the display order and leaves the entries alone: an
    /// `EntryId` still names what it named, so a selection carried across one is
    /// still a selection of the same files.
    #[test]
    fn a_filter_narrows_what_is_shown_and_not_what_exists() {
        let mut listing = fixture();
        let before = listing.len();
        listing.reorder(Sort::default(), false, "img", &[]);
        assert_eq!(names(&listing), vec!["img2.png", "img10.png"]);
        assert_eq!(listing.len(), before, "the filter removed entries rather than hiding them");

        // Case-insensitive, and a substring rather than a prefix.
        listing.reorder(Sort::default(), false, "MAKE", &[]);
        assert_eq!(names(&listing), vec!["Makefile"]);
        listing.reorder(Sort::default(), false, "make", &[]);
        assert_eq!(names(&listing), vec!["Makefile"]);
        listing.reorder(Sort::default(), false, "PNG", &[]);
        assert_eq!(names(&listing), vec!["img2.png", "img10.png"]);

        // Nothing matching is an empty list, not the whole list.
        listing.reorder(Sort::default(), false, "zzz", &[]);
        assert!(names(&listing).is_empty());

        // And an empty filter is every row back, in the order it had.
        listing.reorder(Sort::default(), false, "", &[]);
        assert_eq!(names(&listing), vec!["src", "img2.png", "img10.png", "Makefile"]);
    }

    /// ASCII folds both ways or neither. Lowercasing the needle and then
    /// comparing with `eq_ignore_ascii_case` folded one side twice and the other
    /// once: `Ä` found `äpfel` and hid `Äpfel`, and `İstanbul` was unreachable.
    #[test]
    fn a_non_ascii_capital_matches_itself_rather_than_only_its_lowercase() {
        let mut listing = Listing::default();
        for name in ["Äpfel", "äpfel", "İstanbul", "Istanbul"] {
            listing.push(name, Flags::NONE);
        }
        listing.reorder(Sort::default(), false, "Ä", &[]);
        assert_eq!(names(&listing), vec!["Äpfel"]);
        listing.reorder(Sort::default(), false, "ä", &[]);
        assert_eq!(names(&listing), vec!["äpfel"]);
        listing.reorder(Sort::default(), false, "İ", &[]);
        assert_eq!(names(&listing), vec!["İstanbul"]);
        // And ASCII still folds, which is the bargain this is trading against.
        listing.reorder(Sort::default(), false, "ISTAN", &[]);
        assert_eq!(names(&listing), vec!["Istanbul"]);
    }

    /// A filter and the hidden flag are separate questions, and a dotfile that
    /// matches must still not appear while dotfiles are hidden.
    #[test]
    fn a_filter_does_not_reveal_hidden_entries() {
        let mut listing = fixture();
        listing.push(".imgcache", Flags::NONE);
        listing.reorder(Sort::default(), false, "img", &[]);
        assert_eq!(names(&listing), vec!["img2.png", "img10.png"]);
        listing.reorder(Sort::default(), true, "img", &[]);
        assert_eq!(names(&listing), vec![".imgcache", "img2.png", "img10.png"]);
    }

    #[test]
    fn a_size_sort_falls_back_to_the_name_so_it_is_reproducible() {
        let mut listing = fixture();
        listing.reorder(
            Sort { column: SortColumn::Size, dirs_first: false, ..Sort::default() },
            false,
            "",
            &[],
        );
        // Every size is still 0, so the tiebreak is the whole order.
        assert_eq!(names(&listing), vec!["img2.png", "img10.png", "Makefile", "src"]);
    }

    #[test]
    fn row_of_finds_what_at_returns() {
        let mut listing = fixture();
        listing.reorder(Sort::default(), true, "", &[]);
        for row in 0..listing.rows() {
            let id = listing.at(row).unwrap();
            assert_eq!(listing.row_of(id), Some(row));
        }
    }
}
