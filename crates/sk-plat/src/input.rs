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

use crate::event::{Event, KeyEvent, Modifiers, Point};

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
