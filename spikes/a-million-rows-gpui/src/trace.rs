//! The scroll trace.
//!
//! Deterministic and identical between arms, so the two renderers are compared
//! on the same input rather than on two plausible-sounding gestures. The phases
//! are measured separately because a p99 over all of them would be a p99 of
//! whichever phase has the most frames.

use crate::data::Rng;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Discarded. Jumps, so the glyph atlas sees a representative sample of
    /// the listing's scripts: a warmup that only scrolls past the first fifty
    /// rows leaves the atlas cold and charges the next phase for filling it.
    Warmup,
    /// One device pixel per frame. Every visible row is already shaped, so this
    /// isolates the cost of rebuilding the quad list and the subpixel bucket
    /// changing under each glyph.
    Slow,
    /// Forty rows per frame. Every visible row is a layout-cache miss, which is
    /// the frame a fling actually produces.
    Fling,
    /// A seeded jump each frame: no locality at all, in either cache.
    Jump,
    /// A fling with the layout cache invalidated mid-gesture, standing in for a
    /// listing that finishes arriving while the user is scrolling.
    Arrival,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Warmup => "warmup",
            Phase::Slow => "slow",
            Phase::Fling => "fling",
            Phase::Jump => "jump",
            Phase::Arrival => "arrival",
        }
    }

    pub fn measured(self) -> bool {
        self != Phase::Warmup
    }
}

pub const PLAN: &[(Phase, u32)] = &[
    (Phase::Warmup, 120),
    (Phase::Slow, 600),
    (Phase::Fling, 600),
    (Phase::Jump, 300),
    (Phase::Arrival, 300),
];

pub fn total_frames() -> u32 {
    PLAN.iter().map(|(_, frames)| frames).sum()
}

pub struct Trace {
    rng: Rng,
    row_height: f64,
    span: f64,
    frame: u32,
    phase_index: usize,
    phase_frame: u32,
    pub scroll: f64,
}

pub struct Step {
    pub phase: Phase,
    /// The `arrival` phase drops the layout cache on the frames where a listing
    /// would have landed.
    pub invalidate: bool,
    pub finished: bool,
}

impl Trace {
    pub fn new(rows: u64, row_height: f64, viewport_height: f64) -> Self {
        Self {
            rng: Rng::new(0x5CE7_7A11),
            row_height,
            span: (rows as f64 * row_height - viewport_height).max(0.0),
            frame: 0,
            phase_index: 0,
            phase_frame: 0,
            scroll: 0.0,
        }
    }

    pub fn advance(&mut self) -> Step {
        let (phase, frames) = PLAN[self.phase_index];

        match phase {
            Phase::Slow => self.scroll += 1.0,
            Phase::Fling | Phase::Arrival => self.scroll += self.row_height * 40.0,
            Phase::Warmup | Phase::Jump => {
                self.scroll = (self.rng.next() % 1_000_000) as f64 * self.span / 1.0e6
            }
        }
        if self.scroll > self.span {
            self.scroll -= self.span;
        }

        // Every twelfth frame of `arrival`: roughly a 5 Hz stream of listing
        // updates against a 120 Hz frame rate, which is what a directory
        // arriving in batches looks like.
        let invalidate = phase == Phase::Arrival && self.phase_frame % 12 == 0;

        self.frame += 1;
        self.phase_frame += 1;
        let mut finished = false;
        if self.phase_frame >= frames {
            self.phase_frame = 0;
            self.phase_index += 1;
            if self.phase_index >= PLAN.len() {
                finished = true;
                self.phase_index = PLAN.len() - 1;
            }
        }

        Step { phase, invalidate, finished }
    }
}
