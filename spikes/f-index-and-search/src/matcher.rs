// SPDX-License-Identifier: MIT OR Apache-2.0
//! Subsequence matching with scoring, in the spirit of `fzf`.
//!
//! Written rather than imported. Rule 10 aside, the question this spike asks is
//! whether *the layout in 04-architecture* supports a 50 ms first result, and a
//! matcher crate would answer a different question — one about the crate.
//!
//! The shape that matters for the number: the common case is a *rejection*, and
//! a rejection should touch as few bytes as possible. Two cheap filters run
//! before scoring — a length check, and a first-byte scan — because the
//! overwhelming majority of a million names cannot match a four-character query
//! and it is worth knowing that without reading them.

/// Case-insensitive for ASCII only. Filenames are bytes, a query is typed, and
/// full Unicode case folding on the hot path of a per-keystroke scan is a cost
/// this spike is not paying to measure a layout. Non-ASCII bytes compare exactly,
/// which is correct for matching and wrong for a Turkish user's expectations —
/// a real implementation folds properly and pays for it.
#[inline]
fn fold(byte: u8) -> u8 {
    byte.to_ascii_lowercase()
}

/// Higher is better. Consecutive runs and matches at a word boundary score
/// above scattered ones, which is what makes `fzf` feel like it read your mind.
const BONUS_CONSECUTIVE: i32 = 8;
const BONUS_BOUNDARY: i32 = 16;
const PENALTY_GAP: i32 = -1;

pub struct Query {
    /// Folded once, at the keystroke, not once per candidate.
    needle: Vec<u8>,
}

impl Query {
    pub fn new(text: &str) -> Self {
        Self { needle: text.as_bytes().iter().copied().map(fold).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// `None` if the name does not contain the query as a subsequence.
    ///
    /// The early exits are the measurement: a name shorter than the query, or
    /// one that never contains the query's first byte, is rejected without the
    /// scoring loop ever starting.
    pub fn score(&self, name: &[u8]) -> Option<i32> {
        if self.needle.is_empty() {
            return Some(0);
        }
        if name.len() < self.needle.len() {
            return None;
        }

        let first = self.needle[0];
        let mut start = 0;
        while start < name.len() && fold(name[start]) != first {
            start += 1;
        }
        if start == name.len() {
            return None;
        }

        let mut score = 0;
        let mut needle_at = 0;
        let mut previous_matched = false;
        for (index, &raw) in name.iter().enumerate().skip(start) {
            if needle_at == self.needle.len() {
                break;
            }
            if fold(raw) == self.needle[needle_at] {
                score += if previous_matched {
                    BONUS_CONSECUTIVE
                } else if index == 0 || is_boundary(name[index - 1]) {
                    BONUS_BOUNDARY
                } else {
                    0
                };
                needle_at += 1;
                previous_matched = true;
            } else {
                score += PENALTY_GAP;
                previous_matched = false;
            }
        }

        (needle_at == self.needle.len()).then_some(score)
    }
}

#[inline]
fn is_boundary(byte: u8) -> bool {
    matches!(byte, b'-' | b'_' | b'.' | b' ' | b'/')
}

/// Scan the arena across several threads.
///
/// A keystroke is a full scan — a fuzzy match cannot narrow the previous result
/// set, because a longer query can match at an offset the shorter one did not —
/// and the scan is already close to memory bandwidth. Splitting it is the only
/// lever that does not change what the search means.
///
/// `std::thread::scope` rather than the job pool: the pool is measured elsewhere
/// and what matters here is whether the *work* divides, not how it is dispatched.
/// A real implementation submits ranges to the one pool the process owns.
pub fn count_parallel(flat: &crate::flat::Flat, query: &Query, threads: usize) -> usize {
    let total = flat.len();
    if threads <= 1 || total < threads {
        return (0..total).filter(|i| query.score(flat.name(*i)).is_some()).count();
    }
    let per = total.div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|slot| {
                let (flat, query) = (&flat, &query);
                scope.spawn(move || {
                    let start = slot * per;
                    let end = ((slot + 1) * per).min(total);
                    (start..end).filter(|i| query.score(flat.name(*i)).is_some()).count()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
    })
}

/// The first match, stopping every thread as soon as any of them finds one.
///
/// This is the measurement the 50 ms bar actually describes: a user needs the top
/// of the list, not all of it. The early exit is what makes a query that matches
/// early cost nothing, and a query that matches nothing cost a full scan.
pub fn first_parallel(
    flat: &crate::flat::Flat,
    query: &Query,
    threads: usize,
) -> Option<usize> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let total = flat.len();
    if threads <= 1 {
        return (0..total).find(|i| query.score(flat.name(*i)).is_some());
    }
    let found = AtomicBool::new(false);
    let per = total.div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|slot| {
                let (flat, query, found) = (&flat, &query, &found);
                scope.spawn(move || {
                    let start = slot * per;
                    let end = ((slot + 1) * per).min(total);
                    for index in start..end {
                        // Checked per chunk rather than per name: an atomic load
                        // on every entry costs more than the scan it guards.
                        if index % 4096 == 0 && found.load(Ordering::Relaxed) {
                            return None;
                        }
                        if query.score(flat.name(index)).is_some() {
                            found.store(true, Ordering::Relaxed);
                            return Some(index);
                        }
                    }
                    None
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).min()
    })
}
