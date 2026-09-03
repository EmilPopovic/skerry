//! A page's words, and what is selected out of them.
//!
//! **[ADR 0024](../../../docs/adr/0024-a-page-arrives-with-its-words.md).** A
//! page reply carries the renderer's own text layer beside its pixels; this is
//! what the panel does with it — a hit test, a selection, and the boxes to draw
//! behind it.
//!
//! # Why this is not [`crate::quicklook::Text`]
//!
//! A text file's selection is a line and a byte, because a text file *is* lines.
//! A page's is a word and a character inside it, because a page is boxes on
//! paper: what is above a point, what is nearest to it and what reading order
//! means are all geometry. The two share the anchor-and-caret shape and the three
//! commands above them — `Ctrl+A`, `Ctrl+C` and a drag — and nothing else. One
//! model over both would be the line-and-byte one with a coordinate system
//! bolted to its side.
//!
//! # Everything here is in the page's own points
//!
//! The layer is in PDF points with the origin at the page's top left, and the
//! panel draws the page as a texture that is fitted, zoomed and panned. The one
//! transform between them lives in [`crate::view`], which is also the only place
//! that knows where the page was drawn.

use sk_preview::wire::Words;

/// A place in a page's text: which word, and how far into it.
///
/// **A character index, not a byte.** It is compared, interpolated across a
/// word's box and multiplied by a width; every one of those wants characters, and
/// the byte offset is recovered once, where the text is cut.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub(crate) struct Place {
    /// Which word of the layer, in the order the tool gave them.
    pub(crate) word: usize,
    /// How many characters into that word.
    pub(crate) character: usize,
}

/// One page's text layer and the selection over it.
pub(crate) struct Page {
    /// The words, in the tool's reading order.
    layer: Words,
    /// Where a selection began and where it is now, or nothing.
    chosen: Option<(Place, Place)>,
}

impl Page {
    /// Reads a layer out of a reply's bytes. `None` where there is no text.
    pub(crate) fn of(bytes: &[u8]) -> Option<Self> {
        let layer = Words::unpack(bytes)?;
        (!layer.words.is_empty()).then_some(Self { layer, chosen: None })
    }

    /// The page's size in points, which every box here is relative to.
    pub(crate) fn size(&self) -> (f32, f32) {
        self.layer.page
    }

    /// How many words it holds.
    pub(crate) fn count(&self) -> usize {
        self.layer.words.len()
    }

    /// Starts a selection at `at`, or extends the one there is.
    pub(crate) fn choose(&mut self, at: Place, extend: bool) {
        let anchor = match self.chosen.filter(|_| extend) {
            Some((anchor, _)) => anchor,
            None => at,
        };
        self.chosen = Some((anchor, at));
    }

    /// Moves the open selection's far end.
    pub(crate) fn drag_to(&mut self, at: Place) -> bool {
        let Some((anchor, caret)) = self.chosen else { return false };
        if caret == at {
            return false;
        }
        self.chosen = Some((anchor, at));
        true
    }

    /// Selects the whole page.
    pub(crate) fn select_all(&mut self) {
        let last = self.layer.words.len().saturating_sub(1);
        let end = self.layer.words.last().map_or(0, |word| word.text.chars().count());
        self.chosen = Some((Place::default(), Place { word: last, character: end }));
    }

    /// Forgets what was selected.
    pub(crate) fn clear(&mut self) {
        self.chosen = None;
    }

    /// Whether anything is selected.
    pub(crate) fn has_selection(&self) -> bool {
        self.span().is_some()
    }

    /// The selection in reading order, where it covers anything at all.
    fn span(&self) -> Option<(Place, Place)> {
        let (anchor, caret) = self.chosen?;
        let (from, to) = if anchor <= caret { (anchor, caret) } else { (caret, anchor) };
        (from != to).then_some((from, to))
    }

    /// The selected text, with a newline where the tool changed line.
    ///
    /// **Spaces between words and newlines between lines**, because that is what
    /// the layer knows: poppler gives word boxes and a line grouping, and the
    /// gaps *inside* a line are not in it. A paste of a selected paragraph comes
    /// out as its words with single spaces, which is what a person wants out of a
    /// PDF far more often than its original whitespace.
    pub(crate) fn text(&self) -> Option<String> {
        let (from, to) = self.span()?;
        let mut out = String::new();
        let mut line = None;
        for (index, word) in self.layer.words.iter().enumerate() {
            if index < from.word || index > to.word {
                continue;
            }
            let characters: Vec<char> = word.text.chars().collect();
            let start = if index == from.word { from.character.min(characters.len()) } else { 0 };
            let end = if index == to.word {
                to.character.min(characters.len())
            } else {
                characters.len()
            };
            if start >= end {
                continue;
            }
            match line {
                Some(was) if was != word.line => out.push('\n'),
                Some(_) => out.push(' '),
                None => {}
            }
            line = Some(word.line);
            out.extend(&characters[start..end]);
        }
        (!out.is_empty()).then_some(out)
    }

    /// The boxes to draw the selection behind, in the page's own points.
    ///
    /// One per word, with the two ends cut to the characters they cover — which
    /// is where a word's box is interpolated: a character is assumed to be its
    /// share of the width, exact for a monospaced font and within half a
    /// character for anything else.
    pub(crate) fn highlights(&self) -> Vec<(f32, f32, f32, f32)> {
        let Some((from, to)) = self.span() else { return Vec::new() };
        let mut out = Vec::new();
        for (index, word) in self.layer.words.iter().enumerate() {
            if index < from.word || index > to.word {
                continue;
            }
            let characters = word.text.chars().count().max(1);
            let start = if index == from.word { from.character.min(characters) } else { 0 };
            let end = if index == to.word { to.character.min(characters) } else { characters };
            if start >= end {
                continue;
            }
            let per = word.at.2 / characters as f32;
            let left = word.at.0 + per * start as f32;
            out.push((left, word.at.1, per * (end - start) as f32, word.at.3));
        }
        out
    }

    /// Whether a point in the page's points is inside a word's own box.
    ///
    /// **Not `place_at`, which is never nothing.** This is the question a press
    /// asks — is there text here, or is this the paper — and the answer has to be
    /// able to be no.
    pub(crate) fn covers(&self, at: (f32, f32)) -> bool {
        // **Two points of slack around every box.** A press is a pixel and a
        // pixel is about a point, so a press aimed at a word's first character
        // lands a third of a point outside its box as often as inside it — and
        // the two answers are "select this word" and "move the page", which is
        // not a coin to toss. Two points is under a hundredth of a page's width
        // and invisible between words.
        const SLACK: f32 = 2.0;
        self.layer.words.iter().any(|word| {
            let (left, top, width, height) = word.at;
            at.0 >= left - SLACK
                && at.0 <= left + width + SLACK
                && at.1 >= top - SLACK
                && at.1 <= top + height + SLACK
        })
    }

    /// Which place a point in the page's points lands on.
    ///
    /// **Nearest, never nothing.** A drag that leaves the text has to keep
    /// extending the selection — that is what dragging past the edge means in
    /// every text view — so a point in a margin is the nearest end of the nearest
    /// line, and a point below the last line is the end of the page.
    pub(crate) fn place_at(&self, at: (f32, f32)) -> Place {
        let mut nearest = (f32::MAX, Place::default());
        for (index, word) in self.layer.words.iter().enumerate() {
            let (left, top, width, height) = word.at;
            let inside =
                at.0 >= left && at.0 <= left + width && at.1 >= top && at.1 <= top + height;
            if inside {
                return Place { word: index, character: self.character_at(index, at.0) };
            }
            // **The vertical distance dominates**, so a click far to the right of
            // a short line lands at the end of *that* line rather than in the
            // middle of the long one below it. A page is read in lines and a
            // pointer between two of them belongs to the nearer one.
            let below = (top - at.1).max(at.1 - (top + height)).max(0.0);
            let across = (left - at.0).max(at.0 - (left + width)).max(0.0);
            let distance = below * 4.0 + across;
            if distance < nearest.0 {
                let character =
                    if at.0 > left + width / 2.0 { word.text.chars().count() } else { 0 };
                nearest = (distance, Place { word: index, character });
            }
        }
        nearest.1
    }

    /// How many characters into a word a horizontal point falls.
    fn character_at(&self, index: usize, x: f32) -> usize {
        let Some(word) = self.layer.words.get(index) else { return 0 };
        let characters = word.text.chars().count();
        if word.at.2 <= 0.0 || characters == 0 {
            return 0;
        }
        let part = ((x - word.at.0) / word.at.2 * characters as f32).round();
        (part.max(0.0) as usize).min(characters)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Page, Place};
    use sk_preview::wire::{Word, Words};

    /// Two lines of three words, on a 100×50 point page: `one two three` at the
    /// top, `four five six` below it, each word ten points wide and ten tall.
    fn page() -> Page {
        let mut words = Vec::new();
        for (line, texts) in [["one", "two", "three"], ["four", "five", "six"]].iter().enumerate() {
            for (column, text) in texts.iter().enumerate() {
                words.push(Word {
                    at: (10.0 + column as f32 * 15.0, 10.0 + line as f32 * 20.0, 10.0, 10.0),
                    line: line as u32,
                    text: (*text).to_owned(),
                });
            }
        }
        Page::of(&Words { page: (100.0, 50.0), words }.pack()).expect("a layer")
    }

    /// A point inside a word is that word, and a fraction into it.
    #[test]
    fn a_point_inside_a_word_is_a_place_in_that_word() {
        let page = page();
        assert_eq!(page.place_at((11.0, 12.0)), Place { word: 0, character: 0 });
        // Two thirds across a three-character word is its second character.
        assert_eq!(page.place_at((17.0, 12.0)), Place { word: 0, character: 2 });
        assert_eq!(page.place_at((26.0, 32.0)), Place { word: 4, character: 0 });
    }

    /// A point in a margin is the nearest end of the nearest line, and one below
    /// the page is the end of it — because a drag has to keep extending.
    #[test]
    fn a_point_off_the_text_is_the_nearest_place_and_never_nothing() {
        let page = page();
        // Left of the first line: its start.
        assert_eq!(page.place_at((0.0, 12.0)), Place { word: 0, character: 0 });
        // Right of the second line: the end of its last word.
        assert_eq!(page.place_at((99.0, 32.0)), Place { word: 5, character: 3 });
        // Below everything: the last line, not the first — the vertical distance
        // is what decides, which is the assertion that fails if the weighting in
        // `place_at` is dropped.
        let below = page.place_at((12.0, 49.0));
        assert_eq!(below.word, 3, "a point under the page belongs to the last line");
    }

    /// A selection across two lines copies them with a newline between, and
    /// words within a line with single spaces.
    #[test]
    fn a_selection_across_lines_copies_with_a_newline_in_it() {
        let mut page = page();
        page.choose(Place { word: 1, character: 0 }, false);
        assert!(!page.has_selection(), "an empty selection is not one");
        page.drag_to(Place { word: 4, character: 4 });
        assert!(page.has_selection());
        assert_eq!(page.text().as_deref(), Some("two three\nfour five"));

        // Backwards is the same selection: a drag upward is an ordinary thing.
        let mut back = self::page();
        back.choose(Place { word: 4, character: 4 }, false);
        back.drag_to(Place { word: 1, character: 0 });
        assert_eq!(back.text().as_deref(), Some("two three\nfour five"));

        // Part of one word.
        let mut part = self::page();
        part.choose(Place { word: 2, character: 1 }, false);
        part.drag_to(Place { word: 2, character: 4 });
        assert_eq!(part.text().as_deref(), Some("hre"));
    }

    /// Select all covers every word, and the highlight boxes cover the words
    /// rather than the whole page.
    #[test]
    fn select_all_covers_the_page_and_the_boxes_cover_the_words() {
        let mut page = page();
        page.select_all();
        assert_eq!(page.text().as_deref(), Some("one two three\nfour five six"));
        let boxes = page.highlights();
        assert_eq!(boxes.len(), 6, "one box per word, not one for the page");
        // The first box is the first word's own box.
        assert!((boxes[0].0 - 10.0).abs() < 0.001);
        assert!((boxes[0].2 - 10.0).abs() < 0.001);

        // A partial word at each end is cut to the characters it covers: the
        // middle third of a three-character word is a third of its box, a third
        // of the way in.
        page.choose(Place { word: 0, character: 1 }, false);
        page.drag_to(Place { word: 0, character: 2 });
        let cut = page.highlights();
        assert_eq!(cut.len(), 1);
        assert!((cut[0].0 - (10.0 + 10.0 / 3.0)).abs() < 0.01, "{:?}", cut[0]);
        assert!((cut[0].2 - 10.0 / 3.0).abs() < 0.01, "{:?}", cut[0]);

        page.clear();
        assert!(!page.has_selection());
        assert_eq!(page.text(), None);
    }

    /// A page with no words is no page at all, which is what a scan answers.
    #[test]
    fn a_page_with_no_words_is_none() {
        assert!(Page::of(&[]).is_none());
        assert!(Page::of(&Words { page: (100.0, 50.0), words: Vec::new() }.pack()).is_none());
        let page = page();
        assert_eq!(page.count(), 6);
        assert_eq!(page.size(), (100.0, 50.0));
    }
}
