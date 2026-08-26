//! Scrolling, and which rows a viewport can see.
//!
//! Two decisions here are the difference between a list that feels attached to
//! the input device and one that does not:
//!
//! * **A touchpad is tracked exactly and a wheel is eased.** A touchpad sends a
//!   real distance and the content must follow it 1:1 — any smoothing on top of
//!   that reads as lag. A wheel sends a notch, which is a jump, and easing the
//!   jump over about 90 ms is the whole difference between a list that snaps and
//!   one that moves.
//! * **The animation advances by real elapsed time**, never by frame count.
//!   Step 2 of the frame loop in
//!   [docs/04-architecture.md](../../../docs/04-architecture.md#the-frame-loop-precisely)
//!   says so, and the reason is that a dropped frame during a fling would
//!   otherwise shorten the fling.

use std::ops::Range;

/// Time constant of the wheel easing, in seconds.
///
/// The offset covers 63% of the remaining distance per `TAU`, so a notch is
/// visually finished in about 3 × 30 ms. Slower reads as syrup; faster is
/// indistinguishable from no easing at all.
const TAU: f32 = 0.030;

/// Below this many logical pixels the animation is over.
///
/// Half a pixel: any smaller and the list would keep requesting frames to move
/// a distance no display can show, which is a hole in "zero CPU at idle".
const SETTLED: f32 = 0.5;

/// A scroll position in logical pixels, with its easing.
///
/// Logical rather than device, so that a scale change does not move the view and
/// so that every layout number in `sk-app` is in the same unit.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scroll {
    offset: f32,
    target: f32,
    max: f32,
}

impl Scroll {
    /// The current position, which is what the view draws at.
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Where the position is heading. Equal to [`Scroll::offset`] when settled.
    #[must_use]
    pub fn target(&self) -> f32 {
        self.target
    }

    /// Whether an animation is in flight, and therefore whether the frame loop
    /// owes another frame.
    #[must_use]
    pub fn animating(&self) -> bool {
        (self.target - self.offset).abs() > SETTLED
    }

    /// Sets the furthest position, clamping the current one into range.
    ///
    /// Called every frame because the content grows while a listing streams in.
    /// Clamping the *target* as well is what stops a fling started at the bottom
    /// of a short listing from stranding the view past the end of a long one.
    pub fn set_extent(&mut self, content: f32, viewport: f32) {
        self.max = (content - viewport).max(0.0);
        self.target = self.target.clamp(0.0, self.max);
        self.offset = self.offset.clamp(0.0, self.max);
    }

    /// Moves by `delta` logical pixels.
    ///
    /// `precise` means the delta came from a touchpad, and is applied to the
    /// visible position immediately with no easing.
    pub fn by(&mut self, delta: f32, precise: bool) {
        self.target = (self.target + delta).clamp(0.0, self.max);
        if precise {
            self.offset = self.target;
        }
    }

    /// Jumps without easing, for `Home`, `End` and a scrollbar drag.
    pub fn to(&mut self, offset: f32) {
        self.target = offset.clamp(0.0, self.max);
        self.offset = self.target;
    }

    /// Scrolls the least amount that brings `top..bottom` fully into view.
    ///
    /// Used by every keyboard move. Not centring: a cursor moved one row down
    /// should move one row, and re-centring the list under it is disorienting.
    pub fn reveal(&mut self, top: f32, bottom: f32, viewport: f32) {
        if top < self.target {
            self.to(top);
        } else if bottom > self.target + viewport {
            self.to(bottom - viewport);
        }
    }

    /// Advances the easing by `elapsed` seconds. Returns whether it is still
    /// running.
    pub fn advance(&mut self, elapsed: f32) -> bool {
        let remaining = self.target - self.offset;
        if remaining.abs() <= SETTLED {
            self.offset = self.target;
            return false;
        }
        // Exponential approach, framed in real time so that a 4 ms frame and a
        // 40 ms frame land in the same place.
        self.offset += remaining * (1.0 - (-elapsed / TAU).exp());
        true
    }
}

/// Which rows a viewport of `height` logical pixels can see.
///
/// One row of overscan on each side, so a partially visible row at either edge
/// is drawn rather than popping in — and so a scroll of one pixel does not have
/// to shape a new row on the frame it becomes visible.
#[must_use]
pub fn visible_rows(offset: f32, height: f32, row_height: f32, rows: usize) -> Range<usize> {
    if rows == 0 || row_height <= 0.0 || height <= 0.0 {
        return 0..0;
    }
    let first = (offset / row_height).floor().max(0.0) as usize;
    let count = (height / row_height).ceil() as usize + 2;
    let first = first.saturating_sub(1);
    first..(first + count).min(rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Scroll, visible_rows};

    #[test]
    fn a_touchpad_delta_lands_immediately_and_a_wheel_delta_eases() {
        let mut scroll = Scroll::default();
        scroll.set_extent(10_000.0, 500.0);
        scroll.by(100.0, true);
        assert!((scroll.offset() - 100.0).abs() < f32::EPSILON, "a touchpad tracks 1:1");
        assert!(!scroll.animating());

        scroll.by(100.0, false);
        assert!((scroll.offset() - 100.0).abs() < f32::EPSILON, "a wheel has not moved yet");
        assert!(scroll.animating());
    }

    #[test]
    fn the_easing_is_framed_in_real_time_and_lands_in_the_same_place() {
        let settle = |steps: u32, step: f32| {
            let mut scroll = Scroll::default();
            scroll.set_extent(10_000.0, 500.0);
            scroll.by(240.0, false);
            for _ in 0..steps {
                scroll.advance(step);
            }
            scroll.offset()
        };
        // 120 ms of animation, once at 4 ms per frame and once at 40 ms.
        let fast = settle(30, 0.004);
        let slow = settle(3, 0.040);
        assert!((fast - slow).abs() < 2.0, "{fast} vs {slow}");
    }

    #[test]
    fn an_animation_ends_rather_than_asymptotically_requesting_frames() {
        let mut scroll = Scroll::default();
        scroll.set_extent(10_000.0, 500.0);
        scroll.by(240.0, false);
        let mut frames = 0;
        while scroll.advance(0.008) {
            frames += 1;
            assert!(frames < 200, "the easing never settled");
        }
        assert!(!scroll.animating());
        assert!((scroll.offset() - 240.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_shrinking_listing_pulls_the_position_back_into_range() {
        let mut scroll = Scroll::default();
        scroll.set_extent(10_000.0, 500.0);
        scroll.to(9_000.0);
        scroll.set_extent(800.0, 500.0);
        assert!((scroll.offset() - 300.0).abs() < f32::EPSILON, "clamped to the new extent");
        assert!(!scroll.animating(), "and the target came with it");
    }

    #[test]
    fn reveal_moves_the_least_it_can_and_not_at_all_when_it_need_not() {
        let mut scroll = Scroll::default();
        scroll.set_extent(2_600.0, 500.0);
        scroll.to(100.0);
        scroll.reveal(200.0, 226.0, 500.0);
        assert!((scroll.offset() - 100.0).abs() < f32::EPSILON, "already visible");
        scroll.reveal(700.0, 726.0, 500.0);
        assert!((scroll.offset() - 226.0).abs() < f32::EPSILON, "scrolled by exactly the overflow");
        scroll.reveal(0.0, 26.0, 500.0);
        assert!((scroll.offset() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn the_visible_range_overscans_by_one_row_at_each_edge() {
        let range = visible_rows(0.0, 100.0, 26.0, 1000);
        assert_eq!(range.start, 0);
        assert!(range.end >= 4, "four rows fit in 100 px, plus overscan: {range:?}");

        let scrolled = visible_rows(260.0, 100.0, 26.0, 1000);
        assert_eq!(scrolled.start, 9, "row 10 is at the top, so row 9 is the overscan");
    }

    #[test]
    fn an_empty_listing_has_no_visible_rows_and_does_not_divide_by_zero() {
        assert_eq!(visible_rows(0.0, 100.0, 26.0, 0), 0..0);
        assert_eq!(visible_rows(0.0, 100.0, 0.0, 10), 0..0);
        assert_eq!(visible_rows(0.0, 0.0, 26.0, 10), 0..0);
    }

    #[test]
    fn the_last_visible_row_never_runs_past_the_listing() {
        let range = visible_rows(10_000.0, 500.0, 26.0, 400);
        assert!(range.end <= 400, "{range:?}");
    }
}
