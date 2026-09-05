// SPDX-License-Identifier: Apache-2.0
//! Input policy that must be identical on both display servers.
//!
//! A wheel notch, a double-click window, and how fast a held key repeats are not
//! protocol facts — they are decisions, and a person moving between an X11 and a
//! Wayland session on the same machine must not be able to feel which one they
//! are in. Two copies of a constant is two copies that agree until one is
//! edited, so there is one.
//!
//! What is *not* here is anything a server states: Wayland sends its own repeat
//! rate on `wl_keyboard.repeat_info` and that is honoured, because a person who
//! set it meant it. X11 states one too, in `XkbGetControls`, and it is honoured
//! the same way. The defaults below are what applies when neither says.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use xkbcommon::xkb;

use crate::event::{Event, KeyEvent, Modifiers, Point, Swipe};

/// A wheel notch, in logical pixels.
///
/// Three lines of about 22 logical pixels, which is what GTK and Qt both settle
/// on and therefore what a user's hands already expect. A touchpad sends a real
/// distance and does not go through this.
pub(crate) const WHEEL_NOTCH: f32 = 66.0;

/// How close in time and space two presses must be to count as a double click.
///
/// 400 ms is the freedesktop default; 5 logical pixels is enough slack for a
/// hand on a touchpad and tight enough that a press at the next row is a fresh
/// click.
pub(crate) const MULTI_CLICK_WINDOW_MS: u32 = 400;
pub(crate) const MULTI_CLICK_SLOP: f32 = 5.0;

/// What a held key does when the server has not said.
pub(crate) const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(600);
pub(crate) const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(40);

/// What a key types, if anything.
///
/// Not what it *is*: `Ctrl+C` types nothing a text field should insert, and
/// `Escape` reports a control character. Here rather than in each backend
/// because it is a decision — and because the two copies it replaces had already
/// drifted: the X11 half let `Alt` through, so `Alt+D` typed a `d` into the path
/// field the same chord had just opened.
pub(crate) fn typed(text: String, mods: Modifiers) -> Option<String> {
    (!text.is_empty() && !mods.ctrl && !mods.alt && !mods.logo)
        .then_some(text)
        .filter(|text| !text.chars().any(char::is_control))
}

/// Dead keys and Compose sequences, from the user's own `Compose` file.
///
/// **The half of input-method support that needs no protocol.** `text-input-v3`
/// is how a Wayland compositor drives a full input method, and X11's equivalent
/// is XIM; neither is involved in `Compose + a + '` producing `á`, which
/// libxkbcommon does from the same file every other toolkit reads. So it works
/// on both display servers, with no server support at all, and it is what most
/// of Europe actually needs from an input method.
pub(crate) struct Composer {
    state: Option<xkb::compose::State>,
}

/// What a keysym did to a compose sequence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Composed {
    /// Nothing to do with a sequence. Use the key as it is.
    Passthrough,
    /// Part of one, with nothing to show yet. The key types nothing.
    ///
    /// A cancelled sequence is this too: both mean the key produced no text,
    /// and a cancellation that typed the last dead key would be worse than one
    /// that types nothing.
    Swallowed,
    /// A sequence finished, and this is what it produced.
    Typed(String),
}

impl Composer {
    /// A composer over a table written out here, for a test.
    ///
    /// The locale's own table is whatever this machine has installed, which is a
    /// different table on a runner and on a laptop — so what the sequences *are*
    /// is stated where it is asserted about.
    #[cfg(test)]
    pub(crate) fn from_rules(context: &xkb::Context, rules: &str) -> Self {
        let table = xkb::compose::Table::new_from_buffer(
            context,
            rules.as_bytes(),
            "C",
            xkb::compose::FORMAT_TEXT_V1,
            xkb::compose::COMPILE_NO_FLAGS,
        )
        .ok();
        Self {
            state: table
                .map(|table| xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS)),
        }
    }

    /// Reads the table for the current locale.
    ///
    /// `None` where there is none — a `LANG` nothing is installed for, or a
    /// malformed `~/.XCompose` — and then every key passes through. A missing
    /// compose table is a keyboard without dead keys, not a failure to start.
    pub(crate) fn new(context: &xkb::Context) -> Self {
        let locale = std::ffi::OsString::from(locale());
        let table =
            xkb::compose::Table::new_from_locale(context, &locale, xkb::compose::COMPILE_NO_FLAGS)
                .ok();
        let state =
            table.map(|table| xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS));
        Self { state }
    }

    /// Feeds one keysym, and says what the key should now type.
    ///
    /// **Modifiers are not filtered out here.** The received wisdom is that they
    /// must be, and it is wrong: `xkbcommon-compose.h` says "some keysyms, such
    /// as keysyms for modifier keys, are ignored", and feeding `Shift_L` in the
    /// middle of a sequence was measured returning `Ignored` and leaving the
    /// status at `Composing`. A guard would be a guard against nothing.
    ///
    /// **The reset after a finished sequence is not.** An *ignored* keysym does
    /// not trigger the automatic reset that a real one does, so a `Shift` press
    /// after a sequence completes leaves the status at `Composed` — and a caller
    /// that had not reset would read the same character out of it a second time.
    pub(crate) fn feed(&mut self, keysym: u32) -> Composed {
        let Some(state) = self.state.as_mut() else { return Composed::Passthrough };
        state.feed(keysym.into());
        match state.status() {
            xkb::compose::Status::Composing => Composed::Swallowed,
            xkb::compose::Status::Composed => {
                let text = state.utf8().unwrap_or_default();
                state.reset();
                if text.is_empty() { Composed::Swallowed } else { Composed::Typed(text) }
            }
            xkb::compose::Status::Cancelled => {
                state.reset();
                Composed::Swallowed
            }
            xkb::compose::Status::Nothing => Composed::Passthrough,
        }
    }
}

/// The locale the compose table is read for.
///
/// The three variables in the order POSIX gives them, which is the order every
/// other toolkit reads them in — and `C` when none is set, which has a table
/// with the Unicode sequences in it.
fn locale() -> String {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return value;
        }
    }
    "C".to_owned()
}

/// How far a swipe travels before it means anything, in logical pixels.
///
/// Far enough that resting three fingers and shifting a little is not a
/// navigation, short enough that a deliberate flick is one. 96 is four rows of
/// the list, and it is what a hand on a touchpad crosses in about a fifth of a
/// second.
const SWIPE_THRESHOLD: f32 = 96.0;

/// How much a pinch must grow or shrink to be one step of zoom.
///
/// A ratio rather than a distance, because a pinch is scale-free: 1.15 is about
/// a centimetre of finger travel on a typical pad and gives four steps across a
/// full open-handed pinch.
const PINCH_STEP: f32 = 1.15;

/// Turns a stream of touchpad deltas into discrete gestures.
///
/// **Here rather than in either backend.** Wayland delivers
/// `zwp_pointer_gestures_v1` and X11 delivers `XInput` 2.4, and the two say the
/// same things in different units and different phases — but how far a swipe has
/// to travel to count, and how much of a pinch is one step of zoom, are
/// decisions. Two copies would agree until one was edited.
#[derive(Default)]
pub(crate) struct Gestures {
    fingers: u32,
    dx: f32,
    dy: f32,
    /// The scale the last step was emitted at, or `None` between pinches.
    stepped: Option<f32>,
}

impl Gestures {
    pub(crate) fn swipe_begin(&mut self, fingers: u32) {
        self.fingers = fingers;
        self.dx = 0.0;
        self.dy = 0.0;
    }

    pub(crate) fn swipe_update(&mut self, dx: f32, dy: f32) {
        self.dx += dx;
        self.dy += dy;
    }

    /// The gesture, if it went far enough and was not cancelled.
    ///
    /// **On the dominant axis only.** A hand sweeping right also drifts down,
    /// and a swipe that counted as both would navigate and scroll at once.
    pub(crate) fn swipe_end(&mut self, cancelled: bool) -> Option<(Swipe, u32)> {
        let (dx, dy, fingers) = (self.dx, self.dy, self.fingers);
        self.dx = 0.0;
        self.dy = 0.0;
        self.fingers = 0;
        if cancelled {
            return None;
        }
        let direction = if dx.abs() >= dy.abs() {
            let way = if dx > 0.0 { Swipe::Right } else { Swipe::Left };
            (dx.abs() >= SWIPE_THRESHOLD).then_some(way)
        } else {
            let way = if dy > 0.0 { Swipe::Down } else { Swipe::Up };
            (dy.abs() >= SWIPE_THRESHOLD).then_some(way)
        };
        direction.map(|it| (it, fingers))
    }

    pub(crate) fn pinch_begin(&mut self) {
        self.stepped = Some(1.0);
    }

    /// How many steps of zoom this scale is past the last one, and which way.
    ///
    /// A pinch reports an absolute scale against where it started, so the step
    /// is a ratio against the scale the last step was emitted at — which is what
    /// makes a slow pinch and a fast one cover the same number of steps.
    pub(crate) fn pinch_update(&mut self, scale: f32) -> Option<bool> {
        let last = self.stepped?;
        if scale >= last * PINCH_STEP {
            self.stepped = Some(last * PINCH_STEP);
            return Some(true);
        }
        if scale <= last / PINCH_STEP {
            self.stepped = Some(last / PINCH_STEP);
            return Some(false);
        }
        None
    }

    pub(crate) fn pinch_end(&mut self) {
        self.stepped = None;
    }
}

/// Counts presses into single, double and triple clicks.
///
/// Counted here rather than in a widget because the threshold is a platform
/// convention: getting it wrong turns six drags into six double-clicks, which is
/// what happened in Spike D.
#[derive(Default)]
pub(crate) struct Clicks {
    last: Option<(u32, Point, u8)>,
}

impl Clicks {
    /// How many clicks this press makes, counting the ones before it.
    ///
    /// `time` is the server's millisecond clock, which wraps every 49 days on
    /// both servers — hence `wrapping_sub` rather than a subtraction that would
    /// panic in debug once a month and a half.
    pub(crate) fn count(&mut self, time: u32, position: Point) -> u8 {
        let count = match self.last {
            Some((last_time, last_position, count))
                if time.wrapping_sub(last_time) <= MULTI_CLICK_WINDOW_MS
                    && (position.x - last_position.x).abs() <= MULTI_CLICK_SLOP
                    && (position.y - last_position.y).abs() <= MULTI_CLICK_SLOP =>
            {
                count.saturating_add(1)
            }
            _ => 1,
        };
        self.last = Some((time, position, count));
        count
    }
}

/// A key being held, and when its next repeat is due.
struct Held {
    event: KeyEvent,
    keysym: u32,
    next: Instant,
    interval: Duration,
}

/// Client-side key repeat.
///
/// Both servers leave this to the client. Wayland says so in the protocol;
/// X11 *can* repeat in the server, and this backend turns that off with XKB's
/// detectable auto-repeat — because the server's repeat arrives as a
/// release-then-press pair that is indistinguishable from a person typing fast,
/// and a text field that cannot tell the difference cannot coalesce an undo.
pub(crate) struct Repeater {
    delay: Duration,
    interval: Option<Duration>,
    held: Option<Held>,
}

impl Default for Repeater {
    fn default() -> Self {
        Self { delay: DEFAULT_REPEAT_DELAY, interval: Some(DEFAULT_REPEAT_INTERVAL), held: None }
    }
}

impl Repeater {
    /// Adopts the rate the server states. Zero means "do not repeat".
    pub(crate) fn set_rate(&mut self, delay_ms: i32, rate_per_second: i32) {
        if delay_ms > 0 {
            self.delay = Duration::from_millis(delay_ms.unsigned_abs().into());
        }
        self.interval = (rate_per_second > 0)
            .then(|| Duration::from_secs_f64(1.0 / f64::from(rate_per_second)));
    }

    /// Starts repeating `event`, or stops if this key does not repeat.
    pub(crate) fn press(&mut self, event: &KeyEvent, keysym: u32, repeats: bool) {
        match (repeats, self.interval) {
            (true, Some(interval)) => {
                self.held = Some(Held {
                    event: event.clone(),
                    keysym,
                    next: Instant::now() + self.delay,
                    interval,
                });
            }
            _ => self.held = None,
        }
    }

    /// Stops repeating if `keysym` is the key that was being held.
    ///
    /// By keysym and not unconditionally: releasing a *different* key while one
    /// is held must not stop the held one, which is what typing `Shift` mid-word
    /// does.
    pub(crate) fn release(&mut self, keysym: u32) {
        if self.held.as_ref().is_some_and(|held| held.keysym == keysym) {
            self.held = None;
        }
    }

    /// Stops repeating altogether, for a focus loss.
    ///
    /// A key held when the window loses focus is a key whose release this window
    /// will never see, and repeating it forever is what that costs.
    pub(crate) fn clear(&mut self) {
        self.held = None;
    }

    /// How long until the next repeat is due, if one is pending.
    pub(crate) fn next_in(&self) -> Option<Duration> {
        self.held.as_ref().map(|held| held.next.saturating_duration_since(Instant::now()))
    }

    /// Emits any repeats that have come due.
    pub(crate) fn emit(&mut self, events: &mut VecDeque<Event>) {
        let now = Instant::now();
        while let Some(held) = &mut self.held {
            if held.next > now {
                break;
            }
            held.next += held.interval;
            // A window that was suspended long enough to owe a hundred repeats
            // should not deliver them all at once.
            if held.next < now {
                held.next = now + held.interval;
            }
            let mut event = held.event.clone();
            event.repeat = true;
            events.push_back(Event::KeyDown(event));
        }
    }
}

/// Which modifiers an xkb state has effective.
pub(crate) fn modifiers_of(state: &xkb::State) -> Modifiers {
    let active = |name| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
    Modifiers {
        ctrl: active(xkb::MOD_NAME_CTRL),
        shift: active(xkb::MOD_NAME_SHIFT),
        alt: active(xkb::MOD_NAME_ALT),
        logo: active(xkb::MOD_NAME_LOGO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Key, Named};

    /// A table of two sequences, so the assertions are about a table this file
    /// can see rather than about whatever the machine has installed.
    fn composer() -> Composer {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        Composer::from_rules(
            &context,
            "<Multi_key> <a> <apostrophe> : \"á\"\n<dead_acute> <e> : \"é\"\n",
        )
    }

    /// The half of input-method support that needs no protocol, and the only
    /// half a unit test can reach — an input method is a second process.
    #[test]
    fn a_sequence_types_nothing_until_it_finishes() {
        use xkbcommon::xkb::keysyms as sym;
        let mut compose = composer();
        assert_eq!(compose.feed(sym::KEY_Multi_key), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_a), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_apostrophe), Composed::Typed("á".to_owned()));
        // And the next key is an ordinary key again.
        assert_eq!(compose.feed(sym::KEY_b), Composed::Passthrough);
    }

    #[test]
    fn a_dead_key_composes_with_the_letter_after_it() {
        use xkbcommon::xkb::keysyms as sym;
        let mut compose = composer();
        assert_eq!(compose.feed(sym::KEY_dead_acute), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_e), Composed::Typed("é".to_owned()));
    }

    /// A sequence that goes nowhere types nothing at all — not the dead key it
    /// started with, which is what a caller that ignored the cancellation would
    /// insert.
    #[test]
    fn a_cancelled_sequence_types_nothing() {
        use xkbcommon::xkb::keysyms as sym;
        let mut compose = composer();
        assert_eq!(compose.feed(sym::KEY_dead_acute), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_z), Composed::Swallowed);
        // And the *next* sequence starts from nothing rather than from the wreck
        // of the last one — which is what a missing reset leaves behind, and
        // which a third ordinary key cannot tell apart.
        assert_eq!(compose.feed(sym::KEY_dead_acute), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_e), Composed::Typed("é".to_owned()));
    }

    /// A capital in the middle of a sequence: the `Shift` press must not break
    /// it. libxkbcommon ignores modifier keysyms itself — measured — so this
    /// passes without a guard, and the guard that was written for it was a guard
    /// against nothing.
    #[test]
    fn a_modifier_in_the_middle_does_not_break_a_sequence() {
        use xkbcommon::xkb::keysyms as sym;
        let mut compose = composer();
        assert_eq!(compose.feed(sym::KEY_Multi_key), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_Shift_L), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_a), Composed::Swallowed);
        assert_eq!(compose.feed(sym::KEY_apostrophe), Composed::Typed("á".to_owned()));
    }

    /// **What the reset is for.** An ignored keysym does not trigger the
    /// automatic reset a real one does, so a `Shift` press after a sequence
    /// finishes leaves the status at `Composed` — and a caller that had not
    /// reset reads the same character out of it again, typing `áá`.
    #[test]
    fn a_finished_sequence_is_not_typed_twice() {
        use xkbcommon::xkb::keysyms as sym;
        let mut compose = composer();
        compose.feed(sym::KEY_Multi_key);
        compose.feed(sym::KEY_a);
        assert_eq!(compose.feed(sym::KEY_apostrophe), Composed::Typed("á".to_owned()));
        assert_eq!(compose.feed(sym::KEY_Shift_L), Composed::Passthrough, "typed twice");
    }

    /// The rule both display servers now share. The X11 half used to filter only
    /// control characters, so `Alt+D` typed a `d` into the field it had opened.
    #[test]
    fn a_chord_types_nothing_and_a_letter_types_itself() {
        let plain = Modifiers::default();
        assert_eq!(typed("d".to_owned(), plain), Some("d".to_owned()));
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert_eq!(typed("d".to_owned(), ctrl), None);
        let alt = Modifiers { alt: true, ..Modifiers::default() };
        assert_eq!(typed("d".to_owned(), alt), None);
        let logo = Modifiers { logo: true, ..Modifiers::default() };
        assert_eq!(typed("d".to_owned(), logo), None);
        // Shift is not a chord: it is how a capital is typed.
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        assert_eq!(typed("D".to_owned(), shift), Some("D".to_owned()));
        // And a control character is never text, whatever is held.
        assert_eq!(typed("\u{1b}".to_owned(), plain), None);
    }

    /// **No harness can synthesise a touchpad gesture.** Neither
    /// `zwlr_virtual_pointer_manager_v1` nor `XTEST` has swipe or pinch events,
    /// so the GUI scenarios cannot reach this and these are the only tests it
    /// will ever have. What is tested is therefore the whole of the policy: the
    /// plumbing above it is a `match` on a protocol enum.
    #[test]
    fn a_swipe_shorter_than_the_threshold_is_not_a_gesture() {
        let mut gestures = Gestures::default();
        gestures.swipe_begin(3);
        gestures.swipe_update(SWIPE_THRESHOLD - 1.0, 0.0);
        assert_eq!(gestures.swipe_end(false), None);

        gestures.swipe_begin(3);
        gestures.swipe_update(SWIPE_THRESHOLD / 2.0, 0.0);
        gestures.swipe_update(SWIPE_THRESHOLD / 2.0, 0.0);
        assert_eq!(gestures.swipe_end(false), Some((Swipe::Right, 3)), "the deltas add up");
    }

    #[test]
    fn a_cancelled_swipe_is_no_swipe_and_leaves_nothing_behind() {
        let mut gestures = Gestures::default();
        gestures.swipe_begin(3);
        gestures.swipe_update(500.0, 0.0);
        assert_eq!(gestures.swipe_end(true), None);
        // And the next gesture starts from zero rather than from 500.
        gestures.swipe_begin(3);
        gestures.swipe_update(10.0, 0.0);
        assert_eq!(gestures.swipe_end(false), None, "the cancelled distance carried over");
    }

    /// A hand sweeping right also drifts down. A gesture that counted as both
    /// would navigate and scroll at once.
    #[test]
    fn a_swipe_goes_the_way_it_went_furthest() {
        let mut gestures = Gestures::default();
        gestures.swipe_begin(3);
        gestures.swipe_update(-200.0, 60.0);
        assert_eq!(gestures.swipe_end(false), Some((Swipe::Left, 3)));

        gestures.swipe_begin(4);
        gestures.swipe_update(40.0, -300.0);
        assert_eq!(gestures.swipe_end(false), Some((Swipe::Up, 4)), "the finger count is kept");
    }

    /// A pinch reports an absolute scale, so the step has to be measured against
    /// the last step and not against where the fingers started — otherwise one
    /// slow pinch emits a step per event and a fast one emits none.
    #[test]
    fn a_pinch_steps_once_per_ratio_however_it_is_delivered() {
        let coarse = {
            let mut gestures = Gestures::default();
            gestures.pinch_begin();
            (0..1).filter_map(|_| gestures.pinch_update(PINCH_STEP * PINCH_STEP)).count()
        };
        let fine = {
            let mut gestures = Gestures::default();
            gestures.pinch_begin();
            // The same total, delivered in twenty updates.
            let target = PINCH_STEP * PINCH_STEP;
            (1..=20)
                .filter_map(|step| {
                    let at = 1.0 + (target - 1.0) * (step as f32 / 20.0);
                    gestures.pinch_update(at)
                })
                .count()
        };
        assert_eq!(fine, 2, "twenty small updates over two steps' worth are two steps");
        assert_eq!(coarse, 1, "one update cannot emit the steps it skipped");
    }

    #[test]
    fn a_pinch_goes_both_ways_and_stops_when_the_fingers_lift() {
        let mut gestures = Gestures::default();
        gestures.pinch_begin();
        assert_eq!(gestures.pinch_update(PINCH_STEP), Some(true));
        assert_eq!(gestures.pinch_update(1.0), Some(false), "back where it started is a step out");
        gestures.pinch_end();
        assert_eq!(gestures.pinch_update(4.0), None, "an update after the end is not a gesture");
    }

    fn event() -> KeyEvent {
        KeyEvent {
            key: Key::Named(Named::Backspace),
            text: None,
            mods: Modifiers::default(),
            repeat: false,
        }
    }

    #[test]
    fn presses_close_in_time_and_space_count_up() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.count(1_000, Point::new(10.0, 10.0)), 1);
        assert_eq!(clicks.count(1_100, Point::new(12.0, 10.0)), 2);
        assert_eq!(clicks.count(1_200, Point::new(12.0, 13.0)), 3);
    }

    #[test]
    fn a_press_too_far_away_starts_again() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.count(1_000, Point::new(10.0, 10.0)), 1);
        assert_eq!(clicks.count(1_100, Point::new(10.0, 40.0)), 1);
    }

    #[test]
    fn a_press_too_late_starts_again() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.count(1_000, Point::new(10.0, 10.0)), 1);
        assert_eq!(clicks.count(1_500, Point::new(10.0, 10.0)), 1);
    }

    /// The server clock wraps every 49 days and a press either side of the wrap
    /// is still two presses, not a negative interval.
    #[test]
    fn the_clock_wrapping_is_not_a_triple_click() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.count(u32::MAX - 50, Point::new(10.0, 10.0)), 1);
        assert_eq!(clicks.count(u32::MAX.wrapping_add(51), Point::new(10.0, 10.0)), 2);
    }

    #[test]
    fn releasing_another_key_does_not_stop_the_held_one() {
        let mut repeater = Repeater::default();
        repeater.press(&event(), 100, true);
        repeater.release(200);
        assert!(repeater.next_in().is_some(), "a different key stopped the repeat");
        repeater.release(100);
        assert!(repeater.next_in().is_none());
    }

    #[test]
    fn a_key_that_does_not_repeat_stops_the_one_that_was() {
        let mut repeater = Repeater::default();
        repeater.press(&event(), 100, true);
        repeater.press(&event(), 200, false);
        assert!(repeater.next_in().is_none());
    }

    #[test]
    fn a_rate_of_zero_means_no_repeat_at_all() {
        let mut repeater = Repeater::default();
        repeater.set_rate(600, 0);
        repeater.press(&event(), 100, true);
        assert!(repeater.next_in().is_none());
    }

    #[test]
    fn losing_focus_stops_a_key_whose_release_will_never_arrive() {
        let mut repeater = Repeater::default();
        repeater.press(&event(), 100, true);
        repeater.clear();
        assert!(repeater.next_in().is_none());
    }

    /// A backlog is not delivered all at once: the window was asleep, the key was
    /// not held any harder for it.
    #[test]
    fn a_long_stall_owes_one_repeat_and_not_a_hundred() {
        let mut repeater = Repeater::default();
        repeater.set_rate(1, 1_000);
        repeater.press(&event(), 100, true);
        // A test of elapsed time, on a test thread. Rule 1 is about the frame
        // loop, which this is not.
        std::thread::sleep(Duration::from_millis(60)); // skerry-allow: no-blocking-ui
        let mut events = VecDeque::new();
        repeater.emit(&mut events);
        assert!(events.len() < 60, "{} repeats owed for a 60 ms stall", events.len());
        assert!(!events.is_empty(), "no repeat at all after the delay elapsed");
    }
}
