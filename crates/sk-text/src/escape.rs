// SPDX-License-Identifier: Apache-2.0
//! Code points a name can hold that draw as nothing, or move what is around
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
//! all hold the real name, and only the glyphs differ. **A file's own text opts
//! out** with [`Hidden::Verbatim`]: a byte-order mark, a right-to-left mark in a
//! Hebrew sentence and a soft hyphen in a paragraph are the author's, and nobody
//! reading a file is choosing between two of them.
//!
//! **`<U+202E>`, in ASCII, and dimmed by the painter.** ASCII because every UI
//! face has it — `⟨⟩` are not in Noto Sans, and an escape drawn as a missing
//! glyph is the defect again. The `U+` form because it names the code point in
//! the notation a person can search for, and the same in every language, so it
//! is not a catalogue string. Dimmed — [`crate::ShapedGlyph::escape`] — so a
//! name that literally contains the eight characters `<U+202E>` does not draw
//! the same as one that hides U+202E.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::ops::Range;

use swash::text::{Category, Codepoint, Script};

#[path = "escape_data.rs"]
mod data;

/// The version of the Unicode Character Database the set was derived from.
///
/// The contexts that keep a code point ask `swash`'s own tables for a script or
/// a pictograph, and those are older; this is the version of the set itself.
pub const UNICODE: &str = data::UNICODE;

/// What a run does with a code point [`is_hidden`] names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Hidden {
    /// Drawn as its escape: a name, or anything a name can be inside of.
    #[default]
    Escaped,
    /// Drawn as the font draws it, which is usually as nothing: a preview's or
    /// an editor's body, where the text is a file's own.
    Verbatim,
}

/// Whether the code point that starts at byte `at` of `text` is drawn as an
/// escape, judged against all of `text` around it.
///
/// The set is every code point whose General Category is Cc, Cf, Zl or Zp, and
/// every `Default_Ignorable_Code_Point` — the tables in `escape_data.rs`, derived
/// from the Unicode Character Database. Between two Latin letters each of them
/// draws as nothing, or reorders what follows. Four kinds are left alone where
/// the text around them gives them work to do:
///
/// * **The joiners U+200C and U+200D**, between two letters of a script the
///   shaper treats as complex or cursive, or after a pictograph and before
///   another. [`joins`] says what that buys and what it does not.
/// * **A variation selector**, directly after a base it can select: one of
///   `StandardizedVariants.txt`'s or `emoji-variation-sequences.txt`'s bases, or
///   any Han ideograph — the Ideographic Variation Database registers sequences on
///   tens of thousands of them and is not part of the Character Database.
/// * **The Mongolian selectors U+180B–U+180F**, directly after a Mongolian
///   letter, whose form they choose.
/// * **Tag characters**, inside an emoji tag sequence: a pictograph, tag letters,
///   and the U+E007F that ends them, which is how a subdivision flag is spelled.
#[must_use]
pub fn is_hidden(text: &str, at: usize) -> bool {
    let Some(character) = text.get(at..).and_then(|rest| rest.chars().next()) else {
        return false;
    };
    if !in_ranges(data::HIDDEN, character) {
        return false;
    }
    let before = text[..at].chars().next_back();
    let after = text[at + character.len_utf8()..].chars().next();
    match character {
        '\u{200C}' | '\u{200D}' => !joins(base_before(text, at), after),
        '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}' => {
            !before.is_some_and(|base| selects(base, character))
        }
        '\u{180B}'..='\u{180F}' => before.is_none_or(|base| base.script() != Script::Mongolian),
        '\u{E0020}'..='\u{E007F}' => !in_tag_sequence(text, at, character),
        _ => true,
    }
}

/// Whether a joiner between `before` and `after` is left as it is.
///
/// **Kept so that writing which uses them still reads as itself, not because
/// every one is visible here.** A ZWNJ is how Persian spells a word in two
/// parts and a ZWJ after a virama is how Devanagari asks for a half form, and
/// escaping them would put `<U+200C>` in the middle of ordinary words. What this
/// shaper does with them is narrower: it shapes a cluster at a time and does not
/// join Arabic at all, so a ZWNJ in a Persian name changes nothing drawn, and two
/// Persian names that differ only by one look the same. A ZWJ after a virama
/// does draw a half form, and one between two emoji draws the sequence's own
/// glyph where the emoji face has one.
///
/// Between two Latin letters, at either end, or beside a space a joiner is
/// invisible everywhere, and that is the spoof this exists for.
fn joins(before: Option<char>, after: Option<char>) -> bool {
    let (Some(before), Some(after)) = (before, after) else { return false };
    // `swash` keeps the cursive scripts out of `is_complex` — Arabic is shaped
    // by its joining rules rather than by the cluster machine — so both are asked.
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

/// Whether `selector` selects a variant of `base`.
fn selects(base: char, selector: char) -> bool {
    match selector {
        '\u{FE0E}' | '\u{FE0F}' => in_ranges(data::EMOJI_VARIATION_BASES, base),
        _ if base.script() == Script::Han => true,
        '\u{FE00}'..='\u{FE0D}' => data::STANDARDIZED_VARIANTS
            .binary_search(&(u32::from(base), u32::from(selector)))
            .is_ok(),
        _ => false,
    }
}

/// Whether a tag character at `at` is inside an emoji tag sequence.
///
/// UTS #51 ED-14a: a base — a pictograph, which may carry a presentation
/// selector or a skin tone — then tag specs U+E0020–U+E007E, then U+E007F.
fn in_tag_sequence(text: &str, at: usize, character: char) -> bool {
    let spec = |character: char| ('\u{E0020}'..='\u{E007E}').contains(&character);
    let mut back = text[..at].chars().rev().skip_while(|&previous| spec(previous));
    let base = match back.next() {
        Some('\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}') => back.next(),
        other => other,
    };
    if !base.is_some_and(|base| base.properties().is_extended_pictographic()) {
        return false;
    }
    if character == '\u{E007F}' {
        return text[..at].chars().next_back().is_some_and(spec);
    }
    text[at..].chars().find(|&next| !spec(next)) == Some('\u{E007F}')
}

/// The nearest character before `at` that is not a combining mark.
///
/// A combining mark belongs to the letter before it, so the letter is what a
/// joiner is judged against: an Arabic letter with a vowel sign over it is still
/// the letter a ZWNJ separates. A joiner is not a mark, so a second joiner is
/// judged against the first.
fn base_before(text: &str, at: usize) -> Option<char> {
    text[..at].chars().rev().find(|&previous| {
        !matches!(
            previous.properties().category(),
            Category::NonspacingMark | Category::SpacingMark | Category::EnclosingMark
        )
    })
}

/// Whether `character` is in one of `ranges`, which are sorted and disjoint.
fn in_ranges(ranges: &[(u32, u32)], character: char) -> bool {
    let value = u32::from(character);
    ranges
        .binary_search_by(|&(start, end)| {
            if end < value {
                Ordering::Less
            } else if start > value {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// What `text` is drawn as — [`shown_until`] over all of it.
pub fn shown<'a>(text: &'a str, escapes: &mut Vec<Range<usize>>) -> Cow<'a, str> {
    shown_until(text, text.len(), escapes)
}

/// What the first `end` bytes of `text` are drawn as, judged against all of it.
///
/// **The rest of `text` is context, not decoration.** A caret is placed by
/// shaping what is before it, and a joiner that ends that prefix has nothing
/// after it; judged alone, it was escaped, and the caret after a ZWNJ typed into
/// a Persian name was drawn eight glyphs past the end of the name.
///
/// Borrowed when nothing in it is hidden, which is every ordinary name — so the
/// common case allocates nothing. Otherwise each hidden code point is replaced by
/// `<U+XXXX>`, and `escapes` is filled with the byte range each replacement
/// occupies in the returned string, in order. `escapes` is cleared first either
/// way, so a caller can keep one buffer for every call.
pub fn shown_until<'a>(text: &'a str, end: usize, escapes: &mut Vec<Range<usize>>) -> Cow<'a, str> {
    escapes.clear();
    let end = floor_boundary(text, end);
    let head = &text[..end];
    // Printable ASCII is never hidden, and it is most of every name.
    let mut hidden = head
        .char_indices()
        .filter(|&(at, character)| !(' '..='~').contains(&character) && is_hidden(text, at))
        .peekable();
    if hidden.peek().is_none() {
        return Cow::Borrowed(head);
    }
    let mut out = String::with_capacity(head.len() + ESCAPE_LEN);
    let mut copied = 0;
    for (at, character) in hidden {
        out.push_str(&head[copied..at]);
        let start = out.len();
        // Writing to a `String` cannot fail.
        let _ = write!(out, "<U+{:04X}>", u32::from(character));
        escapes.push(start..out.len());
        copied = at + character.len_utf8();
    }
    out.push_str(&head[copied..]);
    Cow::Owned(out)
}

/// The longest escape is `<U+E0FFF>`'s, for the first reservation.
const ESCAPE_LEN: usize = "<U+E0FFF>".len();

/// `end`, or the character boundary below it.
fn floor_boundary(text: &str, end: usize) -> usize {
    let mut end = end.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{data, is_hidden, shown, shown_until};

    fn drawn(text: &str) -> String {
        shown(text, &mut Vec::new()).into_owned()
    }

    fn hidden_between_latin(character: char) -> bool {
        let text = format!("a{character}b");
        is_hidden(&text, 1)
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
            // The second round, each of which drew as `paypal.txt`.
            ("pay\u{E0070}pal.txt", "pay<U+E0070>pal.txt"),
            ("pay\u{FE00}pal.txt", "pay<U+FE00>pal.txt"),
            ("pay\u{E0100}pal", "pay<U+E0100>pal"),
            ("pay\u{180E}pal", "pay<U+180E>pal"),
            ("pay\u{FFF9}pal", "pay<U+FFF9>pal"),
            ("pay\u{1D173}pal", "pay<U+1D173>pal"),
            ("pay\u{1BCA0}pal", "pay<U+1BCA0>pal"),
            ("pay\u{600}pal", "pay<U+0600>pal"),
            ("pay\u{6DD}pal", "pay<U+06DD>pal"),
            ("pay\u{70F}pal", "pay<U+070F>pal"),
            ("pay\u{8E2}pal", "pay<U+08E2>pal"),
            ("pay\u{3164}pal", "pay<U+3164>pal"),
            ("pay\u{34F}pal", "pay<U+034F>pal"),
        ] {
            assert_eq!(drawn(name), expected, "{name:?}");
        }
    }

    /// Every code point in the tables is hidden between two Latin letters,
    /// because none of the contexts that keep one is there.
    #[test]
    fn every_code_point_in_the_set_is_hidden_between_two_latin_letters() {
        for &(start, end) in data::HIDDEN {
            for value in start..=end {
                let character = char::from_u32(value).unwrap();
                assert!(hidden_between_latin(character), "U+{value:04X}");
            }
        }
    }

    /// The neighbours of the set, which are ordinary characters and must stay so
    /// — the spaces among them, which have a width and are not the spoof.
    #[test]
    fn the_code_points_either_side_of_each_range_are_drawn() {
        for &(start, end) in data::HIDDEN {
            for value in [start.wrapping_sub(1), end + 1] {
                let Some(character) = char::from_u32(value) else { continue };
                if super::in_ranges(data::HIDDEN, character) {
                    continue;
                }
                assert!(!hidden_between_latin(character), "U+{value:04X}");
            }
        }
        for character in [' ', '\u{A0}', '\u{2000}', '\u{202F}', '\u{3000}', '\u{FFFC}'] {
            assert!(!hidden_between_latin(character), "{character:?}");
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
        let text = "\u{202E}é\u{200B}\u{200B}日\n\u{E0070}";
        let drawn = shown(text, &mut escapes).into_owned();
        assert_eq!(drawn, "<U+202E>é<U+200B><U+200B>日<U+000A><U+E0070>");
        let pieces: Vec<&str> = escapes.iter().map(|range| &drawn[range.clone()]).collect();
        assert_eq!(pieces, ["<U+202E>", "<U+200B>", "<U+200B>", "<U+000A>", "<U+E0070>"]);
        for range in &escapes {
            assert!(drawn.is_char_boundary(range.start) && drawn.is_char_boundary(range.end));
        }
    }

    /// The joiners are judged by what they join: kept where writing uses them,
    /// escaped where they are only invisible.
    #[test]
    fn a_joiner_is_kept_only_between_what_it_can_join() {
        for kept in [
            // A family: man, joiner, woman, joiner, girl.
            "family \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.jpg",
            // A rainbow flag: the joiner follows a variation selector.
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            // A woman technologist with a skin tone before the joiner.
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
            // Persian: a ZWNJ between ی and خ, as the language writes it.
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

    /// A selector is kept after a base it can select, and nowhere else.
    #[test]
    fn a_variation_selector_is_kept_only_after_a_base_it_selects() {
        for kept in [
            "holiday \u{1F3D6}\u{FE0F}.jpeg",
            "\u{2764}\u{FE0F}",
            "\u{2764}\u{FE0E}",
            "1\u{FE0F}\u{20E3}",
            // A standardized variant: ≩ with a vertical stroke.
            "\u{2269}\u{FE00}",
            // DIGIT ZERO's short-diagonal-stroke form.
            "0\u{FE00}",
            // A Han ideograph, with a compatibility selector and an IVS.
            "\u{8279}\u{FE00}",
            "\u{845B}\u{E0100}",
            // A Mongolian letter and its free variation selector.
            "\u{1820}\u{180B}",
            "\u{1820}\u{180E}\u{1820}",
        ] {
            assert_eq!(drawn(kept), kept, "{kept:?}");
        }
        for (hidden, expected) in [
            ("pay\u{FE00}pal", "pay<U+FE00>pal"),
            ("pay\u{FE0F}pal", "pay<U+FE0F>pal"),
            ("\u{FE0F}lead", "<U+FE0F>lead"),
            ("\u{2764}\u{FE01}", "\u{2764}<U+FE01>"),
            ("0\u{FE01}", "0<U+FE01>"),
            ("\u{8279}\u{FE0F}", "\u{8279}<U+FE0F>"),
            ("a\u{E0100}", "a<U+E0100>"),
            ("a\u{180B}", "a<U+180B>"),
        ] {
            assert_eq!(drawn(hidden), expected, "{hidden:?}");
        }
    }

    /// Scotland's flag is a black flag, five tag letters and a cancel tag; the
    /// same tag letters anywhere else spell nothing visible.
    #[test]
    fn a_tag_is_kept_only_inside_an_emoji_tag_sequence() {
        let scotland = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}";
        assert_eq!(drawn(scotland), scotland);
        for (hidden, expected) in [
            ("pay\u{E0070}pal", "pay<U+E0070>pal"),
            ("a\u{E0067}\u{E007F}", "a<U+E0067><U+E007F>"),
            // A flag with no cancel tag never ends, so it is not one.
            ("\u{1F3F4}\u{E0067}\u{E0062}", "\u{1F3F4}<U+E0067><U+E0062>"),
            // A cancel tag with nothing to cancel.
            ("\u{1F3F4}\u{E007F}", "\u{1F3F4}<U+E007F>"),
            ("\u{1F3F4}\u{E0001}\u{E007F}", "\u{1F3F4}<U+E0001><U+E007F>"),
        ] {
            assert_eq!(drawn(hidden), expected, "{hidden:?}");
        }
    }

    /// **A prefix is judged against the whole text.** The caret after a joiner
    /// is placed by shaping what is before it, and the joiner ends that prefix.
    #[test]
    fn a_prefix_that_ends_in_a_kept_joiner_keeps_it() {
        for text in [
            "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}",
            "\u{915}\u{94D}\u{200D}\u{937}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
        ] {
            for (at, character) in text.char_indices() {
                let end = at + character.len_utf8();
                let head = shown_until(text, end, &mut Vec::new());
                assert_eq!(head, &text[..end], "{text:?} up to {end}");
            }
        }
        // And a hidden one stays hidden however the prefix ends.
        let spoof = "pay\u{200D}pal";
        assert_eq!(shown_until(spoof, 6, &mut Vec::new()), "pay<U+200D>");
        // An end inside a character falls back to the boundary below it.
        assert_eq!(shown_until("é", 1, &mut Vec::new()), "");
    }

    /// **The tables are what the Character Database they cite derives,** and
    /// this is the derivation. Where `/usr/share/unicode` holds the version the
    /// tables name — Debian's and Ubuntu's `unicode-data` — the file is derived
    /// again and compared byte for byte; set `SKERRY_UNICODE_OUT` to a path to
    /// have the derived file written there. Elsewhere there is nothing to derive
    /// it from, and the test says so and passes.
    #[test]
    fn the_tables_are_what_the_unicode_data_derives() {
        let root = std::path::Path::new("/usr/share/unicode");
        let Ok(header) = std::fs::read_to_string(root.join("DerivedCoreProperties.txt")) else {
            eprintln!("skerry: no Unicode Character Database under {}", root.display());
            return;
        };
        let version = header
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# DerivedCoreProperties-"))
            .and_then(|rest| rest.strip_suffix(".txt"))
            .unwrap_or_default()
            .to_owned();
        let derived = derive::file(root, &version);
        if let Some(out) = std::env::var_os("SKERRY_UNICODE_OUT") {
            std::fs::write(out, &derived).unwrap();
        }
        if version != data::UNICODE {
            eprintln!("skerry: Unicode {version} here, and the tables are {}", data::UNICODE);
            return;
        }
        assert!(
            derived == include_str!("escape_data.rs"),
            "escape_data.rs is not what Unicode {version} derives; \
             run with SKERRY_UNICODE_OUT=<path> to write the derived file"
        );
    }

    /// The derivation, from the Character Database's own files.
    mod derive {
        use std::collections::{BTreeMap, BTreeSet};
        use std::fmt::Write as _;
        use std::path::Path;

        fn lines(root: &Path, name: &str) -> Vec<Vec<String>> {
            std::fs::read_to_string(root.join(name))
                .unwrap()
                .lines()
                .map(|line| line.split('#').next().unwrap_or_default().trim())
                .filter(|line| !line.is_empty())
                .map(|line| line.split(';').map(|field| field.trim().to_owned()).collect())
                .collect()
        }

        fn span(field: &str) -> (u32, u32) {
            let (start, end) = field.split_once("..").unwrap_or((field, field));
            (u32::from_str_radix(start, 16).unwrap(), u32::from_str_radix(end, 16).unwrap())
        }

        fn ranges(points: &BTreeSet<u32>) -> Vec<(u32, u32)> {
            let mut out: Vec<(u32, u32)> = Vec::new();
            for &point in points {
                match out.last_mut() {
                    Some(last) if last.1 + 1 == point => last.1 = point,
                    _ => out.push((point, point)),
                }
            }
            out
        }

        fn table(out: &mut String, name: &str, rows: &[(u32, u32)]) {
            let _ = writeln!(out, "pub(super) const {name}: &[(u32, u32)] = &[");
            for (first, second) in rows {
                let _ = writeln!(out, "    (0x{first:04X}, 0x{second:04X}),");
            }
            out.push_str("];\n");
        }

        pub(super) fn file(root: &Path, version: &str) -> String {
            let mut hidden = BTreeSet::new();
            let mut first = None;
            for fields in lines(root, "UnicodeData.txt") {
                let point = u32::from_str_radix(&fields[0], 16).unwrap();
                let wanted = matches!(fields[2].as_str(), "Cc" | "Cf" | "Zl" | "Zp");
                if fields[1].ends_with(", First>") {
                    first = Some(point);
                    continue;
                }
                let start =
                    if fields[1].ends_with(", Last>") { first.take().unwrap() } else { point };
                if wanted {
                    hidden.extend(start..=point);
                }
            }
            for fields in lines(root, "DerivedCoreProperties.txt") {
                if fields[1] == "Default_Ignorable_Code_Point" {
                    let (start, end) = span(&fields[0]);
                    hidden.extend(start..=end);
                }
            }

            let mut scripts = BTreeMap::new();
            for fields in lines(root, "Scripts.txt") {
                let (start, end) = span(&fields[0]);
                for point in start..=end {
                    scripts.insert(point, fields[1].clone());
                }
            }
            let mut standardized = BTreeSet::new();
            for fields in lines(root, "StandardizedVariants.txt") {
                let mut sequence = fields[0].split_whitespace();
                let base = u32::from_str_radix(sequence.next().unwrap(), 16).unwrap();
                let selector = u32::from_str_radix(sequence.next().unwrap(), 16).unwrap();
                let script = scripts.get(&base).map_or("", String::as_str);
                if (0xFE00..=0xFE0D).contains(&selector) && !matches!(script, "Han" | "Mongolian") {
                    standardized.insert((base, selector));
                }
            }
            let mut emoji = BTreeSet::new();
            for fields in lines(root, "emoji/emoji-variation-sequences.txt") {
                let base = fields[0].split_whitespace().next().unwrap();
                emoji.insert(u32::from_str_radix(base, 16).unwrap());
            }

            let mut out = String::new();
            out.push_str(HEADER);
            let _ = writeln!(out, "pub(super) const UNICODE: &str = \"{version}\";\n");
            out.push_str(HIDDEN_DOC);
            table(&mut out, "HIDDEN", &ranges(&hidden));
            out.push('\n');
            out.push_str(EMOJI_DOC);
            table(&mut out, "EMOJI_VARIATION_BASES", &ranges(&emoji));
            out.push('\n');
            out.push_str(STANDARDIZED_DOC);
            table(&mut out, "STANDARDIZED_VARIANTS", &standardized.into_iter().collect::<Vec<_>>());
            out
        }

        const HEADER: &str = "\
// SPDX-License-Identifier: Apache-2.0
// Generated from the Unicode Character Database by
// `escape::tests::the_tables_are_what_the_unicode_data_derives`, which derives
// it again from /usr/share/unicode and compares; do not edit by hand.

/// The version of the Unicode Character Database the tables were derived from.
";

        const HIDDEN_DOC: &str = "\
/// Every code point whose `General_Category` is Cc, Cf, Zl or Zp in
/// `UnicodeData.txt`, and every `Default_Ignorable_Code_Point` in
/// `DerivedCoreProperties.txt`, as sorted inclusive ranges.
";

        const EMOJI_DOC: &str = "\
/// Every base of a sequence in `emoji/emoji-variation-sequences.txt`, which is
/// what U+FE0E and U+FE0F can select, as sorted inclusive ranges.
";

        const STANDARDIZED_DOC: &str = "\
/// Every `(base, selector)` in `StandardizedVariants.txt` whose selector is
/// U+FE00–U+FE0D and whose base is neither Han nor Mongolian, which have rules
/// of their own, sorted.
";
    }
}
