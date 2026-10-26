// SPDX-License-Identifier: Apache-2.0
//! Shaping one line, with per-cluster fallback and enough of UAX #9 to put a
//! file extension where a reader expects it.

use swash::shape::Direction;
use swash::text::cluster::{CharCluster, Parser, Status, Token};
use swash::text::{Codepoint, Script};

use crate::escape::Hidden;
use crate::fonts::{Fonts, GlyphKey, Weight};

/// One shaped glyph, positioned relative to the run origin in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    /// Index into the fallback chain.
    pub face: u16,
    /// The face's own glyph id.
    pub glyph: u16,
    /// Pen offset from the run origin.
    pub x: f32,
    /// Baseline offset, downward positive.
    pub y: f32,
    /// Whether this glyph draws a hidden code point's escape rather than the
    /// text — see [`crate::escape`]. The painter dims it, so an escape cannot be
    /// mistaken for the same eight characters typed into a name.
    pub escape: bool,
}

/// A shaped line.
#[derive(Clone, Debug, Default)]
pub struct ShapedRun {
    /// The glyphs, in visual order.
    pub glyphs: Vec<ShapedGlyph>,
    /// Total advance in device pixels.
    pub advance: f32,
    /// Bytes of the string this was shaped from.
    ///
    /// Kept so the layout cache can `debug_assert` that a hit was shaped from the
    /// string it is being asked about. Four bytes against a `Vec` of glyphs.
    pub source_len: usize,
}

impl ShapedRun {
    /// How many glyphs fit before `limit` device pixels, and the advance at that
    /// point. Used to decide where to put an ellipsis.
    #[must_use]
    pub fn truncate_at(&self, limit: f32) -> (usize, f32) {
        let mut count = 0;
        let mut advance = 0.0;
        for (index, glyph) in self.glyphs.iter().enumerate() {
            if glyph.x > limit {
                return (count, advance);
            }
            count = index + 1;
            advance = glyph.x;
        }
        (count, self.advance)
    }
}

impl Fonts {
    /// Shapes one line at `px_size` device pixels.
    ///
    /// Segmentation is per run rather than per paragraph: filenames are short,
    /// and a full Unicode bidi pass over a thirty-character string costs more
    /// than it buys. What is kept from UAX #9 is documented on [`segment`].
    ///
    /// The run is of what [`crate::escape::shown`] draws `text` as, so a hidden
    /// code point is shaped as its escape — here, once, for every string any view
    /// draws. `source_len` stays the length of `text` itself, because that is the
    /// string the layout cache is asked about.
    pub fn shape(&mut self, text: &str, px_size: f32, weight: Weight) -> ShapedRun {
        self.shape_part(text, text.len(), px_size, weight, Hidden::Escaped)
    }

    /// Shapes the first `end` bytes of `text`, with its hidden code points drawn
    /// as `hidden` says.
    ///
    /// **The rest of `text` is read and not drawn**, because whether a joiner
    /// that ends a prefix is escaped depends on what follows it — see
    /// [`crate::escape::shown_until`]. This is what a caret is measured with.
    pub fn shape_part(
        &mut self,
        text: &str,
        end: usize,
        px_size: f32,
        weight: Weight,
        hidden: Hidden,
    ) -> ShapedRun {
        let kept = match hidden {
            Hidden::Escaped => self.what_shows(text, px_size, weight),
            Hidden::Verbatim => Vec::new(),
        };
        let primary = self.primary(weight) as usize;
        let Self { faces, shape_context, scratch, .. } = self;
        let shown = match hidden {
            Hidden::Escaped => crate::escape::shown_until(text, end, &kept, &mut scratch.escapes),
            Hidden::Verbatim => {
                scratch.escapes.clear();
                std::borrow::Cow::Borrowed(text.get(..end).unwrap_or(text))
            }
        };
        let source_len = end.min(text.len());
        let text = shown.as_ref();
        segment(text, scratch);

        let mut glyphs = Vec::with_capacity(text.len());
        let mut pen = 0.0f32;

        for index in 0..scratch.runs.len() {
            let (start, end, script, rtl) = {
                let run = &scratch.runs[index];
                (run.start, run.end, run.script, run.rtl)
            };
            let Some(segment_text) = text.get(start..end) else { continue };

            // Glyphs are collected per cluster before being positioned, because
            // an RTL run needs its *clusters* reversed while the glyphs inside
            // each cluster keep their shaped order. Deriving advances back out
            // of already-positioned glyphs loses the last one and invents a
            // first — which is the bug this shape exists to avoid.
            scratch.glyphs.clear();
            scratch.cluster_starts.clear();

            let mut cluster = CharCluster::new();
            let mut parser = Parser::new(
                script,
                segment_text.char_indices().map(|(offset, character)| Token {
                    ch: character,
                    offset: offset as u32,
                    len: character.len_utf8() as u8,
                    info: character.properties().into(),
                    data: 0,
                }),
            );

            while parser.next(&mut cluster) {
                let at = start + cluster.range().start as usize;
                let escape = scratch.escapes.iter().any(|range| range.contains(&at));
                // Cluster-level fallback: the first face that maps the whole
                // cluster wins; a partial map is remembered in case nothing
                // better appears. The primary face is tried first regardless of
                // its position in the chain, so a bold run stays bold.
                //
                // **In two passes, by presentation**: first the faces of the kind
                // the cluster asks for, then the others. So `™` alone comes from
                // the text face and `™` with U+FE0F from the emoji face, and a
                // pictograph only one kind of face has comes from that one.
                let colour = wants_colour(segment_text.get(cluster.range().to_range()));
                let mut chosen = primary;
                'search: for pass in [colour, !colour] {
                    for candidate in std::iter::once(primary).chain(0..faces.len()) {
                        let Some(face) = faces.get(candidate) else { continue };
                        if face.colour != pass {
                            continue;
                        }
                        let charmap = face.as_ref().charmap();
                        match cluster.map(|character| charmap.map(character)) {
                            Status::Complete => {
                                chosen = candidate;
                                break 'search;
                            }
                            Status::Keep => chosen = candidate,
                            Status::Discard => {}
                        }
                    }
                }

                let Some(face) = faces.get(chosen) else { continue };
                let mut shaper = shape_context
                    .builder(face.as_ref())
                    .script(script)
                    .direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight })
                    .size(px_size)
                    .build();
                shaper.add_cluster(&cluster);

                scratch.cluster_starts.push(scratch.glyphs.len());
                let sink = &mut scratch.glyphs;
                shaper.shape_with(|shaped| {
                    for glyph in shaped.glyphs {
                        sink.push((
                            chosen as u16,
                            glyph.id,
                            glyph.advance,
                            glyph.x,
                            glyph.y,
                            escape,
                        ));
                    }
                });
            }

            let clusters = scratch.cluster_starts.len();
            for position in 0..clusters {
                let index = if rtl { clusters - 1 - position } else { position };
                let from = scratch.cluster_starts[index];
                let to =
                    scratch.cluster_starts.get(index + 1).copied().unwrap_or(scratch.glyphs.len());
                for &(face, id, advance, x_offset, y_offset, escape) in &scratch.glyphs[from..to] {
                    let x = pen + x_offset;
                    glyphs.push(ShapedGlyph { face, glyph: id, x, y: y_offset, escape });
                    pen += advance;
                }
            }
        }

        ShapedRun { advance: pen, glyphs, source_len }
    }

    /// The byte offsets of the [`crate::escape::Verdict::Undecided`] code points
    /// in `text` that change what is drawn, and so are drawn as themselves.
    ///
    /// **Asked of the glyphs**, as the face draws them, because whether a face
    /// honours a variation sequence or ligates an emoji sequence is the face's
    /// business: `Brand™\u{FE0E}` and `Brand™` are the same pixels. One is kept
    /// when taking it out changes the inked glyphs of the cluster it is in — a
    /// variant, a family, a subdivision flag. This shaper shapes a cluster at a
    /// time, so nothing outside the cluster can depend on it.
    ///
    /// Only a name holding such a code point pays, and only on a layout-cache miss:
    /// two shapings of its cluster and a raster of the glyphs that differ, once
    /// per cluster — the answer is kept, and a folder of names ending in the same
    /// emoji asks once.
    fn what_shows(&mut self, text: &str, px_size: f32, weight: Weight) -> Vec<usize> {
        let mut undecided = crate::escape::undecided(text).peekable();
        if undecided.peek().is_none() {
            return Vec::new();
        }
        let undecided: Vec<usize> = undecided.collect();
        let mut kept = Vec::new();
        for at in undecided {
            let Some(character) = text[at..].chars().next() else { continue };
            let cluster = cluster_around(text, at);
            let key = (text[cluster.clone()].to_owned(), at - cluster.start, weight as u8);
            let shows = if let Some(&shows) = self.shows.get(&key) {
                shows
            } else {
                let shows = self.decide(&key.0, key.1, character, (px_size, weight));
                // Bounded the way the label cache is, and for its reason.
                if self.shows.len() >= SHOWS_CAP {
                    self.shows.clear();
                }
                self.shows.insert(key, shows);
                shows
            };
            if shows {
                kept.push(at);
            }
        }
        kept
    }

    /// Whether `character`, at `at` in `cluster`, changes what the cluster draws.
    fn decide(&mut self, cluster: &str, at: usize, character: char, size: (f32, Weight)) -> bool {
        let without = [&cluster[..at], &cluster[at + character.len_utf8()..]].concat();
        let bare = self.glyphs(&without, size);
        let with = self.glyphs(cluster, size);
        self.differs((&with, &bare), size.0, &mut std::collections::HashMap::new())
    }

    /// The glyphs `text` shapes to as the font has it, in order.
    fn glyphs(&mut self, text: &str, size: (f32, Weight)) -> Vec<(u16, u16)> {
        if text.is_empty() {
            return Vec::new();
        }
        let (px_size, weight) = size;
        let run = self.shape_part(text, text.len(), px_size, weight, Hidden::Verbatim);
        run.glyphs.iter().map(|glyph| (glyph.face, glyph.glyph)).collect()
    }

    /// Whether two runs put different ink on the screen.
    ///
    /// Only the glyphs one run has and the other does not are rasterized, which
    /// is usually the one a selector maps to: a face that maps it to an empty
    /// glyph, or not at all, draws nothing there, and the missing-glyph box is a
    /// box, not a variant.
    fn differs(
        &mut self,
        runs: (&Glyphs, &Glyphs),
        px_size: f32,
        ink: &mut std::collections::HashMap<(u16, u16), bool>,
    ) -> bool {
        let (first, second) = runs;
        if first == second {
            return false;
        }
        let mut count = std::collections::HashMap::new();
        for &glyph in first {
            *count.entry(glyph).or_insert(0i32) += 1;
        }
        for &glyph in second {
            *count.entry(glyph).or_insert(0) -= 1;
        }
        let odd: Vec<(u16, u16)> =
            count.into_iter().filter(|&(_, n)| n != 0).map(|(glyph, _)| glyph).collect();
        let mut keep =
            |glyph: &(u16, u16)| !odd.contains(glyph) || self.has_ink(*glyph, px_size, ink);
        let first: Vec<_> = first.iter().filter(|glyph| keep(glyph)).collect();
        let second: Vec<_> = second.iter().filter(|glyph| keep(glyph)).collect();
        first != second
    }

    /// Whether a glyph puts any ink down; the missing-glyph box never counts.
    fn has_ink(
        &mut self,
        glyph: (u16, u16),
        px_size: f32,
        ink: &mut std::collections::HashMap<(u16, u16), bool>,
    ) -> bool {
        let (face, id) = glyph;
        if id == 0 {
            return false;
        }
        if let Some(&inked) = ink.get(&glyph) {
            return inked;
        }
        let quarter_px = (px_size * 4.0).round().clamp(0.0, f32::from(u16::MAX)) as u16;
        let key = GlyphKey { face, glyph: id, quarter_px, subpixel: 0 };
        let inked = self.rasterize(key).is_some_and(|image| image.data.iter().any(|&a| a != 0));
        ink.insert(glyph, inked);
        inked
    }
}

/// A run's glyphs as face and glyph id, which is what two runs are compared by.
type Glyphs = [(u16, u16)];

/// Decisions [`Fonts::what_shows`] keeps before clearing them all.
pub(crate) const SHOWS_CAP: usize = 1024;

/// Whether a cluster asks for a colour face: U+FE0F asks for one and U+FE0E for
/// a text face, and a cluster that asks neither is drawn as its first
/// character's `Emoji_Presentation` says.
fn wants_colour(cluster: Option<&str>) -> bool {
    let Some(cluster) = cluster else { return false };
    if cluster.contains('\u{FE0F}') {
        return true;
    }
    if cluster.contains('\u{FE0E}') {
        return false;
    }
    cluster.chars().next().is_some_and(crate::escape::emoji_presentation)
}

/// The byte range of the cluster `swash` would shape the code point at `at` in.
///
/// Parsed under the script of the nearest letter before it, because a cluster's
/// extent is script-dependent only for the scripts that have syllables, and
/// those are the letters this is judged after.
fn cluster_around(text: &str, at: usize) -> std::ops::Range<usize> {
    let script = text[..at]
        .chars()
        .rev()
        .map(Codepoint::script)
        .find(|script| !matches!(script, Script::Common | Script::Inherited | Script::Unknown))
        .unwrap_or(Script::Latin);
    let mut cluster = CharCluster::new();
    let mut parser = Parser::new(
        script,
        text.char_indices().map(|(offset, character)| Token {
            ch: character,
            offset: offset as u32,
            len: character.len_utf8() as u8,
            info: character.properties().into(),
            data: 0,
        }),
    );
    while parser.next(&mut cluster) {
        let range = cluster.range().to_range();
        if range.contains(&at) {
            return range;
        }
    }
    at..at + text[at..].chars().next().map_or(0, char::len_utf8)
}

/// Reusable buffers for segmentation.
///
/// Shaping happens on a cache miss, which during a fling is every visible row.
/// Allocating four vectors per row per frame would show up in the frame budget,
/// so they are owned by [`Fonts`] and cleared instead.
#[derive(Default)]
pub(crate) struct Scratch {
    chars: Vec<(u32, Script, Class)>,
    prev_strong: Vec<Option<usize>>,
    next_strong: Vec<Option<usize>>,
    runs: Vec<Run>,
    glyphs: Vec<(u16, u16, f32, f32, f32, bool)>,
    cluster_starts: Vec<usize>,
    /// Where [`crate::escape::shown`] put each escape in the string being shaped.
    escapes: Vec<std::ops::Range<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Common, Inherited or Unknown: spaces, digits, punctuation, combining
    /// marks. Takes its direction from its neighbours.
    Neutral,
    LeftToRight,
    RightToLeft,
}

struct Run {
    start: usize,
    end: usize,
    script: Script,
    rtl: bool,
}

fn class_of(script: Script) -> Class {
    match script {
        Script::Common | Script::Inherited | Script::Unknown => Class::Neutral,
        Script::Arabic | Script::Hebrew | Script::Thaana | Script::Syriac => Class::RightToLeft,
        _ => Class::LeftToRight,
    }
}

/// Splits a string into runs that each have one script and one direction.
///
/// Not UAX #9. The one rule taken from it is the one whose absence is visible in
/// a file list: a neutral character is right-to-left only when the nearest
/// strong character on **both** sides is. That is what keeps the `.webp` in
/// `קובץ.webp` at the end where a reader expects it, instead of being swept into
/// the Hebrew run and reordered to the far left.
pub(crate) fn segment(text: &str, scratch: &mut Scratch) {
    let Scratch { chars, prev_strong, next_strong, runs, .. } = scratch;
    chars.clear();
    runs.clear();
    for (offset, character) in text.char_indices() {
        let script = character.script();
        chars.push((offset as u32, script, class_of(script)));
    }

    prev_strong.clear();
    let mut last = None;
    for (index, (_, _, class)) in chars.iter().enumerate() {
        prev_strong.push(last);
        if *class != Class::Neutral {
            last = Some(index);
        }
    }

    next_strong.clear();
    next_strong.resize(chars.len(), None);
    let mut upcoming = None;
    for index in (0..chars.len()).rev() {
        next_strong[index] = upcoming;
        if chars[index].2 != Class::Neutral {
            upcoming = Some(index);
        }
    }

    for index in 0..chars.len() {
        let (offset, script, class) = chars[index];
        let end = chars.get(index + 1).map_or(text.len(), |next| next.0 as usize);

        let (script, rtl) = match class {
            Class::RightToLeft => (script, true),
            Class::LeftToRight => (script, false),
            Class::Neutral => {
                let before = prev_strong[index].map(|i| chars[i]);
                let after = next_strong[index].map(|i| chars[i]);
                let both_rtl = matches!(
                    (before, after),
                    (Some((_, _, Class::RightToLeft)), Some((_, _, Class::RightToLeft)))
                );
                if both_rtl {
                    (before.map_or(Script::Latin, |(_, script, _)| script), true)
                } else {
                    // The base direction is left to right. Attach to the
                    // adjacent left-to-right run so shaping still sees one run
                    // per word rather than one per space.
                    let host = match (before, after) {
                        (Some((_, script, Class::LeftToRight)), _)
                        | (_, Some((_, script, Class::LeftToRight))) => script,
                        _ => Script::Latin,
                    };
                    (host, false)
                }
            }
        };

        match runs.last_mut() {
            Some(run) if run.script == script && run.rtl == rtl => run.end = end,
            _ => runs.push(Run { start: offset as usize, end, script, rtl }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Scratch, segment};
    use crate::fonts::{Fonts, GlyphKey, Weight};

    fn runs(text: &str) -> Vec<(String, bool)> {
        let mut scratch = Scratch::default();
        segment(text, &mut scratch);
        scratch.runs.iter().map(|run| (text[run.start..run.end].to_owned(), run.rtl)).collect()
    }

    #[test]
    fn an_extension_after_hebrew_stays_left_to_right() {
        let found = runs("קובץ.webp");
        assert!(found.iter().any(|(text, rtl)| text.contains("webp") && !rtl), "{found:?}");
    }

    #[test]
    fn a_space_between_two_hebrew_words_joins_them() {
        let found = runs("קובץ חדש");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1);
    }

    #[test]
    fn an_ascii_name_is_one_run() {
        assert_eq!(runs("Makefile.am").len(), 1);
    }

    #[test]
    fn shaping_advances_monotonically_and_covers_every_cluster() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        for text in ["Makefile", "日本語のファイル.txt", "קובץ.webp", "a🦀b"] {
            let run = fonts.shape(text, 13.0, Weight::Regular);
            assert!(!run.glyphs.is_empty(), "{text} shaped to nothing");
            assert!(run.advance > 0.0, "{text} has no width");
            let (count, advance) = run.truncate_at(run.advance);
            assert_eq!(count, run.glyphs.len());
            assert!(advance <= run.advance);
        }
    }

    #[test]
    fn truncation_reports_what_fits_and_not_what_does_not() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let run = fonts.shape("a very long file name indeed.txt", 13.0, Weight::Regular);
        let (count, advance) = run.truncate_at(run.advance / 2.0);
        assert!(count > 0 && count < run.glyphs.len(), "{count} of {}", run.glyphs.len());
        assert!(advance <= run.advance / 2.0);
    }

    /// **The defect, as a unit: two names that were the same pixels.** Before
    /// escaping, `pay\u{200D}pal.txt` shaped to exactly the glyphs of
    /// `paypal.txt`, and `invoice\u{202E}fdp.desktop` to those of
    /// `invoicefdp.desktop`.
    #[test]
    fn a_hidden_code_point_is_drawn_where_it_is_and_marked_as_an_escape() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        for (hidden, plain, at) in [
            ("pay\u{200D}pal.txt", "paypal.txt", 3),
            ("invoice\u{202E}fdp.desktop", "invoicefdp.desktop", 7),
            ("line\nbreak.txt", "linebreak.txt", 4),
        ] {
            let run = fonts.shape(hidden, 13.0, Weight::Regular);
            let without = fonts.shape(plain, 13.0, Weight::Regular);
            assert!(run.advance > without.advance, "{hidden:?} draws as wide as {plain:?}");
            assert_eq!(run.source_len, hidden.len(), "the cache is asked about the real name");
            // `<U+XXXX>` is eight ASCII clusters, one glyph each in any UI face,
            // and it sits exactly where the code point is — U+202E reorders
            // nothing, before this change or after it.
            let marked: Vec<usize> =
                (0..run.glyphs.len()).filter(|&index| run.glyphs[index].escape).collect();
            assert_eq!(marked, (at..at + 8).collect::<Vec<_>>(), "{hidden:?}");
            for pair in run.glyphs.windows(2) {
                assert!(pair[0].x <= pair[1].x, "{hidden:?} is not in logical order");
            }
        }
    }

    /// And nothing else is dimmed: an ordinary name, an emoji sequence held
    /// together by joiners, and a right-to-left name all shape with no escape.
    #[test]
    fn a_name_with_nothing_hidden_has_no_escape_glyphs() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        for text in [
            "report.pdf",
            "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}.txt",
            "קובץ.webp",
        ] {
            let run = fonts.shape(text, 13.0, Weight::Regular);
            assert!(run.glyphs.iter().all(|glyph| !glyph.escape), "{text:?}");
        }
    }

    /// What a run puts on the screen: each inked glyph, where it is, and how far
    /// the run reaches — the pixels a person compares two rows by.
    fn pixels(fonts: &mut Fonts, text: &str) -> (Vec<(u16, u16, i32)>, i32) {
        let run = fonts.shape(text, 13.0, Weight::Regular);
        let inked = run
            .glyphs
            .iter()
            .filter(|glyph| {
                let key =
                    GlyphKey { face: glyph.face, glyph: glyph.glyph, quarter_px: 52, subpixel: 0 };
                fonts.rasterize(key).is_some_and(|image| image.data.iter().any(|&a| a != 0))
            })
            .map(|glyph| (glyph.face, glyph.glyph, glyph.x.round() as i32))
            .collect();
        (inked, run.advance.round() as i32)
    }

    /// **The second-round defect, as a unit: three names, one picture.** This
    /// shaper honours neither presentation selector on ™, so `Brand™.png` and the
    /// same name with U+FE0E or U+FE0F drew the same pixels. Whatever the faces
    /// do with a selector, two names that differ by one must not.
    #[test]
    fn names_that_differ_by_a_selector_never_draw_alike() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        for names in [
            ["Brand\u{2122}.png", "Brand\u{2122}\u{FE0E}.png", "Brand\u{2122}\u{FE0F}.png"],
            ["\u{2764}.txt", "\u{2764}\u{FE0E}.txt", "\u{2764}\u{FE0F}.txt"],
            ["\u{A9}.txt", "\u{A9}\u{FE0E}.txt", "\u{A9}\u{FE0F}.txt"],
            ["\u{2192}.txt", "\u{2192}\u{FE0E}.txt", "\u{2192}\u{FE0F}.txt"],
            ["\u{8279}.txt", "\u{8279}\u{FE00}.txt", "\u{8279}\u{E0100}.txt"],
            ["\u{1820}.txt", "\u{1820}\u{180B}.txt", "\u{1820}\u{180C}.txt"],
            ["\u{1F468}\u{1F469}", "\u{1F468}\u{200D}\u{1F469}", "\u{1F468}\u{200C}\u{1F469}"],
        ] {
            let drawn: Vec<_> = names.iter().map(|name| pixels(&mut fonts, name)).collect();
            for first in 0..names.len() {
                for second in first + 1..names.len() {
                    assert_ne!(
                        drawn[first], drawn[second],
                        "{:?} and {:?} draw alike",
                        names[first], names[second]
                    );
                }
            }
        }
    }

    /// And a sequence the emoji face draws as one glyph stays one glyph: a
    /// family's joiners and a rainbow flag's FE0F are what the glyph is spelled
    /// with, so cutting the sequence for an escape would draw three people and a
    /// white flag. Asked only where this machine's emoji face ligates them.
    #[test]
    fn a_sequence_the_emoji_face_draws_as_one_glyph_is_not_escaped() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        for sequence in [
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
        ] {
            let verbatim = fonts.shape_part(
                sequence,
                sequence.len(),
                13.0,
                Weight::Regular,
                crate::escape::Hidden::Verbatim,
            );
            if verbatim.glyphs.len() != 1 {
                continue;
            }
            let run = fonts.shape(sequence, 13.0, Weight::Regular);
            assert!(run.glyphs.iter().all(|glyph| !glyph.escape), "{sequence:?} was cut");
        }
    }

    /// **A presentation selector chooses the face**: `™` alone is the text face's,
    /// `™` with U+FE0F the colour face's, and `™` with U+FE0E the text face's
    /// again — so that one changes nothing and is escaped.
    #[test]
    fn a_presentation_selector_chooses_between_the_text_and_the_colour_face() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let face_of = |fonts: &mut Fonts, text: &str| {
            let run = fonts.shape(text, 13.0, Weight::Regular);
            run.glyphs.first().map(|glyph| fonts.faces[usize::from(glyph.face)].colour)
        };
        let has_colour = fonts.faces.iter().any(|face| face.colour);
        let has_text = fonts
            .faces
            .iter()
            .any(|face| !face.colour && face.as_ref().charmap().map('\u{2122}') != 0);
        if !(has_colour && has_text) {
            return;
        }
        assert_eq!(face_of(&mut fonts, "\u{2122}"), Some(false));
        assert_eq!(face_of(&mut fonts, "\u{2122}\u{FE0F}"), Some(true));
        assert_eq!(face_of(&mut fonts, "\u{2122}\u{FE0E}"), Some(false));
        let text = fonts.shape("\u{2122}\u{FE0E}", 13.0, Weight::Regular);
        assert!(text.glyphs.iter().any(|glyph| glyph.escape), "U+FE0E after ™ changes nothing");
        let emoji = fonts.shape("\u{2122}\u{FE0F}", 13.0, Weight::Regular);
        assert!(emoji.glyphs.iter().all(|glyph| !glyph.escape), "U+FE0F after ™ is the emoji");
    }

    /// **A gap, recorded rather than promised.** U+FE0F after a pictograph is
    /// never escaped, because it is how most emoji in real names are spelled —
    /// but where the chain has the pictograph in one presentation only, the two
    /// forms draw alike. `👨` is an emoji by its own property, so with and
    /// without the selector it is the colour face's; `🏖` is text by default and
    /// no text face on this chain has it. When a chain has both, the second half
    /// of this fails for that pictograph, and the gap is closed there.
    #[test]
    fn a_pictograph_with_one_presentation_draws_alike_with_and_without_fe0f() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        for base in ['\u{1F468}', '\u{1F3D6}'] {
            let (bare, with) = (format!("{base}.jpeg"), format!("{base}\u{FE0F}.jpeg"));
            assert!(fonts.shape(&with, 13.0, Weight::Regular).glyphs.iter().all(|g| !g.escape));
            let text_face = fonts
                .faces
                .iter()
                .any(|face| !face.colour && face.as_ref().charmap().map(base) != 0);
            if !text_face {
                assert_eq!(pixels(&mut fonts, &bare), pixels(&mut fonts, &with), "{base:?}");
            }
        }
    }

    /// **A gap, recorded rather than promised.** A ZWNJ in a Persian word is kept
    /// by rule — escaping it would mark ordinary words — but this shaper does not
    /// join Arabic, so here it changes nothing drawn and two Persian names that
    /// differ only by one draw alike. When the shaper joins Arabic the second
    /// assertion fails, and the gap is closed: delete it then.
    #[test]
    fn a_persian_zwnj_is_kept_by_rule_and_draws_nothing_here_yet() {
        let Ok(mut fonts) = Fonts::discover() else { return };
        let (with, without) = (
            "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}.txt",
            "\u{645}\u{6CC}\u{62E}\u{648}\u{627}\u{647}\u{645}.txt",
        );
        assert!(fonts.shape(with, 13.0, Weight::Regular).glyphs.iter().all(|g| !g.escape));
        assert_eq!(pixels(&mut fonts, with), pixels(&mut fonts, without));
    }
}
