// SPDX-License-Identifier: MIT OR Apache-2.0
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
        let same_key = match (self.key, event.key) {
            // **The keymap applies Shift before we see it.** `Ctrl+Shift+N`
            // arrives as `N`, and a binding is written in lower case — so
            // comparing the characters exactly made every `ctrl+shift+<letter>`
            // chord in the program unreachable, which is where this came from.
            (Key::Character(bound), Key::Character(pressed)) => {
                bound.eq_ignore_ascii_case(&pressed)
            }
            (bound, pressed) => bound == pressed,
        };
        same_key
            && self.mods.ctrl == event.mods.ctrl
            && self.mods.alt == event.mods.alt
            && self.mods.logo == event.mods.logo
            && (!self.shift_matters() || self.mods.shift == event.mods.shift)
    }

    /// The chord as *matching* sees it, for deciding whether two collide.
    ///
    /// **Two chords that are not equal can still match the same press.**
    /// [`Chord::matches`] ignores Shift for anything but a letter, and compares a
    /// letter case-insensitively — both for good reasons, stated below and on
    /// [`Chord::shift_matters`]. So `ctrl+0` and `ctrl+shift+0` are two values and
    /// one chord, and a registry that compared them for equality accepted both and
    /// gave the press to whichever it happened to find first. Which is how
    /// `Ctrl+Shift+0` came to mean "reset the zoom" — and how a test asserting no
    /// two commands share a binding passed while two did.
    #[must_use]
    pub fn canonical(self) -> Self {
        let mut same = self;
        if !self.shift_matters() {
            same.mods.shift = false;
        }
        if let Key::Character(character) = same.key {
            same.key = Key::Character(character.to_ascii_lowercase());
        }
        same
    }

    /// Whether Shift is part of this chord's identity.
    ///
    /// **For a letter and for a named key, yes; for other characters, no.**
    /// Shift on a letter changes its case and nothing else, so `Ctrl+Shift+C`
    /// and `Ctrl+C` are two chords and a person expects them to be two commands.
    /// Shift on punctuation changes which character the key produces at all —
    /// `/` is unshifted on a US layout and `Shift+7` on a German one — so a
    /// binding written `/` has to match both, and comparing Shift there would
    /// make it a binding that works in one country.
    const fn shift_matters(&self) -> bool {
        match self.key {
            Key::Character(character) => character.is_ascii_alphabetic(),
            Key::Named(_) | Key::Unknown(_) => true,
        }
    }

    /// The chord as a person reads it: `Ctrl A`, `Alt ←`, `Page Down`, `F5`.
    ///
    /// Spaces rather than `+`, and arrows rather than the word, because this
    /// string goes on screen next to a command name and not into a file.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::with_capacity(16);
        for (held, key) in [
            (self.mods.logo, "key.super"),
            (self.mods.ctrl, "key.ctrl"),
            (self.mods.alt, "key.alt"),
            (self.mods.shift, "key.shift"),
        ] {
            if held {
                out.push_str(sk_model::i18n::lookup(key));
                out.push(' ');
            }
        }
        out.push_str(&key_name(self.key));
        out
    }

    /// The chord as a *file* holds it: `ctrl+shift+f3`, which is what [`Chord::parse`]
    /// takes.
    ///
    /// **The inverse of `parse`, not a second display form.** `display` is for the
    /// screen — spaces, arrows, the locale's word for `Ctrl` — and putting that in a
    /// file would make the keymap unreadable by the parser that wrote it. The two are
    /// tested against each other over every chord the registry binds: what is written
    /// back can be read back, which is
    /// [ADR 0025](../../../docs/adr/0025-the-config-file-is-the-keymap-written-out.md)'s
    /// contract with the file.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(16);
        for (held, word) in [
            (self.mods.ctrl, "ctrl"),
            (self.mods.alt, "alt"),
            (self.mods.shift, "shift"),
            (self.mods.logo, "super"),
        ] {
            if held {
                out.push_str(word);
                out.push('+');
            }
        }
        // **The three characters that are also separators or invisible.** `parse`
        // splits on `+` and `-`, so writing `ctrl+-` produces a chord nothing can
        // read back — which the round-trip test caught, and which is why `parse`
        // has the words in the first place.
        match self.key {
            Key::Character(' ') => out.push_str("space"),
            Key::Character('+') => out.push_str("plus"),
            Key::Character('-') => out.push_str("minus"),
            Key::Character(character) => out.extend(character.to_lowercase()),
            Key::Named(name) => out.push_str(name_for(name)),
            Key::Unknown(code) => {
                out.push_str("key");
                out.push_str(&code.to_string());
            }
        }
        out
    }
}

/// The word `parse` accepts for a named key.
///
/// **The first spelling of each, so a round trip is stable**: `parse` takes
/// `pgup` and `page_up` as well, and writing one of the aliases would make the
/// file's spelling depend on which alias came first in the table.
const fn name_for(name: Named) -> &'static str {
    match name {
        Named::Enter => "enter",
        Named::Escape => "escape",
        Named::Backspace => "backspace",
        Named::Delete => "delete",
        Named::Tab => "tab",
        Named::Left => "left",
        Named::Right => "right",
        Named::Up => "up",
        Named::Down => "down",
        Named::Home => "home",
        Named::End => "end",
        Named::PageUp => "page_up",
        Named::PageDown => "page_down",
        Named::Insert => "insert",
        Named::Menu => "menu",
        Named::Pause => "pause",
        Named::Function(number) => {
            let at = if number as usize > 24 { 24 } else { number as usize };
            FUNCTION_KEYS[at.saturating_sub(1)]
        }
    }
}

/// `f1` to `f24`, because a `const fn` cannot format a number.
const FUNCTION_KEYS: [&str; 24] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15",
    "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
];

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
        "pause" => Named::Pause,
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

/// The name a person reads for one key.
///
/// Through the catalogue, every one of them. `Ctrl`, `Alt` and `Page Down` are
/// as much a translated word as any other on screen — the keyboard overlay is
/// the discovery surface rule 4 is about, and leaving it in English would leave
/// the whole of it in English in a translated build. The arrows and `F5` are
/// catalogue entries too, so a language that wants a word rather than a glyph
/// can have one.
fn key_name(key: Key) -> String {
    fn named(key: &'static str) -> String {
        sk_model::i18n::lookup(key).to_owned()
    }
    match key {
        Key::Character(' ') => named("key.space"),
        Key::Character(character) => character.to_uppercase().to_string(),
        Key::Unknown(code) => format!("0x{code:x}"),
        Key::Named(name) => match name {
            Named::Enter => named("key.enter"),
            Named::Escape => named("key.escape"),
            Named::Backspace => named("key.backspace"),
            Named::Delete => named("key.delete"),
            Named::Tab => named("key.tab"),
            // Arrows, because a row of four words down the side of a keyboard
            // list is much harder to scan than a row of four arrows.
            Named::Left => named("key.left"),
            Named::Right => named("key.right"),
            Named::Up => named("key.up"),
            Named::Down => named("key.down"),
            Named::Home => named("key.home"),
            Named::End => named("key.end"),
            Named::PageUp => named("key.page_up"),
            Named::PageDown => named("key.page_down"),
            Named::Insert => named("key.insert"),
            Named::Menu => named("key.menu"),
            Named::Pause => named("key.pause"),
            Named::Function(number) => sk_model::i18n::format("key.function", &[&number]),
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

    /// Shift counts for a named key and for a letter, and not for punctuation.
    ///
    /// **This test asserted the opposite for a letter and passed while three
    /// commands could not be reached at all.** `Ctrl+Shift+N` arrives from xkb
    /// as `N` with Shift held; the binding is written `n`; the comparison was
    /// exact, so New folder, Copy the path and Redo were bound to chords no
    /// keyboard could produce — and the assertion that `Ctrl+Shift+A` runs
    /// Select All was the reason nobody looked.
    #[test]
    fn shift_counts_for_a_named_key_and_a_letter_and_not_for_punctuation() {
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        let named = Chord::parse("shift+down").expect("chord");
        assert!(named.matches(&press(Key::Named(Named::Down), shift)));
        assert!(!named.matches(&press(Key::Named(Named::Down), Modifiers::default())));

        // A letter: two chords, and each matches only its own.
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        let ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::default() };
        let plain = Chord::parse("ctrl+a").expect("chord");
        let shifted = Chord::parse("ctrl+shift+a").expect("chord");
        assert!(plain.matches(&press(Key::Character('a'), ctrl)));
        assert!(!plain.matches(&press(Key::Character('A'), ctrl_shift)));
        assert!(shifted.matches(&press(Key::Character('A'), ctrl_shift)));
        assert!(!shifted.matches(&press(Key::Character('a'), ctrl)));

        // Punctuation: one chord, and Shift is whatever the layout needed to
        // produce the character.
        let slash = Chord::parse("/").expect("chord");
        assert!(slash.matches(&press(Key::Character('/'), Modifiers::default())));
        assert!(slash.matches(&press(Key::Character('/'), shift)), "a German layout");
    }

    /// The three chords the defect above made unreachable, as they actually
    /// arrive from xkb: the character comes through shifted.
    #[test]
    fn every_shifted_letter_chord_the_program_binds_is_reachable() {
        let ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::default() };
        for (text, character) in
            [("ctrl+shift+n", 'N'), ("ctrl+shift+c", 'C'), ("ctrl+shift+z", 'Z')]
        {
            let chord = Chord::parse(text).expect(text);
            assert!(chord.matches(&press(Key::Character(character), ctrl_shift)), "{text}");
        }
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
                    Named::Pause,
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
    /// **Two values, one chord.** The identity a collision has to be decided by
    /// is what `matches` will accept, not what `==` says.
    #[test]
    fn a_digit_with_shift_is_the_same_chord_as_the_digit() {
        let plain = Chord::parse("ctrl+0").expect("chord");
        let shifted = Chord::parse("ctrl+shift+0").expect("chord");
        assert_ne!(plain, shifted, "they differ as values");
        assert_eq!(plain.canonical(), shifted.canonical(), "and not as chords");

        // A letter is the other way round: Shift changes the case and nothing
        // else, so the two are genuinely two chords.
        let lower = Chord::parse("ctrl+c").expect("chord");
        let upper = Chord::parse("ctrl+shift+c").expect("chord");
        assert_ne!(lower.canonical(), upper.canonical());

        // And the case a binding is written in is not part of it.
        assert_eq!(
            Chord::parse("ctrl+C").expect("chord").canonical(),
            lower.canonical(),
            "a binding written in capitals is the same chord"
        );
    }
}
