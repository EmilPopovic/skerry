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
    /// A position asked for before there was an extent to clamp it against.
    wanted: Option<f32>,
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

    /// Asks for a position that the extent may not allow yet.
    ///
    /// **The one setter that survives not knowing how long the list is.**
    /// [`Scroll::to`] clamps against `max`, and `max` is zero until a listing has
    /// arrived — so an offset restored from a session before the read lands is
    /// silently set to zero. This one is held until the next
    /// [`Scroll::set_extent`] can honour it, and clamped then.
    pub fn want(&mut self, offset: f32) {
        self.wanted = Some(offset.max(0.0));
    }

    /// Sets the furthest position, clamping the current one into range.
    ///
    /// Called every frame because the content grows while a listing streams in.
    /// Clamping the *target* as well is what stops a fling started at the bottom
    /// of a short listing from stranding the view past the end of a long one.
    pub fn set_extent(&mut self, content: f32, viewport: f32) {
        self.max = (content - viewport).max(0.0);
        // Taken only once the extent is something other than nothing, or a
        // request made before the first row arrived is clamped to zero and lost.
        if self.max > 0.0
            && let Some(wanted) = self.wanted.take()
        {
            self.target = wanted.clamp(0.0, self.max);
            self.offset = self.target;
        }
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

/// The shortest a scrollbar thumb may be, in logical pixels.
///
/// Below this it stops being a thing a pointer can grab.
pub const MIN_THUMB: f32 = 28.0;

/// Where the scrollbar thumb goes, or `None` when there is nothing to scroll.
///
/// Returns its offset from the top of the track and its height, both in logical
/// pixels. **One function because there were two**: the view drew the thumb and
/// the hit test found it, from the same arithmetic written out twice — which is
/// the drift a layout module exists to prevent, and which had already produced
/// two spellings of the same clamp, one of which panicked.
///
/// That panic is the reason for the `min` below: a window short enough that the
/// list is twenty pixels tall asks for `clamp(28.0, 20.0)`, and `f32::clamp`
/// panics on a minimum above its maximum. The floor is a minimum only while
/// there is room for it.
#[must_use]
pub fn thumb(offset: f32, content: f32, track: f32) -> Option<(f32, f32)> {
    if content <= track || track <= 0.0 {
        return None;
    }
    let height = (track * track / content).clamp(MIN_THUMB.min(track), track);
    let extent = (content - track).max(1.0);
    let position = (offset / extent).clamp(0.0, 1.0) * (track - height);
    Some((position, height))
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
    // Clamped to the last row rather than trusted: an offset past the end makes
    // `first` larger than `rows`, and `first..rows` is then a range whose start
    // is after its end. Both callers happen to iterate it, which is harmless;
    // slicing with it panics, and that is one line of new code away.
    let first = (offset / row_height).floor().max(0.0) as usize;
    let first = first.saturating_sub(1).min(rows - 1);
    let count = (height / row_height).ceil() as usize + 2;
    first..(first + count).min(rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Scroll, visible_rows};

    /// A range whose start is past its end is a panic waiting for its first
    /// caller that slices rather than iterates.
    #[test]
    fn an_offset_past_the_end_still_yields_a_range_that_can_be_sliced() {
        for offset in [0.0, 1e9, f32::MAX] {
            let rows = visible_rows(offset, 500.0, 26.0, 400);
            assert!(rows.start <= rows.end, "{offset} gave {rows:?}");
            assert!(rows.end <= 400);
            let _ = &(0..400).collect::<Vec<_>>()[rows];
        }
        assert_eq!(visible_rows(1e9, 500.0, 26.0, 0), 0..0);
    }

    /// A window short enough that the list is shorter than the thumb's own
    /// floor. `f32::clamp` panics when its minimum is above its maximum, and
    /// resizing the window until it did is how this was found.
    #[test]
    fn a_track_shorter_than_the_thumbs_floor_does_not_panic() {
        let (top, height) = super::thumb(0.0, 4000.0, 20.0).expect("something to scroll");
        assert!(height <= 20.0 && height > 0.0, "a {height} thumb in a 20 track");
        assert!(top >= 0.0 && top + height <= 20.0 + f32::EPSILON);
        assert_eq!(super::thumb(0.0, 100.0, 500.0), None, "nothing to scroll");
        assert_eq!(super::thumb(0.0, 4000.0, 0.0), None, "no track at all");
    }

    /// The thumb reaches the bottom of the track exactly when the scroll does,
    /// or the last row is unreachable by dragging.
    #[test]
    fn the_thumb_spans_the_track_from_end_to_end() {
        let (top, height) = super::thumb(0.0, 4000.0, 500.0).expect("a thumb");
        assert!(top.abs() < f32::EPSILON);
        let (bottom, _) = super::thumb(3500.0, 4000.0, 500.0).expect("a thumb");
        assert!((bottom + height - 500.0).abs() < 0.01, "ends at {}", bottom + height);
    }

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
