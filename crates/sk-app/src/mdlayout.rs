// SPDX-License-Identifier: Apache-2.0
//! Rendered markdown, laid out into visual lines once per width.
//!
//! **[ADR 0029](../../../docs/adr/0029-markdown-renders-from-our-own-parser-and-unknown-syntax-is-its-own-text.md).**
//! [`crate::markdown`] turns a document into blocks; this turns blocks into
//! lines of placed runs, measured against the real faces at the real size. It is
//! the second layout pass in the preview panel — the first lays out a character
//! grid — and the two never run at once.
//!
//! **Once per width, not once per frame.** Measuring a thousand words costs more
//! than a frame has, so the result is cached against the width, the scale and the
//! body size, and a frame draws the window of lines it can see. That is the same
//! bargain [`crate::quicklook::Text::wrap`] already makes against a character
//! count.
//!
//! **A line owns its text**, which is what makes selection possible: a place in
//! the rendered view is a line and a byte of that line, exactly as a place in the
//! source view is a line of the file and a byte of it.

use sk_text::Weight;

use crate::markdown::{Align, Block, Kind, Run, Style};

/// What a run is drawn as, once the block it is in has had its say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ink {
    /// Ordinary prose.
    Body,
    /// A heading, at the level's own size.
    Heading(u8),
    /// Inside a code span or a code block.
    Code,
    /// A link's text.
    Link,
    /// A list's bullet or number, and a table's rule — dimmer than the words.
    Faint,
}

/// One run of text, placed on a line.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Placed {
    /// What it says.
    pub(crate) text: String,
    /// Where it starts, from the left of the panel's text column.
    pub(crate) x: f32,
    /// How wide it is.
    pub(crate) width: f32,
    /// Which face.
    pub(crate) weight: Weight,
    /// How big, in logical pixels.
    pub(crate) size: f32,
    /// What colour role.
    pub(crate) ink: Ink,
    /// Whether a line goes through it.
    pub(crate) struck: bool,
    /// Where it starts in the line's own text, for selection.
    pub(crate) from: usize,
}

/// What is drawn behind or beside a line, beyond its runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Rule {
    /// Nothing.
    #[default]
    None,
    /// A thematic break, or the line under a heading.
    Across,
    /// The line under a table's header row.
    Under,
}

/// One visual line of the rendered document.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Line {
    /// Where its top sits, from the top of the document.
    pub(crate) top: f32,
    /// How tall it is, including the space around a heading.
    pub(crate) height: f32,
    /// Where the baseline sits inside that height.
    pub(crate) baseline: f32,
    /// The runs, left to right.
    pub(crate) runs: Vec<Placed>,
    /// Everything the runs say, joined — what a selection covers.
    pub(crate) text: String,
    /// How many blockquotes this line is inside, for the bars at the left.
    pub(crate) quoted: u8,
    /// A rule drawn with it.
    pub(crate) rule: Rule,
    /// Whether a code background is drawn behind the whole line.
    pub(crate) code: bool,
}

/// A whole document, laid out.
#[derive(Clone, Default, PartialEq, Debug)]
pub(crate) struct Laid {
    /// Every visual line, top to bottom.
    pub(crate) lines: Vec<Line>,
    /// How tall the document is.
    pub(crate) height: f32,
    /// What it was laid out against, so a frame knows whether to do it again.
    against: (u32, u32, u32),
}

impl Laid {
    /// Whether this layout still answers for `width` at `size` and `scale`.
    pub(crate) fn fits(&self, width: f32, size: f32, scale: f32) -> bool {
        self.against == key(width, size, scale)
    }

    /// Which line a distance from the top falls on, clamped to the document.
    pub(crate) fn line_at(&self, y: f32) -> usize {
        let after = self.lines.partition_point(|line| line.top <= y);
        after.saturating_sub(1)
    }

    /// The lines that intersect a window, as a range.
    pub(crate) fn window(&self, from: f32, height: f32) -> std::ops::Range<usize> {
        let first = self.line_at(from);
        let mut last = first;
        while last < self.lines.len() && self.lines[last].top < from + height {
            last += 1;
        }
        first..last
    }
}

/// Where a byte of a line's text sits, from the left of the text column.
///
/// **One function, and the drawing and the pointer both use it.** A highlight is
/// drawn from here and a click is turned into a byte by [`byte_at`], which walks
/// the same runs — a second copy of this arithmetic is a selection that lands one
/// word left of where it was made, which is the trap `Hits::name_field` records.
#[must_use]
pub(crate) fn at_byte(line: &Line, byte: usize) -> f32 {
    let mut last = 0.0;
    for run in &line.runs {
        if run.from == usize::MAX {
            // A list marker is drawn, not selectable: it is not in `line.text`.
            continue;
        }
        let end = run.from + run.text.len();
        if byte <= run.from {
            return run.x;
        }
        if byte <= end {
            let inside = byte - run.from;
            let part = run.text.get(..inside).map_or(1.0, |head| {
                if run.text.is_empty() {
                    1.0
                } else {
                    head.chars().count() as f32 / run.text.chars().count() as f32
                }
            });
            return run.x + run.width * part;
        }
        last = run.x + run.width;
    }
    last
}

/// Which byte of a line's text a distance from the left lands on.
///
/// **Proportional inside a run, which is a lie that does not show.** A run is one
/// word; a caret placed a character early inside one is a selection that starts
/// mid-word, and measuring every prefix of every word would be a shaping call per
/// character per frame. The ends of runs are exact, which is where a person aims.
#[must_use]
pub(crate) fn byte_at(line: &Line, x: f32) -> usize {
    let mut best = 0;
    for run in &line.runs {
        if run.from == usize::MAX {
            continue;
        }
        if x < run.x {
            return run.from;
        }
        if x <= run.x + run.width {
            let across = ((x - run.x) / run.width.max(1.0)).clamp(0.0, 1.0);
            let letters = run.text.chars().count() as f32;
            let want = (across * letters).round() as usize;
            let byte = run.text.char_indices().nth(want).map_or(run.text.len(), |(index, _)| index);
            return run.from + byte;
        }
        best = run.from + run.text.len();
    }
    best
}

/// The cache key: everything a layout depends on, in whole units.
fn key(width: f32, size: f32, scale: f32) -> (u32, u32, u32) {
    (
        width.max(0.0).round() as u32,
        (size * 4.0).max(0.0).round() as u32,
        (scale * 100.0).max(0.0).round() as u32,
    )
}

/// How much bigger a heading is than the body, by level.
const HEADING: [f32; 6] = [1.9, 1.55, 1.3, 1.15, 1.05, 1.0];

/// The space above a heading, as a multiple of the body size.
const BEFORE: f32 = 0.9;

/// The gap a blank line leaves, as a multiple of the body size.
const GAP: f32 = 0.55;

/// One list level's indent, as a multiple of the body size.
const STEP: f32 = 1.6;

/// The room a blockquote's bar and its gap take, per level.
const QUOTE: f32 = 1.1;

/// What the caller has to lend the layout: a way to measure text.
pub(crate) trait Measure {
    /// How wide `text` is in logical pixels, in that face at that size.
    fn width_of(&mut self, text: &str, weight: Weight, size: f32) -> f32;
}

/// Lays a parsed document out for a column of `width` logical pixels.
pub(crate) fn lay_out(
    blocks: &[Block],
    width: f32,
    size: f32,
    scale: f32,
    measure: &mut impl Measure,
) -> Laid {
    let mut out = Laid { lines: Vec::new(), height: 0.0, against: key(width, size, scale) };
    let mut y = 0.0f32;
    for block in blocks {
        let left = f32::from(block.indent) * STEP * size + f32::from(block.quoted) * QUOTE * size;
        let room = (width - left).max(size * 4.0);
        match &block.kind {
            Kind::Gap => y += GAP * size,
            Kind::Rule => {
                y += GAP * size;
                out.lines.push(Line {
                    top: y,
                    height: size * 0.6,
                    baseline: size * 0.3,
                    runs: Vec::new(),
                    text: String::new(),
                    quoted: block.quoted,
                    rule: Rule::Across,
                    code: false,
                });
                y += size * 0.6 + GAP * size;
            }
            Kind::Heading(level, runs) => {
                let wants = Wants { left, room, size, ink: Ink::Body, block };
                y = heading(&mut out, runs, (*level, wants), y, measure);
            }
            Kind::Paragraph(runs) => {
                y = flow(
                    &mut out,
                    runs,
                    Wants { left, room, size, ink: Ink::Body, block },
                    y,
                    measure,
                );
            }
            Kind::Item { marker, runs } => {
                let wants = Wants { left, room, size, ink: Ink::Body, block };
                y = item(&mut out, (marker, runs), wants, y, measure);
            }
            Kind::Code { lines, .. } => {
                y += GAP * size * 0.6;
                for source in lines {
                    let width_of = measure.width_of(source, Weight::Mono, size);
                    let height = size * 1.45;
                    out.lines.push(Line {
                        top: y,
                        height,
                        baseline: size * 1.1,
                        runs: vec![Placed {
                            text: source.clone(),
                            x: left + size * 0.6,
                            width: width_of,
                            weight: Weight::Mono,
                            size,
                            ink: Ink::Code,
                            struck: false,
                            from: 0,
                        }],
                        text: source.clone(),
                        quoted: block.quoted,
                        rule: Rule::None,
                        code: true,
                    });
                    y += height;
                }
                y += GAP * size * 0.6;
            }
            Kind::Row { cells, header, align } => {
                y = row(&mut out, cells, (*header, align), (left, room, size), y, measure, block);
            }
        }
    }
    out.height = y;
    out
}

/// A heading: bigger, with space above it and a rule under the top two levels.
fn heading(
    out: &mut Laid,
    runs: &[Run],
    level: (u8, Wants<'_>),
    mut y: f32,
    measure: &mut impl Measure,
) -> f32 {
    let (level, wants) = level;
    let level = level.clamp(1, 6);
    let size = wants.size;
    y += BEFORE * size;
    let start = out.lines.len();
    let big = size * HEADING[usize::from(level) - 1];
    y = flow(out, runs, Wants { size: big, ink: Ink::Heading(level), ..wants }, y, measure);
    // A rule under the top two levels, which is what every renderer does and what
    // makes a long document skimmable.
    if level <= 2
        && let Some(last) = out.lines.get_mut(start..).and_then(<[Line]>::last_mut)
    {
        last.rule = Rule::Under;
    }
    y + GAP * size * 0.5
}

/// One list item: its text indented, and its marker in the margin beside the
/// first line — so a wrapped item lines up under its own words.
fn item(
    out: &mut Laid,
    parts: (&str, &[Run]),
    wants: Wants<'_>,
    y: f32,
    measure: &mut impl Measure,
) -> f32 {
    let (marker, runs) = parts;
    let size = wants.size;
    let mark = measure.width_of(marker, Weight::Regular, size);
    let start = out.lines.len();
    let inside = Wants { left: wants.left + STEP * size, room: wants.room - STEP * size, ..wants };
    let y = flow(out, runs, inside, y, measure);
    if let Some(first) = out.lines.get_mut(start) {
        first.runs.insert(
            0,
            Placed {
                text: marker.to_owned(),
                x: wants.left + STEP * size - mark - size * 0.4,
                width: mark,
                weight: Weight::Regular,
                size,
                ink: Ink::Faint,
                struck: false,
                from: usize::MAX,
            },
        );
    }
    y
}

/// Everything the flow of one block needs that is not the runs themselves.
#[derive(Clone, Copy)]
struct Wants<'a> {
    left: f32,
    room: f32,
    size: f32,
    ink: Ink,
    block: &'a Block,
}

/// Lays runs out into as many lines as they take, and answers the new bottom.
fn flow(
    out: &mut Laid,
    runs: &[Run],
    wants: Wants<'_>,
    mut y: f32,
    measure: &mut impl Measure,
) -> f32 {
    let Wants { left, room, size, ink, block } = wants;
    let height = size * 1.45;
    let mut line = fresh(y, height, size, block);
    let mut x = left;

    for run in runs {
        let weight = face(run.style, ink);
        let ink = if run.style.link {
            Ink::Link
        } else if run.style.code {
            Ink::Code
        } else {
            ink
        };
        // **Word by word, and a hard break is its own word.** Splitting on spaces
        // and keeping the separator is what lets a wrap fall between two words
        // without losing the space between them when they end up on one line.
        for piece in pieces(&run.text) {
            if piece == "\n" {
                out.lines
                    .push(std::mem::replace(&mut line, fresh(y + height, height, size, block)));
                y += height;
                x = left;
                continue;
            }
            let advance = measure.width_of(&piece, weight, size);
            let blank = piece.trim().is_empty();
            if !blank && x > left && x + advance > left + room {
                out.lines
                    .push(std::mem::replace(&mut line, fresh(y + height, height, size, block)));
                y += height;
                x = left;
            }
            if blank && x <= left {
                // A space at the start of a wrapped line is not drawn.
                continue;
            }
            let from = line.text.len();
            line.text.push_str(&piece);
            line.runs.push(Placed {
                text: piece,
                x,
                width: advance,
                weight,
                size,
                ink,
                struck: run.style.struck,
                from,
            });
            x += advance;
        }
    }
    if !line.runs.is_empty() || out.lines.last().is_none_or(|last| last.top < y) {
        out.lines.push(line);
        y += height;
    }
    y
}

/// A line with nothing on it yet.
fn fresh(top: f32, height: f32, size: f32, block: &Block) -> Line {
    Line {
        top,
        height,
        baseline: size * 1.1,
        runs: Vec::new(),
        text: String::new(),
        quoted: block.quoted,
        rule: Rule::None,
        code: false,
    }
}

/// Which face a run wants, given the block it is in.
fn face(style: Style, ink: Ink) -> Weight {
    if style.code {
        return Weight::Mono;
    }
    let bold = style.strong || matches!(ink, Ink::Heading(_));
    match (bold, style.emphasis) {
        (true, true) => Weight::BoldItalic,
        (true, false) => Weight::Bold,
        (false, true) => Weight::Italic,
        (false, false) => Weight::Regular,
    }
}

/// The words of a string, with the spaces between them kept as their own pieces.
fn pieces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut held = String::new();
    let mut spacing = false;
    for letter in text.chars() {
        if letter == '\n' {
            if !held.is_empty() {
                out.push(std::mem::take(&mut held));
            }
            out.push("\n".to_owned());
            continue;
        }
        let blank = letter == ' ' || letter == '\t';
        if !held.is_empty() && blank != spacing {
            out.push(std::mem::take(&mut held));
        }
        spacing = blank;
        held.push(letter);
    }
    if !held.is_empty() {
        out.push(held);
    }
    out
}

/// One table row, as one line with its cells at column stops.
fn row(
    out: &mut Laid,
    cells: &[Vec<Run>],
    head: (bool, &[Align]),
    place: (f32, f32, f32),
    y: f32,
    measure: &mut impl Measure,
    block: &Block,
) -> f32 {
    let (header, align) = head;
    let (left, room, size) = place;
    let columns = cells.len().max(1);
    // Equal columns, because measuring the widest cell needs the whole table and
    // a row is laid out on its own. A README's tables are short and this keeps
    // one row one line — which is what makes the whole document scrollable
    // without laying all of it out.
    let column = room / columns as f32;
    let height = size * 1.55;
    let mut line = fresh(y, height, size, block);
    line.rule = if header { Rule::Under } else { Rule::None };
    for (index, cell) in cells.iter().enumerate() {
        let stop = left + column * index as f32;
        let text: String = cell.iter().map(|run| run.text.as_str()).collect();
        let weight = if header { Weight::Bold } else { Weight::Regular };
        let advance = measure.width_of(&text, weight, size).min(column - size * 0.5);
        let x = match align.get(index).copied().unwrap_or_default() {
            Align::Left => stop,
            Align::Centre => stop + (column - advance) / 2.0,
            Align::Right => stop + column - advance - size * 0.5,
        };
        if !line.text.is_empty() {
            line.text.push('\t');
        }
        let from = line.text.len();
        line.text.push_str(&text);
        line.runs.push(Placed {
            text,
            x,
            width: advance,
            weight,
            size,
            ink: Ink::Body,
            struck: false,
            from,
        });
    }
    out.lines.push(line);
    y + height
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Ink, Laid, Measure, lay_out, pieces};
    use crate::markdown::parse;
    use sk_text::Weight;

    /// A measurer with no fonts: every character is the size wide.
    ///
    /// **Deliberately not the real one.** What is under test is the wrapping and
    /// the placing, and a fixed advance makes both arithmetic a reader can check
    /// — the real faces are what the golden frames are for.
    struct Grid;

    impl Measure for Grid {
        fn width_of(&mut self, text: &str, _weight: Weight, size: f32) -> f32 {
            text.chars().count() as f32 * size * 0.5
        }
    }

    fn laid(text: &str, width: f32) -> Laid {
        lay_out(&parse(text), width, 16.0, 1.0, &mut Grid)
    }

    /// Words go on one line until they do not fit, and then on the next.
    #[test]
    fn a_paragraph_wraps_at_the_width() {
        // Eight characters a word, eight pixels a character: three fit in 200.
        let out = laid("aaaaaaaa bbbbbbbb cccccccc dddddddd eeeeeeee", 200.0);
        assert!(out.lines.len() >= 2, "{:?}", out.lines.len());
        for line in &out.lines {
            let right = line.runs.last().map_or(0.0, |run| run.x + run.width);
            assert!(right <= 210.0, "a line runs to {right}");
        }
        let all: String =
            out.lines.iter().map(|line| line.text.trim().to_owned()).collect::<Vec<_>>().join(" ");
        assert!(all.contains("eeeeeeee"), "the last word is missing from {all:?}");
    }

    /// A heading is bigger than the body and carries a rule at the top levels.
    #[test]
    fn a_heading_is_bigger_and_underlined() {
        let out = laid("# Title\n\nbody\n", 400.0);
        let heading = out.lines.iter().find(|line| line.text.contains("Title")).expect("a heading");
        let body = out.lines.iter().find(|line| line.text.contains("body")).expect("the body");
        let big = heading.runs[0].size;
        assert!(big > body.runs[0].size, "{big} is not bigger");
        assert_eq!(heading.runs[0].ink, Ink::Heading(1));
        assert_eq!(heading.rule, super::Rule::Under);
    }

    /// A list item's marker sits left of its text, on the first line only.
    #[test]
    fn a_marker_is_in_the_margin_of_the_first_line() {
        let out = laid("- aaaaaaaa bbbbbbbb cccccccc dddddddd eeeeeeee ffffffff\n", 200.0);
        let first = out.lines.first().expect("a line");
        assert_eq!(first.runs[0].text, "\u{2022}");
        assert!(first.runs[0].x < first.runs[1].x, "the marker is not in the margin");
        // The marker is not part of the line's text, so a selection cannot take it.
        assert!(!first.text.starts_with('\u{2022}'));
        let second = out.lines.get(1).expect("a wrapped line");
        assert!(second.runs.iter().all(|run| run.text != "\u{2022}"), "the marker repeated");
    }

    /// Code is mono, one line per source line, and never wrapped.
    #[test]
    fn code_keeps_its_lines() {
        let out = laid("```\nlet a = 1;\nlet b = 2;\n```\n", 60.0);
        let code: Vec<&super::Line> = out.lines.iter().filter(|line| line.code).collect();
        assert_eq!(code.len(), 2);
        assert_eq!(code[0].text, "let a = 1;");
        assert!(code[0].runs.iter().all(|run| run.weight == Weight::Mono));
    }

    /// Emphasis and strong pick real faces, and both together pick the fourth.
    #[test]
    fn the_faces_follow_the_markup() {
        let out = laid("plain *em* **strong** ***both*** `code`\n", 900.0);
        let mut seen = Vec::new();
        for line in &out.lines {
            for run in &line.runs {
                seen.push((run.text.trim().to_owned(), run.weight));
            }
        }
        let face =
            |want: &str| seen.iter().find(|(text, _)| text == want).map(|(_, weight)| *weight);
        assert_eq!(face("plain"), Some(Weight::Regular));
        assert_eq!(face("em"), Some(Weight::Italic));
        assert_eq!(face("strong"), Some(Weight::Bold));
        assert_eq!(face("both"), Some(Weight::BoldItalic));
        assert_eq!(face("code"), Some(Weight::Mono));
    }

    /// A quote's lines say how deep they are, so the bars can be drawn.
    #[test]
    fn a_quote_marks_its_lines() {
        let out = laid("> quoted\n\nplain\n", 400.0);
        assert!(out.lines.iter().any(|line| line.quoted == 1 && line.text.contains("quoted")));
        assert!(out.lines.iter().any(|line| line.quoted == 0 && line.text.contains("plain")));
    }

    /// The window is the lines that intersect it, and nothing else.
    #[test]
    fn a_window_is_the_lines_it_covers() {
        let out = laid("one\n\ntwo\n\nthree\n\nfour\n\nfive\n", 400.0);
        let all = out.window(0.0, out.height);
        assert_eq!(all, 0..out.lines.len());
        let top = out.window(0.0, 1.0);
        assert!(top.len() <= 2, "a one-pixel window took {} lines", top.len());
        // Every line in the window really does intersect it.
        let from = out.height / 2.0;
        for index in out.window(from, 40.0) {
            let line = &out.lines[index];
            assert!(line.top + line.height >= from - 1.0 && line.top <= from + 40.0);
        }
    }

    /// A layout answers for the width it was made at and no other.
    #[test]
    fn a_layout_knows_what_it_was_made_for() {
        let out = laid("words\n", 400.0);
        assert!(out.fits(400.0, 16.0, 1.0));
        assert!(!out.fits(401.0, 16.0, 1.0));
        assert!(!out.fits(400.0, 18.0, 1.0));
        assert!(!out.fits(400.0, 16.0, 1.25));
    }

    /// A byte and a distance are the same place, whichever way it is asked.
    #[test]
    fn a_byte_and_a_pixel_agree() {
        let out = laid("one two three\n", 400.0);
        let line = out.lines.first().expect("a line");
        assert_eq!(super::byte_at(line, -50.0), 0);
        for byte in [0, 3, 4, 7, line.text.len()] {
            let x = super::at_byte(line, byte);
            let back = super::byte_at(line, x);
            assert!(
                back.abs_diff(byte) <= 1,
                "byte {byte} is at {x}, which reads back as {back} of {:?}",
                line.text
            );
        }
    }

    /// A marker is drawn and cannot be selected: it is not in the line's text.
    #[test]
    fn a_marker_is_not_selectable() {
        let out = laid("- item\n", 400.0);
        let line = out.lines.first().expect("a line");
        assert_eq!(line.text, "item");
        // The far left is the start of the text, not of the bullet.
        assert_eq!(super::byte_at(line, 0.0), 0);
    }

    /// Words and the spaces between them are separate pieces, so a wrap can fall
    /// between them without eating the space.
    #[test]
    fn the_pieces_of_a_string_are_words_and_spaces() {
        assert_eq!(pieces("a bc  d"), vec!["a", " ", "bc", "  ", "d"]);
        assert_eq!(pieces("one\ntwo"), vec!["one", "\n", "two"]);
        assert_eq!(pieces(""), Vec::<String>::new());
    }
}
