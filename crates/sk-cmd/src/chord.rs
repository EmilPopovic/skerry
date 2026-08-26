//! A key plus its modifiers, parsed from text and rendered back to it.
//!
//! Both directions matter and for the same reason. Parsing is how a binding gets
//! into the registry from a `CommandSpec` — and, later, from the config file,
//! which [docs/04-architecture.md](../../../docs/04-architecture.md#configuration-and-state)
//! makes a serialization of this same registry. Rendering is how the binding
//! reaches the person who has to press it: rule 4 says the shortcut appears next
//! to the thing it acts on, and a *written-out* shortcut beside a bound one is
//! two things that can disagree. Here they cannot.

use sk_plat::{Key, KeyEvent, Modifiers, Named};

/// One key press that a command can be bound to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    /// The key itself.
    pub key: Key,
    /// What must be held with it.
    pub mods: Modifiers,
}

impl Chord {
    /// Parses `ctrl+shift+e`, `alt+left`, `f5`, `escape`, `space`, `1`.
    ///
    /// Case-insensitive, and `-` works as well as `+` because both appear in
    /// every configuration format a user has met.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut mods = Modifiers::default();
        let mut key = None;
        for part in text.split(['+', '-']).filter(|part| !part.is_empty()) {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" | "meta" => mods.alt = true,
                "super" | "logo" | "win" | "cmd" => mods.logo = true,
                other => {
                    key = Some(match named(other) {
                        Some(name) => Key::Named(name),
                        None => character(other)?,
                    });
                }
            }
        }
        Some(Self { key: key?, mods })
    }

    /// Whether `event` is this chord.
    #[must_use]
    pub fn matches(&self, event: &KeyEvent) -> bool {
        // Shift is compared for named keys and ignored for characters, because a
        // keymap already applied it: `shift+3` produces `#` on a US layout and
        // `£` on a British one, and a binding written against the character is
        // the one that survives both.
        let shift_matters = matches!(self.key, Key::Named(_));
        self.key == event.key
            && self.mods.ctrl == event.mods.ctrl
            && self.mods.alt == event.mods.alt
            && self.mods.logo == event.mods.logo
            && (!shift_matters || self.mods.shift == event.mods.shift)
    }

    /// The chord as a person reads it: `Ctrl A`, `Alt ←`, `Page Down`, `F5`.
    ///
    /// Spaces rather than `+`, and arrows rather than the word, because this
    /// string goes on screen next to a command name and not into a file.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::with_capacity(16);
        for (held, name) in [
            (self.mods.logo, "Super"),
            (self.mods.ctrl, "Ctrl"),
            (self.mods.alt, "Alt"),
            (self.mods.shift, "Shift"),
        ] {
            if held {
                out.push_str(name);
                out.push(' ');
            }
        }
        out.push_str(&key_name(self.key));
        out
    }
}

fn named(word: &str) -> Option<Named> {
    Some(match word {
        "enter" | "return" => Named::Enter,
        "escape" | "esc" => Named::Escape,
        "backspace" => Named::Backspace,
        "delete" | "del" => Named::Delete,
        "tab" => Named::Tab,
        "left" => Named::Left,
        "right" => Named::Right,
        "up" => Named::Up,
        "down" => Named::Down,
        "home" => Named::Home,
        "end" => Named::End,
        "pageup" | "page_up" | "pgup" => Named::PageUp,
        "pagedown" | "page_down" | "pgdn" => Named::PageDown,
        "insert" | "ins" => Named::Insert,
        "menu" => Named::Menu,
        function if function.starts_with('f') && function.len() <= 3 => {
            // xkb maps F1 to F24 and nothing above it, so `f99` is a typo and
            // not a key. Accepting it would bind a command to a chord no
            // keyboard can produce, which is worse than refusing the line.
            let number: u8 = function[1..].parse().ok()?;
            (1..=24).contains(&number).then_some(Named::Function(number))?
        }
        _ => return None,
    })
}

fn character(word: &str) -> Option<Key> {
    match word {
        "space" => Some(Key::Character(' ')),
        "plus" => Some(Key::Character('+')),
        "minus" => Some(Key::Character('-')),
        _ => {
            let mut characters = word.chars();
            let first = characters.next()?;
            characters.next().is_none().then_some(Key::Character(first))
        }
    }
}

fn key_name(key: Key) -> String {
    match key {
        Key::Character(' ') => "Space".to_owned(),
        Key::Character(character) => character.to_uppercase().to_string(),
        Key::Unknown(code) => format!("0x{code:x}"),
        Key::Named(name) => match name {
            Named::Enter => "Enter".to_owned(),
            Named::Escape => "Esc".to_owned(),
            Named::Backspace => "Backspace".to_owned(),
            Named::Delete => "Delete".to_owned(),
            Named::Tab => "Tab".to_owned(),
            // Arrows, because a row of four words down the side of a keyboard
            // list is much harder to scan than a row of four arrows.
            Named::Left => "←".to_owned(),
            Named::Right => "→".to_owned(),
            Named::Up => "↑".to_owned(),
            Named::Down => "↓".to_owned(),
            Named::Home => "Home".to_owned(),
            Named::End => "End".to_owned(),
            Named::PageUp => "Page Up".to_owned(),
            Named::PageDown => "Page Down".to_owned(),
            Named::Insert => "Insert".to_owned(),
            Named::Menu => "Menu".to_owned(),
            Named::Function(number) => format!("F{number}"),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Chord;
    use sk_plat::{Key, KeyEvent, Modifiers, Named};

    fn press(key: Key, mods: Modifiers) -> KeyEvent {
        KeyEvent { key, text: None, mods, repeat: false }
    }

    #[test]
    fn modifiers_and_keys_parse_in_any_order_and_either_separator() {
        let wanted = Chord::parse("ctrl+shift+e").expect("chord");
        assert_eq!(Chord::parse("shift+ctrl+e"), Some(wanted));
        assert_eq!(Chord::parse("Ctrl-Shift-E"), Some(wanted));
        assert_eq!(wanted.key, Key::Character('e'));
        assert!(wanted.mods.ctrl && wanted.mods.shift && !wanted.mods.alt);
    }

    #[test]
    fn every_named_key_a_command_can_want_parses() {
        for (text, key) in [
            ("f5", Named::Function(5)),
            ("f12", Named::Function(12)),
            ("escape", Named::Escape),
            ("esc", Named::Escape),
            ("page_down", Named::PageDown),
            ("pagedown", Named::PageDown),
            ("left", Named::Left),
            ("backspace", Named::Backspace),
        ] {
            assert_eq!(Chord::parse(text).map(|chord| chord.key), Some(Key::Named(key)), "{text}");
        }
        assert_eq!(Chord::parse("space").map(|chord| chord.key), Some(Key::Character(' ')));
    }

    #[test]
    fn nonsense_is_refused_rather_than_bound_to_something_nearby() {
        assert_eq!(Chord::parse(""), None);
        assert_eq!(Chord::parse("ctrl"), None, "a modifier alone is not a chord");
        assert_eq!(Chord::parse("ctrl+notakey"), None);
        assert_eq!(Chord::parse("f99"), None);
    }

    #[test]
    fn a_chord_matches_only_its_own_modifier_set() {
        let chord = Chord::parse("ctrl+a").expect("chord");
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert!(chord.matches(&press(Key::Character('a'), ctrl)));
        assert!(!chord.matches(&press(Key::Character('a'), Modifiers::default())));
        let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Modifiers::default() };
        assert!(!chord.matches(&press(Key::Character('a'), ctrl_alt)));
    }

    /// Shift is part of a named chord and not of a character one: a keymap has
    /// already applied it to the character, and which character it produced
    /// depends on the layout.
    #[test]
    fn shift_counts_for_a_named_key_and_not_for_a_character() {
        let named = Chord::parse("shift+down").expect("chord");
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        assert!(named.matches(&press(Key::Named(Named::Down), shift)));
        assert!(!named.matches(&press(Key::Named(Named::Down), Modifiers::default())));

        let character = Chord::parse("ctrl+a").expect("chord");
        let ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::default() };
        assert!(character.matches(&press(Key::Character('a'), ctrl_shift)));
    }

    /// The property the keyboard overlay depends on: **no two chords display the
    /// same string.** If they did, the overlay would show one key twice and be
    /// wrong about one of them, which is a rule 4 failure that no amount of
    /// correct binding makes up for.
    ///
    /// Not a string round trip. `display` writes for a person — `Page Down`, with
    /// a space — and `parse` reads a configuration line, so making one the exact
    /// inverse of the other would mean writing `PageDown` on screen to satisfy a
    /// test. Injectivity is the property that matters and this checks it over
    /// every chord the program can hold.
    #[test]
    fn no_two_chords_display_the_same_string() {
        let mut seen = std::collections::HashMap::new();
        let keys = (1..=24)
            .map(|number| Key::Named(Named::Function(number)))
            .chain(
                [
                    Named::Enter,
                    Named::Escape,
                    Named::Backspace,
                    Named::Delete,
                    Named::Tab,
                    Named::Left,
                    Named::Right,
                    Named::Up,
                    Named::Down,
                    Named::Home,
                    Named::End,
                    Named::PageUp,
                    Named::PageDown,
                    Named::Insert,
                    Named::Menu,
                ]
                .map(Key::Named),
            )
            .chain(('a'..='z').map(Key::Character))
            .chain(('0'..='9').map(Key::Character))
            .chain([Key::Character(' ')]);

        for key in keys {
            for bits in 0..16u8 {
                let mods = Modifiers {
                    ctrl: bits & 1 != 0,
                    shift: bits & 2 != 0,
                    alt: bits & 4 != 0,
                    logo: bits & 8 != 0,
                };
                let chord = Chord { key, mods };
                let shown = chord.display();
                assert!(!shown.is_empty(), "{chord:?} displays as nothing");
                if let Some(other) = seen.insert(shown.clone(), chord) {
                    assert_eq!(other, chord, "`{shown}` is both {other:?} and {chord:?}");
                }
            }
        }
    }

    #[test]
    fn arrows_display_as_arrows_and_modifiers_come_first() {
        assert_eq!(Chord::parse("alt+left").expect("chord").display(), "Alt ←");
        assert_eq!(Chord::parse("shift+up").expect("chord").display(), "Shift ↑");
        assert_eq!(Chord::parse("ctrl+shift+t").expect("chord").display(), "Ctrl Shift T");
        assert_eq!(Chord::parse("f5").expect("chord").display(), "F5");
    }
}
