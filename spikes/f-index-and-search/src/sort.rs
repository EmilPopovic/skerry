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

/// Radix over `(key, index)` pairs held together, rather than over a permutation
/// that looks each key up.
///
/// The permutation version reads `keys[order[i]]` — a random probe into an 8 MB
/// array, once per element per pass, eight passes deep. Packing the key beside
/// the index makes every read sequential and every write the only scattered
/// access left. The pair costs 12 bytes against 4, so this trades 8 MB of
/// footprint for the cache behaviour, which is the trade the Pi's 213 ms
/// suggests is worth making.
pub fn radix_packed(keys: &[u64], out: &mut Vec<u32>) {
    let mut pairs: Vec<(u64, u32)> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (*key, index as u32))
        .collect();
    let mut scratch: Vec<(u64, u32)> = vec![(0, 0); pairs.len()];
    let mut counts = [0u32; 256];

    for shift in (0..64).step_by(8) {
        counts.fill(0);
        for (key, _) in pairs.iter() {
            counts[((key >> shift) & 0xFF) as usize] += 1;
        }
        if counts.iter().any(|&c| c as usize == pairs.len()) {
            continue;
        }
        let mut running = 0u32;
        for count in counts.iter_mut() {
            let start = running;
            running += *count;
            *count = start;
        }
        for pair in pairs.iter() {
            let digit = ((pair.0 >> shift) & 0xFF) as usize;
            scratch[counts[digit] as usize] = *pair;
            counts[digit] += 1;
        }
        std::mem::swap(&mut pairs, &mut scratch);
    }

    out.clear();
    out.extend(pairs.into_iter().map(|(_, index)| index));
}

/// Packed radix, then repair the runs the key could not separate.
pub fn radix_packed_with_ties(flat: &Flat, keys: &[u64], out: &mut Vec<u32>) -> usize {
    radix_packed(keys, out);
    let mut repaired = 0usize;
    let mut start = 0usize;
    while start < out.len() {
        let key = keys[out[start] as usize];
        let mut end = start + 1;
        while end < out.len() && keys[out[end] as usize] == key {
            end += 1;
        }
        if end - start > 1 {
            out[start..end]
                .sort_unstable_by(|a, b| flat.name(*a as usize).cmp(flat.name(*b as usize)));
            repaired += end - start;
        }
        start = end;
    }
    repaired
}

/// Below this, a comparison sort beats setting up a 256-bucket histogram.
const SMALL: usize = 24;

/// Eight bytes of the name starting at `depth`, big-endian, zero-padded.
///
/// Zero padding is a correct terminator because a POSIX filename cannot contain
/// a zero byte, so a shorter name sorts before a longer one sharing its prefix —
/// which is what byte comparison would do anyway.
#[inline]
fn chunk_at(name: &[u8], depth: usize) -> u64 {
    let mut buffer = [0u8; 8];
    if depth < name.len() {
        let take = (name.len() - depth).min(8);
        buffer[..take].copy_from_slice(&name[depth..depth + take]);
    }
    u64::from_be_bytes(buffer)
}

/// Radix sort by name that never falls back to a full string comparison.
///
/// The measured problem with a fixed-width key is not the radix, it is what
/// happens afterwards: 94% of filenames share their first eight bytes, so the
/// tie repair is 82 of 98 ms on a laptop and 540 of 753 ms on a Pi. Widening the
/// key to sixteen bytes does not fix it — 60% still tie.
///
/// So instead of repairing ties by comparison, recurse: sort each tied run by the
/// *next* eight bytes, and again, until the run is short enough that a comparison
/// sort is cheaper than a histogram. Names are finite, so this terminates; a run
/// whose chunk is entirely zero has run off the end of every name in it and is
/// already ordered.
pub fn radix_recursive(flat: &Flat, order: &mut [u32], depth: usize, scratch: &mut Vec<u32>) {
    if order.len() <= SMALL {
        order.sort_unstable_by(|a, b| flat.name(*a as usize).cmp(flat.name(*b as usize)));
        return;
    }

    let mut chunks: Vec<u64> = order
        .iter()
        .map(|index| chunk_at(flat.name(*index as usize), depth))
        .collect();

    // Every name in this run has ended. Nothing left to order by.
    if chunks.iter().all(|c| *c == 0) {
        return;
    }

    // Sort this run by the chunk. Pairs rather than a permutation, so each pass
    // reads sequentially.
    let mut pairs: Vec<(u64, u32)> =
        chunks.drain(..).zip(order.iter().copied()).collect();
    let mut spare: Vec<(u64, u32)> = vec![(0, 0); pairs.len()];
    let mut counts = [0u32; 256];
    for shift in (0..64).step_by(8) {
        counts.fill(0);
        for (key, _) in pairs.iter() {
            counts[((key >> shift) & 0xFF) as usize] += 1;
        }
        if counts.iter().any(|&c| c as usize == pairs.len()) {
            continue;
        }
        let mut running = 0u32;
        for count in counts.iter_mut() {
            let start = running;
            running += *count;
            *count = start;
        }
        for pair in pairs.iter() {
            let digit = ((pair.0 >> shift) & 0xFF) as usize;
            spare[counts[digit] as usize] = *pair;
            counts[digit] += 1;
        }
        std::mem::swap(&mut pairs, &mut spare);
    }

    for (slot, (_, index)) in order.iter_mut().zip(pairs.iter()) {
        *slot = *index;
    }

    // Recurse into each run the chunk could not separate.
    let mut start = 0usize;
    while start < pairs.len() {
        let key = pairs[start].0;
        let mut end = start + 1;
        while end < pairs.len() && pairs[end].0 == key {
            end += 1;
        }
        if end - start > 1 && key != 0 {
            radix_recursive(flat, &mut order[start..end], depth + 8, scratch);
        }
        start = end;
    }
}

/// Recursive radix, with the top level's buckets spread across threads.
///
/// [09-performance-budgets.md](../../../docs/09-performance-budgets.md) budgets
/// this sort "off-thread; the UI stays interactive throughout" — a statement
/// about the *UI* thread, not a promise that the sort is serial. After the first
/// pass the 256 buckets share nothing: each owns a disjoint slice of the order
/// array and reads an immutable arena, so they are independent by construction
/// rather than by locking.
///
/// The first pass stays serial because it is one linear read of the whole arena
/// and one scatter, and splitting that costs more in coordination than it saves.
pub fn radix_parallel(flat: &Flat, order: &mut [u32], threads: usize) {
    if order.len() <= SMALL || threads <= 1 {
        let mut scratch = Vec::new();
        radix_recursive(flat, order, 0, &mut scratch);
        return;
    }

    // One serial pass on the first eight bytes, to cut the work into runs.
    let mut pairs: Vec<(u64, u32)> = order
        .iter()
        .map(|index| (chunk_at(flat.name(*index as usize), 0), *index))
        .collect();
    let mut spare: Vec<(u64, u32)> = vec![(0, 0); pairs.len()];
    let mut counts = [0u32; 256];
    for shift in (0..64).step_by(8) {
        counts.fill(0);
        for (key, _) in pairs.iter() {
            counts[((key >> shift) & 0xFF) as usize] += 1;
        }
        if counts.iter().any(|&c| c as usize == pairs.len()) {
            continue;
        }
        let mut running = 0u32;
        for count in counts.iter_mut() {
            let start = running;
            running += *count;
            *count = start;
        }
        for pair in pairs.iter() {
            let digit = ((pair.0 >> shift) & 0xFF) as usize;
            spare[counts[digit] as usize] = *pair;
            counts[digit] += 1;
        }
        std::mem::swap(&mut pairs, &mut spare);
    }
    for (slot, (_, index)) in order.iter_mut().zip(pairs.iter()) {
        *slot = *index;
    }

    // Split the order array at run boundaries, then hand whole runs to threads.
    let mut bounds = Vec::new();
    let mut start = 0usize;
    while start < pairs.len() {
        let key = pairs[start].0;
        let mut end = start + 1;
        while end < pairs.len() && pairs[end].0 == key {
            end += 1;
        }
        if end - start > 1 && key != 0 {
            bounds.push((start, end));
        }
        start = end;
    }

    // Longest first, so the straggler starts earliest. One run of 100,000 among
    // 200 small ones decides the wall clock otherwise.
    bounds.sort_unstable_by_key(|(start, end)| std::cmp::Reverse(end - start));

    let next = std::sync::atomic::AtomicUsize::new(0);
    let order_ptr = SendPtr(order.as_mut_ptr());
    let total = order.len();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (bounds, next, flat) = (&bounds, &next, &flat);
            let order_ptr = &order_ptr;
            scope.spawn(move || {
                let mut scratch = Vec::new();
                loop {
                    let slot = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(start, end)) = bounds.get(slot) else { return };
                    // SAFETY: the runs are disjoint by construction — each was
                    // taken between two boundaries of the same array — and every
                    // slot is claimed by exactly one thread via `fetch_add`, so
                    // no two threads ever hold overlapping slices.
                    let run = unsafe {
                        debug_assert!(end <= total);
                        std::slice::from_raw_parts_mut(order_ptr.0.add(start), end - start)
                    };
                    radix_recursive(flat, run, 8, &mut scratch);
                }
            });
        }
    });
}

/// A raw pointer that may cross a scoped-thread boundary. The safety argument is
/// at the use site: disjoint runs, one claimant each.
struct SendPtr(*mut u32);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
