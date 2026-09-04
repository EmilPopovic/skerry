// SPDX-License-Identifier: MIT OR Apache-2.0
//! The X11 keyboard: a keymap from the server's own RMLVO names.
//!
//! **Not `libxkbcommon-x11`.** The textbook route is
//! `xkb_x11_keymap_new_from_device`, which needs `libxkbcommon-x11` linked at
//! build time — and a machine with `libxkbcommon.so` and without
//! `libxkbcommon-x11.so` would then fail to start `skerry` at all, including on
//! Wayland where X11 is not wanted. That is verbatim the failure condition rule 9
//! cites for taking `wayland-client` with `dlopen`, so
//! [ADR 0009](../../../../docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md)
//! takes the other road.
//!
//! The other road is `_XKB_RULES_NAMES`, a property every X session sets on the
//! root window holding exactly the five strings `xkb_keymap_new_from_names`
//! wants. Where it is absent — a bare `Xvfb`, which sets nothing — xkbcommon's
//! own defaults resolve against the system's XKB configuration, which is the
//! same `us` keymap the server is using.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use xkbcommon::xkb;

use crate::event::{KeyEvent, Modifiers};
use crate::input::modifiers_of;
use crate::keys;

/// The five RMLVO strings — rules, model, layout, variant, options — in the
/// order `_XKB_RULES_NAMES` stores them, which is also what the acronym spells.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Rmlvo {
    rules: String,
    model: String,
    layout: String,
    variant: String,
    options: String,
}

impl Rmlvo {
    /// Parses the property's payload: NUL-separated, and short where the tail is
    /// empty — a session with no options writes four fields and not five.
    fn parse(bytes: &[u8]) -> Self {
        let mut fields = bytes.split(|byte| *byte == 0).map(String::from_utf8_lossy);
        let mut next = || fields.next().unwrap_or_default().into_owned();
        Self { rules: next(), model: next(), layout: next(), variant: next(), options: next() }
    }

    /// What the server says its keymap is, or nothing where it does not say.
    fn read(
        connection: &impl Connection,
        root: xproto::Window,
        atom: xproto::Atom,
    ) -> Option<Self> {
        let reply = connection
            .get_property(false, root, atom, xproto::AtomEnum::STRING, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        (!reply.value.is_empty()).then(|| Self::parse(&reply.value))
    }
}

/// The keymap, the state it is in, and what is held.
pub(super) struct Keyboard {
    state: xkb::State,
    /// What xkbcommon thinks is held, refreshed from every event's `state` mask.
    pub(super) mods: Modifiers,
    /// The mask bits that mean each modifier in *this* keymap.
    ///
    /// X11 reports a modifier as one of eight numbered bits, and which bit means
    /// `Alt` is a property of the layout rather than of the protocol. Resolved
    /// once from the keymap rather than assumed: assuming `Mod1` is Alt is right
    /// on most keyboards and wrong on the ones that made `xkb_keymap` necessary.
    masks: Masks,
    /// Dead keys and compose sequences, from the same shared policy the Wayland
    /// half uses — so `Compose + a + '` types the same character in both.
    compose: crate::input::Composer,
}

#[derive(Default)]
struct Masks {
    shift: u32,
    ctrl: u32,
    alt: u32,
    logo: u32,
    caps: u32,
    num: u32,
}

impl Keyboard {
    /// Compiles the session's keymap, or the system default where the root
    /// window does not say what it is.
    pub(super) fn new(
        connection: &impl Connection,
        root: xproto::Window,
        rules_atom: xproto::Atom,
    ) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let named = Rmlvo::read(connection, root, rules_atom).unwrap_or_default();
        let options = (!named.options.is_empty()).then(|| named.options.clone());
        let keymap = xkb::Keymap::new_from_names(
            &context,
            &named.rules,
            &named.model,
            &named.layout,
            &named.variant,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;

        let mask = |name: &str| match keymap.mod_get_index(name) {
            xkb::MOD_INVALID => 0,
            index => 1u32 << index,
        };
        let masks = Masks {
            shift: mask(xkb::MOD_NAME_SHIFT),
            ctrl: mask(xkb::MOD_NAME_CTRL),
            alt: mask(xkb::MOD_NAME_ALT),
            logo: mask(xkb::MOD_NAME_LOGO),
            caps: mask(xkb::MOD_NAME_CAPS),
            num: mask("Mod2"),
        };
        let state = xkb::State::new(&keymap);
        let compose = crate::input::Composer::new(&context);
        Some(Self { state, mods: Modifiers::default(), masks, compose })
    }

    /// Adopts the modifier mask that came with an input event.
    ///
    /// Every X11 input event carries the state *before* it, which is the
    /// authoritative answer and cheaper than tracking presses — and it is right
    /// after a `Ctrl` that was held while another window had focus, where
    /// tracking is not.
    pub(super) fn absorb(&mut self, state_mask: u16) {
        let x11 = u32::from(state_mask);
        // The X11 wire bits are fixed; which xkb modifier each *means* is not,
        // which is why the two are mapped through the keymap rather than equated.
        let mut depressed = 0;
        for (bit, modifier) in [
            (u32::from(xproto::KeyButMask::SHIFT.bits()), self.masks.shift),
            (u32::from(xproto::KeyButMask::CONTROL.bits()), self.masks.ctrl),
            (u32::from(xproto::KeyButMask::MOD1.bits()), self.masks.alt),
            (u32::from(xproto::KeyButMask::MOD4.bits()), self.masks.logo),
        ] {
            if x11 & bit != 0 {
                depressed |= modifier;
            }
        }
        let mut locked = 0;
        if x11 & u32::from(xproto::KeyButMask::LOCK.bits()) != 0 {
            locked |= self.masks.caps;
        }
        if x11 & u32::from(xproto::KeyButMask::MOD2.bits()) != 0 {
            locked |= self.masks.num;
        }
        self.state.update_mask(depressed, 0, locked, 0, 0, 0);
        self.mods = modifiers_of(&self.state);
    }

    /// Builds the event for one X11 keycode.
    ///
    /// Returns the event, its keysym, and whether the key is one that repeats —
    /// which is a property of the keymap, so `Shift` held down does not type.
    /// `down` because a compose sequence advances on a *press*: fed on the
    /// release as well, every physical key would feed the sequence twice and a
    /// dead key would cancel itself.
    pub(super) fn event(&mut self, keycode: u8, down: bool) -> (KeyEvent, u32, bool) {
        // X11 keycodes and xkb keycodes are the same numbers; it is *evdev* that
        // is offset by eight, and Wayland speaks evdev. Two protocols, one
        // conversion each, and getting this backwards shifts the whole keyboard
        // by one row — which is what it looked like the first time.
        let code = xkb::Keycode::new(u32::from(keycode));
        let keysym = self.state.key_get_one_sym(code);
        let key = keys::from_keysym(keysym.raw());
        let text = self.state.key_get_utf8(code);
        let repeats = self.state.get_keymap().key_repeats(code);
        let mods = self.mods;
        // The shared rule, not a second copy of it: this half used to filter
        // only control characters, so `Alt+D` typed a `d` into the path field
        // the same chord had just opened.
        let composed =
            if down { self.compose.feed(keysym.raw()) } else { crate::input::Composed::Swallowed };
        let text = match composed {
            crate::input::Composed::Passthrough => crate::input::typed(text, mods),
            crate::input::Composed::Swallowed => None,
            crate::input::Composed::Typed(composed) => Some(composed),
        };
        (KeyEvent { key, text, mods, repeat: false }, keysym.raw(), repeats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_property_is_five_nul_separated_strings() {
        let bytes = b"evdev\0pc105\0us\0\0terminate:ctrl_alt_bksp";
        let rules = Rmlvo::parse(bytes);
        assert_eq!(rules.rules, "evdev");
        assert_eq!(rules.model, "pc105");
        assert_eq!(rules.layout, "us");
        assert_eq!(rules.variant, "");
        assert_eq!(rules.options, "terminate:ctrl_alt_bksp");
    }

    /// A session with no options writes four fields, not five with an empty one.
    #[test]
    fn a_short_property_leaves_the_tail_empty_rather_than_failing() {
        let rules = Rmlvo::parse(b"evdev\0pc105\0de");
        assert_eq!(rules.layout, "de");
        assert_eq!(rules.variant, "");
        assert_eq!(rules.options, "");
    }

    #[test]
    fn an_empty_property_is_the_default_keymap_and_not_a_crash() {
        assert_eq!(Rmlvo::parse(b""), Rmlvo::default());
    }

    /// The whole point of not linking `libxkbcommon-x11`: with no property at
    /// all there is still a keymap, and it still resolves ordinary keys.
    #[test]
    fn the_default_names_compile_into_a_usable_keymap() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let default = Rmlvo::default();
        let keymap = xkb::Keymap::new_from_names(
            &context,
            &default.rules,
            &default.model,
            &default.layout,
            &default.variant,
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        let Some(keymap) = keymap else {
            // No XKB configuration installed: nothing to assert, and saying so
            // beats a failure that looks like this code's fault.
            return;
        };
        let state = xkb::State::new(&keymap);
        // Keycode 38 is `a` on every PC layout, because the keycodes are the
        // physical keys and only the symbols move.
        let symbol = state.key_get_one_sym(xkb::Keycode::new(38));
        assert_ne!(symbol.raw(), 0, "the default keymap resolved no symbol at all");
    }
}
