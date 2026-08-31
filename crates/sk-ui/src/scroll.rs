//! Scrolling, and which rows a viewport can see.
//!
//! Three decisions here are the difference between a list that feels attached to
//! the input device and one that does not:
//!
//! * **A touchpad is tracked exactly and a wheel is eased.** A touchpad sends a
//!   real distance and the content must follow it 1:1 — any smoothing on top of
//!   that reads as lag. A wheel sends a notch, which is a jump, and easing the
//!   jump over about 90 ms is the whole difference between a list that snaps and
//!   one that moves.
//! * **A finger that leaves the pad while still moving throws the list.** The
//!   distance a flick travels is its velocity when the fingers lifted times
//!   [`FLING_TAU`], because a velocity decaying as `e^(-t/τ)` covers `v·τ` in
//!   total — which is exactly the distance the easing below already covers when
//!   the target is that far away. So a fling is not a second animation. It is
//!   this one with a longer time constant and a target the finger chose.
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

/// Time constant of a fling's decay, in seconds.
///
/// Both the friction and the reach, since the two are the same number: a flick
/// of 3,000 logical pixels a second — a vigorous one — travels 750 px, about
/// twenty-nine rows at the default row height, and is visually done in a second
/// and a half.
const FLING_TAU: f32 = 0.25;

/// The window a finger's velocity is averaged over, in seconds.
///
/// Fifty milliseconds is about six events from a 125 Hz touchpad: short enough
/// that the number describes the end of the gesture rather than its middle, long
/// enough that one jittery event cannot decide where the list lands.
const VELOCITY_TAU: f32 = 0.050;

/// Below this many logical pixels a second, a lift is not a flick.
///
/// 150 px/s would travel 37 px, under a row and a half. Drifting to a stop over
/// a distance that small reads as the list failing to settle, so it settles.
const FLING_MIN: f32 = 150.0;

/// How close a fling has to get before it is over, in logical pixels.
///
/// Looser than [`SETTLED`] because an exponential's tail is long: from 750 px at
/// [`FLING_TAU`] the last half-pixel is a further 800 ms of crawl at a fraction
/// of a pixel per frame, which reads as the list sticking rather than stopping.
/// A wheel notch has no tail worth the same licence.
const FLING_SETTLED: f32 = 1.5;

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
    /// Whether the animation in flight is a fling rather than a wheel's easing,
    /// which is the only thing that differs between them.
    ///
    /// **Set by [`Scroll::released`] and cleared by every setter that moves the
    /// target away from the offset**, which is the whole of [`Scroll::by`] and
    /// [`Scroll::to`]. A new setter that forgets is a wheel notch eased over a
    /// fling's time constant, eight times too long.
    flinging: bool,
    /// The finger's estimated speed in logical pixels a second, positive down.
    velocity: f32,
    /// Distance travelled since the last sample that had a span to divide by.
    ///
    /// Two events inside one millisecond have no span, and the timestamps are
    /// milliseconds because that is what both display servers report. Holding the
    /// distance rather than dropping it means a 1,000 Hz pad measures the same
    /// speed as a 125 Hz one instead of losing half its travel.
    pending: f32,
    /// The display server's millisecond clock at the last sample.
    ///
    /// **The device's clock, not this process's.** A velocity taken from
    /// `Instant::now()` is a velocity divided by however late the frame loop was,
    /// and this loop is allowed to be late — a flick that lands in the same pass
    /// as a 200,000-entry listing would arrive as four events in the same instant
    /// and throw the list three times as far as the finger asked. Wraps every 49
    /// days on both servers, hence `wrapping_sub`, which is the same reasoning
    /// the click counter in `sk-plat` records.
    sampled: Option<u32>,
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
        (self.target - self.offset).abs() > self.settled()
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

    /// Moves by `delta` logical pixels, `at` the moment the event arrived.
    ///
    /// `precise` means the delta came from a touchpad, and is applied to the
    /// visible position immediately with no easing. `at` is only read for a
    /// precise delta, where it is what makes a velocity — and therefore a fling
    /// — knowable; a wheel notch is a distance with no speed attached.
    pub fn by(&mut self, delta: f32, precise: bool, at: u32) {
        // A zero delta is a frame some compositors send beside `axis_stop`. It
        // must not read as a finger that has come to rest, and it must not stop
        // a fling that the same frame is about to start.
        if delta == 0.0 {
            return;
        }
        if self.flinging {
            // A finger back on the pad stops the list where it is. Adding to a
            // target the fling has not reached yet would jump the view forward
            // by however much of the fling was left.
            self.target = self.offset;
            self.flinging = false;
        }
        let before = self.target;
        self.target = (self.target + delta).clamp(0.0, self.max);
        if precise {
            self.offset = self.target;
            // The distance actually travelled rather than the one asked for, so
            // a flick against the end of the list carries no velocity into a
            // fling that could not move anyway.
            self.sample(self.target - before, at);
        }
    }

    /// The fingers left the touchpad. Starts a fling if they were still moving.
    ///
    /// **A finger that came to rest before it left is not a flick.** Nothing is
    /// sent while a finger rests, so the last velocity this saw can be seconds
    /// old and pointing anywhere; it is decayed by the age of the sample it came
    /// from, which takes 63% of it away over one [`VELOCITY_TAU`] and all but 2%
    /// over four.
    pub fn released(&mut self, at: u32) {
        self.pending = 0.0;
        let Some(last) = self.sampled.take() else {
            // No sample means no gesture *here*: a lift belonging to a panel
            // that took the wheel, or a second `axis_stop` for the same finger.
            // Without this the velocity of the last real flick is still on hand
            // and the list throws itself again, under whatever is covering it.
            self.velocity = 0.0;
            return;
        };
        let idle = at.wrapping_sub(last) as f32 / 1_000.0;
        self.velocity *= (-idle / VELOCITY_TAU).exp();
        if self.velocity.abs() < FLING_MIN {
            self.velocity = 0.0;
            return;
        }
        self.target = (self.offset + self.velocity * FLING_TAU).clamp(0.0, self.max);
        self.flinging = true;
    }

    /// Jumps without easing, for `Home`, `End` and a scrollbar drag.
    pub fn to(&mut self, offset: f32) {
        self.target = offset.clamp(0.0, self.max);
        self.offset = self.target;
        // A keystroke or a grabbed thumb outranks a fling: the list goes where it
        // was sent and does not carry on afterwards.
        self.flinging = false;
    }

    /// Folds one precise delta into the velocity estimate.
    ///
    /// Weighted by how much of the averaging window the sample covers, so a 2 ms
    /// event and a 20 ms one do not carry the same authority. The first sample of
    /// a gesture only starts the clock: one event is a distance, not a speed.
    fn sample(&mut self, moved: f32, at: u32) {
        self.pending += moved;
        let Some(last) = self.sampled else {
            // The first sample of a gesture only starts the clock, and the
            // gesture before it does not get to lend it a speed: two flicks in a
            // row would otherwise compound into a third the finger never made.
            self.sampled = Some(at);
            self.velocity = 0.0;
            self.pending = 0.0;
            return;
        };
        let elapsed = at.wrapping_sub(last);
        if elapsed == 0 {
            // No span to divide by. `pending` keeps the distance for the next
            // event that has one.
            return;
        }
        self.sampled = Some(at);
        let span = elapsed as f32 / 1_000.0;
        // **Clamped at one, which is what makes a stale sample harmless.** Any
        // span of a whole [`VELOCITY_TAU`] or more replaces the velocity outright
        // instead of averaging with it, so a gesture the display server never
        // ended — X11 sends no stop at all — cannot lend its speed to the next
        // one however long the window sat idle in between.
        let weight = (span / VELOCITY_TAU).min(1.0);
        self.velocity += (std::mem::take(&mut self.pending) / span - self.velocity) * weight;
    }

    /// The time constant in flight: a wheel notch is a jump to smooth over and a
    /// fling is a velocity to spend.
    const fn tau(&self) -> f32 {
        if self.flinging { FLING_TAU } else { TAU }
    }

    /// How close is close enough for the animation in flight.
    const fn settled(&self) -> f32 {
        if self.flinging { FLING_SETTLED } else { SETTLED }
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
        if remaining.abs() <= self.settled() {
            self.offset = self.target;
            return false;
        }
        // Exponential approach, framed in real time so that a 4 ms frame and a
        // 40 ms frame land in the same place.
        self.offset += remaining * (1.0 - (-elapsed / self.tau()).exp());
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

    use super::{FLING_TAU, Scroll, visible_rows};

    /// The device clock the tests start from, chosen to be a long way into the
    /// 49-day wrap so that nothing here depends on an origin of zero.
    const T0: u32 = 4_000_000_000;

    /// One touchpad event every 8 ms, which is what a 125 Hz pad sends, followed
    /// by the lift. Returns the scroll and the millisecond the fingers left at.
    fn flick(speed: f32, events: u32) -> (Scroll, u32) {
        let mut scroll = Scroll::default();
        scroll.set_extent(100_000.0, 500.0);
        for index in 1..=events {
            scroll.by(speed * 0.008, true, T0.wrapping_add(index * 8));
        }
        let lifted = T0.wrapping_add(events * 8);
        scroll.released(lifted);
        (scroll, lifted)
    }

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
        scroll.by(100.0, true, T0);
        assert!((scroll.offset() - 100.0).abs() < f32::EPSILON, "a touchpad tracks 1:1");
        assert!(!scroll.animating());

        scroll.by(100.0, false, T0);
        assert!((scroll.offset() - 100.0).abs() < f32::EPSILON, "a wheel has not moved yet");
        assert!(scroll.animating());
    }

    #[test]
    fn the_easing_is_framed_in_real_time_and_lands_in_the_same_place() {
        let settle = |steps: u32, step: f32| {
            let mut scroll = Scroll::default();
            scroll.set_extent(10_000.0, 500.0);
            scroll.by(240.0, false, T0);
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
        scroll.by(240.0, false, T0);
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

    /// The reach of a flick is its velocity times the time constant, and it is
    /// proportional to the speed of the finger rather than to the number of
    /// events the pad happened to send.
    #[test]
    fn a_finger_that_leaves_while_moving_throws_the_list() {
        let (slow, _) = flick(2_000.0, 20);
        let reach = slow.target() - slow.offset();
        assert!(reach > 300.0 && reach < 600.0, "2,000 px/s threw the list {reach} px");

        let (fast, _) = flick(4_000.0, 20);
        let further = fast.target() - fast.offset();
        assert!(
            (further / reach - 2.0).abs() < 0.05,
            "twice the speed threw it {} times as far",
            further / reach
        );
        // Both are still animating, or nothing was thrown at all.
        assert!(slow.animating() && fast.animating());
    }

    /// **The case that makes a fling feel possessed.** Nothing is sent while a
    /// finger rests on the pad, so the last velocity seen can be arbitrarily old;
    /// a lift after a pause has to fling by nothing at all.
    #[test]
    fn a_finger_that_came_to_rest_before_it_left_throws_nothing() {
        let mut scroll = Scroll::default();
        scroll.set_extent(100_000.0, 500.0);
        for index in 1..=20 {
            scroll.by(16.0, true, T0.wrapping_add(index * 8));
        }
        let settled = scroll.offset();
        // The fingers sat still for 300 ms and then left.
        scroll.released(T0.wrapping_add(20 * 8 + 300));
        assert!(!scroll.animating(), "a rest of 300 ms still threw the list");
        assert!((scroll.target() - settled).abs() < f32::EPSILON);
    }

    /// One event is a distance, not a speed.
    #[test]
    fn one_event_and_a_lift_is_not_a_flick() {
        let (scroll, _) = flick(4_000.0, 1);
        assert!(!scroll.animating(), "a single event threw the list");
    }

    /// A finger back on the pad stops the list where it is.
    ///
    /// Adding to a target the fling had not reached would jump the view forward
    /// by whatever was left of it, and it moves from where it stands instead.
    #[test]
    fn a_finger_back_on_the_pad_stops_a_fling_where_it_stands() {
        let (mut scroll, lifted) = flick(3_000.0, 20);
        for _ in 0..6 {
            scroll.advance(0.008);
        }
        let standing = scroll.offset();
        assert!(scroll.target() - standing > 100.0, "the fling had somewhere left to go");

        scroll.by(9.0, true, lifted.wrapping_add(100));
        assert!(
            (scroll.offset() - (standing + 9.0)).abs() < 0.01,
            "the list jumped to {} instead of {}",
            scroll.offset(),
            standing + 9.0
        );
        assert!(!scroll.animating(), "and it is following the finger, not coasting");
    }

    /// A fling into the end of the list stops at the end, and stops asking for
    /// frames — the same hole in "zero CPU at idle" that [`super::SETTLED`]
    /// exists to close.
    #[test]
    fn a_fling_into_the_end_of_a_short_listing_settles_there() {
        let mut scroll = Scroll::default();
        scroll.set_extent(900.0, 500.0);
        for index in 1..=20 {
            scroll.by(48.0, true, T0.wrapping_add(index * 8));
        }
        scroll.released(T0.wrapping_add(20 * 8));
        let mut frames = 0;
        while scroll.advance(0.008) {
            frames += 1;
            assert!(frames < 1_000, "the fling never settled");
        }
        assert!((scroll.offset() - 400.0).abs() < f32::EPSILON, "at {}", scroll.offset());
    }

    /// **A fling that has landed is over.** The time constant is the only thing
    /// that differs between the two animations, so a flag left set after one
    /// settles would ease the next wheel notch over eight times too long.
    #[test]
    fn a_landed_fling_leaves_the_next_wheel_notch_eased_as_a_wheel_notch() {
        let (mut scroll, _) = flick(3_000.0, 20);
        while scroll.advance(0.008) {}
        assert!(!scroll.animating());

        let from = scroll.offset();
        scroll.by(66.0, false, T0.wrapping_add(9_000));
        scroll.advance(FLING_TAU);
        // One fling time constant covers 63% of a fling and all but a rounding
        // error of a wheel notch, which is eight wheel time constants long.
        let covered = (scroll.offset() - from) / 66.0;
        assert!(covered > 0.99, "a notch was only {}% delivered in {FLING_TAU} s", covered * 100.0);
    }

    /// **Milliseconds are what both display servers report**, and a pad that
    /// reports faster than one has events that share a timestamp. Dropping their
    /// distance rather than carrying it to the next event with a span would make
    /// a 2,000 Hz pad measure half the speed a 125 Hz one does.
    #[test]
    fn a_pad_reporting_twice_a_millisecond_measures_the_speed_a_slower_one_does() {
        let (coarse, _) = flick(2_000.0, 20);
        let by_eights = coarse.target() - coarse.offset();

        // The same 160 ms at the same 2,000 px/s, from a pad reporting every half
        // millisecond: every second event shares a timestamp with the one before.
        let mut scroll = Scroll::default();
        scroll.set_extent(100_000.0, 500.0);
        for index in 1..=320_u32 {
            scroll.by(1.0, true, T0.wrapping_add(index / 2));
        }
        scroll.released(T0.wrapping_add(160));
        let by_halves = scroll.target() - scroll.offset();
        assert!(
            (by_halves / by_eights - 1.0).abs() < 0.05,
            "a half-millisecond pad threw {by_halves} where an eight-millisecond one threw {by_eights}"
        );
    }

    /// **A gesture nobody ended does not join the next one.** X11 reports no
    /// source and no stop, and a compositor need not send one either, so the
    /// sample from a gesture that was never lifted is still on hand when the next
    /// one starts. What holds this is the weight clamp in `sample`: a span of a
    /// whole averaging window or more replaces the velocity rather than blending
    /// with it, and the gap between two gestures is always longer than that.
    #[test]
    fn a_gesture_with_no_lift_under_it_does_not_bleed_into_the_next() {
        let mut scroll = Scroll::default();
        scroll.set_extent(100_000.0, 500.0);
        // A hard flick, and no lift reported at the end of it.
        for index in 1..=20 {
            scroll.by(32.0, true, T0.wrapping_add(index * 8));
        }
        // Two seconds later, three slow events and a lift.
        let later = T0.wrapping_add(2_000);
        for index in 1..=3 {
            scroll.by(4.0, true, later.wrapping_add(index * 8));
        }
        scroll.released(later.wrapping_add(3 * 8));
        assert!(
            !scroll.animating(),
            "the flick before it threw the list {} px",
            scroll.target() - scroll.offset()
        );
    }

    /// A lift that belongs to something else — a panel that took the wheel, or a
    /// second stop for the same finger — throws nothing, however hard the last
    /// real flick was.
    #[test]
    fn a_lift_with_no_gesture_under_it_throws_nothing() {
        let (mut scroll, lifted) = flick(4_000.0, 20);
        while scroll.advance(0.008) {}
        let landed = scroll.offset();

        scroll.released(lifted.wrapping_add(3_000));
        assert!(!scroll.animating(), "a stray lift threw the list again");
        assert!((scroll.offset() - landed).abs() < f32::EPSILON);
    }

    /// **A gesture is thrown by its own speed.** A velocity carried over from
    /// the flick before it turns a slow drag that should not fling at all into a
    /// throw of several hundred pixels, in whatever direction the last flick went.
    #[test]
    fn a_slow_drag_after_a_hard_flick_throws_nothing() {
        let (mut scroll, _) = flick(4_000.0, 20);
        while scroll.advance(0.008) {}

        // Three events at 500 px/s: too slow and too short to be a flick.
        let base = T0.wrapping_add(10_000);
        for index in 1..=3 {
            scroll.by(4.0, true, base.wrapping_add(index * 8));
        }
        scroll.released(base.wrapping_add(3 * 8));
        assert!(
            !scroll.animating(),
            "a slow drag threw the list {} px",
            scroll.target() - scroll.offset()
        );
    }

    /// A zero-valued axis frame is not a finger, and a fling in flight must
    /// survive one. Without the guard it takes the same path a returning finger
    /// does and stops the list dead, one frame after it was thrown.
    #[test]
    fn a_zero_delta_does_not_cancel_a_fling_in_flight() {
        let (mut scroll, lifted) = flick(2_000.0, 20);
        let thrown = scroll.target();
        scroll.advance(0.008);
        scroll.by(0.0, true, lifted.wrapping_add(8));
        assert!((scroll.target() - thrown).abs() < f32::EPSILON, "the zero frame stopped it");
        assert!(scroll.animating());
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
