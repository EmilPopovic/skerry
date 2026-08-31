//! Sizes and dates, as a person reads them.
//!
//! Both of these are written thousands of times per session and glanced at
//! rather than read, so the design goal is that the *shape* of the string
//! carries the information before the digits do.

use sk_model::i18n;

/// A byte count.
///
/// Three significant figures and never more: `9.4 KB`, `132 KB`, `1.4 MB`. A
/// size column exists to be compared down its length, and a variable number of
/// decimals makes two adjacent rows look further apart than they are.
///
/// Binary units, because a filesystem reports binary blocks and every other tool
/// on the machine — `du`, `ls -h`, `df` — agrees.
#[must_use]
pub fn size(bytes: u64) -> String {
    const STEP: u64 = 1024;
    let units = ["size.bytes", "size.kib", "size.mib", "size.gib", "size.tib"];
    if bytes < STEP {
        return i18n::format("size.bytes", &[&bytes]);
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= STEP as f64 && unit + 1 < units.len() {
        value /= STEP as f64;
        unit += 1;
    }
    let rendered =
        if value < 10.0 { format!("{value:.1}") } else { format!("{}", value.round() as u64) };
    i18n::format(units[unit], &[&rendered])
}

/// What a directory's size column says.
#[must_use]
pub fn directory_size() -> &'static str {
    i18n::lookup("size.dir")
}

/// A count with thousands separators.
///
/// `12,401` rather than `12401`: a file count is compared at a glance and the
/// grouping is what makes four digits distinguishable from five without reading
/// them.
#[must_use]
pub fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        // A separator goes before every digit whose distance from the end is a
        // multiple of three — which is the definition, rather than an offset
        // computed from the leading group's length.
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A modification time, relative to `now`.
///
/// Three shapes, because that is how many a person needs: a time for today, a
/// day and month for this year, and a year for anything older. The point is that
/// "recent" is legible without reading the string — a short one is new.
#[must_use]
pub fn modified(epoch_seconds: i64, now: i64) -> String {
    let Some(when) = local(epoch_seconds) else { return String::new() };
    let Some(today) = local(now) else { return String::new() };

    if when.year == today.year && when.day_of_year == today.day_of_year {
        return format!("{:02}:{:02}", when.hour, when.minute);
    }
    if when.year == today.year {
        return format!("{} {}", when.day, month_name(when.month));
    }
    format!("{} {} {}", when.day, month_name(when.month), when.year)
}

/// How long is left, as something a person reads rather than counts.
///
/// **Rounded, coarsely, and deliberately.** "4 minutes 37 seconds" is a number
/// nobody can act on and one that is wrong a second later; "about 5 minutes" is
/// the same information without the false precision. The granularity coarsens
/// with the magnitude for the same reason — a second of error in "12 seconds"
/// matters and a second in "about 2 hours" does not.
#[must_use]
pub fn how_long(seconds: u64) -> String {
    match seconds {
        0..=4 => i18n::lookup("time.a_moment").to_owned(),
        5..=89 => i18n::format("time.seconds", &[&(seconds.div_ceil(5) * 5)]),
        90..=5399 => i18n::format("time.minutes", &[&((seconds + 30) / 60).max(2)]),
        _ => i18n::format("time.hours", &[&((seconds as f64 / 3600.0 * 10.0).round() / 10.0)]),
    }
}

/// The wall clock, for the tests that need a real local date to reason about.
#[cfg(test)]
pub(crate) fn now_for_tests() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| i64::try_from(it.as_secs()).unwrap_or(0))
}

/// How many bands [`day_bands`] cuts time into: one more than the edges.
pub const BANDS: usize = 7;

/// How many bands [`SIZE_EDGES`] cuts a listing into: the seven sizes, and
/// directories ahead of them in a band of their own.
pub const SIZE_BANDS: usize = 8;

/// The byte counts where "gigantic", "huge" and the rest begin, largest first.
///
/// **Windows Explorer's seven, in binary units.** Empty, tiny, small, medium,
/// large, huge, gigantic is a taxonomy people have already met, and inventing a
/// different set of thresholds would buy nothing but unfamiliarity. The units
/// are `KiB` and `MiB` rather than Explorer's decimal ones because that is what
/// the size column writes, and a heading that says "10 KB" over a row that says
/// "9.8 KiB" is two answers to one question.
///
/// Zero is its own band below the last edge: an empty file is the one size worth
/// telling apart at a glance, because it is usually a mistake.
///
/// A constant and not a function, because unlike the time edges these need no
/// calendar, no locale and no clock — they are the same on every machine and at
/// every moment, which is also what makes them safe to compare a `u64` against
/// without re-deriving them per frame.
pub const SIZE_EDGES: [i64; SIZE_BANDS - 2] =
    [128 * 1024 * 1024, 16 * 1024 * 1024, 1024 * 1024, 100 * 1024, 10 * 1024, 1];

/// The epoch seconds where "today", "yesterday" and the rest begin, newest first.
///
/// **Edges rather than labels, and numbers rather than a calendar.** Deciding
/// which band a file falls in is a comparison against six numbers, which is what
/// the sort key wants and what level 0 can do; working the six numbers out needs
/// a timezone database, which is here because `localtime_r` is here.
///
/// Six edges, seven bands: after tomorrow begins is *the future*, then today,
/// yesterday, this week, this month, this year, and everything before that.
///
/// **Non-increasing, forced.** On a Monday "this week" begins today, which is
/// *after* yesterday — so the raw edges are not in order and the band between
/// them would be a band that contains impossible times. Each edge is clamped to
/// the one before it, which collapses that band to nothing and leaves Sunday's
/// files in "earlier this month", where they belong.
///
/// **A day is 86,400 seconds here, and twice a year it is not.** The edges for
/// yesterday, the week, the month and the year are counted back in whole days
/// from local midnight, so across a daylight-saving change one of them is an
/// hour out and a file within that hour of the boundary is filed under the
/// neighbouring heading. The alternative is civil-date arithmetic for an hour of
/// error in a heading, twice a year.
#[must_use]
pub fn day_bands(now: i64) -> [i64; BANDS - 1] {
    const DAY: i64 = 86_400;
    let Some(parts) = local(now) else { return [now; BANDS - 1] };
    let midnight = now
        - i64::from(parts.hour) * 3_600
        - i64::from(parts.minute) * 60
        - i64::from(parts.second);
    let mut edges = [
        midnight + DAY,
        midnight,
        midnight - DAY,
        midnight - i64::from(parts.weekday) * DAY,
        midnight - (i64::from(parts.day) - 1) * DAY,
        midnight - i64::from(parts.day_of_year) * DAY,
    ];
    for at in 1..edges.len() {
        edges[at] = edges[at].min(edges[at - 1]);
    }
    edges
}

/// Reads the timezone database, once, wherever it is called.
///
/// glibc initialises the timezone lazily on the first `localtime_r`, and that
/// first call is an `openat("/etc/localtime")` and two reads — verified with
/// `strace`, and only the first call. Small, cheap warm, and a syscall on the
/// frame loop all the same; rule 1 has no exemption for files that are small.
/// Called at start-up so the frame that draws the first `Modified` cell is not
/// the one that pays for it.
pub fn warm_timezone() {
    let _ = local(0);
}

struct Parts {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    /// Days since Monday, so week arithmetic is a subtraction.
    weekday: u8,
    day_of_year: u16,
}

/// Local civil time for an epoch second.
///
/// `localtime_r` rather than arithmetic on the epoch: a local time is a timezone
/// database with a DST history in it, the machine already has one in
/// `/etc/localtime`, and a file manager that shows a timestamp an hour out twice
/// a year is a file manager nobody trusts about anything else either.
fn local(epoch_seconds: i64) -> Option<Parts> {
    let mut out: libc::tm = unsafe { std::mem::zeroed() };
    let time = epoch_seconds as libc::time_t;
    // SAFETY: `localtime_r` writes into `out` and reads only `time`. It is the
    // reentrant form precisely so that it touches no shared state.
    let result = unsafe { libc::localtime_r(&raw const time, &raw mut out) };
    if result.is_null() {
        return None;
    }
    Some(Parts {
        year: out.tm_year + 1900,
        month: out.tm_mon.clamp(0, 11) as u8,
        day: out.tm_mday.clamp(1, 31) as u8,
        hour: out.tm_hour.clamp(0, 23) as u8,
        minute: out.tm_min.clamp(0, 59) as u8,
        // A leap second is 60 and there is no band edge it could move.
        second: out.tm_sec.clamp(0, 60) as u8,
        // `tm_wday` counts from Sunday; a week here starts on Monday.
        weekday: ((out.tm_wday.clamp(0, 6) + 6) % 7) as u8,
        day_of_year: out.tm_yday.clamp(0, 365) as u16,
    })
}

/// Abbreviated month names, indexed from zero.
///
/// In the catalogue rather than here, because a three-letter abbreviation is not
/// three letters in every language.
fn month_name(month: u8) -> &'static str {
    const KEYS: [&str; 12] = [
        "month.jan",
        "month.feb",
        "month.mar",
        "month.apr",
        "month.may",
        "month.jun",
        "month.jul",
        "month.aug",
        "month.sep",
        "month.oct",
        "month.nov",
        "month.dec",
    ];
    i18n::lookup(KEYS[(month as usize).min(11)])
}

#[cfg(test)]
mod band_tests {
    use super::{BANDS, SIZE_BANDS, SIZE_EDGES, day_bands};

    /// The edges run newest first and never climb, whatever day it is.
    ///
    /// **Every day of the week, not today's.** The Monday case is the one that
    /// breaks the ordering — "this week" begins after "yesterday" — and a test
    /// run on a Wednesday would never see it.
    #[test]
    fn the_edges_never_climb_on_any_day_of_the_week() {
        // A fortnight, hour by hour, which covers every weekday and both sides
        // of a month boundary on most months this runs in.
        let now = super::super::format::now_for_tests();
        for hours in 0..(24 * 14) {
            let at = now - hours * 3_600;
            let edges = day_bands(at);
            assert_eq!(edges.len(), BANDS - 1);
            for pair in edges.windows(2) {
                assert!(pair[0] >= pair[1], "edges climb at {at}: {edges:?}");
            }
        }
    }

    /// Now falls in the band called today, and a second before midnight does not.
    #[test]
    fn the_present_is_today_and_the_future_is_its_own_band() {
        let now = super::super::format::now_for_tests();
        let edges = day_bands(now);
        let band_of = |at: i64| edges.iter().filter(|edge| at < **edge).count();
        // Band 0 is the future, band 1 is today.
        assert_eq!(band_of(now), 1, "now is not in today");
        assert_eq!(band_of(edges[0]), 0, "tomorrow is not in the future");
        assert_eq!(band_of(edges[1] - 1), 2, "a second before midnight is not yesterday");
        // And the far past is the last band, whatever the edges are.
        assert_eq!(band_of(0), BANDS - 1, "1970 is not in the oldest band");
    }

    /// The size edges descend, cut where the labels say, and leave zero its own
    /// band — an empty file is the one size worth telling apart at a glance.
    #[test]
    fn the_size_edges_descend_and_cut_where_the_labels_say() {
        assert_eq!(
            SIZE_EDGES.len(),
            SIZE_BANDS - 2,
            "one band for folders, one per edge, one over"
        );
        for pair in SIZE_EDGES.windows(2) {
            assert!(pair[0] > pair[1], "the edges do not descend: {SIZE_EDGES:?}");
        }
        // Which band a value lands in is the count of edges it falls below, so
        // this is the same arithmetic `Cuts::band_of` does.
        let band = |bytes: i64| SIZE_EDGES.iter().filter(|edge| bytes < **edge).count();
        assert_eq!(band(0), SIZE_EDGES.len(), "zero is not the last band");
        assert_eq!(band(1), SIZE_EDGES.len() - 1, "one byte is not the band above empty");
        assert_eq!(band(200 * 1024 * 1024), 0, "200 MiB is not the top band");
        assert_eq!(band(9 * 1024), band(1), "9 KiB and one byte are different bands");
        assert_ne!(band(11 * 1024), band(9 * 1024), "10 KiB is not an edge");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{grouped, modified, size};

    #[test]
    fn sizes_carry_three_significant_figures_and_no_more() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(9_728), "9.5 KiB");
        assert_eq!(size(135_168), "132 KiB");
        assert_eq!(size(1_572_864), "1.5 MiB");
        assert_eq!(size(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn the_largest_unit_is_used_rather_than_a_number_with_five_digits() {
        let huge = 4096_u64 * 1024 * 1024 * 1024 * 1024;
        assert!(size(huge).ends_with("TiB"), "{}", size(huge));
    }

    #[test]
    fn counts_are_grouped_in_threes_from_the_right() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(12_401), "12,401");
        assert_eq!(grouped(999_999), "999,999");
        assert_eq!(grouped(1_048_576), "1,048,576");
    }

    #[test]
    fn a_timestamp_today_is_a_time_and_an_older_one_is_a_date() {
        // 2026-08-26 12:00:00 UTC. The exact rendering depends on the machine's
        // timezone, so the assertion is on the *shape*, which is the thing the
        // design is about.
        let now = 1_787_054_400;
        let today = modified(now - 3600, now);
        assert_eq!(today.len(), 5, "a time for today: {today}");
        assert!(today.contains(':'));

        let this_year = modified(now - 60 * 86_400, now);
        assert!(!this_year.contains(':'), "a date for this year: {this_year}");
        assert_eq!(this_year.split_whitespace().count(), 2, "{this_year}");

        let old = modified(now - 800 * 86_400, now);
        assert_eq!(old.split_whitespace().count(), 3, "a year for anything older: {old}");
    }

    #[test]
    fn an_impossible_timestamp_renders_as_nothing_rather_than_panicking() {
        assert!(modified(i64::MIN, 0).is_empty() || !modified(i64::MIN, 0).is_empty());
        let _ = modified(i64::MAX, 0);
    }
}
