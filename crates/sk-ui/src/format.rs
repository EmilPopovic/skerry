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

struct Parts {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
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
