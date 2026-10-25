// SPDX-License-Identifier: Apache-2.0
//! Markdown, as much of it as a README needs, parsed into blocks and runs.
//!
//! **[ADR 0029](../../../docs/adr/0029-markdown-renders-from-our-own-parser-and-unknown-syntax-is-its-own-text.md).**
//! Not `CommonMark`: the specification is six hundred examples and no small correct
//! implementation. What a preview owes a reader is legibility — headings that
//! look like headings, code that looks like code, lists that line up — and the
//! rule that keeps that honest is the one this module is built on:
//!
//! **Anything it does not recognise is its own text.** An HTML block, a
//! footnote, a directive, a construct invented next year: each renders as the
//! characters it is made of, in the paragraph it was found in. A reader always
//! sees everything the file says; the worst case is that something is less
//! pretty than it could be.
//!
//! Nothing here knows about pixels. The blocks are flat — a list item carries its
//! own depth rather than nesting — because the layout above draws a window of
//! them and a tree would have to be walked to find where that window starts.

/// How a run of text is drawn.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Style {
    /// `**this**`.
    pub(crate) strong: bool,
    /// `*this*`.
    pub(crate) emphasis: bool,
    /// `` `this` `` — mono, and never parsed for anything else inside.
    pub(crate) code: bool,
    /// `~~this~~`.
    pub(crate) struck: bool,
    /// Part of a link's text, which is drawn in the accent colour.
    pub(crate) link: bool,
}

/// One stretch of text in one style.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Run {
    /// What it says. A `\n` in here is a hard break the author asked for.
    pub(crate) text: String,
    /// How it is drawn.
    pub(crate) style: Style,
    /// Where a link points, kept for the day something can follow it.
    pub(crate) href: Option<String>,
}

impl Run {
    /// Plain text, unstyled.
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: Style::default(), href: None }
    }
}

/// Which way a table's column is set.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum Align {
    /// The default, and what a column with no colons gets.
    #[default]
    Left,
    /// `:-:`
    Centre,
    /// `--:`
    Right,
}

/// What one block of the document is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// `# One` through `###### Six`, and the two underlined forms.
    Heading(u8, Vec<Run>),
    /// Everything that is not one of the others.
    Paragraph(Vec<Run>),
    /// One item of a list, with the bullet or number already decided.
    Item {
        /// `•`, `3.`, `☐` — whatever goes in the margin.
        marker: String,
        /// The item's own text.
        runs: Vec<Run>,
    },
    /// A fenced or indented block, drawn mono and never wrapped.
    Code {
        /// The info string, for the day the highlighter is pointed at it.
        language: String,
        /// The lines, without their terminators.
        lines: Vec<String>,
    },
    /// `---`
    Rule,
    /// One row of a pipe table.
    Row {
        /// The cells, already parsed for emphasis.
        cells: Vec<Vec<Run>>,
        /// Whether this is the header row, which is drawn bold with a rule under.
        header: bool,
        /// How each column is set.
        align: Vec<Align>,
    },
    /// A blank line inside a quote or between blocks, which is drawn as space.
    Gap,
}

/// One block, and how deep in the document's structure it sits.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Block {
    /// How many blockquotes it is inside.
    pub(crate) quoted: u8,
    /// How many list levels it is inside.
    pub(crate) indent: u8,
    /// What it is.
    pub(crate) kind: Kind,
}

/// Every word one block holds, for the corpus check that nothing was dropped.
#[cfg(test)]
#[must_use]
fn spoken(block: &Block) -> String {
    match &block.kind {
        Kind::Heading(_, runs) | Kind::Paragraph(runs) => {
            runs.iter().map(|it| it.text.as_str()).collect()
        }
        Kind::Item { marker, runs } => {
            let mut out = marker.clone();
            out.push(' ');
            out.extend(runs.iter().map(|it| it.text.as_str()));
            out
        }
        Kind::Code { language, lines } => format!("{language}\n{}", lines.join("\n")),
        Kind::Row { cells, .. } => cells
            .iter()
            .map(|cell| cell.iter().map(|it| it.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join(" "),
        Kind::Rule | Kind::Gap => String::new(),
    }
}

/// How deep nesting may go before it is flattened.
///
/// **Because a parser that recurses on input is a parser that can be handed
/// input.** Sixteen is deeper than any document written to be read. A
/// seventeenth quote or list level is drawn at the sixteenth's indent rather
/// than refused, and what is inside a seventeenth emphasis or link is drawn as
/// the characters it is made of. Inline nesting had no bound until 52,428 links
/// each inside the last — what 256 KiB holds — overflowed an eight-mebibyte
/// stack, and eight thousand of them a job thread's two.
const DEEPEST: u8 = 16;

#[cfg(test)]
thread_local! {
    /// How many documents this thread has parsed.
    ///
    /// **So a test can say which thread parsed**, which is the property rule 1
    /// asks of the quick look: a test that timed the frame loop's delivery
    /// instead would pass because the parser is fast, not because it is not on
    /// the frame loop.
    pub(crate) static PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Parses a document into blocks, in the order they are drawn.
#[must_use]
pub(crate) fn parse(text: &str) -> Vec<Block> {
    #[cfg(test)]
    PARSED.with(|it| it.set(it.get() + 1));
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        at = block(&lines, at, 0, &mut out);
    }
    out
}

/// Parses one block starting at `at`, and answers where the next one begins.
fn block(lines: &[&str], at: usize, quoted: u8, out: &mut Vec<Block>) -> usize {
    let line = lines[at];
    let bare = line.trim_start();

    if bare.is_empty() {
        out.push(Block { quoted, indent: 0, kind: Kind::Gap });
        return at + 1;
    }
    if quoted < DEEPEST
        && let Some(rest) = quote_line(line)
    {
        // A quote's content is a document of its own, one level deeper. Every
        // line of the run is stripped and parsed together, so a paragraph inside
        // a quote is one paragraph rather than one per line.
        let mut inner: Vec<&str> = vec![rest];
        let mut next = at + 1;
        while next < lines.len() {
            let Some(more) = quote_line(lines[next]) else { break };
            inner.push(more);
            next += 1;
        }
        let mut within = 0;
        while within < inner.len() {
            within = block(&inner, within, quoted + 1, out);
        }
        return next;
    }
    if let Some((fence, language)) = fence_at(bare) {
        return fenced(lines, at, quoted, (&fence, language), out);
    }
    if is_rule(bare) {
        out.push(Block { quoted, indent: 0, kind: Kind::Rule });
        return at + 1;
    }
    if let Some((level, rest)) = atx(bare) {
        out.push(Block { quoted, indent: 0, kind: Kind::Heading(level, inline(rest)) });
        return at + 1;
    }
    if indented_code(line) {
        return indented(lines, at, quoted, out);
    }
    if let Some(item) = list_item(line) {
        return listed(lines, at, quoted, item, out);
    }
    if let Some(table) = table_at(lines, at, quoted, out) {
        return table;
    }

    // A paragraph: this line and every following one that is not the start of
    // something else. The underline forms of a heading are decided here, because
    // `Setext` is a property of the *paragraph* rather than of its own line.
    // Trailing spaces are markup rather than text — two of them are the break
    // handled below — so a paragraph's lines go in trimmed at both ends.
    let mut text = bare.trim_end().to_owned();
    let mut previous = line;
    let mut next = at + 1;
    while next < lines.len() {
        let more = lines[next];
        let bare = more.trim_start();
        if let Some(level) = setext(bare) {
            out.push(Block { quoted, indent: 0, kind: Kind::Heading(level, inline(&text)) });
            return next + 1;
        }
        if bare.is_empty()
            || is_rule(bare)
            || atx(bare).is_some()
            || fence_at(bare).is_some()
            || list_item(more).is_some()
            || quote_line(more).is_some()
        {
            break;
        }
        // **A hard break is two spaces or a backslash at the end of the line
        // being continued** — the previous one, not this one. Everything else
        // joins with a space, which is what a paragraph in markdown means.
        if previous.ends_with("  ") || text.ends_with('\\') {
            if text.ends_with('\\') {
                text.pop();
            }
            text.push('\n');
        } else {
            text.push(' ');
        }
        text.push_str(bare.trim_end());
        previous = more;
        next += 1;
    }
    out.push(Block { quoted, indent: 0, kind: Kind::Paragraph(inline(&text)) });
    next
}

/// A fenced code block, from its opening fence to its closing one or the end.
fn fenced(
    lines: &[&str],
    at: usize,
    quoted: u8,
    fence: (&str, String),
    out: &mut Vec<Block>,
) -> usize {
    let (mark, language) = fence;
    let closing = mark.chars().next().unwrap_or('`');
    let mut body = Vec::new();
    let mut next = at + 1;
    while next < lines.len() {
        let line = lines[next].trim();
        if line.starts_with(mark) && line.chars().all(|it| it == closing) {
            next += 1;
            break;
        }
        body.push(lines[next].to_owned());
        next += 1;
    }
    out.push(Block { quoted, indent: 0, kind: Kind::Code { language, lines: body } });
    next
}

/// A run of four-space indented lines, which is a code block.
fn indented(lines: &[&str], at: usize, quoted: u8, out: &mut Vec<Block>) -> usize {
    let mut body = Vec::new();
    let mut next = at;
    while next < lines.len() && (indented_code(lines[next]) || lines[next].trim().is_empty()) {
        // A blank line inside an indented block belongs to it; one after it does
        // not, and this is decided by looking past it.
        if lines[next].trim().is_empty() && !lines.get(next + 1).is_some_and(|it| indented_code(it))
        {
            break;
        }
        let line = lines[next];
        body.push(
            line.strip_prefix('\t').or_else(|| line.strip_prefix("    ")).unwrap_or("").to_owned(),
        );
        next += 1;
    }
    out.push(Block {
        quoted,
        indent: 0,
        kind: Kind::Code { language: String::new(), lines: body },
    });
    next
}

/// One list item, and whatever lines continue it.
fn listed(lines: &[&str], at: usize, quoted: u8, item: Listed, out: &mut Vec<Block>) -> usize {
    let mut text = item.text;
    let mut next = at + 1;
    while next < lines.len() {
        let more = lines[next];
        if more.trim().is_empty() || list_item(more).is_some() || quote_line(more).is_some() {
            break;
        }
        // A continuation is indented past the marker; anything else ends the item
        // and starts a paragraph of its own.
        if leading(more) <= item.column {
            break;
        }
        text.push(' ');
        text.push_str(more.trim());
        next += 1;
    }
    out.push(Block {
        quoted,
        indent: item.depth,
        kind: Kind::Item { marker: item.marker, runs: inline(&text) },
    });
    next
}

/// The content of a `>` line, if it is one.
fn quote_line(line: &str) -> Option<&str> {
    let bare = line.trim_start();
    let rest = bare.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// The fence and its info string, if the line opens a fenced block.
fn fence_at(bare: &str) -> Option<(String, String)> {
    for mark in ['`', '~'] {
        let run: String = bare.chars().take_while(|it| *it == mark).collect();
        if run.len() >= 3 {
            let language = bare[run.len()..].trim().to_owned();
            // An info string with a backtick in it is not a fence — that is a
            // code span on a line of its own.
            if mark == '`' && language.contains('`') {
                return None;
            }
            return Some((run, language));
        }
    }
    None
}

/// Whether the line is a thematic break.
fn is_rule(bare: &str) -> bool {
    for mark in ['-', '*', '_'] {
        let kept: String = bare.chars().filter(|it| !it.is_whitespace()).collect();
        if kept.len() >= 3 && kept.chars().all(|it| it == mark) {
            return true;
        }
    }
    false
}

/// The level and text of an ATX heading.
fn atx(bare: &str) -> Option<(u8, &str)> {
    let hashes = bare.chars().take_while(|it| *it == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &bare[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    // A closing run of hashes is decoration and not text.
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((u8::try_from(hashes).unwrap_or(1), text))
}

/// The level an underline gives the paragraph above it.
fn setext(bare: &str) -> Option<u8> {
    if bare.len() < 2 {
        return None;
    }
    if bare.chars().all(|it| it == '=') {
        return Some(1);
    }
    if bare.chars().all(|it| it == '-') {
        return Some(2);
    }
    None
}

/// Whether the line is an indented code line.
fn indented_code(line: &str) -> bool {
    !line.trim().is_empty() && (line.starts_with("    ") || line.starts_with('\t'))
}

/// How many columns of space a line starts with, tabs counted as four.
fn leading(line: &str) -> usize {
    line.chars().take_while(|it| it.is_whitespace()).map(|it| if it == '\t' { 4 } else { 1 }).sum()
}

/// What a list line turned out to be.
struct Listed {
    marker: String,
    text: String,
    depth: u8,
    column: usize,
}

/// The marker, text and depth of a list item, if the line is one.
fn list_item(line: &str) -> Option<Listed> {
    let column = leading(line);
    let bare = line.trim_start();
    let depth = u8::try_from(column / 2).unwrap_or(DEEPEST).min(DEEPEST);
    let (marker, rest) = if let Some(rest) = bare
        .strip_prefix("- ")
        .or_else(|| bare.strip_prefix("* "))
        .or_else(|| bare.strip_prefix("+ "))
    {
        ("\u{2022}".to_owned(), rest)
    } else {
        let digits: String = bare.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        let rest = bare[digits.len()..]
            .strip_prefix(". ")
            .or_else(|| bare[digits.len()..].strip_prefix(") "))?;
        (format!("{digits}."), rest)
    };
    // **A task list is a box in the margin**, which is what GitHub draws and what
    // makes a checklist readable at a glance rather than a list of brackets.
    let (marker, rest) = match rest.get(..4) {
        Some("[ ] ") => ("\u{2610}".to_owned(), &rest[4..]),
        Some("[x] " | "[X] ") => ("\u{2611}".to_owned(), &rest[4..]),
        _ => (marker, rest),
    };
    Some(Listed { marker, text: rest.to_owned(), depth, column })
}

/// Parses a pipe table starting at `at`, if one starts there.
fn table_at(lines: &[&str], at: usize, quoted: u8, out: &mut Vec<Block>) -> Option<usize> {
    let header = lines[at];
    if !header.contains('|') {
        return None;
    }
    let align = alignment(lines.get(at + 1)?)?;
    let cells = row_cells(header);
    if cells.len() < 2 {
        return None;
    }
    out.push(Block {
        quoted,
        indent: 0,
        kind: Kind::Row {
            cells: cells.iter().map(|it| inline(it)).collect(),
            header: true,
            align: align.clone(),
        },
    });
    let mut next = at + 2;
    while next < lines.len() && lines[next].contains('|') && !lines[next].trim().is_empty() {
        let cells = row_cells(lines[next]);
        out.push(Block {
            quoted,
            indent: 0,
            kind: Kind::Row {
                cells: cells.iter().map(|it| inline(it)).collect(),
                header: false,
                align: align.clone(),
            },
        });
        next += 1;
    }
    Some(next)
}

/// The alignment row of a table, if that is what the line is.
fn alignment(line: &str) -> Option<Vec<Align>> {
    let cells = row_cells(line);
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for cell in cells {
        let bare = cell.trim();
        // **One dash is a column.** GFM asks for at least one, and `| - | - |`
        // is what a person writes for a narrow table — demanding three made that
        // table a paragraph of pipes.
        let body = bare.trim_start_matches(':').trim_end_matches(':');
        if body.is_empty() || !body.chars().all(|it| it == '-') {
            return None;
        }
        out.push(match (bare.starts_with(':'), bare.ends_with(':')) {
            (true, true) => Align::Centre,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(out)
}

/// One table row's cells, with the outer pipes dropped.
fn row_cells(line: &str) -> Vec<String> {
    let bare = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut held = String::new();
    let mut escaped = false;
    for letter in bare.chars() {
        if escaped {
            held.push(letter);
            escaped = false;
        } else if letter == '\\' {
            escaped = true;
        } else if letter == '|' {
            cells.push(held.trim().to_owned());
            held = String::new();
        } else {
            held.push(letter);
        }
    }
    cells.push(held.trim().to_owned());
    cells
}

/// Parses the inline syntax of one stretch of text into styled runs.
///
/// **One pass, and everything it does not know is text.** A `<` that opens no
/// tag it recognises, an unclosed `*`, a `[` with no `]`: each stays where it is
/// and is drawn. That is the rule the module header states and it is enforced
/// here, by every branch falling through to "push the character".
#[must_use]
pub(crate) fn inline(text: &str) -> Vec<Run> {
    let letters: Vec<char> = text.chars().collect();
    spans(&letters, 0)
}

/// [`inline`] over `letters`, which are inside `depth` emphases and links.
///
/// **Linear in the length at each depth, and [`DEEPEST`] depths at most.** An
/// opener's questions — how long its run is, where its closer is, which `]`
/// is its own — are answered from [`Found`], which is one pass over the text.
/// They used to be answered by scanning ahead from the opener, and an opener
/// with nothing to close it scanned to the end of the paragraph before the
/// parser moved on by one character: 10.5 seconds over 256 KiB of `[`.
fn spans(letters: &[char], depth: u8) -> Vec<Run> {
    if depth >= DEEPEST {
        return vec![Run::plain(letters.iter().collect::<String>())];
    }
    let mut out: Vec<Run> = Vec::new();
    let mut held = String::new();
    let mut found = Found::new(letters);
    let mut at = 0;

    let flush = |held: &mut String, out: &mut Vec<Run>| {
        if !held.is_empty() {
            out.push(Run::plain(std::mem::take(held)));
        }
    };

    while at < letters.len() {
        let letter = letters[at];
        match letter {
            '\\' if at + 1 < letters.len() && letters[at + 1].is_ascii_punctuation() => {
                held.push(letters[at + 1]);
                at += 2;
            }
            '`' => {
                let ticks = found.run(at);
                if let Some(end) = found.closing(at + ticks, '`', ticks) {
                    flush(&mut held, &mut out);
                    let body: String = letters[at + ticks..end].iter().collect();
                    out.push(Run {
                        text: body.trim().to_owned(),
                        style: Style { code: true, ..Style::default() },
                        href: None,
                    });
                    at = end + ticks;
                } else {
                    held.push(letter);
                    at += 1;
                }
            }
            '*' | '_' | '~' => {
                if let Some((end, runs)) = emphasis(&mut found, at, letter, depth) {
                    flush(&mut held, &mut out);
                    out.extend(runs);
                    at = end;
                } else {
                    held.push(letter);
                    at += 1;
                }
            }
            '<' | '!' | '[' => {
                if let Some((end, runs)) = linked(&mut found, at, depth) {
                    flush(&mut held, &mut out);
                    out.extend(runs);
                    at = end;
                } else {
                    held.push(letter);
                    at += 1;
                }
            }
            other => {
                held.push(other);
                at += 1;
            }
        }
    }
    flush(&mut held, &mut out);
    if out.is_empty() {
        out.push(Run::plain(""));
    }
    out
}

/// An emphasis, strong, or strikethrough span: where it ends and what is in it.
///
/// **One, two or three of `*` or `_`** — emphasis, strong and both. `***this***`
/// is the third and is common enough in a README that treating it as two openers
/// loses the text between them. `~~` is the fourth mark and behaves the same way.
fn emphasis(found: &mut Found<'_>, at: usize, mark: char, depth: u8) -> Option<(usize, Vec<Run>)> {
    let letters = found.letters;
    let count = found.run(at);
    let run = if mark == '~' {
        if count < 2 {
            return None;
        }
        2
    } else {
        count.min(3)
    };
    // `snake_case` is not emphasis: an underscore between two word characters
    // opens nothing, which is the rule every renderer has and the reason `a_b_c`
    // reads as itself.
    if mark == '_'
        && at > 0
        && letters[at - 1].is_alphanumeric()
        && letters.get(at + run).is_some_and(|it| it.is_alphanumeric())
    {
        return None;
    }
    let end = found.closing(at + run, mark, run)?;
    // **Nothing between the marks is not emphasis**, or `******` — a masked
    // password, a row of a hand-drawn box — would draw as nothing at all.
    if end == at + run {
        return None;
    }
    let strong = mark != '~' && run >= 2;
    let slanted = mark != '~' && run % 2 == 1;
    let runs = spans(&letters[at + run..end], depth + 1)
        .into_iter()
        .map(|mut nested| {
            nested.style.strong |= strong;
            nested.style.emphasis |= slanted;
            nested.style.struck |= mark == '~';
            nested
        })
        .collect();
    Some((end + run, runs))
}

/// A link, an image or an autolink: where it ends and the runs it becomes.
fn linked(found: &mut Found<'_>, at: usize, depth: u8) -> Option<(usize, Vec<Run>)> {
    let letters = found.letters;
    if letters[at] == '<' {
        let (end, url) = autolink(found, at)?;
        let style = Style { link: true, ..Style::default() };
        return Some((end, vec![Run { text: url.clone(), style, href: Some(url) }]));
    }
    let image = letters[at] == '!';
    let open = if image { at + 1 } else { at };
    let (close, end) = link_at(found, open)?;
    let target: String = letters[close + 2..end].iter().collect();
    // A title after the destination — `(url "the title")` — is not shown.
    let href = target.split_whitespace().next().unwrap_or("").to_owned();
    // **An image is its alt text.** Fetching it is a decode, a sandbox and a
    // network question; the words the author wrote for somebody who cannot see it
    // are already here.
    let label = &letters[open + 1..close];
    let runs = if image && label.is_empty() {
        spans(&href.chars().collect::<Vec<_>>(), depth + 1)
    } else {
        spans(label, depth + 1)
    };
    let runs = runs
        .into_iter()
        .map(|mut nested| {
            nested.style.link = true;
            nested.href = Some(href.clone());
            nested
        })
        .collect();
    Some((end + 1, runs))
}

/// A `<http://…>` autolink: where it ends, and what it points at.
fn autolink(found: &mut Found<'_>, at: usize) -> Option<(usize, String)> {
    let letters = found.letters;
    let after = &letters[at + 1..];
    let looks_like = ["http://", "https://", "mailto:", "ftp://"];
    if !looks_like.iter().any(|scheme| after.iter().copied().take(scheme.len()).eq(scheme.chars()))
    {
        return None;
    }
    // None of the schemes holds a space or a `>`, so the first of either past
    // the `<` is where the address ends — and it is only one if it is a `>`.
    let end = found.stop(at + 1);
    if letters.get(end) != Some(&'>') {
        return None;
    }
    Some((end + 1, letters[at + 1..end].iter().collect()))
}

/// A `[label](href)` whose `[` is at `at`: where its `]` is, and its `)`.
fn link_at(found: &mut Found<'_>, at: usize) -> Option<(usize, usize)> {
    if found.letters.get(at) != Some(&'[') {
        return None;
    }
    // Brackets nest: `[![badge](a)](b)` is a link around an image, which is what
    // the top of every README is made of.
    let close = found.bracket(at)?;
    if found.letters.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = found.paren(close + 1)?;
    Some((close, end))
}

/// The four marks that come in runs, in the order of [`Found::runs`].
const MARKS: [char; 4] = ['`', '*', '_', '~'];

/// One maximal run of a mark.
#[derive(Clone, Copy)]
struct Stretch {
    start: usize,
    length: usize,
    /// The longest run of the same mark from this one to the end of the text,
    /// which is what lets an opener with nothing to close it find that out at
    /// once.
    longest: usize,
}

impl Stretch {
    fn end(self) -> usize {
        self.start + self.length
    }
}

/// Where things are in one stretch of text, found once rather than by every
/// opener that asks.
///
/// **Each part is worked out the first time it is wanted, and each mark's runs
/// on their own**, because most of what [`spans`] is handed is the few words
/// inside one emphasis, with one mark in them or none.
struct Found<'a> {
    letters: &'a [char],
    /// The runs of each of [`MARKS`], in order.
    runs: [Option<Marked>; 4],
    /// Every `[` in order, and the `]` that closes it if one does.
    brackets: Option<Vec<(usize, Option<usize>)>>,
    /// Every `(` in order, and the `)` that closes it if one does.
    parens: Option<Vec<(usize, Option<usize>)>>,
    /// The last [`Self::stop`] asked, and what it answered.
    stopped: Option<(usize, usize)>,
}

/// The runs of one mark, and where the last question about them was answered.
struct Marked {
    each: Vec<Stretch>,
    /// The index [`Found::locate`] last answered with.
    ///
    /// **Walked from rather than searched for**, because the parser only moves
    /// forward and so the next answer is almost always this one or the next.
    /// Over 256 KiB of `*_`, a binary search per opener took 16.2 ms where the
    /// scan this replaced took 6.6 ms; walking takes 9.2 ms. Medians of 41,
    /// interleaved in one process, release build, load average 3.5.
    near: usize,
}

impl<'a> Found<'a> {
    fn new(letters: &'a [char]) -> Self {
        Self { letters, runs: Default::default(), brackets: None, parens: None, stopped: None }
    }

    /// Every run of `mark` in order, and the index of the first that ends past
    /// `at`.
    fn locate(&mut self, mark: char, at: usize) -> (&[Stretch], usize) {
        let letters = self.letters;
        let slot = MARKS.iter().position(|it| *it == mark).unwrap_or(MARKS.len() - 1);
        let marked = self.runs[slot].get_or_insert_with(|| {
            let mut each = Vec::new();
            let mut at = 0;
            while at < letters.len() {
                if letters[at] == mark {
                    let length = letters[at..].iter().take_while(|it| **it == mark).count();
                    each.push(Stretch { start: at, length, longest: length });
                    at += length;
                } else {
                    at += 1;
                }
            }
            let mut longest = 0;
            for stretch in each.iter_mut().rev() {
                longest = longest.max(stretch.length);
                stretch.longest = longest;
            }
            Marked { each, near: 0 }
        });
        let each = &marked.each;
        let mut index = marked.near;
        while index > 0 && each[index - 1].end() > at {
            index -= 1;
        }
        while index < each.len() && each[index].end() <= at {
            index += 1;
        }
        marked.near = index;
        (&marked.each, index)
    }

    /// How many of the mark at `at` there are from `at` to the end of its run.
    fn run(&mut self, at: usize) -> usize {
        let (runs, index) = self.locate(self.letters[at], at);
        runs.get(index).map_or(1, |it| it.end() - at)
    }

    /// Where a run of at least `count` `mark`s starts, searching from `from`.
    ///
    /// **The part of a run `from` lands inside counts as a run**, which is what
    /// the scan this replaces did and what `****bold****` is parsed by.
    fn closing(&mut self, from: usize, mark: char, count: usize) -> Option<usize> {
        let (runs, mut index) = self.locate(mark, from);
        if let Some(inside) = runs.get(index)
            && inside.start <= from
        {
            if inside.end() - from >= count {
                return Some(from);
            }
            index += 1;
        }
        if runs.get(index).map_or(0, |it| it.longest) < count {
            return None;
        }
        runs[index..].iter().find(|it| it.length >= count).map(|it| it.start)
    }

    /// The `]` that closes the `[` at `at`.
    fn bracket(&mut self, at: usize) -> Option<usize> {
        let letters = self.letters;
        let pairs = self.brackets.get_or_insert_with(|| pairs(letters, '[', ']'));
        pairs.binary_search_by_key(&at, |it| it.0).ok().and_then(|index| pairs[index].1)
    }

    /// The `)` that closes the `(` at `at`.
    fn paren(&mut self, at: usize) -> Option<usize> {
        let letters = self.letters;
        let pairs = self.parens.get_or_insert_with(|| pairs(letters, '(', ')'));
        pairs.binary_search_by_key(&at, |it| it.0).ok().and_then(|index| pairs[index].1)
    }

    /// The first whitespace or `>` at or after `from`, or the end.
    ///
    /// **Remembered, because `from` only moves forward** within one stretch: an
    /// answer past the new `from` is still the first one, so a paragraph of
    /// `<http://` with no `>` is scanned once rather than once per `<`.
    fn stop(&mut self, from: usize) -> usize {
        if let Some((asked, answer)) = self.stopped
            && asked <= from
            && from <= answer
        {
            return answer;
        }
        let answer = self.letters[from..]
            .iter()
            .position(|it| it.is_whitespace() || *it == '>')
            .map_or(self.letters.len(), |it| from + it);
        self.stopped = Some((from, answer));
        answer
    }
}

/// Every `open` in `letters`, in order, and the `close` that ends it.
///
/// **Counted, not parsed** — a bracket inside a code span counts — because that
/// is what the scan this replaces did: the first `close` at which as many have
/// closed as opened since.
fn pairs(letters: &[char], open: char, close: char) -> Vec<(usize, Option<usize>)> {
    let mut out = Vec::new();
    let mut waiting = Vec::new();
    for (at, letter) in letters.iter().enumerate() {
        if *letter == open {
            waiting.push(out.len());
            out.push((at, None));
        } else if *letter == close
            && let Some(index) = waiting.pop()
        {
            out[index].1 = Some(at);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Align, Block, Kind, Run, Style, inline, parse, spoken};

    /// The text of a block, whatever kind it is, for the assertions below.
    fn words(block: &Block) -> String {
        match &block.kind {
            Kind::Heading(_, runs) | Kind::Paragraph(runs) | Kind::Item { runs, .. } => {
                runs.iter().map(|it| it.text.as_str()).collect()
            }
            Kind::Code { lines, .. } => lines.join("\n"),
            Kind::Row { cells, .. } => cells
                .iter()
                .map(|cell| cell.iter().map(|it| it.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("|"),
            Kind::Rule => "---".to_owned(),
            Kind::Gap => String::new(),
        }
    }

    /// Everything a block holds, flattened, so nothing can be silently dropped.
    fn everything(blocks: &[Block]) -> String {
        blocks.iter().map(words).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn headings_are_both_forms_and_a_level() {
        let blocks = parse("# One\n\n### Three ###\n\nUnderlined\n===\n\nSecond\n---\n");
        let headings: Vec<(u8, String)> = blocks
            .iter()
            .filter_map(|block| match &block.kind {
                Kind::Heading(level, _) => Some((*level, words(block))),
                _ => None,
            })
            .collect();
        assert_eq!(
            headings,
            vec![
                (1, "One".to_owned()),
                (3, "Three".to_owned()),
                (1, "Underlined".to_owned()),
                (2, "Second".to_owned()),
            ]
        );
    }

    /// A paragraph joins its lines; two spaces at the end of one is a break.
    #[test]
    fn a_paragraph_joins_and_a_hard_break_does_not() {
        let blocks = parse("one\ntwo\n\nthree  \nfour\n");
        assert_eq!(words(&blocks[0]), "one two");
        assert_eq!(words(&blocks[2]), "three\nfour");
    }

    #[test]
    fn a_fence_keeps_its_lines_and_its_language() {
        let blocks = parse("before\n\n```rust\nfn main() {}\n  indented\n```\n\nafter\n");
        let Kind::Code { language, lines } = &blocks[2].kind else { panic!("{blocks:?}") };
        assert_eq!(language, "rust");
        assert_eq!(lines, &["fn main() {}".to_owned(), "  indented".to_owned()]);
        assert_eq!(words(&blocks[4]), "after");
    }

    /// **An unclosed fence takes the rest of the document and does not panic.**
    #[test]
    fn an_unclosed_fence_ends_at_the_end() {
        let blocks = parse("```\nstill code\n");
        let Kind::Code { lines, .. } = &blocks[0].kind else { panic!("{blocks:?}") };
        assert_eq!(lines, &["still code".to_owned()]);
    }

    #[test]
    fn lists_carry_their_marker_and_their_depth() {
        let blocks = parse("- one\n- two\n  - nested\n\n1. first\n2. second\n");
        let items: Vec<(String, u8, String)> = blocks
            .iter()
            .filter_map(|block| match &block.kind {
                Kind::Item { marker, .. } => Some((marker.clone(), block.indent, words(block))),
                _ => None,
            })
            .collect();
        assert_eq!(items[0], ("\u{2022}".to_owned(), 0, "one".to_owned()));
        assert_eq!(items[2], ("\u{2022}".to_owned(), 1, "nested".to_owned()));
        assert_eq!(items[3], ("1.".to_owned(), 0, "first".to_owned()));
        assert_eq!(items[4], ("2.".to_owned(), 0, "second".to_owned()));
    }

    /// A task list is a box, because a list of brackets is not a checklist.
    #[test]
    fn a_task_list_is_a_box() {
        let blocks = parse("- [ ] owed\n- [x] done\n");
        let markers: Vec<String> = blocks
            .iter()
            .filter_map(|block| match &block.kind {
                Kind::Item { marker, .. } => Some(marker.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec!["\u{2610}".to_owned(), "\u{2611}".to_owned()]);
    }

    #[test]
    fn a_quote_is_its_contents_one_level_deeper() {
        let blocks = parse("> quoted words\n> still quoted\n\nplain\n");
        assert_eq!(blocks[0].quoted, 1);
        assert_eq!(words(&blocks[0]), "quoted words still quoted");
        assert_eq!(blocks.last().map(|it| it.quoted), Some(0));
    }

    #[test]
    fn a_table_is_a_header_an_alignment_and_rows() {
        let text = "| Name | Size |\n| :--- | ---: |\n| a.txt | 12 |\n| b.txt | 3 |\n";
        let blocks = parse(text);
        let Kind::Row { header, align, cells } = &blocks[0].kind else { panic!("{blocks:?}") };
        assert!(header);
        assert_eq!(align, &[Align::Left, Align::Right]);
        assert_eq!(cells.len(), 2);
        assert_eq!(words(&blocks[2]), "b.txt|3");
        assert_eq!(blocks.len(), 3, "a header and two rows");
    }

    /// **One dash is a column**, which is what GFM says and what people write.
    #[test]
    fn a_delimiter_row_needs_only_one_dash() {
        for delimiter in ["| - | - |", "|---|---|", "| :- | -: |", "| :---: | --- |"] {
            let text = format!("| A | B |\n{delimiter}\n| 1 | 2 |\n");
            let blocks = parse(&text);
            assert!(
                matches!(blocks[0].kind, Kind::Row { header: true, .. }),
                "{delimiter:?} gave {blocks:?}"
            );
        }
    }

    /// A line with pipes and no alignment row under it is a paragraph.
    #[test]
    fn pipes_alone_are_not_a_table() {
        let blocks = parse("a | b | c\nand more\n");
        assert!(matches!(blocks[0].kind, Kind::Paragraph(_)));
    }

    #[test]
    fn a_rule_is_three_of_anything() {
        for text in ["---\n", "***\n", "___\n", "- - -\n"] {
            let blocks = parse(text);
            assert!(matches!(blocks[0].kind, Kind::Rule), "{text:?} gave {blocks:?}");
        }
    }

    // ---- Inline ----

    fn styled(text: &str) -> Vec<(String, Style)> {
        inline(text).into_iter().map(|run| (run.text, run.style)).collect()
    }

    #[test]
    fn emphasis_and_strong_nest() {
        let runs = styled("plain *em* and **strong** and ***both***");
        assert_eq!(runs[0].0, "plain ");
        assert!(runs.iter().any(|(text, style)| text == "em" && style.emphasis && !style.strong));
        assert!(runs.iter().any(|(text, style)| text == "strong" && style.strong));
        assert!(runs.iter().any(|(text, style)| text == "both" && style.strong && style.emphasis));
    }

    /// **`snake_case` is a word, not emphasis** — the rule every renderer has.
    #[test]
    fn an_underscore_inside_a_word_is_a_character() {
        let runs = styled("call read_to_end on it");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, "call read_to_end on it");
        assert!(!runs[0].1.emphasis);
    }

    #[test]
    fn a_code_span_is_never_parsed_inside() {
        let runs = styled("use `a *b* c` here");
        let code = runs.iter().find(|(_, style)| style.code).expect("a code span");
        assert_eq!(code.0, "a *b* c");
    }

    #[test]
    fn a_link_keeps_its_text_and_its_target() {
        let runs = inline("see [the docs](https://example.com/a) now");
        let link = runs.iter().find(|run| run.style.link).expect("a link");
        assert_eq!(link.text, "the docs");
        assert_eq!(link.href.as_deref(), Some("https://example.com/a"));
        assert_eq!(runs.iter().map(|it| it.text.as_str()).collect::<String>(), "see the docs now");
    }

    /// The badge line every README starts with: a link around an image.
    #[test]
    fn a_link_around_an_image_is_the_alt_text() {
        let runs = inline("[![build](https://img.example/b.svg)](https://ci.example/x)");
        assert_eq!(runs.iter().map(|it| it.text.as_str()).collect::<String>(), "build");
        assert!(runs[0].style.link);
    }

    #[test]
    fn an_autolink_is_its_own_text() {
        let runs = inline("write to <https://example.com> today");
        let link = runs.iter().find(|run| run.style.link).expect("a link");
        assert_eq!(link.text, "https://example.com");
        assert_eq!(link.href.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn strikethrough_and_escapes() {
        let runs = styled("~~gone~~ and \\*kept\\*");
        assert!(runs.iter().any(|(text, style)| text == "gone" && style.struck));
        assert_eq!(
            runs.iter().map(|(text, _)| text.as_str()).collect::<String>(),
            "gone and *kept*"
        );
    }

    /// **The rule the whole module rests on**: anything unknown is its own text,
    /// and nothing is ever dropped.
    #[test]
    fn what_it_does_not_understand_is_still_there() {
        let odd = concat!(
            "<div class=\"note\">\n",
            "  <b>raw html</b>\n",
            "</div>\n",
            "\n",
            "A footnote[^1] and a [ref][label] and an unclosed *emphasis\n",
            "\n",
            "[^1]: the note itself\n",
            "[label]: https://example.com\n",
            "\n",
            "| broken | table\n",
            "::: a directive\n",
        );
        let blocks = parse(odd);
        let all = everything(&blocks);
        for owed in [
            "raw html",
            "<div",
            "footnote",
            "[^1]",
            "[ref][label]",
            "*emphasis",
            "broken",
            "::: a directive",
        ] {
            assert!(all.contains(owed), "{owed:?} is missing from {all:?}");
        }
    }

    /// Nesting is bounded, and the seventeenth level draws at the sixteenth.
    #[test]
    fn nesting_stops_at_a_depth() {
        let deep = ">".repeat(40) + " far in\n";
        let blocks = parse(&deep);
        assert!(blocks.iter().all(|it| it.quoted <= super::DEEPEST));
        assert!(everything(&blocks).contains("far in"));
    }

    /// Nothing at all, and one blank line, are documents too.
    #[test]
    fn an_empty_document_is_no_blocks() {
        assert!(parse("").is_empty());
        assert_eq!(parse("\n").len(), 1);
        assert!(matches!(parse("\n")[0].kind, Kind::Gap));
    }

    /// Every character of the input reaches a run — the property that makes
    /// "unknown syntax is its own text" testable rather than aspirational.
    #[test]
    fn no_word_of_a_document_is_lost() {
        let text = concat!(
            "# Title\n\nA paragraph with `code`, *emphasis*, and a [link](x).\n\n",
            "- item one\n- item two\n\n> a quote\n\n```\nliteral\n```\n\n",
            "| a | b |\n| - | - |\n| 1 | 2 |\n"
        );
        let all = everything(&parse(text));
        for word in [
            "Title",
            "paragraph",
            "code",
            "emphasis",
            "link",
            "item one",
            "item two",
            "a quote",
            "literal",
            "1",
            "2",
        ] {
            assert!(all.contains(word), "{word:?} is missing from {all:?}");
        }
    }

    /// **Every word of four real documents survives being parsed.**
    ///
    /// The property the module header claims, measured rather than asserted: over
    /// this repository's own markdown — tables, fenced code, nested lists, block
    /// quotes, links, badges, em dashes, and a two-thousand-line document — every
    /// alphanumeric character of the source turns up in some run.
    ///
    /// **Except a link's destination and a ticked box.** A renderer is meant to
    /// drop the first, so `readable` strips it before comparing; the second is the
    /// `x` of `- [x]`, which becomes a box in the margin. Both exceptions are
    /// named here rather than absorbed into a tolerance.
    ///
    /// Measured against 445 files from the crate registry when it was written:
    /// four of them lost anything at all, and every character lost was an `x`.
    #[test]
    fn no_word_of_a_real_document_is_lost() {
        let corpus = [
            ("README.md", include_str!("../../../README.md")),
            ("CLAUDE.md", include_str!("../../../CLAUDE.md")),
            ("08-roadmap.md", include_str!("../../../docs/08-roadmap.md")),
            ("12-measurement.md", include_str!("../../../docs/12-measurement.md")),
        ];
        for (name, text) in corpus {
            let rendered: String = parse(text).iter().map(spoken).collect::<Vec<_>>().join("\n");
            let mut have: std::collections::HashMap<char, i64> = std::collections::HashMap::new();
            for letter in rendered.chars().filter(|it| it.is_alphanumeric()) {
                *have.entry(letter).or_insert(0) += 1;
            }
            let mut lost = Vec::new();
            for letter in readable(text).chars().filter(|it| it.is_alphanumeric()) {
                let held = have.entry(letter).or_insert(0);
                if *held <= 0 {
                    lost.push(letter);
                } else {
                    *held -= 1;
                }
            }
            // The `x` of a ticked box, which the marker replaces.
            lost.retain(|letter| *letter != 'x' && *letter != 'X');
            assert!(lost.is_empty(), "{name} lost {lost:?}");
        }
    }

    /// The source with what a renderer is *meant* to drop taken out: a link's
    /// destination. What is left is text a reader must still see.
    fn readable(text: &str) -> String {
        let letters: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        while at < letters.len() {
            if letters[at] == ']' && letters.get(at + 1) == Some(&'(') {
                out.push(']');
                at += 1;
                let mut depth = 0;
                while at < letters.len() {
                    match letters[at] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                at += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                continue;
            }
            out.push(letters[at]);
            at += 1;
        }
        out
    }

    /// **An emphasis with nothing in it is its own text**, so a masked password
    /// reads as the asterisks it is rather than as nothing — which is
    /// `CommonMark`'s answer as well as ADR 0029's.
    #[test]
    fn an_empty_emphasis_is_its_own_text() {
        for text in ["Password: ******", "~~~~", "a ______ b", "**bold**~~~~"] {
            let runs = inline(text);
            let drawn: String = runs.iter().map(|it| it.text.as_str()).collect();
            let expected = text.replace("**bold**", "bold");
            assert_eq!(drawn, expected, "{text:?} gave {runs:?}");
        }
    }

    /// **A tab indents code the way four spaces do**, and comes off as one
    /// character rather than four bytes: three letters went with it from every
    /// tab-indented line, and the whole line where the fourth byte fell inside
    /// a character.
    #[test]
    fn a_tab_indented_code_line_keeps_every_character() {
        let blocks = parse("Before.\n\n\tlet x = 1;\n\tééx\n    four spaces\n");
        let Kind::Code { lines, .. } = &blocks[2].kind else { panic!("{blocks:?}") };
        assert_eq!(lines, &["let x = 1;", "ééx", "four spaces"]);
    }

    /// A run of plain text is one run, not one per character.
    #[test]
    fn plain_text_is_one_run() {
        assert_eq!(inline("just some words").len(), 1);
        assert_eq!(inline(""), vec![Run::plain("")]);
    }

    /// A document built to make the parser slow, `bytes` long, or `None` for a
    /// case nobody wrote.
    ///
    /// Each is one paragraph, and each starts with a letter so that it stays
    /// one — a line of nothing but `*` is a rule and a line of backticks is a
    /// fence.
    fn hostile(case: &str, bytes: usize) -> Option<String> {
        let fill = |unit: &str| {
            let mut out = "x".to_owned();
            while out.len() + unit.len() <= bytes {
                out.push_str(unit);
            }
            out
        };
        Some(match case {
            "stars" => fill("*"),
            "ticks" => fill("`"),
            "brackets" => fill("["),
            "angles" => fill("<"),
            "alternating" => fill("*_"),
            "nested" => {
                let depth = bytes / 5;
                format!("{}x{}", "[".repeat(depth), "](u)".repeat(depth))
            }
            "honest" => include_str!("../../../docs/12-measurement.md").to_owned(),
            _ => return None,
        })
    }

    /// How long a test waits for a parse of the most the panel reads.
    ///
    /// **Loose on purpose.** All four of the openers' paragraphs parse in 0.24 s
    /// in a debug build, and the fastest of them took longer than this before in
    /// the same build, so it is a bound the old parser fails with twenty times
    /// the room the new one needs — which is what a debug build beside somebody
    /// else's compile asks for.
    const BOUND: std::time::Duration = std::time::Duration::from_secs(5);

    /// The blocks of `text`, or `None` if the parse was still running after
    /// `bound`.
    ///
    /// **On a thread of its own, so a regression fails at the bound** rather
    /// than after the minutes a quadratic parse takes in a debug build; the
    /// thread is left to finish on its own. Its stack is Rust's default, which is
    /// the job pool's, which is where the window parses.
    fn parsed_within(text: String, bound: std::time::Duration) -> Option<Vec<Block>> {
        let (done, waiting) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(parse(&text));
        });
        waiting.recv_timeout(bound).ok()
    }

    /// **No paragraph costs more than time linear in its length.** Each of these
    /// took seconds over 256 KiB in a release build — 2.6 s for the asterisks,
    /// 10.5 s for the brackets — because every opener without a closer scanned
    /// to the end of its paragraph and the parser then moved on by one character.
    #[test]
    fn a_paragraph_of_openers_is_parsed_in_linear_time() {
        for case in ["stars", "ticks", "brackets", "angles"] {
            let text = hostile(case, crate::quicklook::TEXT_LIMIT).expect("a case");
            let blocks = parsed_within(text.clone(), BOUND)
                .unwrap_or_else(|| panic!("{case} took longer than {BOUND:?}"));
            assert_eq!(blocks.len(), 1, "{case} is one paragraph");
            // An opener nothing closes is its own text, and so is an emphasis
            // with nothing in it, which between them is every character here.
            assert_eq!(words(&blocks[0]), text, "{case} lost a character");
        }
    }

    /// **A link inside a link inside a link stops being parsed at a depth**, and
    /// what is past it is drawn as the characters it is made of.
    ///
    /// Before the bound, this document — 52,428 links each inside the last, the
    /// most the panel reads — overflowed an eight-mebibyte stack, and eight
    /// thousand of them overflowed a job thread's two.
    #[test]
    fn nesting_inside_a_paragraph_stops_at_a_depth() {
        let text = hostile("nested", crate::quicklook::TEXT_LIMIT).expect("a case");
        let levels = text.matches('[').count();
        let blocks = parsed_within(text, BOUND).expect("the parse finished");
        let past = levels - usize::from(super::DEEPEST);
        let Kind::Paragraph(runs) = &blocks[0].kind else { panic!("{:?}", blocks[0]) };
        assert_eq!(
            runs.iter().map(|it| it.text.as_str()).collect::<String>(),
            format!("{}x{}", "[".repeat(past), "](u)".repeat(past)),
            "the first sixteen levels are links and the rest is text"
        );
        assert!(runs.iter().all(|it| it.style.link), "inside sixteen links is inside a link");
    }

    /// What one parse costs, over a document built to be slow or an honest one.
    ///
    /// One case per process, because before the depth bound the deepest of them
    /// overflowed the stack and took the process with it. Each repetition parses
    /// on a fresh thread with the job pool's stack — two mebibytes, Rust's
    /// default, which is what `sk_util::Pool` asks for by asking for nothing —
    /// unless `SKERRY_MARKDOWN_STACK_MIB` says otherwise:
    ///
    /// ```text
    /// SKERRY_MARKDOWN_CASE=brackets cargo test -p sk-app --release --lib \
    ///     measure_the_parser -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement; run it deliberately"]
    fn measure_the_parser() {
        let number = |name: &str, default: usize| {
            std::env::var(name).ok().and_then(|it| it.parse().ok()).unwrap_or(default)
        };
        let case = std::env::var("SKERRY_MARKDOWN_CASE").unwrap_or_else(|_| "honest".to_owned());
        let bytes = number("SKERRY_MARKDOWN_BYTES", crate::quicklook::TEXT_LIMIT);
        let repetitions = number("SKERRY_MARKDOWN_REPS", 11);
        let stack = number("SKERRY_MARKDOWN_STACK_MIB", 2) * 1024 * 1024;
        let budget = std::time::Duration::from_secs(number("SKERRY_MARKDOWN_BUDGET_S", 300) as u64);
        let Some(text) = hostile(&case, bytes) else {
            println!("skerry-markdown no case called {case:?}");
            return;
        };
        let load = || std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        println!("skerry-markdown load before {}", load().trim());
        let text = std::sync::Arc::new(text);
        let started = std::time::Instant::now();
        let mut taken = Vec::new();
        let mut blocks = 0;
        while taken.len() < repetitions && (taken.is_empty() || started.elapsed() < budget) {
            let text = std::sync::Arc::clone(&text);
            let worker = std::thread::Builder::new().stack_size(stack).spawn(move || {
                let at = std::time::Instant::now();
                let parsed = parse(&text);
                (at.elapsed(), parsed.len())
            });
            let (took, count) = worker.expect("a thread").join().expect("the parse");
            taken.push(took.as_secs_f64() * 1000.0);
            blocks = count;
        }
        taken.sort_by(f64::total_cmp);
        println!(
            "skerry-markdown case={case} bytes={} reps={} blocks={blocks} \
             min={:.3} median={:.3} max={:.3} ms",
            text.len(),
            taken.len(),
            taken[0],
            taken[taken.len() / 2],
            taken[taken.len() - 1],
        );
        println!("skerry-markdown load after {}", load().trim());
    }
}
