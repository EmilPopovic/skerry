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

/// Least-significant-digit radix sort over the precomputed keys.
///
/// The measured problem with the comparison sorts is not the comparison, it is
/// that sorting a permutation dereferences a random index into an 8 MB array on
/// every step. Radix never compares and never dereferences: it reads the key
/// array in order, eight times, and writes into buckets.
///
/// Eight passes of eight bits, because a 256-entry histogram stays in L1 where a
/// 65,536-entry one for 16-bit digits does not.
pub fn radix(keys: &[u64], order: &mut Vec<u32>, scratch: &mut Vec<u32>) {
    scratch.resize(order.len(), 0);
    let mut counts = [0u32; 256];

    for shift in (0..64).step_by(8) {
        counts.fill(0);
        for &index in order.iter() {
            counts[((keys[index as usize] >> shift) & 0xFF) as usize] += 1;
        }
        // A pass whose digit is constant changes nothing; skipping it is worth
        // real time on keys that do not use their high bytes, which is every key
        // derived from a short ASCII name.
        if counts.iter().any(|&c| c as usize == order.len()) {
            continue;
        }
        let mut running = 0u32;
        for count in counts.iter_mut() {
            let start = running;
            running += *count;
            *count = start;
        }
        for &index in order.iter() {
            let digit = ((keys[index as usize] >> shift) & 0xFF) as usize;
            scratch[counts[digit] as usize] = index;
            counts[digit] += 1;
        }
        std::mem::swap(order, scratch);
    }
}

/// Radix by key, then repair the runs the key could not separate.
///
/// An eight-byte key is a prefix, and a prefix leaves ties. On this workload the
/// ties are enormous — names are drawn from thirty stems, so `report-b` is the
/// first eight bytes of a great many of them — which makes this the honest
/// version and the pure radix above an optimistic floor.
pub fn radix_with_ties(flat: &Flat, keys: &[u64], order: &mut Vec<u32>, scratch: &mut Vec<u32>) -> usize {
    radix(keys, order, scratch);

    let mut repaired = 0usize;
    let mut start = 0usize;
    while start < order.len() {
        let key = keys[order[start] as usize];
        let mut end = start + 1;
        while end < order.len() && keys[order[end] as usize] == key {
            end += 1;
        }
        if end - start > 1 {
            order[start..end].sort_unstable_by(|a, b| {
                flat.name(*a as usize).cmp(flat.name(*b as usize))
            });
            repaired += end - start;
        }
        start = end;
    }
    repaired
}

/// A sixteen-byte key, as two `u64`s.
///
/// Eight bytes leaves 94% of this workload tied, and the tie repair is 79 of the
/// 96 ms a radix sort costs. That tie rate is partly an artefact of a generator
/// with thirty stems — but only partly. Real filenames collide on long prefixes
/// constantly: `IMG_20240101_`, `Screenshot from 2024-`, `part-00001-of-`. If
/// widening the key collapses the repair, the sort budget is a key-width
/// decision rather than an algorithm one.
pub fn keys16(flat: &Flat) -> (Vec<u64>, Vec<u64>) {
    let mut high = Vec::with_capacity(flat.len());
    let mut low = Vec::with_capacity(flat.len());
    for index in 0..flat.len() {
        let name = flat.name(index);
        let mut buffer = [0u8; 16];
        let take = name.len().min(16);
        buffer[..take].copy_from_slice(&name[..take]);
        high.push(u64::from_be_bytes(buffer[..8].try_into().expect("8 bytes")));
        low.push(u64::from_be_bytes(buffer[8..].try_into().expect("8 bytes")));
    }
    (high, low)
}

/// Radix over the low half then the high half — least significant digit order,
/// so the stable passes leave the pair correctly ordered.
pub fn radix16_with_ties(
    flat: &Flat,
    high: &[u64],
    low: &[u64],
    order: &mut Vec<u32>,
    scratch: &mut Vec<u32>,
) -> usize {
    radix(low, order, scratch);
    radix(high, order, scratch);

    let mut repaired = 0usize;
    let mut start = 0usize;
    while start < order.len() {
        let (h, l) = (high[order[start] as usize], low[order[start] as usize]);
        let mut end = start + 1;
        while end < order.len()
            && high[order[end] as usize] == h
            && low[order[end] as usize] == l
        {
            end += 1;
        }
        if end - start > 1 {
            order[start..end]
                .sort_unstable_by(|a, b| flat.name(*a as usize).cmp(flat.name(*b as usize)));
            repaired += end - start;
        }
        start = end;
    }
    repaired
}
