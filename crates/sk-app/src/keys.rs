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
    if !event.mods.none() {
        return None;
    }
    let text = event.text.as_deref()?;
    (!text.trim().is_empty()).then_some(text)
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
