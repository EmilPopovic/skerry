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
use std::time::Duration;

use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use xkbcommon::xkb;

use crate::event::{Button, Event, KeyEvent, Modifiers, Point};
use crate::input::{Clicks, Repeater, WHEEL_NOTCH, modifiers_of};
use crate::keys;

use super::State;

/// One gesture step's worth of scroll, before it is resolved into a distance.
///
/// The three sources are kept apart and chosen between at `frame` rather than as
/// they arrive, because **the protocol does not order them**. It guarantees
/// `axis_source` before the axes and nothing about `axis` against
/// `axis_value120` or `axis_discrete`, so a first-one-wins guard is a guard that
/// depends on which compositor is running.
#[derive(Default)]
struct PendingScroll {
    /// `axis_value120`, in 120ths of a notch. v8, and the best answer: a
    /// free-spinning wheel sends fractions of a notch and rounding them to whole
    /// notches is what makes such a wheel feel like it is skipping.
    high: Point,
    /// `axis_discrete`, in whole notches. v5 to v7.
    notches: Point,
    /// `axis`, a surface-local distance. Right for a touchpad, and for a wheel
    /// it is whatever the compositor decided a notch is worth — typically ten or
    /// fifteen pixels, which is less than half a row.
    distance: Point,
    precise: bool,
    any: bool,
}

impl PendingScroll {
    /// The distance this step should scroll, in logical pixels.
    fn resolve(&self) -> Point {
        let scale = |value: Point, by: f32| Point::new(value.x * by, value.y * by);
        if self.high.x != 0.0 || self.high.y != 0.0 {
            return scale(self.high, WHEEL_NOTCH / 120.0);
        }
        if self.notches.x != 0.0 || self.notches.y != 0.0 {
            return scale(self.notches, WHEEL_NOTCH);
        }
        self.distance
    }
}

/// The seat's keyboard and pointer, and everything derived from them.
pub struct Seat {
    /// The seat object itself. Needed by `xdg_toplevel.move` and `.resize`,
    /// which are requests *about* a seat rather than about a pointer.
    object: wl_seat::WlSeat,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    context: xkb::Context,
    keymap: Option<xkb::Keymap>,
    xkb_state: Option<xkb::State>,
    mods: Modifiers,

    repeat: Repeater,

    position: Point,
    scroll: PendingScroll,
    clicks: Clicks,

    /// The serial of the last keyboard enter or key event. A selection may only
    /// be set with a serial from a real input event, which is the protocol's way
    /// of saying a background window cannot steal the clipboard.
    pub keyboard_serial: Option<u32>,
    /// The serial of the last button press, which is the only thing that makes a
    /// drag legal.
    pub button_serial: Option<u32>,
    /// The serial of the last `wl_pointer.enter`, which is the only thing that
    /// lets a client set the pointer's image. The protocol allows it while the
    /// pointer is over this surface and not otherwise.
    pub enter_serial: Option<u32>,
}

impl Seat {
    pub fn new(seat: &wl_seat::WlSeat, _handle: &QueueHandle<State>) -> Self {
        Self {
            object: seat.clone(),
            keyboard: None,
            pointer: None,
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            xkb_state: None,
            mods: Modifiers::default(),
            repeat: Repeater::default(),
            position: Point::default(),
            scroll: PendingScroll::default(),
            clicks: Clicks::default(),
            keyboard_serial: None,
            button_serial: None,
            enter_serial: None,
        }
    }

    /// The seat, for the requests that name one.
    pub fn seat(&self) -> &wl_seat::WlSeat {
        &self.object
    }

    /// The pointer, where the seat has one.
    pub fn pointer(&self) -> Option<&wl_pointer::WlPointer> {
        self.pointer.as_ref()
    }

    /// How long until the next key repeat is due, if one is pending.
    pub fn next_repeat_in(&self) -> Option<Duration> {
        self.repeat.next_in()
    }

    /// Emits any key repeats that have come due.
    pub fn emit_repeats(&mut self, events: &mut VecDeque<Event>) {
        self.repeat.emit(events);
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
        self.mods = modifiers_of(state);
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
                state.seat.repeat.clear();
                state.seat.mods = Modifiers::default();
                state.events.push_back(Event::Focused(false));
            }
            wl_keyboard::Event::Key { serial, key, state: pressed, .. } => {
                state.seat.keyboard_serial = Some(serial);
                let Some((event, keysym, repeats)) = state.seat.key_event(key) else { return };
                let down = matches!(pressed, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if down {
                    state.seat.repeat.press(&event, keysym, repeats);
                    state.events.push_back(Event::KeyDown(event));
                } else {
                    state.seat.repeat.release(keysym);
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
                // A rate of zero means the compositor is telling us not to
                // repeat at all, which is a setting a user can choose.
                state.seat.repeat.set_rate(delay, rate);
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
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                // The only serial that lets a client dress the pointer, and the
                // protocol says its image is undefined until one does.
                seat.enter_serial = Some(serial);
                seat.position = Point::new(surface_x as f32, surface_y as f32);
                state
                    .events
                    .push_back(Event::PointerMoved { position: seat.position, mods: seat.mods });
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                seat.position = Point::new(surface_x as f32, surface_y as f32);
                state
                    .events
                    .push_back(Event::PointerMoved { position: seat.position, mods: seat.mods });
            }
            wl_pointer::Event::Leave { .. } => {
                seat.enter_serial = None;
                seat.clicks = Clicks::default();
                state.events.push_back(Event::PointerLeft);
            }
            wl_pointer::Event::Button { serial, time, button, state: pressed, .. } => {
                let down = matches!(pressed, WEnum::Value(wl_pointer::ButtonState::Pressed));
                if down {
                    seat.button_serial = Some(serial);
                }
                let position = seat.position;
                // **A drag ends when the button does.** Spike D measured
                // `dnd_drop_performed` and `dnd_finished` delivered by some peers
                // and neither by others, so a source that waited for one to know
                // it was free would never start a second drag. The *source object*
                // stays alive to serve bytes — a target asks for them after the
                // release — and only the "in flight" flag goes.
                if !down {
                    state.drags.out = false;
                }
                let seat = &mut state.seat;
                let clicks = if down { seat.clicks.count(time, position) } else { 0 };
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
                let (axis, amount) = (axis, f32::from(value120 as i16));
                accumulate(&mut seat.scroll.high, &mut seat.scroll.any, axis, amount);
            }
            // v5 to v7's notch count. Without it a wheel notch on a compositor
            // that has no `axis_value120` moved whatever `axis` said, which is
            // ten or fifteen pixels — under half a row, where the same notch is
            // three rows everywhere else.
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                let amount = discrete as f32;
                accumulate(&mut seat.scroll.notches, &mut seat.scroll.any, axis, amount);
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let amount = value as f32;
                accumulate(&mut seat.scroll.distance, &mut seat.scroll.any, axis, amount);
            }
            wl_pointer::Event::Frame if seat.scroll.any => {
                let scroll = std::mem::take(&mut seat.scroll);
                state.events.push_back(Event::Scroll {
                    delta: scroll.resolve(),
                    precise: scroll.precise,
                    mods: seat.mods,
                });
            }
            _ => {}
        }
    }
}

fn accumulate(into: &mut Point, any: &mut bool, axis: WEnum<wl_pointer::Axis>, amount: f32) {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => into.y += amount,
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => into.x += amount,
        _ => return,
    }
    *any = true;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{PendingScroll, button_of};
    use crate::event::{Button, Point};
    use crate::input::WHEEL_NOTCH;

    /// A wheel notch is a wheel notch whichever of the three the compositor
    /// sends, and the choice cannot depend on which arrives first: the protocol
    /// orders `axis_source` before the axes and says nothing about the rest.
    #[test]
    fn one_wheel_notch_travels_the_same_distance_on_every_protocol_version() {
        // v8: 120ths of a notch.
        let high = PendingScroll { high: Point::new(0.0, 120.0), ..Default::default() };
        assert!((high.resolve().y - WHEEL_NOTCH).abs() < f32::EPSILON);

        // v5 to v7: a notch count, and an `axis` distance beside it that the
        // same compositor also sends. The notch count wins whichever order they
        // arrived in, because both are recorded and neither is a guard.
        let old = PendingScroll {
            notches: Point::new(0.0, 1.0),
            distance: Point::new(0.0, 10.0),
            ..Default::default()
        };
        assert!((old.resolve().y - WHEEL_NOTCH).abs() < f32::EPSILON);

        // And v8 beside both of those still wins.
        let all = PendingScroll {
            high: Point::new(0.0, 60.0),
            notches: Point::new(0.0, 1.0),
            distance: Point::new(0.0, 10.0),
            ..Default::default()
        };
        assert!((all.resolve().y - WHEEL_NOTCH / 2.0).abs() < f32::EPSILON);
    }

    /// A touchpad sends a real distance and nothing else, and that distance is
    /// the answer: scaling it to notches would make a one-pixel swipe jump.
    #[test]
    fn a_touchpad_distance_is_taken_as_it_is() {
        let swipe =
            PendingScroll { distance: Point::new(3.0, 17.5), precise: true, ..Default::default() };
        assert!((swipe.resolve().y - 17.5).abs() < f32::EPSILON);
        assert!((swipe.resolve().x - 3.0).abs() < f32::EPSILON);
    }

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
