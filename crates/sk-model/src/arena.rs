// SPDX-License-Identifier: MIT OR Apache-2.0
//! One bump arena of UTF-8 bytes, addressed by offset and length.
//!
//! This is the first half of the data layout in
//! [docs/04-architecture.md](../../../docs/04-architecture.md#data-layout): a
//! directory entry's name is not a `String` in a `Vec<Entry>`, it is a slice of
//! one contiguous allocation. Spike B measured what that buys — the per-entry
//! allocation is CPU time rather than footprint, because the allocator hands
//! the same pages back either way.

/// Where one string lives in a [`StrArena`].
///
/// Eight bytes, `Copy`, and meaningless without the arena it came from. That is
/// deliberate: a `StrRef` in a struct cannot outlive its listing by accident,
/// because it cannot be read without the arena that is dropped with it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StrRef {
    offset: u32,
    len: u32,
}

impl StrRef {
    /// The empty string, valid against any arena.
    pub const EMPTY: Self = Self { offset: 0, len: 0 };

    /// Length in bytes, which is not the number of characters.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len as usize
    }

    /// Whether this reference is zero bytes long.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// A growable arena of UTF-8 bytes.
#[derive(Clone, Default, Debug)]
pub struct StrArena {
    bytes: Vec<u8>,
}

impl StrArena {
    /// An empty arena with room for `capacity` bytes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { bytes: Vec::with_capacity(capacity) }
    }

    /// Copies `text` in and returns where it landed.
    ///
    /// A string longer than `u32::MAX`, or one that would push the arena past
    /// `u32::MAX` bytes, is truncated to nothing rather than corrupting every
    /// later reference. A single listing that large is not reachable from a
    /// directory read — `getdents64` caps a name at 255 bytes — so this is a
    /// guard against a caller misusing the arena, not a case to design for.
    pub fn push(&mut self, text: &str) -> StrRef {
        let offset = self.bytes.len();
        if offset > u32::MAX as usize - text.len() {
            return StrRef::EMPTY;
        }
        self.bytes.extend_from_slice(text.as_bytes());
        StrRef { offset: offset as u32, len: text.len() as u32 }
    }

    /// The bytes `reference` points at, or `""` if it is out of range.
    #[must_use]
    pub fn get(&self, reference: StrRef) -> &str {
        let start = reference.offset as usize;
        let end = start + reference.len as usize;
        match self.bytes.get(start..end) {
            // `push` is the only way to make a `StrRef`, and it only ever
            // records the bounds of a `&str` it has just copied in.
            Some(slice) => std::str::from_utf8(slice).unwrap_or(""),
            None => "",
        }
    }

    /// The bytes `reference` points at, without the UTF-8 check [`StrArena::get`]
    /// pays for.
    ///
    /// For a caller that works in bytes anyway. Building a collation key for a
    /// million names revalidated 27 MB of UTF-8 that [`StrArena::push`] had
    /// already validated on the way in.
    #[must_use]
    pub fn raw(&self, reference: StrRef) -> &[u8] {
        let start = reference.offset as usize;
        let end = start + reference.len as usize;
        self.bytes.get(start..end).unwrap_or(&[])
    }

    /// Bytes currently held, which is what the store reports as its name cost.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Drops every string, keeping the allocation for the next listing.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{StrArena, StrRef};

    #[test]
    fn round_trips_every_string_it_is_given() {
        let mut arena = StrArena::default();
        let inputs = ["", "a", "Makefile", "קובץ.webp", "日本語のファイル", "🦀"];
        let refs: Vec<_> = inputs.iter().map(|s| arena.push(s)).collect();
        for (input, reference) in inputs.iter().zip(&refs) {
            assert_eq!(arena.get(*reference), *input);
        }
    }

    #[test]
    fn an_out_of_range_reference_reads_as_empty_rather_than_panicking() {
        let arena = StrArena::default();
        assert_eq!(arena.get(StrRef::EMPTY), "");
    }

    #[test]
    fn clear_keeps_the_allocation_and_forgets_the_contents() {
        let mut arena = StrArena::with_capacity(64);
        let first = arena.push("one");
        arena.clear();
        let second = arena.push("two");
        assert_eq!(arena.get(second), "two");
        assert_eq!(first.len(), 3, "the reference survives; what it points at does not");
    }
}
