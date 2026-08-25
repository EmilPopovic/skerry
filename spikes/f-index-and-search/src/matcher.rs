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
