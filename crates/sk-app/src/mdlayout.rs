// SPDX-License-Identifier: Apache-2.0
//! Rendered markdown, laid out into visual lines once per width.
//!
//! **[ADR 0029](../../../docs/adr/0029-markdown-renders-from-our-own-parser-and-unknown-syntax-is-its-own-text.md).**
//! [`crate::markdown`] turns a document into blocks; this turns blocks into
//! lines of placed runs, measured against the real faces at the real size. It is
//! the second layout pass in the preview panel — the first lays out a character
//! grid — and the two never run at once.
//!
//! **Once per width, on a job, never in a frame.** Measuring a thousand words
//! costs more than a frame has, so the result is cached against the width, the
//! scale and the body size, made by
//! [`crate::quicklook::QuickLook::lay_out_for`]'s job, and a frame draws the
//! window of lines it can see.
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
pub(crate) fn key(width: f32, size: f32, scale: f32) -> (u32, u32, u32) {
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

    /// How wide each character of `word` is, in order, in logical pixels.
    ///
    /// **What a word too wide for its column is cut by**, so that cutting it is
    /// one shaping rather than one per prefix — which, through a label cache
    /// that empties at 256 entries, reshaped every prefix of a 256 KiB word: 11.4
    /// seconds at a 2,500-pixel column (one run, release build, load 4.5). This
    /// default measures each character alone, which is exact wherever every
    /// character is the same width.
    fn advances(&mut self, word: &str, weight: Weight, size: f32) -> Vec<f32> {
        let mut letter = [0; 4];
        word.chars().map(|it| self.width_of(it.encode_utf8(&mut letter), weight, size)).collect()
    }

    /// Whether whoever asked has stopped wanting the layout, so it can end early.
    fn stopped(&self) -> bool {
        false
    }
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
    let mut at = 0;
    while at < blocks.len() && !measure.stopped() {
        let block = &blocks[at];
        at += 1;
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
                let cell = measure.width_of("0", Weight::Mono, size);
                for source in lines {
                    let shown = seen(source, room, cell);
                    let width_of = measure.width_of(shown, Weight::Mono, size);
                    let height = size * 1.45;
                    out.lines.push(Line {
                        top: y,
                        height,
                        baseline: size * 1.1,
                        runs: vec![Placed {
                            text: shown.to_owned(),
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
            Kind::Row { .. } => {
                // **The whole table at once.** A column's width is the widest
                // cell *in that column*, which cannot be known from one row — and
                // laying rows out one at a time is what made every column the
                // same width and pushed the last one off the panel.
                let mut rows = at - 1;
                while blocks.get(rows).is_some_and(|it| matches!(it.kind, Kind::Row { .. })) {
                    rows += 1;
                }
                y = table(&mut out, &blocks[at - 1..rows], (left, room, size), y, measure);
                at = rows;
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

/// The part of a code line a column `room` wide can show, in a face whose
/// characters are `cell` wide.
///
/// **What is drawn, while the line's text stays whole** for a selection and a
/// copy. A code line is never wrapped — ADR 0029 — so the panel's edge cuts it,
/// and a run of the whole of it was a label the frame shaped: a 200 KiB line
/// among 1,400 words, which push it out of a label cache of 256, was reshaped
/// every frame, 41.6 ms of each (median of nine, release build, load 3.8).
/// Twice what fits, and then some, because a tab and a combining mark take no
/// room and the panel's edge is past the column's.
fn seen(source: &str, room: f32, cell: f32) -> &str {
    let fits = (room / cell.max(1.0)).ceil() as usize;
    source.char_indices().nth(fits * 2 + 8).map_or(source, |(at, _)| &source[..at])
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
            // A single paragraph can be the whole document, so a layout nobody
            // wants any more ends here as well as between blocks.
            if measure.stopped() {
                break;
            }
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
            // **A word wider than the column is cut.** A URL, a hash, a column of
            // dashes: without this it runs past the panel's edge and is clipped,
            // which reads as text that has gone missing.
            if !blank && advance > room {
                for (part, advance) in cut(&piece, room, (weight, size), measure) {
                    if x > left {
                        out.lines.push(std::mem::replace(
                            &mut line,
                            fresh(y + height, height, size, block),
                        ));
                        y += height;
                        x = left;
                    }
                    let from = line.text.len();
                    line.text.push_str(&part);
                    line.runs.push(Placed {
                        text: part,
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

/// A whole table: its columns measured together, its cells wrapped inside them.
///
/// **Measured as a group, because a column's width is a fact about the column.**
/// One row at a time gives every column the same share, which is what put a
/// `Values` cell over the top of the `Notes` beside it and the last column off
/// the panel entirely. The natural width of a column is its widest cell; where
/// they all fit, that is what they get, and where they do not, every column is
/// shrunk in proportion with a floor of about six characters — and what no longer
/// fits **wraps inside its cell** rather than running into the next one.
fn table(
    out: &mut Laid,
    rows: &[Block],
    place: (f32, f32, f32),
    mut y: f32,
    measure: &mut impl Measure,
) -> f32 {
    let (left, room, size) = place;
    let gap = size * 0.8;
    let columns = rows
        .iter()
        .filter_map(|block| match &block.kind {
            Kind::Row { cells, .. } => Some(cells.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if columns == 0 {
        return y;
    }
    let widths = share(rows, columns, (room, gap, size), measure);
    let mut stops = Vec::with_capacity(columns);
    let mut x = left;
    for width in &widths {
        stops.push(x);
        x += width + gap;
    }
    for block in rows {
        let Kind::Row { cells, header, align } = &block.kind else { continue };
        y = one_row(out, (cells, *header, align), (&stops, &widths, size), y, measure, block);
    }
    y + GAP * size * 0.5
}

/// How wide each column may be.
fn share(
    rows: &[Block],
    columns: usize,
    place: (f32, f32, f32),
    measure: &mut impl Measure,
) -> Vec<f32> {
    let (room, gap, size) = place;
    let mut natural = vec![0.0f32; columns];
    for block in rows {
        let Kind::Row { cells, header, .. } = &block.kind else { continue };
        let weight = if *header { Weight::Bold } else { Weight::Regular };
        for (index, cell) in cells.iter().enumerate().take(columns) {
            let text: String = cell.iter().map(|run| run.text.as_str()).collect();
            natural[index] = natural[index].max(measure.width_of(&text, weight, size));
        }
    }
    let available = (room - gap * (columns as f32 - 1.0)).max(size);
    let total: f32 = natural.iter().sum();
    if total <= available {
        return natural;
    }
    // Proportional, with a floor so a narrow column does not vanish, and a second
    // pass so the floors do not push the total back over the room.
    let floor = (size * 3.0).min(available / columns as f32);
    let mut widths: Vec<f32> =
        natural.iter().map(|it| (it * available / total).max(floor)).collect();
    let over: f32 = widths.iter().sum::<f32>() - available;
    if over > 0.0 {
        let slack: f32 = widths.iter().map(|it| (it - floor).max(0.0)).sum();
        if slack > 0.0 {
            for width in &mut widths {
                *width -= (*width - floor).max(0.0) * over / slack;
            }
        }
    }
    widths
}

/// One table row, wrapped inside its columns.
fn one_row(
    out: &mut Laid,
    what: (&[Vec<Run>], bool, &[Align]),
    place: (&[f32], &[f32], f32),
    mut y: f32,
    measure: &mut impl Measure,
    block: &Block,
) -> f32 {
    let (cells, header, align) = what;
    let (stops, widths, size) = place;
    let weight = if header { Weight::Bold } else { Weight::Regular };
    let height = size * 1.55;

    // Each cell wrapped inside its own column, and the row is as tall as the
    // deepest of them — which is the whole of what "a table lays out" means.
    let mut wrapped: Vec<Vec<Vec<(String, f32)>>> = Vec::with_capacity(cells.len());
    for (index, cell) in cells.iter().enumerate() {
        let width = widths.get(index).copied().unwrap_or(size * 4.0);
        let text: String = cell.iter().map(|run| run.text.as_str()).collect();
        wrapped.push(wrap_cell(&text, width, (weight, size), measure));
    }
    let deep = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

    for row in 0..deep {
        let mut line = fresh(y, height, size, block);
        // The rule goes under the *last* line of a header that wrapped.
        line.rule = if header && row + 1 == deep { Rule::Under } else { Rule::None };
        for (index, cell) in wrapped.iter().enumerate() {
            let Some(pieces) = cell.get(row) else { continue };
            let stop = stops.get(index).copied().unwrap_or(0.0);
            let width = widths.get(index).copied().unwrap_or(size * 4.0);
            let taken: f32 = pieces.iter().map(|(_, advance)| advance).sum();
            let offset = match align.get(index).copied().unwrap_or_default() {
                Align::Left => 0.0,
                Align::Centre => (width - taken).max(0.0) / 2.0,
                Align::Right => (width - taken).max(0.0),
            };
            if !line.text.is_empty() {
                line.text.push('\t');
            }
            let mut x = stop + offset;
            for (text, advance) in pieces {
                let from = line.text.len();
                line.text.push_str(text);
                line.runs.push(Placed {
                    text: text.clone(),
                    x,
                    width: *advance,
                    weight,
                    size,
                    ink: Ink::Body,
                    struck: false,
                    from,
                });
                x += advance;
            }
        }
        out.lines.push(line);
        y += height;
    }
    y
}

/// One cell's text, cut into lines that fit `width`.
///
/// Returns the pieces of each line with their advances, so the caller can align
/// the line inside the column without measuring it a second time.
fn wrap_cell(
    text: &str,
    width: f32,
    face: (Weight, f32),
    measure: &mut impl Measure,
) -> Vec<Vec<(String, f32)>> {
    let (weight, size) = face;
    let mut lines: Vec<Vec<(String, f32)>> = vec![Vec::new()];
    let mut x = 0.0f32;
    for piece in pieces(text) {
        let advance = measure.width_of(&piece, weight, size);
        let blank = piece.trim().is_empty();
        if !blank && x > 0.0 && x + advance > width {
            lines.push(Vec::new());
            x = 0.0;
        }
        if blank && x <= 0.0 {
            continue;
        }
        // A single word wider than the column is cut rather than left to run into
        // the next one — a column of hashes or a URL is where this happens.
        if !blank && advance > width && width > 0.0 {
            for (part, advance) in cut(&piece, width, (weight, size), measure) {
                if x > 0.0 {
                    lines.push(Vec::new());
                }
                x = advance;
                let Some(last) = lines.last_mut() else { continue };
                last.push((part, advance));
            }
            continue;
        }
        let Some(last) = lines.last_mut() else { continue };
        last.push((piece, advance));
        x += advance;
    }
    lines.retain(|line| !line.is_empty());
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// A word too wide for `width`, cut into pieces that are not, each with its width.
///
/// **From one measurement of the word**, a width per character, summed — never
/// by measuring a piece and then the piece with one more character on it.
fn cut(
    word: &str,
    width: f32,
    face: (Weight, f32),
    measure: &mut impl Measure,
) -> Vec<(String, f32)> {
    let (weight, size) = face;
    let advances = measure.advances(word, weight, size);
    let mut out = Vec::new();
    let mut held = String::new();
    let mut across = 0.0f32;
    for (letter, advance) in
        word.chars().zip(advances.iter().copied().chain(std::iter::repeat(0.0)))
    {
        if !held.is_empty() && across + advance > width {
            out.push((std::mem::take(&mut held), across));
            across = 0.0;
        }
        held.push(letter);
        across += advance;
    }
    if !held.is_empty() {
        out.push((held, across));
    }
    out
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

    /// **A column is as wide as its own widest cell, not a share of the panel.**
    ///
    /// The reported defect: every column got the same width, so a long `Values`
    /// cell ran over the `Notes` beside it and the last column left the panel.
    #[test]
    fn a_columns_width_is_its_own_widest_cell() {
        let text = concat!(
            "| A | Values | N |\n",
            "| - | ------ | - |\n",
            "| x | a much longer value than the others | y |\n",
        );
        let out = laid(text, 600.0);
        let header = out.lines.first().expect("a header");
        // The stop of each column is where its first word starts, and the gap
        // between them is that column's width — which is its own widest cell.
        let stop = |word: &str| {
            header.runs.iter().find(|run| run.text == word).map(|run| run.x).expect(word)
        };
        let (a, values, n) = (stop("A"), stop("Values"), stop("N"));
        assert!(values - a < n - values, "the columns are still even: {a} {values} {n}");
    }

    /// A cell too wide for its column wraps inside it, and the row grows.
    #[test]
    fn a_long_cell_wraps_inside_its_column() {
        let text = concat!(
            "| Variable | Values |\n",
            "| -------- | ------ |\n",
            "| ARCH | x86_64, x86_32, riscv_64, riscv_32, and several more besides |\n",
        );
        let narrow = laid(text, 260.0);
        // The header is one line; the body row is more than one.
        let body: Vec<&super::Line> =
            narrow.lines.iter().filter(|line| line.text.contains("ARCH")).collect();
        assert_eq!(body.len(), 1, "the row's first line holds the short cell");
        assert!(narrow.lines.len() > 2, "the long cell did not wrap: {:?}", narrow.lines.len());
        // Nothing is drawn past the width it was laid out for.
        for line in &narrow.lines {
            for run in &line.runs {
                assert!(run.x + run.width <= 262.0, "{:?} runs to {}", run.text, run.x + run.width);
            }
        }
        // And every word is still there.
        let all: String = narrow.lines.iter().map(|it| it.text.clone()).collect();
        for word in ["ARCH", "x86_64", "riscv_32", "besides"] {
            assert!(all.contains(word), "{word} is missing from {all:?}");
        }
    }

    /// **A code line longer than the column is drawn as what the column shows**,
    /// and is still the whole line to a selection and a copy: a run is a label
    /// the frame shapes, and the whole of a 200 KiB line was one.
    #[test]
    fn a_long_code_line_draws_what_fits_and_keeps_its_text() {
        let long = "y".repeat(200 * 1024);
        let out = laid(&format!("```\n{long}\nshort\n```\n"), 400.0);
        let code: Vec<&super::Line> = out.lines.iter().filter(|line| line.code).collect();
        assert_eq!(code.len(), 2, "a code line was wrapped");
        assert_eq!(code[0].text, long, "the line lost its text");
        let drawn: usize = code[0].runs.iter().map(|run| run.text.chars().count()).sum();
        // Grid's characters are eight pixels wide, so fifty fit in 400.
        assert!((50..=2 * 50 + 8).contains(&drawn), "{drawn} characters drawn of 200 KiB");
        assert_eq!(code[1].runs[0].text, "short", "a line that fits is drawn whole");
    }

    /// A word wider than the column is cut rather than left to overrun it.
    #[test]
    fn a_word_too_long_for_the_line_is_cut() {
        let out = laid("https://example.com/a/very/long/path/that/never/breaks/anywhere\n", 120.0);
        assert!(out.lines.len() > 1, "the long word did not break");
        for line in &out.lines {
            for run in &line.runs {
                assert!(run.x + run.width <= 122.0, "{:?} runs to {}", run.text, run.x + run.width);
            }
        }
        let all: String = out.lines.iter().map(|it| it.text.clone()).collect();
        assert_eq!(all, "https://example.com/a/very/long/path/that/never/breaks/anywhere");
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

    /// A measurer that counts the characters it was asked about.
    struct Counting {
        measured: usize,
        allowed: usize,
    }

    impl Measure for Counting {
        fn width_of(&mut self, text: &str, _weight: Weight, size: f32) -> f32 {
            self.measured += text.chars().count();
            text.chars().count() as f32 * size * 0.5
        }

        fn stopped(&self) -> bool {
            self.measured > self.allowed
        }
    }

    /// **A word wider than the column is measured once, not once per prefix.**
    /// Cutting by measuring the piece with one more character on it measured
    /// each character as many times as its piece was long, which through a real
    /// shaper and a label cache that empties at 256 entries was eleven seconds.
    #[test]
    fn a_word_wider_than_the_column_is_measured_in_proportion_to_its_length() {
        let word = "y".repeat(64 * 1024);
        let mut counting = Counting { measured: 0, allowed: usize::MAX };
        let out = lay_out(&parse(&word), 1100.0, 16.0, 1.0, &mut counting);
        let all: String = out.lines.iter().map(|it| it.text.clone()).collect();
        assert_eq!(all, word, "a character went missing in the cut");
        assert!(
            counting.measured <= 3 * word.len(),
            "{} characters measured for a word of {}",
            counting.measured,
            word.len()
        );
        for line in &out.lines {
            for run in &line.runs {
                assert!(run.x + run.width <= 1100.5, "a piece runs to {}", run.x + run.width);
            }
        }
    }

    /// **A layout nobody wants any more ends early** — between blocks, and inside
    /// a paragraph, because one paragraph can be the whole document.
    #[test]
    fn a_layout_that_is_stopped_ends_early() {
        let text = "word ".repeat(20_000);
        let whole = lay_out(&parse(&text), 400.0, 16.0, 1.0, &mut Grid);
        let mut counting = Counting { measured: 0, allowed: 1_000 };
        let cut_short = lay_out(&parse(&text), 400.0, 16.0, 1.0, &mut counting);
        assert!(cut_short.lines.len() * 10 < whole.lines.len(), "it carried on to the end");
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
