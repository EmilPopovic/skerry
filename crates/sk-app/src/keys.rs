// SPDX-License-Identifier: MIT OR Apache-2.0
//! What a bare keystroke means when no command claims it.
//!
//! The bindings themselves moved to `crate::commands` and `sk-cmd` when the
//! registry arrived. What is left is the one keyboard behaviour that is *not* a
//! command: typing letters to jump to a name, which has no chord because it is
//! every chord.

use sk_plat::KeyEvent;

/// The text a key press should add to the type-ahead buffer, if any.
///
/// A bare printable character and nothing else. `Ctrl+A` types nothing, and a
/// space would otherwise start a jump to a filename beginning with a space.
#[must_use]
pub fn type_ahead(event: &KeyEvent) -> Option<&str> {
    let text = typed(event)?;
    (!text.trim().is_empty()).then_some(text)
}

/// The text a key press should add to a **text field**, if any.
///
/// Not [`type_ahead`]: a field takes a space, because `VirtualBox VMs` and
/// `My Documents` are filenames and a filter that cannot reach them is not a
/// filter. Control characters are still refused — `Enter` and `Tab` arrive with
/// text of their own and mean something else in a field.
#[must_use]
pub fn typed_text(event: &KeyEvent) -> Option<&str> {
    let text = typed(event)?;
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

/// The character a bare key press produced, before either policy above.
fn typed(event: &KeyEvent) -> Option<&str> {
    // Shift is not a modifier here: it is how a capital letter is typed.
    if event.mods.ctrl || event.mods.alt || event.mods.logo {
        return None;
    }
    event.text.as_deref()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::type_ahead;
    use sk_plat::{Key, KeyEvent, Modifiers, Named};

    fn typed(text: &str, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key: Key::Character(text.chars().next().unwrap_or('x')),
            text: Some(text.to_owned()),
            mods,
            repeat: false,
        }
    }

    #[test]
    fn type_ahead_takes_bare_characters_and_nothing_else() {
        assert_eq!(type_ahead(&typed("m", Modifiers::default())), Some("m"));
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert_eq!(type_ahead(&typed("m", ctrl)), None);
        assert_eq!(type_ahead(&typed(" ", Modifiers::default())), None, "a space is not a jump");
    }

    /// A field is not a jump-to-name buffer. `VirtualBox VMs` is the canonical
    /// filename with a space in this repository, and no filter that refuses one
    /// can reach it.
    #[test]
    fn a_text_field_takes_a_space_and_a_capital_letter() {
        use super::typed_text;
        assert_eq!(typed_text(&typed(" ", Modifiers::default())), Some(" "));
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        assert_eq!(typed_text(&typed("V", shift)), Some("V"));
        assert_eq!(type_ahead(&typed("V", shift)), Some("V"), "and so does type-ahead");

        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert_eq!(typed_text(&typed("f", ctrl)), None, "a chord is not typing");
        assert_eq!(typed_text(&typed("\r", Modifiers::default())), None, "Enter is not text");
        assert_eq!(typed_text(&typed("\t", Modifiers::default())), None, "Tab is not text");
    }

    #[test]
    fn a_key_with_no_text_types_nothing() {
        let arrow = KeyEvent {
            key: Key::Named(Named::Down),
            text: None,
            mods: Modifiers::default(),
            repeat: false,
        };
        assert_eq!(type_ahead(&arrow), None);
    }
}
