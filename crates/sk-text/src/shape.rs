// SPDX-License-Identifier: Apache-2.0
//! Shaping one line, with per-cluster fallback and enough of UAX #9 to put a
//! file extension where a reader expects it.

use swash::shape::Direction;
use swash::text::cluster::{CharCluster, Parser, Status, Token};
use swash::text::{Codepoint, Script};

use crate::escape::Hidden;
use crate::fonts::{Fonts, Weight};

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
        let primary = self.primary(weight) as usize;
        let Self { faces, shape_context, scratch, .. } = self;
        let shown = match hidden {
            Hidden::Escaped => crate::escape::shown_until(text, end, &mut scratch.escapes),
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
                let mut chosen = primary;
                let order = std::iter::once(primary).chain(0..faces.len());
                for candidate in order {
                    let Some(face) = faces.get(candidate) else { continue };
                    let charmap = face.as_ref().charmap();
                    match cluster.map(|character| charmap.map(character)) {
                        Status::Complete => {
                            chosen = candidate;
                            break;
                        }
                        Status::Keep => chosen = candidate,
                        Status::Discard => {}
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
    use crate::fonts::{Fonts, Weight};

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
            "family \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.jpg",
            "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}.txt",
            "קובץ.webp",
        ] {
            let run = fonts.shape(text, 13.0, Weight::Regular);
            assert!(run.glyphs.iter().all(|glyph| !glyph.escape), "{text:?}");
        }
    }
}
