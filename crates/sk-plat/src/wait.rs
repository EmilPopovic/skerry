//! How long the frame loop is allowed to sleep.
//!
//! Both backends block in one `poll` and neither has a timer. A timeout exists
//! only because two things have a *next* moment — an animation with another
//! frame, and a held key with another repeat — and the sleep is the shorter of
//! whatever those say.

use std::time::Duration;

pub(crate) fn shorter(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (some, None) | (None, some) => some,
    }
}

pub(crate) fn to_timespec(duration: Duration) -> rustix::fs::Timespec {
    rustix::fs::Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sooner_of_two_deadlines_wins() {
        let soon = Some(Duration::from_millis(5));
        let later = Some(Duration::from_millis(50));
        assert_eq!(shorter(soon, later), soon);
        assert_eq!(shorter(later, soon), soon);
    }

    /// `None` is "sleep until something happens", so it never shortens anything.
    #[test]
    fn no_deadline_at_all_does_not_shorten_one_that_exists() {
        let soon = Some(Duration::from_millis(5));
        assert_eq!(shorter(soon, None), soon);
        assert_eq!(shorter(None, soon), soon);
        assert_eq!(shorter(None, None), None);
    }

    #[test]
    fn a_duration_becomes_seconds_and_nanoseconds() {
        let spec = to_timespec(Duration::from_millis(1_500));
        assert_eq!(spec.tv_sec, 1);
        assert_eq!(spec.tv_nsec, 500_000_000);
    }
}
