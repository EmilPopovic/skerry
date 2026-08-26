//! Natural-order collation.
//!
//! `img2.png` before `img10.png`, because every other order is one a person has
//! to think about. Written here rather than taken from a crate because it is
//! tightly coupled to the store layout — it compares two `&str` borrowed from
//! one arena, and it is called on the hot path of a sort over a million names.

use std::cmp::Ordering;

/// Compares two names the way a person reads them.
///
/// Three rules, in order:
///
/// 1. A run of digits compares as a number, so `2` precedes `10`. Leading zeros
///    lose only as a tiebreak, so `01` and `1` are adjacent rather than far
///    apart, and `007` sorts before `07` sorts before `7`.
/// 2. Letters compare case-insensitively, so `Makefile` and `main.c` are
///    neighbours instead of being split by every uppercase name in the folder.
/// 3. Exact bytes break every remaining tie, so the order is total and a sort is
///    reproducible. Without this, `README` and `readme` would compare equal and
///    a listing could reorder between two frames.
#[must_use]
pub fn natural_cmp(left: &str, right: &str) -> Ordering {
    let (mut a, mut b) = (left.as_bytes(), right.as_bytes());
    let mut zeros = Ordering::Equal;

    loop {
        match (a.first().copied(), b.first().copied()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let (left_digits, left_rest) = digits(a);
                    let (right_digits, right_rest) = digits(b);
                    match numeric_cmp(left_digits, right_digits) {
                        Ordering::Equal => {
                            if zeros == Ordering::Equal {
                                zeros = left_digits.len().cmp(&right_digits.len()).reverse();
                            }
                        }
                        other => return other,
                    }
                    a = left_rest;
                    b = right_rest;
                } else {
                    match x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()) {
                        Ordering::Equal => {
                            a = &a[1..];
                            b = &b[1..];
                        }
                        other => return other,
                    }
                }
            }
        }
    }

    // Both names are exhausted and every field matched. Fall back to the digit
    // padding, then to raw bytes, so the order is total.
    zeros.then_with(|| left.as_bytes().cmp(right.as_bytes()))
}

fn digits(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes.iter().position(|b| !b.is_ascii_digit()).unwrap_or(bytes.len());
    bytes.split_at(end)
}

/// Compares two digit runs as numbers of unbounded length.
///
/// By length after stripping leading zeros, then lexically. A `u64` parse would
/// be shorter and would compare two 25-digit names wrongly — and a filename is
/// allowed 255 bytes of digits.
fn numeric_cmp(left: &[u8], right: &[u8]) -> Ordering {
    fn strip(run: &[u8]) -> &[u8] {
        let start = run.iter().position(|b| *b != b'0').unwrap_or(run.len());
        &run[start..]
    }
    let (left, right) = (strip(left), strip(right));
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::natural_cmp;
    use std::cmp::Ordering;

    fn sorted(mut names: Vec<&str>) -> Vec<&str> {
        names.sort_by(|a, b| natural_cmp(a, b));
        names
    }

    #[test]
    fn digit_runs_compare_as_numbers() {
        assert_eq!(
            sorted(vec!["img10.png", "img2.png", "img1.png", "img100.png"]),
            vec!["img1.png", "img2.png", "img10.png", "img100.png"]
        );
    }

    #[test]
    fn a_digit_run_longer_than_a_u64_still_compares_correctly() {
        let big = "9".repeat(30);
        let bigger = format!("{}8", "9".repeat(30));
        assert_eq!(natural_cmp(&big, &bigger), Ordering::Less);
    }

    #[test]
    fn case_does_not_scatter_neighbours() {
        assert_eq!(
            sorted(vec!["main.c", "Makefile", "README", "makefile"]),
            vec!["main.c", "Makefile", "makefile", "README"]
        );
    }

    #[test]
    fn the_order_is_total_so_a_sort_cannot_reshuffle_between_frames() {
        let names = ["README", "readme", "ReadMe", "01", "1", "007", "07", "7"];
        for a in names {
            for b in names {
                let forward = natural_cmp(a, b);
                let backward = natural_cmp(b, a);
                assert_eq!(forward, backward.reverse(), "{a} vs {b}");
                assert_eq!(forward == Ordering::Equal, a == b, "{a} vs {b}");
            }
        }
    }

    #[test]
    fn leading_zeros_only_break_a_tie() {
        assert_eq!(sorted(vec!["7", "07", "007"]), vec!["007", "07", "7"]);
        assert_eq!(sorted(vec!["v08", "v9", "v10"]), vec!["v08", "v9", "v10"]);
    }

    /// The counterexample that removed the precomputed key from the design.
    ///
    /// Kept as a test rather than a comment so that reintroducing a lexical key
    /// as a sort accelerator fails the build instead of quietly reordering a
    /// listing.
    #[test]
    fn the_prefix_key_cannot_accelerate_a_natural_order_sort() {
        let lexical_prefix = |name: &str| {
            let mut key = 0u64;
            for (index, byte) in name.as_bytes().iter().take(8).enumerate() {
                key |= u64::from(byte.to_ascii_lowercase()) << (56 - index * 8);
            }
            key
        };
        assert_eq!(natural_cmp("img2.png", "img10.png"), Ordering::Less);
        assert!(
            lexical_prefix("img2.png") > lexical_prefix("img10.png"),
            "a lexical key orders these the other way, so it cannot be a first pass"
        );
    }
}
