//! A ring buffer of recent frame times.
//!
//! [docs/04-architecture.md](../../../docs/04-architecture.md#observability)
//! asks for an always-on frame profiler with a few seconds of history. This is
//! the smallest honest version: fixed capacity, no allocation after start-up,
//! and percentiles rather than a mean.
//!
//! **Percentiles, not a mean**, because
//! [docs/12-measurement.md](../../../docs/12-measurement.md) says so and because
//! the interesting frame is the slow one. A mean over a scroll hides exactly the
//! frame that made the scroll feel bad.

/// Frames kept. At 120 Hz this is four seconds, which is long enough to hold a
/// whole flick gesture and short enough that the percentile follows the present
//  rather than the session.
const CAPACITY: usize = 480;

/// Recent frame costs, in microseconds.
pub struct Frames {
    samples: Vec<u32>,
    next: usize,
    filled: usize,
    /// Scratch for the percentile sort, so reading a percentile allocates
    /// nothing.
    sorted: Vec<u32>,
    worst: u32,
    over_budget: u64,
    total: u64,
}

/// The frame budget, in microseconds — the 8 ms row in
/// [docs/09-performance-budgets.md](../../../docs/09-performance-budgets.md).
pub const BUDGET_US: u32 = 8_000;

impl Frames {
    pub fn new() -> Self {
        Self {
            samples: vec![0; CAPACITY],
            next: 0,
            filled: 0,
            sorted: Vec::with_capacity(CAPACITY),
            worst: 0,
            over_budget: 0,
            total: 0,
        }
    }

    pub fn record(&mut self, microseconds: u64) {
        let value = microseconds.min(u64::from(u32::MAX)) as u32;
        self.samples[self.next] = value;
        self.next = (self.next + 1) % CAPACITY;
        self.filled = (self.filled + 1).min(CAPACITY);
        self.worst = self.worst.max(value);
        self.total += 1;
        if value > BUDGET_US {
            self.over_budget += 1;
        }
    }

    /// The `fraction` percentile of the window, in microseconds.
    ///
    /// Nearest rank, which is the convention every latency report uses: the p99
    /// of one hundred samples is the ninety-ninth of them, so a single outlier in
    /// a hundred is **not** the p99. That is the point of also reporting
    /// [`Frames::worst`], which no window forgets.
    pub fn percentile(&mut self, fraction: f32) -> u32 {
        if self.filled == 0 {
            return 0;
        }
        self.sorted.clear();
        self.sorted.extend_from_slice(&self.samples[..self.filled]);
        self.sorted.sort_unstable();
        let rank = (self.sorted.len() as f32 * fraction.clamp(0.0, 1.0)).ceil() as usize;
        let index = rank.max(1) - 1;
        self.sorted.get(index.min(self.sorted.len() - 1)).copied().unwrap_or(0)
    }

    /// The worst frame since start-up, which no window can forget.
    pub fn worst(&self) -> u32 {
        self.worst
    }

    /// Frames drawn, and how many of them missed the budget.
    pub fn budget(&self) -> (u64, u64) {
        (self.total, self.over_budget)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{BUDGET_US, CAPACITY, Frames};

    #[test]
    fn a_percentile_is_the_nearest_rank_of_the_window() {
        let mut frames = Frames::new();
        for value in 1..=100u64 {
            frames.record(value);
        }
        assert_eq!(frames.percentile(0.5), 50);
        assert_eq!(frames.percentile(0.99), 99);
        assert_eq!(frames.percentile(1.0), 100);
    }

    /// The semantics worth stating out loud, because a reader will expect the
    /// other one: one slow frame in a hundred does not move the p99, and that is
    /// exactly why the worst frame is reported beside it.
    #[test]
    fn one_outlier_in_a_hundred_is_not_the_p99_but_is_the_worst() {
        let mut frames = Frames::new();
        for _ in 0..99 {
            frames.record(1_000);
        }
        frames.record(30_000);
        assert_eq!(frames.percentile(0.5), 1_000);
        assert_eq!(frames.percentile(0.99), 1_000);
        assert_eq!(frames.worst(), 30_000);
    }

    #[test]
    fn ten_slow_frames_in_a_hundred_do_move_the_p99() {
        let mut frames = Frames::new();
        for _ in 0..90 {
            frames.record(1_000);
        }
        for _ in 0..10 {
            frames.record(30_000);
        }
        assert_eq!(frames.percentile(0.99), 30_000);
    }

    #[test]
    fn the_window_forgets_but_the_worst_frame_and_the_misses_do_not() {
        let mut frames = Frames::new();
        frames.record(u64::from(BUDGET_US) + 1);
        for _ in 0..CAPACITY {
            frames.record(500);
        }
        assert_eq!(frames.percentile(1.0), 500, "the slow frame has left the window");
        assert_eq!(frames.worst(), BUDGET_US + 1, "and is still the worst on record");
        assert_eq!(frames.budget().1, 1, "and is still counted as a miss");
    }

    #[test]
    fn an_empty_profiler_reports_zero_rather_than_panicking() {
        let mut frames = Frames::new();
        assert_eq!(frames.percentile(0.5), 0);
        assert_eq!(frames.budget(), (0, 0));
    }
}
