// SPDX-License-Identifier: MIT OR Apache-2.0
//! A dense bitset.
//!
//! Selection is a bitset rather than a `HashSet<EntryId>` because the operations
//! a file manager actually performs are "select all, then deselect two hundred"
//! and "snapshot the selection every frame". Both are memory bandwidth over a
//! contiguous array here, and both are pointer chasing plus an allocation in a
//! hash set.

/// A fixed-length set of bit indices.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

const BITS: usize = u64::BITS as usize;

impl BitSet {
    /// A set with room for `len` indices, all clear.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self { words: vec![0; len.div_ceil(BITS)], len }
    }

    /// How many indices this set can hold.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this set can hold nothing at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Grows or shrinks to `len`, clearing any bit that is now out of range.
    pub fn resize(&mut self, len: usize) {
        self.words.resize(len.div_ceil(BITS), 0);
        self.len = len;
        self.mask_tail();
    }

    /// Whether `index` is in the set. Out of range reads as absent.
    #[must_use]
    pub fn contains(&self, index: usize) -> bool {
        index < self.len && self.words[index / BITS] & (1 << (index % BITS)) != 0
    }

    /// Adds `index`. Out of range is ignored.
    pub fn insert(&mut self, index: usize) {
        if index < self.len {
            self.words[index / BITS] |= 1 << (index % BITS);
        }
    }

    /// Removes `index`. Out of range is ignored.
    pub fn remove(&mut self, index: usize) {
        if index < self.len {
            self.words[index / BITS] &= !(1 << (index % BITS));
        }
    }

    /// Adds `index` if absent, removes it if present, and returns the new state.
    pub fn toggle(&mut self, index: usize) -> bool {
        let present = self.contains(index);
        if present {
            self.remove(index);
        } else {
            self.insert(index);
        }
        !present
    }

    /// Adds every index in `range`, clamped to the set's length.
    pub fn insert_range(&mut self, range: std::ops::Range<usize>) {
        for index in range.start..range.end.min(self.len) {
            self.insert(index);
        }
    }

    /// Removes every index.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// Adds every index.
    pub fn fill(&mut self) {
        self.words.fill(u64::MAX);
        self.mask_tail();
    }

    /// How many indices are present. One `popcnt` per 64 indices.
    #[must_use]
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Every present index, ascending.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(word_index, word)| {
            let mut bits = *word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(word_index * BITS + bit)
            })
        })
    }

    /// The lowest present index at or after `from`.
    #[must_use]
    pub fn first_from(&self, from: usize) -> Option<usize> {
        (from..self.len).find(|index| self.contains(*index))
    }

    /// Clears the bits above `len` in the final word.
    ///
    /// Every counting and iterating method reads whole words, so a stale bit in
    /// the tail is a phantom member: `fill()` on a set of length 3 would
    /// otherwise report 64.
    fn mask_tail(&mut self) {
        let tail = self.len % BITS;
        if tail != 0
            && let Some(last) = self.words.last_mut()
        {
            *last &= (1u64 << tail) - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::BitSet;

    #[test]
    fn fill_counts_the_length_and_not_the_word_size() {
        for len in [0, 1, 3, 63, 64, 65, 1000] {
            let mut set = BitSet::new(len);
            set.fill();
            assert_eq!(set.count(), len, "len {len}");
            assert_eq!(set.iter().count(), len, "len {len}");
        }
    }

    #[test]
    fn resize_down_forgets_the_bits_that_left() {
        let mut set = BitSet::new(200);
        set.fill();
        set.resize(70);
        assert_eq!(set.count(), 70);
        set.resize(200);
        assert_eq!(set.count(), 70, "growing does not resurrect them");
    }

    #[test]
    fn iteration_is_ascending_and_exact() {
        let mut set = BitSet::new(300);
        let wanted = [0, 1, 63, 64, 65, 127, 299];
        for index in wanted {
            set.insert(index);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), wanted);
        assert_eq!(set.first_from(64), Some(64));
        assert_eq!(set.first_from(66), Some(127));
        assert_eq!(set.first_from(300), None);
    }

    #[test]
    fn out_of_range_is_ignored_rather_than_a_panic() {
        let mut set = BitSet::new(8);
        set.insert(9999);
        set.remove(9999);
        assert!(!set.contains(9999));
        assert_eq!(set.count(), 0);
    }
}
