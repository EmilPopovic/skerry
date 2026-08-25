//! Sorting a million entries.
//!
//! [09-performance-budgets.md](../../../docs/09-performance-budgets.md) budgets
//! this at 120 ms with a 300 ms hard fail, off-thread, and nothing had measured
//! it. [04-architecture.md](../../../docs/04-architecture.md#data-layout) makes a
//! specific claim about how to get there:
//!
//! > Sort keys are derived once at listing time so a re-sort never recomputes a
//! > natural-order collation.
//!
//! That claim is testable, so the two are measured against each other: sorting by
//! chasing the name arena on every comparison, against sorting by a `u64` key
//! computed once. The gap between them is what the sentence is worth.

use crate::flat::Flat;

/// The first eight bytes of a name, big-endian, so a plain integer comparison
/// orders the same way a byte comparison would for the overwhelming majority of
/// pairs. Ties fall back to the arena, which is rare enough not to matter.
///
/// A real implementation folds case and handles digit runs — `file10` after
/// `file9` — which costs more to build and nothing more to compare. The point
/// here is the *shape*: one pass at listing time, then integer comparisons.
pub fn keys(flat: &Flat) -> Vec<u64> {
    (0..flat.len())
        .map(|index| {
            let name = flat.name(index);
            let mut key = [0u8; 8];
            let take = name.len().min(8);
            key[..take].copy_from_slice(&name[..take]);
            u64::from_be_bytes(key)
        })
        .collect()
}

/// Sort an index permutation by comparing names through the arena. Every
/// comparison is a random read into 28 MB, which is where the time goes.
pub fn by_name_arena(flat: &Flat, order: &mut [u32]) {
    order.sort_unstable_by(|a, b| flat.name(*a as usize).cmp(flat.name(*b as usize)));
}

/// Sort by the precomputed key, falling back to the arena only on a tie.
pub fn by_precomputed(flat: &Flat, keys: &[u64], order: &mut [u32]) {
    order.sort_unstable_by(|a, b| {
        let (ka, kb) = (keys[*a as usize], keys[*b as usize]);
        match ka.cmp(&kb) {
            std::cmp::Ordering::Equal => flat.name(*a as usize).cmp(flat.name(*b as usize)),
            other => other,
        }
    });
}

/// Sort by a metadata column. No indirection at all — the comparison reads one
/// contiguous array — so this is the floor the other two are measured against.
pub fn by_size(flat: &Flat, order: &mut [u32]) {
    order.sort_unstable_by_key(|index| flat.size[*index as usize]);
}

pub fn by_mtime(flat: &Flat, order: &mut [u32]) {
    order.sort_unstable_by_key(|index| flat.mtime[*index as usize]);
}
