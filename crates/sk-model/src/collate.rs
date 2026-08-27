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

/// Writes the collation key for `name` into `out`.
///
/// **Lexicographic order over these keys is exactly [`natural_cmp`] order.** That
/// is the whole point of them: a radix sort can produce a natural order without
/// ever comparing two names, which is what takes the sort row from a comparison
/// sort's O(n log n) name walks to one pass per byte of key.
///
/// Three sections, separated by `0x00` — the one byte a filename cannot contain:
///
/// 1. **The fields.** A non-digit byte, lowercased. A digit run as a length
///    prefix followed by the run with its leading zeros stripped.
/// 2. **The leading-zero signature**, one byte per digit run. Reached only when
///    every field matched, which is [`natural_cmp`]'s `zeros` tiebreak.
/// 3. **The raw bytes**, so the order is total and two names that differ only in
///    case cannot compare equal.
///
/// The length prefix is `b'0' + <decimal digits in the length>` followed by that
/// length in decimal, which is monotone in the length and — the load-bearing part
/// — **keeps the first byte of a digit run inside `0x30..=0x39`**. A digit run
/// meets a non-digit byte at the same key position whenever two names diverge
/// there, and [`natural_cmp`] resolves that by comparing the raw bytes. Every
/// byte a filename can hold is either a digit or outside that range, so any first
/// byte inside it compares against a non-digit with the same sign the raw digit
/// would. A prefix of `0x01` instead would put `a1b` before `a.b`, which is the
/// wrong way round.
pub fn write_key(name: &str, out: &mut Vec<u8>) {
    write_key_bytes(name.as_bytes(), out);
}

/// [`write_key`] over bytes that are already known to be a name.
///
/// The key is a byte transformation and never needs the UTF-8 property, so a
/// caller holding a slice of the name arena can skip revalidating it.
pub fn write_key_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    // The signature is collected on the way through and written after the
    // fields, because a second pass over the name to find the digit runs again
    // was a measurable share of the key cost at a million names. A name is at
    // most 255 bytes and a run is at least one, so 128 runs is the ceiling;
    // beyond that the signature is truncated, which costs the leading-zero
    // tiebreak and never the total order — the raw bytes still follow it.
    let mut signature = [0u8; 128];
    let mut runs = 0;
    let mut index = 0;
    while index < bytes.len() {
        // A whole run of non-digits at a time, copied and then folded in place.
        // Pushing one lowercased byte at a time was 105 ms of a 237 ms sort at
        // 974,000 names: a capacity check and a branch per byte, where
        // `make_ascii_lowercase` over a slice is vectorised.
        let plain =
            bytes[index..].iter().position(u8::is_ascii_digit).unwrap_or(bytes.len() - index);
        if plain > 0 {
            let at = out.len();
            out.extend_from_slice(&bytes[index..index + plain]);
            out[at..].make_ascii_lowercase();
            index += plain;
            continue;
        }
        let (run, _) = digits(&bytes[index..]);
        write_digit_run(run, out);
        if runs < signature.len() {
            signature[runs] = 255 - u8::try_from(run.len()).unwrap_or(255);
            runs += 1;
        }
        index += run.len();
    }
    out.push(0);

    // One byte per digit run, `255 - raw length`, so a run with more leading
    // zeros sorts first — `007` before `07` before `7`. Compared position by
    // position, which is well defined because this section is only ever reached
    // when every field matched, and matching fields means the same runs in the
    // same places.
    out.extend_from_slice(&signature[..runs]);
    out.push(0);
    out.extend_from_slice(bytes);
}

/// Room to reserve for one name's key, so the arena grows once per listing
/// rather than once per name.
///
/// The key holds the name twice — lowercased as fields, then raw as the final
/// tiebreak — plus two bytes of length prefix per digit run, plus one signature
/// byte per run, plus two separators. A name of alternating digits and letters is
/// the worst case at three and a half times its length; three plus a constant
/// covers everything a directory read can produce and over-reserving a few bytes
/// per name is cheaper than one reallocation.
#[must_use]
pub const fn key_capacity(name_len: usize) -> usize {
    name_len * 3 + 8
}

/// How many decimal digits of a digit run's length the prefix may carry.
///
/// The prefix byte is `b'0' + count`, so a count of 9 is the last one that stays
/// inside `0x30..=0x39` — a run of 10^9 significant digits. `getdents64` caps a
/// whole name at 255 bytes, so the reachable count is one.
const MAX_LENGTH_DIGITS: usize = 9;

fn write_digit_run(run: &[u8], out: &mut Vec<u8>) {
    let significant = strip_zeros(run);
    let mut decimal = [0u8; MAX_LENGTH_DIGITS];
    let mut count = 0;
    let mut value = significant.len();
    loop {
        decimal[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        count += 1;
        value /= 10;
        if value == 0 || count == MAX_LENGTH_DIGITS {
            break;
        }
    }
    out.push(b'0' + u8::try_from(count).unwrap_or(9));
    for position in (0..count).rev() {
        out.push(decimal[position]);
    }
    out.extend_from_slice(significant);
}

/// A digit run without its leading zeros, which is what decides its value.
fn strip_zeros(run: &[u8]) -> &[u8] {
    let start = run.iter().position(|byte| *byte != b'0').unwrap_or(run.len());
    &run[start..]
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
    let (left, right) = (strip_zeros(left), strip_zeros(right));
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

    /// The corpus every claim about the key is checked against.
    ///
    /// Chosen for the cases that break a key: a digit run against punctuation on
    /// both sides of the digit range, leading zeros at three depths, a run longer
    /// than a `u64`, case that only differs, an empty name, a name that is a
    /// prefix of another, and bytes above ASCII where `to_ascii_lowercase` is a
    /// no-op and the comparison is raw.
    const CORPUS: &[&str] = &[
        "",
        "a",
        "ab",
        "a.b",
        "a1b",
        "a-b",
        "a_b",
        "a~b",
        "a!b",
        "a0b",
        "a9b",
        "a10b",
        "a010b",
        "a0010b",
        "img2.png",
        "img10.png",
        "img1.png",
        "img100.png",
        "IMG2.PNG",
        "Makefile",
        "makefile",
        "main.c",
        "README",
        "readme",
        "ReadMe",
        "1",
        "01",
        "007",
        "07",
        "7",
        "0",
        "00",
        "10",
        "9",
        "v08",
        "v9",
        "v10",
        "a01b",
        "a1c",
        "a1b",
        "a01c",
        "999999999999999999999999999999",
        "9999999999999999999999999999998",
        "résumé-1.py",
        "résumé-10.py",
        "회의록-2",
        "회의록-10",
        "...",
        "..1",
        "1.2.3",
        "1.10.3",
        "2024-01-02",
        "2024-1-2",
        "file 1",
        "file  1",
    ];

    fn key(name: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(super::key_capacity(name.len()));
        super::write_key(name, &mut out);
        out
    }

    /// The one assertion the radix sort rests on.
    ///
    /// [`super::natural_cmp`] is the definition of the order; the key exists only
    /// to let a radix produce it without comparing names. If the two ever
    /// disagree the sort is fast and wrong, which is why this is checked over
    /// every ordered pair of the corpus rather than over a handful of examples —
    /// see [an accelerator has to agree with the thing it
    /// accelerates](../../../docs/12-measurement.md).
    #[test]
    fn the_key_orders_every_pair_exactly_as_the_comparison_does() {
        for left in CORPUS {
            for right in CORPUS {
                let wanted = natural_cmp(left, right);
                let got = key(left).cmp(&key(right));
                assert_eq!(got, wanted, "{left:?} vs {right:?}");
            }
        }
    }

    /// Sorting the whole corpus both ways has to agree too. Pairwise agreement
    /// permits an intransitive comparison to hide; a full sort does not.
    #[test]
    fn sorting_by_key_and_by_comparison_produce_the_same_order() {
        let mut by_comparison = CORPUS.to_vec();
        by_comparison.sort_by(|a, b| natural_cmp(a, b));
        let mut by_key = CORPUS.to_vec();
        by_key.sort_by_key(|name| key(name));
        assert_eq!(by_key, by_comparison);
    }

    /// The property the encoding exists for, stated on its own so a regression
    /// names it. A digit run's first key byte has to sit where a digit sits, or
    /// it stops comparing against punctuation the way the raw byte does.
    #[test]
    fn a_digit_run_starts_its_key_inside_the_digit_range() {
        for name in ["1", "007", "999", "1234567890", "0"] {
            let first = key(name)[0];
            assert!(first.is_ascii_digit(), "{name:?} keyed as {first:#04x}");
        }
        // The case that a length prefix below the digit range gets wrong.
        assert_eq!(natural_cmp("a.b", "a1b"), Ordering::Less);
        assert!(key("a.b") < key("a1b"));
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
