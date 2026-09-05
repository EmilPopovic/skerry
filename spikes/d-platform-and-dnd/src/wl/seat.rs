// SPDX-License-Identifier: Apache-2.0
//! Keyboard mapping through `xkbcommon`, and the pointer, which exists here for
//! one reason: `wl_data_device.start_drag` needs the serial of a button press
//! inside our own surface. A drag source is not something a client can fake.

use std::os::fd::OwnedFd;

use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use xkbcommon::xkb;

use super::App;

pub struct SeatState {
    pub keyboard: wl_keyboard::WlKeyboard,
    pub pointer: wl_pointer::WlPointer,
    ctx: xkb::Context,
    pub keymap: Option<xkb::Keymap>,
    pub xkb_state: Option<xkb::State>,
    pub keymap_bytes: usize,
    pub keymap_compile_us: u128,
    pub layout_name: String,
    /// Set from `wl_pointer.button`, and the only thing that makes a drag legal.
    pub last_button_serial: Option<u32>,
    pub last_enter_serial: Option<u32>,
    /// A selection may only be set with a serial from an input event, which is
    /// the protocol's way of saying a background window cannot steal the
    /// clipboard.
    pub keyboard_enter_serial: Option<u32>,
    pub pointer_inside: bool,
    pub keyboard_focus: bool,
    pub typed: String,
    pub resolved_keysyms: Vec<String>,
}

impl SeatState {
    pub fn new(seat: &wl_seat::WlSeat, qh: &QueueHandle<App>) -> Self {
        Self {
            keyboard: seat.get_keyboard(qh, ()),
            pointer: seat.get_pointer(qh, ()),
            ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            xkb_state: None,
            keymap_bytes: 0,
            keymap_compile_us: 0,
            layout_name: String::new(),
            last_button_serial: None,
            last_enter_serial: None,
            keyboard_enter_serial: None,
            pointer_inside: false,
            keyboard_focus: false,
            typed: String::new(),
            resolved_keysyms: Vec::new(),
        }
    }

    fn load_keymap(&mut self, fd: OwnedFd, size: u32) {
        let t = std::time::Instant::now();
        let keymap = unsafe {
            xkb::Keymap::new_from_fd(
                &self.ctx,
                fd,
                size as usize,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        }
        .ok()
        .flatten();
        self.keymap_compile_us = t.elapsed().as_micros();
        self.keymap_bytes = size as usize;
        if let Some(km) = keymap {
            self.layout_name = km.layout_get_name(0).to_string();
            self.xkb_state = Some(xkb::State::new(&km));
            self.keymap = Some(km);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let s = &mut app.seat_state;
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if let WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) = format {
                    s.load_keymap(fd, size);
                }
            }
            wl_keyboard::Event::Enter { serial, .. } => {
                s.keyboard_enter_serial = Some(serial);
                s.keyboard_focus = true;
                app.paint.keyboard_focus = true;
            }
            wl_keyboard::Event::Leave { .. } => {
                s.keyboard_focus = false;
                app.paint.keyboard_focus = false;
            }
            wl_keyboard::Event::Key { key, state, serial, .. } => {
                s.keyboard_enter_serial = Some(serial);
                if let (WEnum::Value(wl_keyboard::KeyState::Pressed), Some(st)) = (state, &s.xkb_state) {
                    // The protocol's keycode is evdev's; xkb's is that plus 8.
                    // wayland.xml says so, and getting it wrong shifts the whole
                    // keyboard by one row.
                    let code = xkb::Keycode::new(key + 8);
                    let sym = st.key_get_one_sym(code);
                    s.resolved_keysyms.push(xkb::keysym_get_name(sym));
                    let utf8 = st.key_get_utf8(code);
                    s.typed.push_str(&utf8);
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(st) = &mut s.xkb_state {
                    st.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let s = &mut app.seat_state;
        match event {
            wl_pointer::Event::Enter { serial, .. } => {
                s.pointer_inside = true;
                s.last_enter_serial = Some(serial);
            }
            wl_pointer::Event::Leave { .. } => s.pointer_inside = false,
            wl_pointer::Event::Button { serial, state, .. } => {
                if let WEnum::Value(wl_pointer::ButtonState::Pressed) = state {
                    s.last_button_serial = Some(serial);
                }
            }
            _ => {}
        }
    }
}

/// A one-pixel-tall surface is not a legal drag icon on every compositor, so the
/// icon is a real surface with a real buffer. It is also the only visible sign
/// that a drag is in flight.
pub fn drag_icon(app: &App, qh: &QueueHandle<App>) -> (wl_surface::WlSurface, super::paint::Canvas) {
    let surface = app.globals.compositor.create_surface(qh, ());
    let mut canvas = super::paint::Canvas::new(&app.globals.shm, qh, 48, 48);
    canvas.paint(&super::paint::Paint::default());
    surface.attach(Some(&canvas.buffer), 0, 0);
    surface.damage_buffer(0, 0, 48, 48);
    surface.commit();
    (surface, canvas)
}
