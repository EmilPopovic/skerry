// SPDX-License-Identifier: MIT OR Apache-2.0
//! An MSD radix sort over collation keys, which never compares two names.
//!
//! [Spike F](../../../spikes/f-index-and-search/README.md#sorting-chased-down-from-948-ms-to-191-ms)
//! measured eight approaches to sorting a million filenames and this is the shape
//! that won: split on the most significant byte, recurse onto the next one rather
//! than repairing ties by comparison, and pack the leading key bytes beside the
//! index so the passes that discriminate most keys never touch the arena.
//! Repairing ties by string comparison cost **600 of 775 ms** there, because 94%
//! of the corpus shared its first eight bytes.
//!
//! What the spike could not do is produce a *natural* order — its keys were
//! lexical, and `img2` after `img10` is the wrong answer whatever it costs. The
//! keys here come from [`crate::collate::write_key`], whose byte order is
//! [`crate::natural_cmp`] exactly, which is the one assertion this module rests
//! on and the one its tests are about.
//!
//! Iterative rather than recursive. Each level holds three 257-entry counters —
//! about 3 KB — and a key can be 700 bytes long, so recursion would put 2 MB on
//! the stack of a thread that has 2 MB. The order is computed on a job.

use crate::collate;

/// One key per element to be sorted, in one allocation.
pub struct KeySet {
    bytes: Vec<u8>,
    /// One past the end of each key, so key `i` runs from `ends[i - 1]` to
    /// `ends[i]`. Ends rather than starts because appending only knows the end.
    ends: Vec<u32>,
}

impl KeySet {
    /// Room for `count` keys over `bytes` of key data.
    #[must_use]
    pub fn with_capacity(count: usize, bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes), ends: Vec::with_capacity(count) }
    }

    /// Appends `prefix`, then the collation key for `name`.
    ///
    /// The prefix is how a column other than the name is sorted: the bytes of a
    /// size or a timestamp, most significant first, ahead of the name key that
    /// breaks its ties. One radix pass then serves every column.
    pub fn push(&mut self, prefix: &[u8], name: &[u8]) {
        self.bytes.extend_from_slice(prefix);
        collate::write_key_bytes(name, &mut self.bytes);
        self.ends.push(u32::try_from(self.bytes.len()).unwrap_or(u32::MAX));
    }

    /// How many keys are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether no key has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Bytes held, which is what a caller reserving for the next listing reads.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes.len()
    }

    fn span(&self, index: usize) -> (u32, u32) {
        let start = if index == 0 { 0 } else { self.ends[index - 1] };
        (start, self.ends[index] - start)
    }
}

/// A key and where it came from, packed so a pass reads one array.
///
/// A permutation sort reads `keys[order[i]]`, which is a random probe per element
/// per pass. Carrying the leading bytes here makes the first eight passes
/// sequential — worth 232 → 131 ms on a Pi in Spike F, and 17 → 15 ms on a
/// laptop, which is the difference between a memory-bound machine and a
/// cache-rich one showing the same defect.
#[derive(Clone, Copy, Default)]
struct Item {
    /// The first eight key bytes, most significant first, zero-padded.
    head: u64,
    start: u32,
    len: u32,
    index: u32,
}

/// Runs below this length finish with a comparison sort on their key tails.
///
/// Swept at 974,000 names: 24 gives a 124 ms radix, 256 gives 117, and 1024 gives
/// 101 — above which the difference is inside the run-to-run spread on a machine
/// in use. What moves is the number of levels, because these names share long
/// word prefixes and every shared byte is another pass.
///
/// This is *not* Spike F's "repair ties by comparison", which cost 600 of 775 ms
/// there. That fell back after a fixed eight-byte key had been exhausted, so 94%
/// of the corpus fell back and each comparison walked a whole name. Here the run
/// is already narrowed to a thousand and the comparison is a `u64` of packed key
/// bytes that never reaches the arena.
const SMALL: usize = 1024;

/// How many buckets one byte of key splits into: 256 values, plus one for the
/// keys that end at this depth.
///
/// Bucket zero is those, and a key that ends is a prefix of every key that does
/// not — which is exactly where [`crate::natural_cmp`] puts a prefix.
const BUCKETS: usize = 257;

/// Above this many elements a run splits on two bytes at once.
///
/// One byte per pass spends most of its time re-visiting runs a second byte
/// would have separated: filenames share their leading byte heavily — Spike F
/// found 29 non-empty first-byte buckets with the largest at 13.4% — where two
/// bytes give 66,049 and leave almost every one of them already below [`SMALL`].
/// The threshold is where 258 KB of counters costs four bytes an element, which
/// is less than the element.
const WIDE: usize = 1 << 16;

/// The permutation that puts `keys` in order.
///
/// Returns the indices of `keys`, so a caller sorting a listing maps them back
/// through whatever it built the keys from.
#[must_use]
pub fn order(keys: &KeySet) -> Vec<u32> {
    let count = keys.len();
    let mut items: Vec<Item> = (0..count)
        .map(|index| {
            let (start, len) = keys.span(index);
            Item {
                head: head_of(&keys.bytes, start, len),
                start,
                len,
                index: u32::try_from(index).unwrap_or(u32::MAX),
            }
        })
        .collect();

    if count <= SMALL {
        finish(&mut items, &keys.bytes, 0);
        return items.iter().map(|item| item.index).collect();
    }

    let mut scratch = vec![Item::default(); count];
    let mut narrow = [0u32; BUCKETS];
    let mut cursor = [0u32; BUCKETS];
    // Allocated on first use and then reused: 258 KB is not something to put on
    // a stack, nor to allocate once per run.
    let mut wide: Vec<u32> = Vec::new();
    let mut wide_cursor: Vec<u32> = Vec::new();
    let mut work = vec![(0, count, 0)];

    while let Some((at, run, depth)) = work.pop() {
        let (region, spare) = (&mut items[at..at + run], &mut scratch[..run]);
        if run > WIDE {
            if wide.is_empty() {
                wide = vec![0u32; BUCKETS * BUCKETS];
                wide_cursor = vec![0u32; BUCKETS * BUCKETS];
            }
            let mut buckets = Buckets { counts: &mut wide, cursor: &mut wide_cursor };
            split::<2>(region, spare, &keys.bytes, at, depth, &mut buckets, &mut work);
        } else {
            let mut buckets = Buckets { counts: &mut narrow, cursor: &mut cursor };
            split::<1>(region, spare, &keys.bytes, at, depth, &mut buckets, &mut work);
        }
    }

    items.iter().map(|item| item.index).collect()
}

/// The two counter arrays a pass needs, allocated once and reused.
///
/// One struct because there are two of them at two sizes — a narrow pass counts
/// 257 buckets on the stack, a wide one 66,049 on the heap — and because eight
/// separate arguments is one more than a function should take.
struct Buckets<'a> {
    counts: &'a mut [u32],
    cursor: &'a mut [u32],
}

/// One counting pass over `WIDTH` bytes of key, then the sub-runs it leaves.
///
/// `at` is where `items` begins in the whole array, because the work stack holds
/// absolute ranges — a sub-run has to be findable again after this frame is gone.
fn split<const WIDTH: usize>(
    items: &mut [Item],
    scratch: &mut [Item],
    bytes: &[u8],
    at: usize,
    depth: usize,
    buckets: &mut Buckets<'_>,
    work: &mut Vec<(usize, usize, usize)>,
) {
    let used = BUCKETS.pow(WIDTH as u32);
    let Buckets { counts, cursor } = buckets;
    counts[..used].fill(0);
    for item in items.iter() {
        counts[bucket::<WIDTH>(item, bytes, depth)] += 1;
    }
    let mut running = 0;
    for (slot, count) in cursor[..used].iter_mut().zip(&counts[..used]) {
        *slot = running;
        running += count;
    }
    for item in items.iter() {
        let target = bucket::<WIDTH>(item, bytes, depth);
        scratch[cursor[target] as usize] = *item;
        cursor[target] += 1;
    }
    items.copy_from_slice(scratch);

    // Bucket zero is the keys that ended before this pass: they share every byte
    // and are therefore equal, which the raw-bytes section of a collation key
    // makes impossible for two different names, so it holds at most one.
    let mut position = at + counts[0] as usize;
    for size in counts[1..used].iter().map(|count| *count as usize) {
        if size > SMALL {
            work.push((position, size, depth + WIDTH));
        } else if size > 1 {
            finish(&mut items[position - at..position - at + size], bytes, depth + WIDTH);
        }
        position += size;
    }
}

/// Which bucket an item falls in over `WIDTH` bytes from `depth`.
fn bucket<const WIDTH: usize>(item: &Item, bytes: &[u8], depth: usize) -> usize {
    let mut index = 0;
    for step in 0..WIDTH {
        index = index * BUCKETS + slot(item, bytes, depth + step);
    }
    index
}

/// One byte of key, offset by one so that zero means the key ended.
fn slot(item: &Item, bytes: &[u8], depth: usize) -> usize {
    if depth >= item.len as usize {
        return 0;
    }
    let byte = if depth < 8 {
        u8::try_from((item.head >> (56 - depth * 8)) & 0xff).unwrap_or(0)
    } else {
        bytes[item.start as usize + depth]
    };
    byte as usize + 1
}

fn head_of(bytes: &[u8], start: u32, len: u32) -> u64 {
    let start = start as usize;
    let end = start + (len as usize).min(8);
    let mut head = 0u64;
    for (position, byte) in bytes[start..end].iter().enumerate() {
        head |= u64::from(*byte) << (56 - position * 8);
    }
    head
}

/// Sorts a run whose keys already agree up to `depth`, by what is left of them.
///
/// Compares the packed head as one integer before touching the arena: almost
/// every pair of filenames differs inside eight bytes of key, and the key arena
/// at a million entries is 63 MB — larger than this machine's L3 — so a
/// comparison that reads it is a trip to memory where one that does not is a
/// register.
fn finish(items: &mut [Item], bytes: &[u8], depth: usize) {
    items.sort_unstable_by(|left, right| order_of(left, right, bytes, depth));
}

/// How `left` and `right` order, given that both agree up to `depth`.
fn order_of(left: &Item, right: &Item, bytes: &[u8], depth: usize) -> std::cmp::Ordering {
    if depth < 8 {
        // The head from `depth` on, which is what remains unknown. Where these
        // differ, the shorter key holds a padding zero and the longer one a real
        // byte above it — so the integer order is the key order, and a prefix
        // still sorts first.
        let shift = depth * 8;
        let (a, b) = (left.head << shift, right.head << shift);
        if a != b {
            return a.cmp(&b);
        }
    }
    tail(left, bytes, depth).cmp(tail(right, bytes, depth))
}

fn tail<'a>(item: &Item, bytes: &'a [u8], depth: usize) -> &'a [u8] {
    let start = item.start as usize;
    let end = start + item.len as usize;
    &bytes[(start + depth).min(end)..end]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{KeySet, SMALL, order};
    use crate::natural_cmp;

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut keys = KeySet::with_capacity(names.len(), 0);
        for name in names {
            keys.push(&[], name.as_bytes());
        }
        order(&keys).iter().map(|index| names[*index as usize].to_owned()).collect()
    }

    fn by_comparison(names: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        out.sort_by(|a, b| natural_cmp(a, b));
        out
    }

    /// A corpus long enough to take the counting path, built so that runs share
    /// prefixes at several depths — which is the case the recursion exists for
    /// and the one a single pass gets wrong.
    fn corpus(count: usize) -> Vec<String> {
        let mut names = Vec::with_capacity(count);
        for index in 0..count {
            let shape = index % 6;
            names.push(match shape {
                0 => format!("report-{index}.txt"),
                1 => format!("image{index}.png"),
                2 => format!("a-very-long-shared-prefix-that-defeats-eight-bytes-{index}"),
                3 => format!("{:08}-padded", index % 97),
                4 => format!("MiXeD-CaSe-{index}"),
                _ => format!("résumé-{index}.pdf"),
            });
        }
        names
    }

    #[test]
    fn a_short_run_takes_the_insertion_path_and_still_agrees() {
        let names = ["img10.png", "img2.png", "Makefile", "main.c", "007", "07", "7", ""];
        assert!(names.len() <= SMALL);
        assert_eq!(sorted(&names), by_comparison(&names));
    }

    /// The assertion the module exists to satisfy, at a size that forces the
    /// counting pass, the recursion and the insertion cutoff all three.
    #[test]
    fn a_long_run_agrees_with_the_comparison_sort_it_replaces() {
        let owned = corpus(5_000);
        let names: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert_eq!(sorted(&names), by_comparison(&names));
    }

    /// A run that shares more than eight bytes exhausts `head` and moves the
    /// bucket read into the arena. Without the `depth < 8` branch this is where a
    /// sort silently stops discriminating.
    #[test]
    fn keys_that_share_more_than_eight_bytes_are_still_separated() {
        let owned: Vec<String> =
            (0..400).map(|index| format!("shared-prefix-beyond-eight-{index:04}")).collect();
        let names: Vec<&str> = owned.iter().map(String::as_str).collect();
        let got = sorted(&names);
        assert_eq!(got, by_comparison(&names));
        assert_eq!(got.first().map(String::as_str), Some("shared-prefix-beyond-eight-0000"));
    }

    /// A prefix sorts before the key that extends it, which is bucket zero's
    /// whole job.
    #[test]
    fn a_key_that_ends_sorts_before_one_that_continues() {
        let owned: Vec<String> = std::iter::once(String::new())
            .chain((0..60).map(|index| "a".repeat(index + 1)))
            .collect();
        let names: Vec<&str> = owned.iter().map(String::as_str).collect();
        let mut shuffled = names.clone();
        shuffled.reverse();
        assert_eq!(sorted(&shuffled), by_comparison(&shuffled));
    }

    /// The prefix is what lets one radix serve the size and modified columns.
    #[test]
    fn a_prefix_orders_ahead_of_the_name() {
        let mut keys = KeySet::with_capacity(3, 0);
        keys.push(&2u64.to_be_bytes(), b"zzz");
        keys.push(&1u64.to_be_bytes(), b"aaa");
        keys.push(&2u64.to_be_bytes(), b"aaa");
        assert_eq!(order(&keys), vec![1, 2, 0]);
    }

    #[test]
    fn nothing_to_sort_is_not_a_special_case() {
        let keys = KeySet::with_capacity(0, 0);
        assert!(keys.is_empty());
        assert!(order(&keys).is_empty());
    }
}
