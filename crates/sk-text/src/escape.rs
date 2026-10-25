// SPDX-License-Identifier: Apache-2.0
//! Code points a string can hold that draw as nothing, or move what is around
//! them — drawn as their own names instead.
//!
//! **Measured before this existed:** every one of them drew as nothing. A capture
//! of a folder holding `invoice\u{202E}fdp.desktop`, `pay\u{200D}pal.txt` and
//! `line\nbreak.txt` drew `invoicefdp.desktop`, `paypal.txt` and `linebreak.txt`
//! — no glyph, no box, no advance — in the list, the breadcrumbs, the rename
//! field, and the delete-for-good and Replace panels. U+202E did not reorder
//! anything, because [`crate::shape`] makes no explicit-embedding pass; the
//! defect was that two different names were the same pixels, and the panel that
//! asks before deleting for good named a file the person could not tell apart
//! from another.
//!
//! **Applied in [`crate::Fonts::shape`], so no view can forget it.** Every string
//! this program draws becomes glyphs there — a row, a crumb, a field, and a name
//! inside a sentence the status line or a panel built. The string itself is
//! never changed: the layout cache, the clipboard, a rename, a sort and a search
//! all hold the real name, and only the glyphs differ.
//!
//! **`<U+202E>`, in ASCII, and dimmed by the painter.** ASCII because every UI
//! face has it — `⟨⟩` are not in Noto Sans, and an escape drawn as a missing
//! glyph is the defect again. The `U+` form because it names the code point in
//! the notation a person can search for, and the same in every language, so it
//! is not a catalogue string. Dimmed — [`crate::ShapedGlyph::escape`] — so a
//! name that literally contains the eight characters `<U+202E>` does not draw
//! the same as one that hides U+202E.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::ops::Range;

use swash::text::{Category, Codepoint, Script};

/// Whether `character` is drawn as an escape, given its neighbours.
///
/// `before` is the nearest preceding character that is not a combining mark,
/// and `after` the character immediately following, because the two joiners
/// are judged by what they join.
///
/// The set is the code points that draw nothing in every context this program
/// draws in, or that reorder what follows them:
///
/// * C0 and C1 controls and DEL — a newline is legal in a Linux name.
/// * The bidi controls: U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069.
/// * Zero-width and invisible: U+00AD, U+200B, U+2060–U+2064, U+206A–U+206F,
///   U+FEFF. A soft hyphen is here because nothing in this program hyphenates,
///   so it never becomes visible.
/// * The line and paragraph separators U+2028 and U+2029, which are a newline
///   by another name.
///
/// **The joiners U+200C and U+200D are escaped only where they join nothing.**
/// Between two letters of a script that shapes with them — a Persian word, a
/// Devanagari conjunct — or between two pictographs of an emoji sequence, they
/// change what is drawn and are left alone; escaping them there would draw a
/// family emoji as three people and a code point. Between two Latin letters, at
/// either end, or beside a space, they are invisible, and that is the spoof.
///
/// Variation selectors, tag characters and the Mongolian vowel separator are
/// not here: each does visible work in context — an emoji's presentation, a
/// subdivision flag, a Mongolian letter's form.
#[must_use]
pub fn is_hidden(before: Option<char>, character: char, after: Option<char>) -> bool {
    match character {
        '\u{0}'..='\u{1F}'
        | '\u{7F}'..='\u{9F}'
        | '\u{AD}'
        | '\u{61C}'
        | '\u{200B}'
        | '\u{200E}'
        | '\u{200F}'
        | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}' => true,
        '\u{200C}' | '\u{200D}' => !joins(before, after),
        _ => false,
    }
}

/// Whether a joiner between `before` and `after` changes what is drawn.
fn joins(before: Option<char>, after: Option<char>) -> bool {
    let (Some(before), Some(after)) = (before, after) else { return false };
    // `swash` keeps the cursive scripts out of `is_complex` — Arabic is shaped
    // by its joining rules rather than by the cluster machine — so both are
    // asked. Without the second, every ZWNJ in a Persian word was escaped.
    let shaped = |character: char| {
        let script = character.script();
        script != Script::Common
            && script != Script::Inherited
            && (script.is_complex() || script.is_joined())
    };
    let pictograph = |character: char| character.properties().is_extended_pictographic();
    // A skin tone and a tag's terminator end an emoji too: a woman technologist
    // with a skin tone has the modifier, not the woman, before its joiner.
    let ends_emoji = |character: char| {
        pictograph(character) || matches!(character, '\u{1F3FB}'..='\u{1F3FF}' | '\u{E007F}')
    };
    (shaped(before) && shaped(after)) || (ends_emoji(before) && pictograph(after))
}

/// What `text` is drawn as.
///
/// Borrowed when nothing in it is hidden, which is every ordinary name — so the
/// common case allocates nothing. Otherwise each hidden code point is replaced by
/// `<U+XXXX>`, and `escapes` is filled with the byte range each replacement
/// occupies in the returned string, in order. `escapes` is cleared first either
/// way, so a caller can keep one buffer for every call.
pub fn shown<'a>(text: &'a str, escapes: &mut Vec<Range<usize>>) -> Cow<'a, str> {
    escapes.clear();
    let mut hidden = hidden_in(text).peekable();
    if hidden.peek().is_none() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + ESCAPE_LEN);
    let mut copied = 0;
    for (at, character) in hidden {
        out.push_str(&text[copied..at]);
        let start = out.len();
        // Writing to a `String` cannot fail.
        let _ = write!(out, "<U+{:04X}>", u32::from(character));
        escapes.push(start..out.len());
        copied = at + character.len_utf8();
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// How long one escape is. Every code point in the set is below U+10000, so it
/// is four digits and always this.
const ESCAPE_LEN: usize = "<U+0000>".len();

/// Every hidden code point in `text`, with its byte offset.
///
/// Only a joiner is judged by its neighbours, so only a joiner pays to look them
/// up; every other character costs one `match`.
fn hidden_in(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    text.char_indices().filter(move |&(at, character)| {
        let (before, after) = if matches!(character, '\u{200C}' | '\u{200D}') {
            // A combining mark belongs to the letter before it, so the letter is
            // what the joiner is judged against: an Arabic letter with a vowel
            // sign over it is still the letter a ZWNJ separates. A joiner is not
            // a mark, so a second joiner is judged against the first.
            let before = text[..at].chars().rev().find(|&previous| !is_mark(previous));
            (before, text[at + character.len_utf8()..].chars().next())
        } else {
            (None, None)
        };
        is_hidden(before, character, after)
    })
}

fn is_mark(character: char) -> bool {
    matches!(
        character.properties().category(),
        Category::NonspacingMark | Category::SpacingMark | Category::EnclosingMark
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{is_hidden, shown};

    fn drawn(text: &str) -> String {
        shown(text, &mut Vec::new()).into_owned()
    }

    /// Each name in the finding that motivated this, as the person now sees it.
    #[test]
    fn a_hidden_code_point_is_drawn_as_its_name() {
        for (name, expected) in [
            ("invoice\u{202E}fdp.desktop", "invoice<U+202E>fdp.desktop"),
            ("line\nbreak.txt", "line<U+000A>break.txt"),
            ("tab\there", "tab<U+0009>here"),
            ("esc\u{1B}[31mred", "esc<U+001B>[31mred"),
            ("del\u{7F}", "del<U+007F>"),
            ("nel\u{85}c1", "nel<U+0085>c1"),
            ("pay\u{200D}pal.txt", "pay<U+200D>pal.txt"),
            ("zw\u{200B}space", "zw<U+200B>space"),
            ("bom\u{FEFF}", "bom<U+FEFF>"),
            ("lri\u{2066}x\u{2069}", "lri<U+2066>x<U+2069>"),
            ("alm\u{61C}", "alm<U+061C>"),
            ("soft\u{AD}hyphen", "soft<U+00AD>hyphen"),
            ("para\u{2029}graph", "para<U+2029>graph"),
            ("\u{0}", "<U+0000>"),
        ] {
            assert_eq!(drawn(name), expected, "{name:?}");
        }
    }

    #[test]
    fn every_code_point_in_the_set_is_hidden_between_two_latin_letters() {
        let set = (0u32..=0x1F)
            .chain(0x7F..=0x9F)
            .chain([0xAD, 0x61C, 0x200B, 0x200C, 0x200D, 0x200E, 0x200F])
            .chain(0x2028..=0x202E)
            .chain(0x2060..=0x2064)
            .chain(0x2066..=0x206F)
            .chain([0xFEFF]);
        for value in set {
            let character = char::from_u32(value).unwrap();
            assert!(is_hidden(Some('a'), character, Some('b')), "U+{value:04X}");
        }
    }

    /// The neighbours of the set, which are ordinary characters and must stay so.
    #[test]
    fn the_code_points_either_side_of_each_range_are_drawn() {
        for value in [0x20u32, 0x7E, 0xA0, 0xAE, 0x61B, 0x61D, 0x200A, 0x2010, 0x2027, 0x202F]
            .into_iter()
            .chain([0x2065, 0x2070, 0xFEFE, 0xFF00])
        {
            let character = char::from_u32(value).unwrap();
            assert!(!is_hidden(Some('a'), character, Some('b')), "U+{value:04X}");
        }
    }

    /// **An ordinary name is borrowed, not copied.** This runs on every shaping
    /// miss, so the common case must cost a scan and nothing else.
    #[test]
    fn a_name_with_nothing_hidden_is_returned_as_it_is() {
        let mut escapes = vec![0..1, 4..9];
        for name in [
            "report.pdf",
            "étude.txt",
            "שלום-עולם.md",
            "holiday 🏖\u{FE0F}.jpeg",
            "日本語のファイル.txt",
            "...",
            "",
        ] {
            let drawn = shown(name, &mut escapes);
            assert!(matches!(drawn, std::borrow::Cow::Borrowed(_)), "{name:?} was copied");
            assert!(escapes.is_empty(), "{name:?} reported escapes");
        }
    }

    /// The ranges are what the shaper dims, so they have to cover exactly the
    /// escape and nothing either side of it — including next to a character that
    /// is more than one byte, which is where a byte-counting version would drift.
    #[test]
    fn the_ranges_cover_each_escape_exactly() {
        let mut escapes = Vec::new();
        let text = "\u{202E}é\u{200B}\u{200B}日\n";
        let drawn = shown(text, &mut escapes).into_owned();
        assert_eq!(drawn, "<U+202E>é<U+200B><U+200B>日<U+000A>");
        let pieces: Vec<&str> = escapes.iter().map(|range| &drawn[range.clone()]).collect();
        assert_eq!(pieces, ["<U+202E>", "<U+200B>", "<U+200B>", "<U+000A>"]);
        for range in &escapes {
            assert!(drawn.is_char_boundary(range.start) && drawn.is_char_boundary(range.end));
        }
    }

    /// The joiners are judged by what they join: kept where they do work that is
    /// visible, escaped where they are only invisible.
    #[test]
    fn a_joiner_is_kept_only_where_it_joins_something() {
        for kept in [
            // A family: man, joiner, woman, joiner, girl.
            "family \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.jpg",
            // A rainbow flag: the joiner follows a variation selector.
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            // A woman technologist with a skin tone before the joiner.
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
            // Persian: a ZWNJ keeps ی from joining خ.
            "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}.txt",
            // An Arabic letter with a vowel sign between it and the joiner.
            "\u{644}\u{64E}\u{200C}\u{627}",
            // Devanagari: a half form, made with a joiner after the virama.
            "\u{915}\u{94D}\u{200D}\u{937}",
        ] {
            assert_eq!(drawn(kept), kept, "{kept:?}");
        }
        for (hidden, expected) in [
            ("pay\u{200D}pal", "pay<U+200D>pal"),
            ("pay\u{200C}pal", "pay<U+200C>pal"),
            ("\u{200D}lead", "<U+200D>lead"),
            ("trail\u{200D}", "trail<U+200D>"),
            ("\u{1F468}\u{200D}", "\u{1F468}<U+200D>"),
            ("\u{1F468}\u{200D}a", "\u{1F468}<U+200D>a"),
            ("\u{645}\u{200C} ", "\u{645}<U+200C> "),
            ("\u{645}\u{200C}a", "\u{645}<U+200C>a"),
            // Cyrillic is not shaped with joiners, so this is a spoof too.
            ("\u{43F}\u{200D}\u{430}", "\u{43F}<U+200D>\u{430}"),
            // Digits are emoji by Unicode's own property, for keycaps; a joiner
            // between two of them still joins nothing.
            ("1\u{200D}2", "1<U+200D>2"),
            // Two joiners where one belongs: the second is judged against the
            // first, which joins nothing, so neither can hide behind the other.
            ("\u{1F468}\u{200D}\u{200D}\u{1F469}", "\u{1F468}<U+200D><U+200D>\u{1F469}"),
        ] {
            assert_eq!(drawn(hidden), expected, "{hidden:?}");
        }
    }
}
