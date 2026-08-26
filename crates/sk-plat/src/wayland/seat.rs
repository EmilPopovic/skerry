//! The seat: keyboard mapping through `xkbcommon`, key repeat, and a pointer
//! that counts clicks and accumulates scroll.
//!
//! Three things here are policy rather than plumbing, and each is a thing a
//! windowing crate would have decided for us:
//!
//! * **Key repeat is ours.** The compositor sends a rate and a delay and then
//!   nothing; a client that does not implement the timer has no key repeat, so
//!   holding `Down` moves the cursor once.
//! * **A click count is ours.** Spike D learned the threshold the expensive
//!   way: a press 250 ms after a click at the same position is a double click,
//!   and treating it as a fresh press turned six drags into six double-clicks.
//! * **Scroll is coalesced per `frame`.** One `wl_pointer.frame` is one user
//!   gesture step; emitting the axes separately makes a diagonal touchpad swipe
//!   two events and one of them wrong.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use xkbcommon::xkb;

use crate::event::{Button, Event, KeyEvent, Modifiers, Point};
use crate::keys;

use super::State;

/// A wheel notch, in logical pixels.
///
/// Three lines of about 22 logical pixels, which is what GTK and Qt both settle
/// on and therefore what a user's hands already expect. A touchpad sends a real
/// distance and does not go through this.
const WHEEL_NOTCH: f32 = 66.0;

/// How close in time and space two presses must be to count as a double click.
///
/// 400 ms is the freedesktop default; 5 logical pixels is enough slack for a
/// hand on a touchpad and tight enough that a press at the next row is a fresh
/// click.
const MULTI_CLICK_WINDOW_MS: u32 = 400;
const MULTI_CLICK_SLOP: f32 = 5.0;

struct Repeat {
    event: KeyEvent,
    keysym: u32,
    next: Instant,
    interval: Duration,
}

#[derive(Default)]
struct PendingScroll {
    delta: Point,
    precise: bool,
    any: bool,
}

/// The seat's keyboard and pointer, and everything derived from them.
pub struct Seat {
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    context: xkb::Context,
    keymap: Option<xkb::Keymap>,
    xkb_state: Option<xkb::State>,
    mods: Modifiers,

    repeat_delay: Duration,
    repeat_interval: Option<Duration>,
    repeating: Option<Repeat>,

    position: Point,
    scroll: PendingScroll,
    last_click: Option<(u32, Point, u8)>,

    /// The serial of the last keyboard enter or key event. A selection may only
    /// be set with a serial from a real input event, which is the protocol's way
    /// of saying a background window cannot steal the clipboard.
    pub keyboard_serial: Option<u32>,
    /// The serial of the last button press, which is the only thing that makes a
    /// drag legal.
    pub button_serial: Option<u32>,
}

impl Seat {
    pub fn new(seat: &wl_seat::WlSeat, _handle: &QueueHandle<State>) -> Self {
        let _ = seat;
        Self {
            keyboard: None,
            pointer: None,
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            xkb_state: None,
            mods: Modifiers::default(),
            repeat_delay: Duration::from_millis(600),
            repeat_interval: Some(Duration::from_millis(40)),
            repeating: None,
            position: Point::default(),
            scroll: PendingScroll::default(),
            last_click: None,
            keyboard_serial: None,
            button_serial: None,
        }
    }

    /// How long until the next key repeat is due, if one is pending.
    pub fn next_repeat_in(&self) -> Option<Duration> {
        self.repeating.as_ref().map(|repeat| repeat.next.saturating_duration_since(Instant::now()))
    }

    /// Emits any key repeats that have come due.
    pub fn emit_repeats(&mut self, events: &mut VecDeque<Event>) {
        let now = Instant::now();
        while let Some(repeat) = &mut self.repeating {
            if repeat.next > now {
                break;
            }
            repeat.next += repeat.interval;
            // A window that was suspended long enough to owe a hundred repeats
            // should not deliver them all at once.
            if repeat.next < now {
                repeat.next = now + repeat.interval;
            }
            let mut event = repeat.event.clone();
            event.repeat = true;
            events.push_back(Event::KeyDown(event));
        }
    }

    fn load_keymap(&mut self, fd: OwnedFd, size: u32) {
        // SAFETY: the compositor sends a descriptor to a private, read-only
        // mapping of exactly `size` bytes, per wl_keyboard.keymap.
        let keymap = unsafe {
            xkb::Keymap::new_from_fd(
                &self.context,
                fd,
                size as usize,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        if let Ok(Some(keymap)) = keymap {
            self.xkb_state = Some(xkb::State::new(&keymap));
            self.keymap = Some(keymap);
        }
    }

    fn refresh_modifiers(&mut self) {
        let Some(state) = &self.xkb_state else { return };
        let active = |name| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
        self.mods = Modifiers {
            ctrl: active(xkb::MOD_NAME_CTRL),
            shift: active(xkb::MOD_NAME_SHIFT),
            alt: active(xkb::MOD_NAME_ALT),
            logo: active(xkb::MOD_NAME_LOGO),
        };
    }

    /// Builds the event for one physical key, or `None` without a keymap.
    fn key_event(&self, evdev_code: u32) -> Option<(KeyEvent, u32, bool)> {
        let state = self.xkb_state.as_ref()?;
        // wayland.xml: the protocol's keycode is evdev's, and xkb's is that plus
        // eight. Getting it wrong shifts the whole keyboard by one row.
        let code = xkb::Keycode::new(evdev_code + 8);
        let keysym = state.key_get_one_sym(code);
        let text = state.key_get_utf8(code);
        let repeats = self.keymap.as_ref().is_some_and(|keymap| keymap.key_repeats(code));
        let key = keys::from_keysym(keysym.raw());
        // Text is what the key typed, not what it is: `Ctrl+C` types nothing a
        // text field should insert, and `Escape` reports a control character.
        let text = (!text.is_empty() && !self.mods.ctrl && !self.mods.alt && !self.mods.logo)
            .then_some(text)
            .filter(|text| !text.chars().any(char::is_control));
        Some((KeyEvent { key, text, mods: self.mods, repeat: false }, keysym.raw(), repeats))
    }

    fn multi_click(&mut self, time: u32, position: Point) -> u8 {
        let count = match self.last_click {
            Some((last_time, last_position, count))
                if time.wrapping_sub(last_time) <= MULTI_CLICK_WINDOW_MS
                    && (position.x - last_position.x).abs() <= MULTI_CLICK_SLOP
                    && (position.y - last_position.y).abs() <= MULTI_CLICK_SLOP =>
            {
                count.saturating_add(1)
            }
            _ => 1,
        };
        self.last_click = Some((time, position, count));
        count
    }
}

fn button_of(code: u32) -> Button {
    // linux/input-event-codes.h. Named here rather than imported because three
    // constants are not worth a dependency and these three never change.
    match code {
        0x110 => Button::Primary,
        0x111 => Button::Secondary,
        0x112 => Button::Middle,
        other => Button::Other(other),
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } = event {
            // A seat can gain and lose devices while the window is open — a
            // docked laptop, a tablet that sleeps. Creating them on the event
            // rather than at open is what makes that work rather than crash.
            let has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
            let has_pointer = capabilities.contains(wl_seat::Capability::Pointer);
            state.capabilities.keyboard = has_keyboard;
            state.capabilities.pointer = has_pointer;
            if has_keyboard && state.seat.keyboard.is_none() {
                state.seat.keyboard = Some(seat.get_keyboard(handle, ()));
            }
            if has_pointer && state.seat.pointer.is_none() {
                state.seat.pointer = Some(seat.get_pointer(handle, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.seat.load_keymap(fd, size);
                state.seat.refresh_modifiers();
            }
            wl_keyboard::Event::Enter { serial, .. } => {
                state.seat.keyboard_serial = Some(serial);
                state.events.push_back(Event::Focused(true));
            }
            wl_keyboard::Event::Leave { .. } => {
                // Every held key is released by a focus change, so a repeat that
                // outlives the focus would type into a window that is not ours.
                state.seat.repeating = None;
                state.seat.mods = Modifiers::default();
                state.events.push_back(Event::Focused(false));
            }
            wl_keyboard::Event::Key { serial, key, state: pressed, .. } => {
                state.seat.keyboard_serial = Some(serial);
                let Some((event, keysym, repeats)) = state.seat.key_event(key) else { return };
                let down = matches!(pressed, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if down {
                    if let Some(interval) = state.seat.repeat_interval.filter(|_| repeats) {
                        state.seat.repeating = Some(Repeat {
                            event: event.clone(),
                            keysym,
                            next: Instant::now() + state.seat.repeat_delay,
                            interval,
                        });
                    }
                    state.events.push_back(Event::KeyDown(event));
                } else {
                    if state.seat.repeating.as_ref().is_some_and(|r| r.keysym == keysym) {
                        state.seat.repeating = None;
                    }
                    state.events.push_back(Event::KeyUp(event));
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = &mut state.seat.xkb_state {
                    xkb_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
                state.seat.refresh_modifiers();
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.seat.repeat_delay = Duration::from_millis(delay.max(0) as u64);
                // A rate of zero means the compositor is telling us not to
                // repeat at all, which is a setting a user can choose.
                state.seat.repeat_interval =
                    (rate > 0).then(|| Duration::from_secs_f32(1.0 / rate as f32));
                if state.seat.repeat_interval.is_none() {
                    state.seat.repeating = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let seat = &mut state.seat;
        match event {
            wl_pointer::Event::Enter { surface_x, surface_y, .. }
            | wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                seat.position = Point::new(surface_x as f32, surface_y as f32);
                state
                    .events
                    .push_back(Event::PointerMoved { position: seat.position, mods: seat.mods });
            }
            wl_pointer::Event::Leave { .. } => {
                seat.last_click = None;
                state.events.push_back(Event::PointerLeft);
            }
            wl_pointer::Event::Button { serial, time, button, state: pressed, .. } => {
                let down = matches!(pressed, WEnum::Value(wl_pointer::ButtonState::Pressed));
                if down {
                    seat.button_serial = Some(serial);
                }
                let position = seat.position;
                let clicks = if down { seat.multi_click(time, position) } else { 0 };
                state.events.push_back(Event::Button {
                    button: button_of(button),
                    pressed: down,
                    position,
                    clicks,
                    mods: seat.mods,
                });
            }
            wl_pointer::Event::AxisSource { axis_source } => {
                seat.scroll.precise = matches!(
                    axis_source,
                    WEnum::Value(
                        wl_pointer::AxisSource::Finger | wl_pointer::AxisSource::Continuous
                    )
                );
            }
            // v8's high-resolution axis, in 120ths of a notch. Preferred where
            // it exists: a free-spinning wheel sends fractions of a notch, and
            // rounding them to whole notches is what makes such a wheel feel
            // like it is skipping.
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let amount = f32::from(value120 as i16) / 120.0 * WHEEL_NOTCH;
                accumulate(&mut seat.scroll, axis, amount);
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if !seat.scroll.any {
                    accumulate(&mut seat.scroll, axis, value as f32);
                }
            }
            wl_pointer::Event::Frame if seat.scroll.any => {
                let scroll = std::mem::take(&mut seat.scroll);
                state.events.push_back(Event::Scroll {
                    delta: scroll.delta,
                    precise: scroll.precise,
                    mods: seat.mods,
                });
            }
            _ => {}
        }
    }
}

fn accumulate(scroll: &mut PendingScroll, axis: WEnum<wl_pointer::Axis>, amount: f32) {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => scroll.delta.y += amount,
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => scroll.delta.x += amount,
        _ => return,
    }
    scroll.any = true;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{WHEEL_NOTCH, button_of};
    use crate::event::Button;

    #[test]
    fn the_three_buttons_a_file_manager_binds_are_named() {
        assert_eq!(button_of(0x110), Button::Primary);
        assert_eq!(button_of(0x111), Button::Secondary);
        assert_eq!(button_of(0x112), Button::Middle);
        assert_eq!(button_of(0x116), Button::Other(0x116), "back is carried, not dropped");
    }

    #[test]
    fn a_wheel_notch_is_three_rows_worth_of_travel() {
        assert!((WHEEL_NOTCH - 66.0).abs() < f32::EPSILON);
    }
}
