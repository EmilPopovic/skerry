// SPDX-License-Identifier: MIT OR Apache-2.0
//! xkb keysyms to [`Key`].
//!
//! The mapping is small on purpose: a keysym we do not name is carried through
//! as [`Key::Unknown`] rather than dropped, so a user can still bind it.

use xkbcommon::xkb::keysyms as sym;

use crate::event::{Key, Named};

/// Translates one keysym.
#[must_use]
pub fn from_keysym(keysym: u32) -> Key {
    if let Some(named) = named(keysym) {
        return Key::Named(named);
    }
    // `keysym_to_utf32` returns 0 for a keysym with no character, which is every
    // modifier and every function key.
    let code = xkbcommon::xkb::keysym_to_utf32(keysym.into());
    match char::from_u32(code) {
        Some(character) if code != 0 && !character.is_control() => Key::Character(character),
        _ => Key::Unknown(keysym),
    }
}

fn named(keysym: u32) -> Option<Named> {
    Some(match keysym {
        sym::KEY_Return | sym::KEY_KP_Enter => Named::Enter,
        sym::KEY_Escape => Named::Escape,
        sym::KEY_BackSpace => Named::Backspace,
        sym::KEY_Delete | sym::KEY_KP_Delete => Named::Delete,
        sym::KEY_Tab | sym::KEY_ISO_Left_Tab => Named::Tab,
        sym::KEY_Left | sym::KEY_KP_Left => Named::Left,
        sym::KEY_Right | sym::KEY_KP_Right => Named::Right,
        sym::KEY_Up | sym::KEY_KP_Up => Named::Up,
        sym::KEY_Down | sym::KEY_KP_Down => Named::Down,
        sym::KEY_Home | sym::KEY_KP_Home => Named::Home,
        sym::KEY_End | sym::KEY_KP_End => Named::End,
        sym::KEY_Page_Up | sym::KEY_KP_Page_Up => Named::PageUp,
        sym::KEY_Page_Down | sym::KEY_KP_Page_Down => Named::PageDown,
        sym::KEY_Insert | sym::KEY_KP_Insert => Named::Insert,
        sym::KEY_Menu => Named::Menu,
        sym::KEY_Pause => Named::Pause,
        sym::KEY_F1..=sym::KEY_F24 => Named::Function((keysym - sym::KEY_F1 + 1) as u8),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::from_keysym;
    use crate::event::{Key, Named};
    use xkbcommon::xkb::keysyms as sym;

    #[test]
    fn named_keys_are_named_and_not_characters() {
        assert_eq!(from_keysym(sym::KEY_Escape), Key::Named(Named::Escape));
        assert_eq!(from_keysym(sym::KEY_F7), Key::Named(Named::Function(7)));
        assert_eq!(from_keysym(sym::KEY_KP_Enter), Key::Named(Named::Enter));
    }

    #[test]
    fn a_character_key_carries_its_character() {
        assert_eq!(from_keysym(sym::KEY_a), Key::Character('a'));
        assert_eq!(from_keysym(sym::KEY_slash), Key::Character('/'));
        assert_eq!(from_keysym(sym::KEY_space), Key::Character(' '));
    }

    #[test]
    fn a_modifier_is_neither_and_is_still_carried() {
        assert_eq!(from_keysym(sym::KEY_Control_L), Key::Unknown(sym::KEY_Control_L));
    }
}
