// SPDX-License-Identifier: Apache-2.0
//! Quick look: one file, as large as the window will allow.
//!
//! `Space` over a row and the file is on screen — a picture, a document's first
//! page, or its own text. `Space` again, or `Escape`, and it is gone.
//!
//! # Where each of the three comes from
//!
//! A picture and a page come from a sandboxed child, because decoding is the
//! attack surface — [ADR 0006](../../../docs/adr/0006-out-of-process-decoders.md).
//! Text is read here, on a job, because reading a file is not decoding and a
//! process round trip to do a `read` would be paying for nothing.
//!
//! # A preview is not a thumbnail
//!
//! It is one image at a time, at whatever size the panel is, and it is not cached
//! —
//! [ADR 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md).
//! Caching it would mean a file per window size for a picture somebody is looking
//! at once. It is not in the slot atlas either, for the same reason: it is not
//! 128 pixels square and it never will be.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use sk_gpu::ImageKey;
use sk_preview::Refusal;

use crate::previews::Shows;

/// How much of a text file is read.
///
/// **A screenful, not a file.** Quick look is a glance, and a hundred megabytes
/// of log is a hundred megabytes of allocation for the forty lines a panel can
/// draw. The same figure `sk_fs::text` uses for *Copy the text inside*, for the
/// same reason.
const TEXT_LIMIT: usize = 256 * 1024;

/// How many pages are rendered before they are asked for.
///
/// **Two** — [ADR
/// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md).
/// It is the figure that fits the byte budget beside the page on screen on a 4K
/// panel, where one page is 11.5 MB of the 24 MiB allowed.
const AHEAD: u32 = 2;

/// The most lines held. Beyond this a file is longer than the panel can show and
/// the rest is what an editor is for.
const LINES: usize = 400;

/// How far a picture can be magnified past the panel's fit.
///
/// **Eight**, which is the point at which a photograph on this panel is showing
/// individual pixels of the original and there is nothing further to see. It is
/// also the bound that keeps the requested box's arithmetic away from the wire's
/// sixty-four-megabyte ceiling.
const MOST_ZOOM: f32 = 8.0;

/// What a tab is drawn as.
///
/// **Because the shaper gives `\t` no advance at all**, and a preview that drew
/// nothing for it put an indented line hard against the margin — visible in
/// `quick-look-text`, where the tabbed line and the space-indented line below it
/// started in different places for no reason a reader could see. Four spaces
/// rather than a tab stop, because a stop needs a column and a preview that
/// elides long lines is not the place to start counting them. It matters most for
/// the thing a highlighter will later colour: code, whose shape *is* its
/// indentation.
const TAB: &str = "    ";

/// What a run of characters is drawn as.
///
/// **A role, never a colour**, so the theme decides what a keyword looks like and
/// a light theme is not a second table of hexadecimal — [`sk_ui::Code`] is where
/// the four hues live. The highlighter that produces these is
/// [`crate::highlight`], and it is the only thing that has to change to add a
/// language: text is stored, drawn, scrolled, selected and edited the same way
/// whatever the tones are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tone {
    /// Ordinary text, in the body colour — most of every file.
    Plain,
    /// A line or block comment.
    Comment,
    /// A word the language reserves.
    Keyword,
    /// A string or character literal.
    Literal,
    /// A number.
    Number,
}

/// One run of characters in one tone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Span {
    /// Bytes from the start of the line.
    pub(crate) from: usize,
    /// Bytes, not characters: a slice of the line.
    pub(crate) to: usize,
    /// What it is drawn as.
    pub(crate) tone: Tone,
}

/// A place in the text: which line, and which byte of it.
///
/// **A byte rather than a character**, because that is what a slice takes and
/// what the clipboard wants; `sk_ui::caret_offset` is what turns a pixel into
/// one, and it lands on character boundaries by construction.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Spot {
    /// Which line, indexed from zero.
    pub(crate) line: usize,
    /// Which byte of that line.
    pub(crate) byte: usize,
}

/// A selection, as the two ends a drag produced.
///
/// **Anchor and caret rather than start and end**, because dragging upward is an
/// ordinary thing to do and a selection that reordered itself as the pointer
/// crossed its own start would grow from the wrong end. The ordering happens
/// where it is needed, which is drawing and copying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Chosen {
    /// Where the drag began.
    pub(crate) anchor: Spot,
    /// Where it is now.
    pub(crate) caret: Spot,
}

impl Chosen {
    /// The two ends in reading order.
    pub(crate) fn ordered(self) -> (Spot, Spot) {
        if self.anchor <= self.caret {
            (self.anchor, self.caret)
        } else {
            (self.caret, self.anchor)
        }
    }

    /// Whether it covers nothing, which a click without a drag produces.
    pub(crate) fn is_empty(self) -> bool {
        self.anchor == self.caret
    }
}

/// One step of the caret.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    /// One character back.
    Back,
    /// One character on.
    On,
    /// The row above — the *drawn* row, so a wrapped line has as many as it took.
    Up,
    /// The row below.
    Down,
    /// The start of this drawn row.
    LineStart,
    /// The end of it.
    LineEnd,
    /// The start of the word before the caret.
    WordBack,
    /// The end of the word after it.
    WordOn,
    /// The first character of the file.
    Start,
    /// The last one.
    End,
}

/// One drawn row: the part of one line that fitted across the panel.
///
/// **The unit of drawing, of clicking and of `Up`/`Down`**, because a line and a
/// row stopped being the same thing when the text learned to wrap. A line that
/// fits is one row; a paragraph is as many as it took.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    /// Which line of the file it is part of.
    pub(crate) line: usize,
    /// The first byte of that line on this row.
    pub(crate) from: usize,
    /// One past its last byte.
    pub(crate) to: usize,
}

/// A file's text, cut into lines and each line into tones.
pub(crate) struct Text {
    /// The lines, without their terminators.
    pub(crate) lines: Vec<String>,
    /// One run list per line, the same length as `lines`.
    pub(crate) spans: Vec<Vec<Span>>,
    /// Whether the file went on past what was read.
    pub(crate) truncated: bool,
    /// The drawn rows, in order: every line's, cut where it wrapped.
    rows: Vec<Row>,
    /// How many characters wide the rows were wrapped to. Zero is unwrapped.
    cells: usize,
    /// Whose rules the tones follow, from the file's own name.
    language: crate::highlight::Language,
}

/// Cuts each line into tones, by the rules of whatever language this is.
///
/// **A pass over the file, not a property of drawing it.** It runs when the text
/// lands and after an edit, never per frame: a scanner inside the 8 ms budget is
/// a scanner that runs sixty times a second over a file nobody is changing.
fn highlight(lines: &[String], language: crate::highlight::Language) -> Vec<Vec<Span>> {
    crate::highlight::all(lines, language)
}

impl Text {
    /// The whole file, as it would be written back.
    ///
    /// **One trailing newline, always.** Every line was split on one and the last
    /// terminator is not a line — a file that ends without one gains it here, and
    /// that is the single normalisation this editor makes. POSIX text files end
    /// with a newline and every tool that reads them expects it.
    pub(crate) fn body(&self) -> String {
        let mut out = self.lines.join("\n");
        out.push('\n');
        out
    }

    /// Puts `text` in at `at`, and says where the caret ends up.
    ///
    /// **Newlines split lines**, which is what makes this the same function for
    /// typing a letter, pressing Enter and pasting six lines.
    pub(crate) fn insert(&mut self, at: Spot, text: &str) -> Spot {
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.spans.push(Vec::new());
        }
        let line = at.line.min(self.lines.len() - 1);
        let byte = at.byte.min(self.lines[line].len());
        let tail = self.lines[line].split_off(byte);
        let mut pieces = text.split('\n');
        let first = pieces.next().unwrap_or_default();
        self.lines[line].push_str(&first.replace('\t', TAB));
        let mut spot = Spot { line, byte: self.lines[line].len() };
        for piece in pieces {
            spot.line += 1;
            let piece = piece.replace('\t', TAB);
            spot.byte = piece.len();
            self.lines.insert(spot.line, piece);
            self.spans.insert(spot.line, Vec::new());
        }
        self.lines[spot.line].push_str(&tail);
        self.retone(line..=spot.line);
        self.rewrap();
        spot
    }

    /// Takes out everything between two spots, and says where the caret lands.
    pub(crate) fn remove(&mut self, from: Spot, to: Spot) -> Spot {
        if self.lines.is_empty() || from >= to {
            return from;
        }
        let last = self.lines.len() - 1;
        let (from, to) = (self.clamped(from, last), self.clamped(to, last));
        let tail = self.lines[to.line].split_off(to.byte);
        self.lines[from.line].truncate(from.byte);
        if to.line > from.line {
            self.lines.drain(from.line + 1..=to.line);
            self.spans.drain(from.line + 1..=to.line);
        }
        self.lines[from.line].push_str(&tail);
        self.retone(from.line..=from.line);
        self.rewrap();
        from
    }

    /// The spot one step back from `at`, or `at` at the very beginning.
    pub(crate) fn before(&self, at: Spot) -> Spot {
        if at.byte > 0 {
            let line = self.lines.get(at.line).map_or("", String::as_str);
            let byte = line[..at.byte.min(line.len())]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index);
            return Spot { line: at.line, byte };
        }
        if at.line == 0 {
            return at;
        }
        let line = at.line - 1;
        Spot { line, byte: self.lines.get(line).map_or(0, String::len) }
    }

    /// The spot one step on from `at`, or `at` at the very end.
    pub(crate) fn after(&self, at: Spot) -> Spot {
        let line = self.lines.get(at.line).map_or("", String::as_str);
        if at.byte < line.len() {
            let byte = line[at.byte..]
                .chars()
                .next()
                .map_or(line.len(), |first| at.byte + first.len_utf8());
            return Spot { line: at.line, byte };
        }
        if at.line + 1 >= self.lines.len() {
            return at;
        }
        Spot { line: at.line + 1, byte: 0 }
    }

    /// A spot that names a place this text actually has.
    fn clamped(&self, spot: Spot, last: usize) -> Spot {
        let line = spot.line.min(last);
        Spot { line, byte: spot.byte.min(self.lines[line].len()) }
    }

    /// Re-tones after an edit — the whole file, and it has to be.
    ///
    /// **The assumption this replaces was that highlighting is line-local.** It
    /// is not: a `*/` typed on line 12 changes the colour of every line after it,
    /// and a `/*` deleted does the same. What makes the whole file affordable is
    /// the cap a preview already has — [`LINES`], four hundred — so this is tens
    /// of microseconds per keystroke over text that cannot grow.
    fn retone(&mut self, _lines: std::ops::RangeInclusive<usize>) {
        self.spans = highlight(&self.lines, self.language);
    }

    /// Cuts `body` into lines and tones.
    fn of(body: &str, language: crate::highlight::Language) -> Self {
        let mut lines: Vec<String> =
            body.lines().take(LINES).map(|line| line.replace('\t', TAB)).collect();
        let truncated = body.lines().nth(LINES).is_some() || body.len() >= TEXT_LIMIT;
        // A file with no newline at all is one line, and an empty file is none —
        // which draws as an empty panel rather than as a panel with one blank row.
        if lines.is_empty() && !body.is_empty() {
            lines.push(body.replace('\t', TAB));
        }
        let spans = highlight(&lines, language);
        let mut text = Self { lines, spans, truncated, rows: Vec::new(), cells: 0, language };
        text.rewrap();
        text
    }

    /// Cuts every line to `cells` characters wide, where that has changed.
    ///
    /// **Called once a frame from the one place that knows the width.** The rows
    /// are the layout, so recomputing them per frame would be a pass over the
    /// file inside the 8 ms budget; recomputing them when the panel's width
    /// changes is a pass over four hundred lines when a window is resized.
    pub(crate) fn wrap(&mut self, cells: usize) {
        if cells == self.cells {
            return;
        }
        self.cells = cells;
        self.rewrap();
    }

    /// How wide the rows are cut, in characters.
    pub(crate) fn cells(&self) -> usize {
        self.cells
    }

    /// The drawn rows.
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Cuts the lines again at the width already remembered.
    ///
    /// **After every edit**, because a typed character can push a word onto the
    /// next row and a `Backspace` can pull it back. Four hundred lines is the
    /// most a preview reads — `LINES` — so this is a bounded pass and not a
    /// function of the file's size.
    fn rewrap(&mut self) {
        self.rows.clear();
        for (line, body) in self.lines.iter().enumerate() {
            wrap_one(body, self.cells, line, &mut self.rows);
        }
        if self.rows.is_empty() {
            self.rows.push(Row { line: 0, from: 0, to: 0 });
        }
    }

    /// Which drawn row a spot is on.
    ///
    /// At a wrap the answer is the *following* row, which is where the caret
    /// belongs: the character it is in front of is the first one of that row.
    pub(crate) fn row_of(&self, at: Spot) -> usize {
        let after = self.rows.partition_point(|row| (row.line, row.from) <= (at.line, at.byte));
        after.saturating_sub(1)
    }

    /// How many characters into its row a spot is.
    pub(crate) fn cell_of(&self, at: Spot) -> usize {
        let Some(row) = self.rows.get(self.row_of(at)) else { return 0 };
        let body = self.lines.get(row.line).map_or("", String::as_str);
        let from = row.from.min(body.len());
        let to = at.byte.clamp(from, body.len());
        body.get(from..to).map_or(0, |slice| slice.chars().count())
    }

    /// The spot `cell` characters into the drawn row at `row`.
    pub(crate) fn spot_at(&self, row: usize, cell: usize) -> Spot {
        let Some(row) = self.rows.get(row.min(self.rows.len().saturating_sub(1))) else {
            return Spot { line: 0, byte: 0 };
        };
        let body = self.lines.get(row.line).map_or("", String::as_str);
        let (from, to) = (row.from.min(body.len()), row.to.min(body.len()));
        let byte = body
            .get(from..to)
            .and_then(|slice| slice.char_indices().nth(cell))
            .map_or(to, |(index, _)| from + index);
        Spot { line: row.line, byte }
    }

    /// The start of the word before `at`, or the end of the line above.
    pub(crate) fn word_before(&self, at: Spot) -> Spot {
        let body = self.lines.get(at.line).map_or("", String::as_str);
        let head = &body[..at.byte.min(body.len())];
        if head.is_empty() {
            return self.before(at);
        }
        // Whitespace first, then the run of word characters behind it: two
        // presses to cross `foo   bar` is what every field in this program does,
        // and `Field` settled the same rule for a name being typed.
        let trimmed = head.trim_end();
        let byte = trimmed
            .char_indices()
            .rev()
            .find(|(_, letter)| letter.is_whitespace())
            .map_or(0, |(index, letter)| index + letter.len_utf8());
        Spot { line: at.line, byte }
    }

    /// The end of the word after `at`, or the start of the line below.
    pub(crate) fn word_after(&self, at: Spot) -> Spot {
        let body = self.lines.get(at.line).map_or("", String::as_str);
        let from = at.byte.min(body.len());
        let tail = &body[from..];
        if tail.is_empty() {
            return self.after(at);
        }
        let skipped = tail.len() - tail.trim_start().len();
        let byte = tail[skipped..]
            .char_indices()
            .find(|(_, letter)| letter.is_whitespace())
            .map_or(body.len(), |(index, _)| from + skipped + index);
        Spot { line: at.line, byte }
    }
}

/// Cuts one line into rows of at most `cells` characters.
///
/// **Greedy, and at a space where there is one.** A hard cut every `cells`
/// characters splits words in half, which is what makes wrapped prose unreadable;
/// a word longer than a whole row is cut, because the alternative is a row wider
/// than the panel. `cells` of zero is a panel too narrow to have measured yet, and
/// leaves the line whole.
fn wrap_one(body: &str, cells: usize, line: usize, out: &mut Vec<Row>) {
    if cells == 0 || body.chars().count() <= cells {
        out.push(Row { line, from: 0, to: body.len() });
        return;
    }
    let mut from = 0;
    while from < body.len() {
        let rest = &body[from..];
        if rest.chars().count() <= cells {
            out.push(Row { line, from, to: body.len() });
            return;
        }
        let hard = from + rest.char_indices().nth(cells).map_or(rest.len(), |(index, _)| index);
        // The space is kept at the end of the row it broke, which is where a
        // caret at the end of a wrapped row belongs.
        let to = body[from..hard]
            .rfind(' ')
            .map(|index| from + index + 1)
            .filter(|breaking| *breaking > from)
            .unwrap_or(hard);
        out.push(Row { line, from, to });
        from = to;
    }
}

/// What was last asked of a child: the box, and the page.
///
/// **One value rather than two fields**, because they are one question. A resize
/// and a page turn are both "the panel wants a different image now", and a
/// `wants` that compared them separately would be two chances to forget one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Shot {
    /// The box, in device pixels.
    pub(crate) within: (u32, u32),
    /// The page, one-based.
    pub(crate) page: u32,
}

/// What is on screen, and what is still coming.
pub(crate) struct QuickLook {
    /// The file being looked at.
    pub(crate) path: PathBuf,
    /// Its modification time when it was asked for.
    pub(crate) mtime: i64,
    /// Which of the three it is.
    pub(crate) shows: Shows,
    /// The picture, once the atlas holds it, and the size it came back at.
    pub(crate) image: Option<(ImageKey, (u32, u32))>,
    /// The file's own thumbnail, standing in until the full picture lands.
    ///
    /// **Free, because it is already on the GPU.** The row `Space` was pressed on
    /// was visible, so `settle_thumbnails` has already put its 128-pixel
    /// thumbnail in the atlas — drawing that, blurred up to the panel, costs one
    /// quad and turns a 60–155 ms wait into no wait at all. Pictures only: a
    /// document has no thumbnail, by ADR 0018, because a page costs a process.
    ///
    /// A key and the thumbnail's own size, which is the picture's aspect — that
    /// is what places the stand-in where its replacement will go.
    pub(crate) standby: Option<(ImageKey, (u32, u32))>,
    /// The pages already rendered and on the GPU, by page number.
    ///
    /// **What makes a turn instant** — [ADR
    /// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md).
    /// The renderer holds a texture each, bounded by bytes and evicted by what was
    /// last drawn; this is the host's half of that, which is the page number a key
    /// stands for. An entry here whose texture has been evicted resolves to
    /// nothing and is rendered again, which is the right degradation.
    pub(crate) held: std::collections::HashMap<u32, (ImageKey, (u32, u32))>,
    /// The page being fetched ahead of being wanted, if any.
    pub(crate) ahead: Option<u32>,
    /// Whether what is *on screen* is a draft rather than the finished render.
    ///
    /// **Not the same question as [`Self::coarse`], and conflating them is what
    /// made a page turn look like a rescale.** `coarse` says what the *next* ask
    /// is, and [`Self::took`] clears it the moment the draft arrives — so the
    /// draft was on screen with `coarse` already false, drawn at one to one
    /// because that is what a finished picture gets: a third-size page in the
    /// middle of the panel, jumping to full size when the render landed.
    ///
    /// A draft is drawn scaled up to fill the box the finished one will occupy,
    /// which is the whole point of having one.
    pub(crate) drafted: bool,
    /// Whether the next ask is the quick coarse one rather than the full size.
    ///
    /// **Documents only.** A page's cost is proportional to what is drawn —
    /// 60–70 ms at 320 pixels against 135–200 ms at 1200, from ADR 0019's table —
    /// so a coarse pass is a third of the wait for something on screen. An image
    /// gets no coarse pass because there the cost is the decode: 60 ms at 128
    /// pixels and 67 ms at 1200, so a first pass would be a second full price.
    pub(crate) coarse: bool,
    /// The text, once it has been read.
    pub(crate) text: Option<Text>,
    /// Why there is nothing to show, if there is nothing.
    pub(crate) refusal: Option<Refusal>,
    /// Which page is wanted, one-based. Always 1 for anything that is not paged.
    pub(crate) page: u32,
    /// How many pages the document has, or zero for unknown — ADR 0019.
    pub(crate) pages: u32,
    /// What was last asked for, so a resize or a page turn asks again.
    pub(crate) asked: Option<Shot>,
    /// How far the text is scrolled.
    pub(crate) scroll: sk_ui::Scroll,
    /// What is selected in a text preview, if anything.
    pub(crate) chosen: Option<Chosen>,
    /// Whether the pointer is still down and extending it.
    pub(crate) dragging: bool,
    /// Whether the text is being edited rather than read.
    ///
    /// **Off until somebody asks.** A preview is for looking, and a panel that
    /// took keystrokes as edits the moment it opened would turn `Space` — the key
    /// that opens it — into the first character of a file. What edit mode changes
    /// is what the keyboard means; the drawing gains a caret and nothing else.
    pub(crate) editing: bool,
    /// Whether the buffer differs from the file.
    ///
    /// **Saved on the way out rather than asked about.** A panel that asked
    /// "save?" would be a question with an answer that loses work, and the undo
    /// stack is what makes the other choice safe: an edit is one entry, so
    /// `Ctrl+Z` takes back a save nobody meant — which is the whole reason
    /// `Change::Edited` carries both sides.
    pub(crate) unsaved: bool,
    /// The find bar over a page, where one is open, and what it found.
    ///
    /// **This page's words, and it says so.** Searching a whole document means a
    /// child round trip per page — the layer arrives with the pixels and there is
    /// one page on screen — so what this finds is what is in front of you, and
    /// the bar's own label carries that rather than leaving it to be discovered.
    pub(crate) finding: Option<crate::field::Field>,
    /// How many matches the query has, and which one is selected.
    pub(crate) found: (usize, usize),
    /// The words on the page on screen, where the child could say.
    ///
    /// **Per page, and dropped when the page turns** — ADR 0024. A layer belongs
    /// to one page and arrives with it; keeping the last one across a turn would
    /// draw page three's highlight over page four's text.
    pub(crate) page_text: Option<crate::pagetext::Page>,
    /// How much the picture is magnified past the size it was fitted to.
    ///
    /// **One is fitted, not one hundred percent.** What a person zooms *from* is
    /// what they can see, which is the picture at the panel's size — a photograph
    /// six thousand pixels wide has never been at 100% in this panel and starting
    /// there would open every holiday snap magnified eight times.
    pub(crate) zoom: f32,
    /// Where the picture is dragged to, in logical pixels from centred.
    pub(crate) pan: (f32, f32),
    /// Whether an animation is playing.
    ///
    /// **On, for a file that has frames.** A GIF that sat on its first frame
    /// until somebody found a control would be a GIF that looks broken; every
    /// viewer plays one and this one does too. A press on a frame arrow stops it,
    /// because somebody stepping frames has said what they want.
    pub(crate) playing: bool,
    /// How long the frame on screen is shown for, from the child's answer.
    pub(crate) delay_ms: u32,
    /// When the frame on screen went up, for the clock that turns the next one.
    shown_at: Option<std::time::Instant>,
    /// The text as it last stood on disk, while edit mode is on.
    ///
    /// **What Discard puts back.** An edit that has not been written has nothing
    /// on the undo stack to walk back — the stack is about the filesystem — so
    /// the one copy of the file as it was has to be here, taken when edit mode
    /// starts and replaced by every save.
    as_on_disk: Option<String>,
    reading: Option<mpsc::Receiver<Option<String>>>,
}

impl QuickLook {
    /// Opens a look at `path`, starting whatever has to be started.
    pub(crate) fn open(path: PathBuf, mtime: i64, shows: Shows, jobs: &crate::pane::Jobs) -> Self {
        let mut look = Self {
            path,
            mtime,
            shows,
            image: None,
            standby: None,
            held: std::collections::HashMap::new(),
            ahead: None,
            coarse: shows == Shows::Page,
            drafted: false,
            text: None,
            refusal: None,
            page: 1,
            pages: 0,
            asked: None,
            scroll: sk_ui::Scroll::default(),
            chosen: None,
            dragging: false,
            editing: false,
            unsaved: false,
            page_text: None,
            finding: None,
            found: (0, 0),
            zoom: 1.0,
            pan: (0.0, 0.0),
            playing: true,
            delay_ms: 0,
            shown_at: None,
            as_on_disk: None,
            reading: None,
        };
        if shows == Shows::Text {
            look.start_reading(jobs);
        }
        look
    }

    /// Reads the file on a job, because reading a file is a syscall — rule 1.
    fn start_reading(&mut self, jobs: &crate::pane::Jobs) {
        let (done, waiting) = mpsc::channel();
        self.reading = Some(waiting);
        let path = self.path.clone();
        let wake = jobs.wake.clone();
        jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let _ = done.send(sk_fs::text::read_text(&path, TEXT_LIMIT).ok());
            wake.wake();
        });
    }

    /// Takes delivery of a text read. Whether anything arrived.
    pub(crate) fn drain(&mut self) -> bool {
        let Some(waiting) = &self.reading else { return false };
        let Ok(body) = waiting.try_recv() else { return false };
        self.reading = None;
        match body {
            Some(body) => {
                let language = crate::highlight::Language::of(&self.title());
                self.text = Some(Text::of(&body, language));
            }
            None => self.refusal = Some(Refusal::Unreadable),
        }
        true
    }

    /// Whether an image still has to be asked for, at `within`.
    ///
    /// **Also true after a resize.** A preview is rendered at the size of the panel
    /// drawing it, so a window twice as wide is a different question — and for a
    /// vector or a PDF page, a materially better answer.
    pub(crate) fn wants(&self, within: (u32, u32)) -> bool {
        self.shows != Shows::Text && self.refusal.is_none() && self.asked != Some(self.shot(within))
    }

    /// What to ask for at this size.
    ///
    /// The coarse pass asks for a third of the long edge, which is where the
    /// three-fold difference in [`Self::coarse`] comes from.
    pub(crate) fn shot(&self, within: (u32, u32)) -> Shot {
        // **A zoom asks for a bigger picture, not a bigger drawing.** A
        // photograph six thousand pixels wide was fitted to a thousand, so
        // magnifying what came back is magnifying a downscale — the detail is in
        // the file and the child has to be asked for it. Bounded twice: eight
        // megapixels, which is a third of ADR 0020's budget for the one picture
        // on screen, and four thousand a side, which is the wire's own ceiling.
        //
        // **Not for an animation.** Composing every frame of a GIF at eight
        // megapixels is a decode the panel cannot keep up with at ten frames a
        // second, so a zoomed animation is an upscale and says so by being soft.
        if self.zoomed() && !self.animated() && !self.coarse {
            let bigger = |it: u32, zoom: f32| ((it as f32 * zoom) as u32).clamp(1, 4096);
            let mut wanted = (bigger(within.0, self.zoom), bigger(within.1, self.zoom));
            while u64::from(wanted.0) * u64::from(wanted.1) > 8 * 1024 * 1024 {
                wanted = (wanted.0 * 3 / 4, wanted.1 * 3 / 4);
            }
            return Shot { within: wanted, page: self.page };
        }
        if self.coarse {
            let long = within.0.max(within.1).max(1);
            let side = (long / 3).clamp(64, 512);
            let shrink = |it: u32| (it * side / long).max(1);
            return Shot { within: (shrink(within.0), shrink(within.1)), page: self.page };
        }
        Shot { within, page: self.page }
    }

    /// Whether this look answers to `key` — the file, when it changed, and the
    /// box it is currently asking at.
    ///
    /// **The page is deliberately not part of it**, and it was: a reply for a page
    /// other than the one on screen is not stale, it is the *prefetch* — ADR 0020
    /// — and matching on the page dropped every one of them, silently, leaving
    /// `look_held` at one for ever. Which page a reply is of is
    /// [`Self::took`]'s business, and that is where "a page-3 reply must not be
    /// drawn as page 4" lives.
    ///
    /// The box still is part of it, and that is what makes a *draft* reply stale
    /// once the full size has been asked for.
    pub(crate) fn answers_to(&self, key: &sk_preview::Key) -> bool {
        self.is_of(&key.path, key.mtime) && self.asked.is_some_and(|shot| shot.within == key.within)
    }

    /// Turns `delta` pages, and says whether the page moved.
    ///
    /// **Clamped where the count is known and open where it is not.** A machine
    /// without `pdfinfo` previews the document and cannot say how long it is —
    /// ADR 0019 — so `↓` there is allowed to ask for a page that may not exist,
    /// and `NoSuchPage` is how it finds out.
    pub(crate) fn turn(&mut self, delta: i32) -> bool {
        if !self.paged() {
            return false;
        }
        let wanted = self.page.saturating_add_signed(delta).max(1);
        // **An animation wraps and a document stops.** Coming off the end of a
        // GIF is its next loop; coming off the end of a PDF is the end of the PDF.
        let wanted = match (self.animated(), self.pages) {
            (true, pages) if pages > 0 && wanted > pages => 1,
            (true, pages) if pages > 0 && delta < 0 && self.page == 1 => pages,
            (_, pages) if pages > 0 => wanted.min(pages),
            _ => wanted,
        };
        let moved = wanted != self.page;
        if moved {
            // The words belong to the page that is going away, and so does what
            // was found in them.
            self.page_text = None;
            self.finding = None;
            self.found = (0, 0);
        }
        self.page = wanted;
        // **A turn is a draft first, for the same reason opening one is.** `↓`
        // twice in a row is a person looking for something, and two 65 ms drafts
        // answer that better than one 150 ms page.
        //
        // **Not for an animation**, whose frames are already the panel's size and
        // whose next two are on the GPU: a draft would be a third-size frame in
        // the middle of a loop.
        self.coarse |= moved && self.shows == Shows::Page;
        moved
    }

    /// Opens the find bar, or closes it. Whether it is open now.
    pub(crate) fn toggle_finding(&mut self) -> bool {
        if self.page_text.is_none() {
            return false;
        }
        if self.finding.take().is_some() {
            self.found = (0, 0);
            if let Some(page) = self.page_text.as_mut() {
                page.clear();
            }
            return false;
        }
        self.finding = Some(crate::field::Field::new(String::new()));
        true
    }

    /// Runs the query again, from match `at`.
    pub(crate) fn find_again(&mut self, at: usize) {
        let Some(query) = self.finding.as_ref().map(|field| field.text.clone()) else { return };
        let Some(page) = self.page_text.as_mut() else { return };
        self.found = page.find(&query, at);
    }

    /// Steps to the next match, or the previous one.
    pub(crate) fn step_found(&mut self, by: isize) {
        let (count, at) = self.found;
        if count == 0 {
            return;
        }
        let next = (at.cast_signed() + by).rem_euclid(count.cast_signed()).unsigned_abs();
        self.find_again(next);
    }

    /// What the find bar says between its field and its edge.
    pub(crate) fn found_label(&self) -> String {
        let (count, at) = self.found;
        if count == 0 {
            let empty = self.finding.as_ref().is_none_or(|field| field.text.trim().is_empty());
            return if empty {
                String::new()
            } else {
                sk_model::i18n::lookup("look.found_none").to_owned()
            };
        }
        sk_model::i18n::format("look.found", &[&(at + 1), &count])
    }

    /// Whether there is a picture that a zoom would mean anything to.
    pub(crate) fn zoomable(&self) -> bool {
        self.shows != Shows::Text && self.image.is_some()
    }

    /// Whether it is drawn larger than the panel it is in.
    pub(crate) fn zoomed(&self) -> bool {
        self.zoom > 1.001
    }

    /// Multiplies the zoom, keeping `holding` — a fraction of the panel from its
    /// centre — under the same part of the picture. Whether anything moved.
    ///
    /// **About the pointer, not about the centre.** A zoom that always grows from
    /// the middle makes a person chase the thing they were looking at across the
    /// panel with a pan, and the arithmetic that avoids it is two lines: the pan
    /// scales with the picture, so the offset from the held point scales too.
    pub(crate) fn zoom_by(&mut self, factor: f32, holding: (f32, f32)) -> bool {
        if !self.zoomable() {
            return false;
        }
        let was = self.zoom;
        let zoom = (was * factor).clamp(1.0, MOST_ZOOM);
        if (zoom - was).abs() < 0.001 {
            return false;
        }
        let growth = zoom / was;
        self.zoom = zoom;
        // The point under the pointer stays where it is: everything else moves
        // away from it by however much the picture grew.
        self.pan.0 = (self.pan.0 - holding.0) * growth + holding.0;
        self.pan.1 = (self.pan.1 - holding.1) * growth + holding.1;
        if !self.zoomed() {
            self.pan = (0.0, 0.0);
        }
        true
    }

    /// Puts the picture back to the size the panel fits it at.
    pub(crate) fn unzoom(&mut self) -> bool {
        if !self.zoomable() || (!self.zoomed() && self.pan == (0.0, 0.0)) {
            return false;
        }
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        true
    }

    /// Drags the picture, where it is larger than the panel.
    pub(crate) fn pan_by(&mut self, by: (f32, f32)) -> bool {
        if !self.zoomed() {
            return false;
        }
        self.pan.0 += by.0;
        self.pan.1 += by.1;
        true
    }

    /// Whether this look has more than one image in it.
    ///
    /// **A document, or an animation.** Both are a stack of pictures the panel
    /// walks with the same two arrows, the same prefetch and the same byte
    /// budget — ADR 0020 — and the only difference is that one of them turns its
    /// own pages.
    pub(crate) fn paged(&self) -> bool {
        self.shows == Shows::Page || self.pages > 1
    }

    /// Whether it is a stack of frames that plays rather than pages that turn.
    pub(crate) fn animated(&self) -> bool {
        self.shows == Shows::Picture && self.pages > 1
    }

    /// How long until the frame on screen has had its time, while playing.
    ///
    /// `None` where nothing is animating, which is what keeps an idle window
    /// asleep: an animation is a wake-up with an owner, and this is the owner
    /// saying when it wants one.
    pub(crate) fn due_in(&self) -> Option<std::time::Duration> {
        if !self.playing || !self.animated() || self.image.is_none() {
            return None;
        }
        let delay = std::time::Duration::from_millis(u64::from(self.delay_ms.max(20)));
        let since = self.shown_at?.elapsed();
        Some(delay.saturating_sub(since))
    }

    /// Turns to the next frame if this one has had its time. Whether it did.
    pub(crate) fn advance(&mut self) -> bool {
        if self.due_in() != Some(std::time::Duration::ZERO) {
            return false;
        }
        // **The clock restarts here and not when the picture lands.** A frame
        // whose neighbour is not on the GPU yet would otherwise be asked for once
        // per frame of the window, which is a spin rather than an animation.
        self.shown_at = Some(std::time::Instant::now());
        self.turn(1)
    }

    /// Takes a picture, and stops the coarse pass standing in for it.
    ///
    /// **The coarse pass is not the answer, it is the first draft of it.** A page
    /// drawn at a third of the panel is legible enough to read the shape of and
    /// too soft to read the words of, so the full size is asked for as soon as the
    /// draft is on screen — and the draft stays up until it arrives, because a
    /// panel that blanked between the two would be worse than one that waited.
    pub(crate) fn took(&mut self, key: ImageKey, size: (u32, u32), page: u32) {
        // **A draft is not remembered.** Only the full render goes in `held`, or a
        // turn back to a page would show the third-size version of it for ever.
        if !self.coarse && self.paged() {
            self.held.insert(page, (key, size));
        }
        if page == self.page {
            self.image = Some((key, size));
            // **What arrived is a draft exactly when a draft was asked for.** The
            // reply is matched against the shot currently outstanding, so there is
            // no third possibility — and this is the flag the drawing reads, one
            // line before the flag that decides the *next* ask is cleared.
            self.drafted = self.coarse;
            self.shown_at = Some(std::time::Instant::now());
            // The stand-in has done its job and must stop being drawn under a
            // picture with transparency in it.
            self.standby = None;
            self.coarse = false;
        }
    }

    /// Drops pages whose textures the renderer no longer has.
    ///
    /// **Because the two stores are bounded separately and only one of them
    /// knows.** The renderer holds 24 MiB of pages and evicts by what was last
    /// drawn — [ADR
    /// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md)
    /// — and at a large panel a page is 11.9 MB, so opening a document evicts a
    /// prefetch almost at once. This map kept the key anyway, `show_held` handed
    /// it out, and the drawing emitted no quad for a key the atlas has never heard
    /// of: **a blank panel with a title and a page number, for ever**, and
    /// `wants` false because the shot matched. ADR 0020 says an evicted page "is
    /// rendered again, which is the right degradation" — this is the line that
    /// makes that true rather than aspirational.
    pub(crate) fn retain_held(&mut self, still_there: impl Fn(ImageKey) -> bool) {
        self.held.retain(|_, (key, _)| still_there(*key));
        if self.image.is_some_and(|(key, _)| !still_there(key)) {
            self.image = None;
            self.asked = None;
        }
    }

    /// Shows a page that is already on the GPU. Whether there was one.
    ///
    /// **The whole of what the prefetch buys.** A turn to a held page is a field
    /// assignment and a frame, with no child, no render and no wait.
    pub(crate) fn show_held(&mut self, within: (u32, u32)) -> bool {
        let Some(&(key, size)) = self.held.get(&self.page) else { return false };
        self.image = Some((key, size));
        self.shown_at = Some(std::time::Instant::now());
        self.standby = None;
        self.coarse = false;
        // Only a full render is ever held — see `took` — so what is on screen now
        // is not a stand-in.
        self.drafted = false;
        self.asked = Some(Shot { within, page: self.page });
        true
    }

    /// The next page worth rendering before anybody asks for it.
    ///
    /// **Both ways, the neighbours first.** It used to look only forward, so `↑`
    /// was always a wait and `↓` was free — which is a preview that is quick in
    /// the direction nobody notices and slow in the one they do. The order is the
    /// next page, the previous page, then one further on: the two either side are
    /// [ADR
    /// 0028](../../../docs/adr/0028-the-page-you-are-on-and-its-two-neighbours-are-never-evicted.md)'s
    /// promise and the third is what makes reading forwards stay ahead.
    ///
    /// **Only once the page on screen is finished.** Guessing while the current
    /// page is still being drafted would put the guess in front of the thing
    /// being waited for — see [`sk_preview::Want::is_urgent`].
    pub(crate) fn next_ahead(&self) -> Option<u32> {
        if !self.paged() || self.coarse || self.image.is_none() {
            return None;
        }
        self.neighbours().into_iter().find(|page| !self.held.contains_key(page))
    }

    /// The pages either side of this one, and one further on, that exist.
    ///
    /// In the order they are worth having, and never the page on screen: that one
    /// is `image` and is asked for by the ordinary path.
    pub(crate) fn neighbours(&self) -> Vec<u32> {
        let exists = |page: u32| page >= 1 && (self.pages == 0 || page <= self.pages);
        let mut out = Vec::with_capacity(3);
        for page in [self.page + 1, self.page.saturating_sub(1), self.page + AHEAD] {
            if page != self.page && exists(page) && !out.contains(&page) {
                out.push(page);
            }
        }
        out
    }

    /// The textures the renderer must not drop: this page and its neighbours.
    pub(crate) fn pinned(&self) -> Vec<ImageKey> {
        let mut keys: Vec<ImageKey> = self.image.map(|(key, _)| key).into_iter().collect();
        for page in self.neighbours() {
            if let Some((key, _)) = self.held.get(&page) {
                keys.push(*key);
            }
        }
        keys
    }

    /// Puts the page back after a child said there is no such page.
    ///
    /// **Not a refusal with a sentence.** The only way to ask for a page past the
    /// end is to press `↓` on the last one, and a panel that answered that with a
    /// message would be scolding somebody for reaching the end: the page it has
    /// stays on screen and the count is now known.
    pub(crate) fn no_such_page(&mut self, pages: u32) {
        // **Unless there is no page on screen to keep.** Saying nothing is right
        // when `↓` runs off the end of a document somebody is reading; it is a
        // blank panel with nothing to explain it when the very first request comes
        // back this way, which is the one thing an open panel must never be.
        if self.image.is_none() {
            self.refusal = Some(Refusal::NoSuchPage);
        }
        if pages > 0 {
            self.pages = pages;
            self.page = self.page.min(pages).max(1);
        } else if self.page > 1 {
            self.page -= 1;
        }
        // **And nothing is re-rendered.** The page we came back to is the one
        // already on screen and it is there at full size — `took` cleared the
        // draft when it arrived — so the shot that failed becomes the shot for it.
        // Clearing `asked` instead would send a fresh request for a page we have,
        // which is a spawn and a render for no change at all.
        self.coarse = false;
        if let Some(shot) = &mut self.asked {
            shot.page = self.page;
        }
    }

    /// The sentence to draw when there is no picture and no text.
    ///
    /// **Nothing while nothing has been refused, and never nothing after.** A
    /// refusal that reaches an open panel has to be legible even where the wire
    /// has no message for it — `NotItsKind` means the helper and
    /// `previews::shows` disagree, and a well-formed reply with no pixels means
    /// something odder still. Either way the panel says one sentence rather than
    /// looking like a program that has stopped.
    pub(crate) fn said(&self) -> Option<&'static str> {
        let refusal = self.refusal?;
        Some(refusal.message().unwrap_or("preview.unavailable"))
    }

    /// What a screen reader is given to read, or nothing.
    ///
    /// **The text and not the picture.** A preview of a photograph has nothing
    /// to say that its filename has not said already; a preview of a file's text
    /// or of a page's words is the content itself, and the whole of what was read
    /// is what a reader moves through.
    pub(crate) fn spoken_text(&self) -> Option<String> {
        if let Some(page) = self.page_text.as_ref()
            && page.count() > 0
        {
            return Some(page.every_word());
        }
        let text = self.text.as_ref()?;
        Some(text.lines.join("\n"))
    }

    /// Selects the whole file — `Ctrl+A`.
    ///
    /// The whole of what was *read*, which is a screenful and not a file: the
    /// panel says so with an ellipsis and the clipboard gets what is on screen.
    /// Copying bytes nobody has seen would be a paste nobody could predict.
    pub(crate) fn select_all(&mut self) {
        if let Some(page) = self.page_text.as_mut() {
            page.select_all();
            return;
        }
        let Some(text) = &self.text else { return };
        let Some(last) = text.lines.len().checked_sub(1) else { return };
        self.chosen = Some(Chosen {
            anchor: Spot { line: 0, byte: 0 },
            caret: Spot { line: last, byte: text.lines[last].len() },
        });
    }

    /// Selects the word at `at`, or the whole line it is on.
    ///
    /// **What a double and a triple click mean**, and the third copy of that
    /// sentence: [`crate::pagetext::Page::select_run`] says it for a page's
    /// words and [`crate::field::Field::select_run`] for one line of a field.
    /// The run is [`crate::word::around`]'s in all three, so a double click on
    /// `holiday_2024` takes all of it whichever surface it is on.
    ///
    /// **The whole *line*, not the drawn row.** A line that wrapped is still one
    /// paragraph, and a third click that took a third of one because the panel is
    /// narrow would make the gesture a function of the window's width.
    pub(crate) fn select_run(&mut self, at: Spot, whole_line: bool) {
        let Some(text) = self.text.as_ref() else { return };
        let Some(body) = text.lines.get(at.line) else { return };
        let (from, to) =
            if whole_line { (0, body.len()) } else { crate::word::around(body, at.byte) };
        self.chosen = Some(Chosen {
            anchor: Spot { line: at.line, byte: from },
            caret: Spot { line: at.line, byte: to },
        });
    }

    /// Whether there is a selection with anything in it.
    ///
    /// **Either kind of text**, because the command above this does not care
    /// which: `Ctrl+C` over a text file copies lines and over a page copies
    /// words, and both are "there is something to copy".
    pub(crate) fn has_selection(&self) -> bool {
        self.chosen.is_some_and(|chosen| !chosen.is_empty())
            || self.page_text.as_ref().is_some_and(crate::pagetext::Page::has_selection)
    }

    /// Whether there is text on screen at all — a file's, or a page's words.
    pub(crate) fn has_text(&self) -> bool {
        self.text.is_some() || self.page_text.is_some()
    }

    /// The selected text, lines joined by newlines.
    ///
    /// **`\n` and not the platform's separator**, because there is one platform
    /// and it is the one whose separator is `\n`.
    pub(crate) fn chosen_text(&self) -> Option<String> {
        if let Some(page) = self.page_text.as_ref() {
            return page.text();
        }
        let text = self.text.as_ref()?;
        let chosen = self.chosen.filter(|it| !it.is_empty())?;
        let (from, to) = chosen.ordered();
        let mut out = String::new();
        for line in from.line..=to.line.min(text.lines.len().saturating_sub(1)) {
            let body = text.lines.get(line)?;
            let start = if line == from.line { from.byte.min(body.len()) } else { 0 };
            let end = if line == to.line { to.byte.min(body.len()) } else { body.len() };
            if start > end || !body.is_char_boundary(start) || !body.is_char_boundary(end) {
                continue;
            }
            if line > from.line {
                out.push('\n');
            }
            out.push_str(&body[start..end]);
        }
        Some(out)
    }

    /// Where the caret is, while the text is being edited.
    ///
    /// **The selection's own caret**, because a click without a drag already
    /// produces an empty selection — anchor and caret at the same spot — and
    /// having two of them would be two answers to "where does the next character
    /// go". Nothing there yet means the start of the file, which is where a
    /// buffer nobody has clicked in begins.
    pub(crate) fn caret(&self) -> Spot {
        self.chosen.map_or(Spot { line: 0, byte: 0 }, |chosen| chosen.caret)
    }

    /// Puts the caret at `spot`, with nothing selected.
    pub(crate) fn put_caret(&mut self, spot: Spot) {
        self.chosen = Some(Chosen { anchor: spot, caret: spot });
    }

    /// Turns edit mode on, remembering the file as it stands.
    pub(crate) fn start_editing(&mut self) {
        let Some(text) = self.text.as_ref() else { return };
        self.as_on_disk = Some(text.body());
        self.editing = true;
        if self.chosen.is_none() {
            self.put_caret(Spot { line: 0, byte: 0 });
        }
    }

    /// Remembers what was just written as the file's own text.
    pub(crate) fn saved_as(&mut self, body: String) {
        self.as_on_disk = Some(body);
        self.unsaved = false;
    }

    /// Puts the file back the way it was and forgets the edit.
    ///
    /// **The one way out that writes nothing.** Every other way of leaving edit
    /// mode saves, which is what makes not asking safe; this is the answer to
    /// "actually, never mind", and it is why the panel draws a button for it
    /// rather than leaving `Escape` to be discovered.
    pub(crate) fn discard(&mut self) -> bool {
        let Some(body) = self.as_on_disk.clone() else { return false };
        let Some(text) = self.text.as_ref() else { return false };
        if !self.unsaved {
            return false;
        }
        let cells = text.cells();
        let mut fresh = Text::of(&body, text.language);
        fresh.wrap(cells);
        self.text = Some(fresh);
        self.unsaved = false;
        self.chosen = None;
        true
    }

    /// Types `text` in, replacing whatever is selected.
    ///
    /// Whether anything changed, which is what tells a key that was taken from
    /// one that was not.
    pub(crate) fn type_in(&mut self, text: &str) -> bool {
        if !self.editing || text.is_empty() {
            return false;
        }
        self.take_out_the_selection();
        let Some(buffer) = self.text.as_mut() else { return false };
        let at = self.chosen.map_or(Spot { line: 0, byte: 0 }, |chosen| chosen.caret);
        let landed = buffer.insert(at, text);
        self.put_caret(landed);
        self.unsaved = true;
        true
    }

    /// Backspace: the selection if there is one, else the character before the
    /// caret.
    pub(crate) fn delete_back(&mut self) -> bool {
        if !self.editing {
            return false;
        }
        if self.take_out_the_selection() {
            self.unsaved = true;
            return true;
        }
        let Some(buffer) = self.text.as_mut() else { return false };
        let to = self.chosen.map_or(Spot { line: 0, byte: 0 }, |chosen| chosen.caret);
        let from = buffer.before(to);
        if from == to {
            return false;
        }
        let landed = buffer.remove(from, to);
        self.put_caret(landed);
        self.unsaved = true;
        true
    }

    /// Delete: the selection if there is one, else the character after the caret.
    pub(crate) fn delete_forward(&mut self) -> bool {
        if !self.editing {
            return false;
        }
        if self.take_out_the_selection() {
            self.unsaved = true;
            return true;
        }
        let Some(buffer) = self.text.as_mut() else { return false };
        let from = self.chosen.map_or(Spot { line: 0, byte: 0 }, |chosen| chosen.caret);
        let to = buffer.after(from);
        if from == to {
            return false;
        }
        let landed = buffer.remove(from, to);
        self.put_caret(landed);
        self.unsaved = true;
        true
    }

    /// Removes what is selected. Whether there was anything to remove.
    fn take_out_the_selection(&mut self) -> bool {
        let Some(chosen) = self.chosen.filter(|it| !it.is_empty()) else { return false };
        let Some(buffer) = self.text.as_mut() else { return false };
        let (from, to) = chosen.ordered();
        let landed = buffer.remove(from, to);
        self.put_caret(landed);
        true
    }

    /// Moves the caret one step, optionally dragging the selection with it.
    ///
    /// **`Up` and `Down` walk drawn rows, not lines**, and `Home` and `End` are
    /// about the row too — which is what wrapping means for a caret: a paragraph
    /// that took six rows takes six presses to cross, and the alternative is a
    /// `Down` that jumps a screenful because the line it was on was long.
    ///
    /// The column is kept as a character count into the row rather than a byte
    /// offset, which is honest for a monospaced panel: every character is one
    /// cell wide until a line holds something wider than one — an emoji, or a
    /// combining mark.
    pub(crate) fn move_caret(&mut self, step: Step, extend: bool) -> bool {
        let Some(buffer) = self.text.as_ref() else { return false };
        let from = self.caret();
        let last = buffer.lines.len().saturating_sub(1);
        let bottom = buffer.rows().len().saturating_sub(1);
        let row = buffer.row_of(from);
        let spot = match step {
            Step::Back => buffer.before(from),
            Step::On => buffer.after(from),
            Step::Up if row == 0 => Spot { line: 0, byte: 0 },
            Step::Up => buffer.spot_at(row - 1, buffer.cell_of(from)),
            Step::Down if row >= bottom => {
                Spot { line: last, byte: buffer.lines.get(last).map_or(0, String::len) }
            }
            Step::Down => buffer.spot_at(row + 1, buffer.cell_of(from)),
            Step::LineStart => buffer.spot_at(row, 0),
            Step::LineEnd => buffer.spot_at(row, usize::MAX),
            Step::WordBack => buffer.word_before(from),
            Step::WordOn => buffer.word_after(from),
            Step::Start => Spot { line: 0, byte: 0 },
            Step::End => Spot { line: last, byte: buffer.lines.get(last).map_or(0, String::len) },
        };
        let anchor = if extend { self.chosen.map_or(spot, |chosen| chosen.anchor) } else { spot };
        self.chosen = Some(Chosen { anchor, caret: spot });
        true
    }

    /// What the page control says between its arrows.
    ///
    /// **`3 / 14`, or `3` alone where the count is unknown** — ADR 0019's zero.
    /// A total invented from nothing would be the one thing [rule
    /// 7](../../../docs/03-principles.md) forbids: a number on screen that
    /// nothing behind it knows.
    ///
    /// Through the catalogue, both of them, because a locale that says "3 of 14"
    /// — or puts the total first — is a locale this cannot spell with a `/`.
    pub(crate) fn page_label(&self) -> String {
        if self.pages > 0 {
            sk_model::i18n::format("preview.page_of", &[&self.page, &self.pages])
        } else {
            sk_model::i18n::format("preview.page", &[&self.page])
        }
    }

    /// The file's name, for the panel's title.
    pub(crate) fn title(&self) -> String {
        self.path
            .file_name()
            .map_or_else(|| self.path.display().to_string(), |it| it.to_string_lossy().into_owned())
    }

    /// Whether this look is of `path` at `mtime`, so a re-open is not a reload.
    pub(crate) fn is_of(&self, path: &Path, mtime: i64) -> bool {
        self.path == path && self.mtime == mtime
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Chosen, ImageKey, LINES, QuickLook, Shot, Spot, Step, Text, Tone};
    use crate::previews::Shows;
    use sk_preview::Refusal;

    /// **Every line has runs covering exactly it.** The drawing slices a line by
    /// its spans, so a span past the end would be a panic and a gap would be text
    /// nobody sees — which is what a real highlighter has to keep true.
    #[test]
    fn every_line_is_covered_by_its_spans_exactly_once() {
        let text = Text::of("one\ntwo words\n\nlast", crate::highlight::Language::Plain);
        assert_eq!(text.lines, ["one", "two words", "", "last"]);
        assert_eq!(text.spans.len(), text.lines.len());
        for (line, spans) in text.lines.iter().zip(&text.spans) {
            let mut at = 0;
            for span in spans {
                assert_eq!(span.from, at, "a gap or an overlap in {line:?}");
                assert!(span.to <= line.len(), "a span past the end of {line:?}");
                assert!(line.get(span.from..span.to).is_some(), "a span splits a character");
                at = span.to;
            }
            assert_eq!(at, line.len(), "the end of {line:?} is uncoloured");
        }
    }

    /// Today every run is plain, which is the highlighter that is not written.
    #[test]
    fn the_only_tone_is_plain_until_there_is_a_highlighter() {
        let text = Text::of("fn main() {}\n", crate::highlight::Language::Plain);
        assert!(text.spans.iter().flatten().all(|span| span.tone == Tone::Plain));
    }

    /// A tab is an indent, because the shaper gives it no advance of its own.
    #[test]
    fn a_tab_is_drawn_as_spaces_and_the_spans_still_cover_the_line() {
        let text = Text::of("\tindented\n\t\ttwice\n", crate::highlight::Language::Plain);
        assert_eq!(text.lines, ["    indented", "        twice"]);
        for (line, spans) in text.lines.iter().zip(&text.spans) {
            assert_eq!(spans.last().map(|span| span.to), Some(line.len()));
        }
    }

    #[test]
    fn a_file_with_no_newline_is_one_line_and_an_empty_one_is_none() {
        assert_eq!(
            Text::of("no terminator", crate::highlight::Language::Plain).lines,
            ["no terminator"]
        );
        assert!(Text::of("", crate::highlight::Language::Plain).lines.is_empty());
        assert!(!Text::of("", crate::highlight::Language::Plain).truncated);
    }

    /// **The stand-in stops being drawn the moment the real one lands.**
    ///
    /// Two pictures at once would be the thumbnail showing through wherever the
    /// full one has transparency — the same picture-shaped hole the thumbnail
    /// frames caught in the rows, one panel over.
    #[test]
    fn taking_a_picture_puts_the_stand_in_away_and_ends_the_coarse_pass() {
        let mut look = paged(3);
        look.standby = Some((sk_gpu::ImageKey(9), (128, 72)));
        look.coarse = true;
        look.took(sk_gpu::ImageKey(10), (800, 600), look.page);
        assert_eq!(look.standby, None, "the thumbnail is still under the picture");
        assert!(!look.coarse, "the draft is still owed after the full size arrived");
        assert_eq!(look.image, Some((sk_gpu::ImageKey(10), (800, 600))));
    }

    /// **A draft is a third of the long edge, and only for a document.**
    ///
    /// The asymmetry is the measurement, not a preference: a page costs what it
    /// draws — 61–76 ms at 400 pixels against 135–200 ms at 1200 — and an image
    /// costs its decode, 60 ms at 128 pixels and 67 ms at 1200, so a draft there
    /// would be a second full price for nothing.
    #[test]
    fn a_document_asks_for_a_draft_first_and_an_image_does_not() {
        let mut page = paged(3);
        page.coarse = true;
        assert_eq!(page.shot((1200, 900)).within, (400, 300));

        page.coarse = false;
        assert_eq!(page.shot((1200, 900)).within, (1200, 900));

        // A turn owes a draft again, which is what makes `↓` feel like `↓`.
        assert!(page.turn(1));
        assert_eq!(page.shot((1200, 900)).within, (400, 300));

        // And a tiny panel is not divided into nothing.
        page.coarse = true;
        assert_eq!(page.shot((90, 60)).within, (64, 42));
    }

    /// **A page whose texture the renderer dropped is rendered again, not shown.**
    ///
    /// The two stores are bounded separately — 24 MiB of textures against a map
    /// of page numbers — and only the renderer knows what it evicted. Without
    /// this the map handed out a dead key, the drawing emitted no quad for it,
    /// and the panel was blank with a title and a page number and no way back:
    /// `wants` was false because the shot matched what had been asked.
    #[test]
    fn a_page_the_renderer_dropped_is_asked_for_again() {
        let mut look = paged(9);
        look.coarse = false;
        look.took(sk_gpu::ImageKey(1), (848, 1200), 1);
        look.took(sk_gpu::ImageKey(2), (848, 1200), 2);
        assert_eq!(look.held.len(), 2);

        // The renderer kept page 1 and dropped page 2, which is what a big panel
        // and a 24 MiB budget produce as soon as a prefetch lands.
        look.retain_held(|key| key == sk_gpu::ImageKey(1));
        assert_eq!(look.held.len(), 1);

        // A turn onto the dropped page shows nothing and asks again, rather than
        // showing a key nothing is behind.
        assert!(look.turn(1));
        assert!(!look.show_held((800, 600)), "a dropped page was shown");
        assert!(look.wants((800, 600)), "a dropped page was not asked for again");

        // And the page on screen going away takes the picture with it, so the
        // panel asks rather than drawing a hole.
        look.page = 1;
        assert!(look.show_held((800, 600)));
        look.retain_held(|_| false);
        assert_eq!(look.image, None);
        assert!(look.wants((800, 600)));
    }

    /// **An open panel is never blank with nothing to say.**
    ///
    /// Three refusals have no sentence of their own, because they decide whether
    /// the panel *opens* rather than what it says. Reaching an open one, each has
    /// to be legible: a person cannot tell an empty panel from a program that has
    /// stopped.
    #[test]
    fn every_refusal_that_reaches_an_open_panel_says_something() {
        for refusal in [
            Refusal::None,
            Refusal::NotItsKind,
            Refusal::Unreadable,
            Refusal::Undecodable,
            Refusal::NoRenderer,
            Refusal::TooLarge,
            Refusal::NoSuchPage,
            Refusal::Interrupted,
        ] {
            let mut look = paged(0);
            look.refusal = Some(refusal);
            let said = look.said().expect("a refusal with nothing to say");
            assert_ne!(
                sk_model::i18n::lookup(said),
                said,
                "`{said}` for {refusal:?} is not in the catalogue"
            );
        }
        // And nothing is said while nothing has been refused, because the draft
        // arrives in sixty milliseconds.
        assert_eq!(paged(3).said(), None);
    }

    /// A `↓` past the end keeps the page it has — and says so where it has none.
    #[test]
    fn running_off_the_end_with_no_page_on_screen_explains_itself() {
        let mut look = paged(0);
        look.no_such_page(0);
        assert!(look.said().is_some(), "a first request that found no page said nothing");

        // With a page on screen it stays silent, because reaching the end of a
        // document is not an error to report.
        let mut reading = paged(3);
        reading.coarse = false;
        reading.took(sk_gpu::ImageKey(1), (848, 1200), 1);
        reading.no_such_page(3);
        assert_eq!(reading.said(), None);
        assert!(reading.image.is_some());
    }

    /// **A turn to a held page is instant, and a guess never precedes the wait.**
    ///
    /// The two halves of [ADR
    /// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md):
    /// what has been rendered is shown without asking, and nothing is guessed
    /// while the page on screen is still a draft — because a guess queued then
    /// would sit in front of the thing being waited for.
    #[test]
    fn a_held_page_is_shown_at_once_and_a_draft_guesses_nothing() {
        let mut look = paged(9);
        // Still drafting: nothing to guess yet, whatever else is true.
        look.coarse = true;
        assert_eq!(look.next_ahead(), None, "a draft asked for a page ahead");

        // The draft lands, then the full size — only the second is remembered.
        look.took(sk_gpu::ImageKey(1), (283, 400), 1);
        assert!(look.held.is_empty(), "a draft was remembered as the page");
        look.coarse = false;
        look.took(sk_gpu::ImageKey(2), (848, 1200), 1);
        assert_eq!(look.held.len(), 1);
        assert_eq!(look.next_ahead(), Some(2));

        // Once two are held it asks for the one after them, and stops at the end.
        look.took(sk_gpu::ImageKey(3), (848, 1200), 2);
        look.took(sk_gpu::ImageKey(4), (848, 1200), 3);
        assert_eq!(look.next_ahead(), None, "it guessed past the two it holds");

        // And a turn onto a held page needs no child at all.
        assert!(look.turn(1));
        assert!(look.show_held((800, 600)), "a held page was not shown");
        assert_eq!(look.image, Some((sk_gpu::ImageKey(3), (848, 1200))));
        assert!(!look.wants((800, 600)), "a held page was asked for again");
        assert!(!look.coarse, "a held page still owed a draft");

        // A turn onto one that is not held does want a render.
        look.page = 7;
        assert!(look.wants((800, 600)));
        assert!(!look.show_held((800, 600)));
    }

    /// A look with no child behind it, for the page arithmetic.
    fn paged(pages: u32) -> QuickLook {
        QuickLook {
            path: std::path::PathBuf::from("/a/doc.pdf"),
            mtime: 7,
            shows: Shows::Page,
            image: None,
            text: None,
            refusal: None,
            page: 1,
            pages,
            standby: None,
            held: std::collections::HashMap::new(),
            ahead: None,
            coarse: false,
            drafted: false,
            asked: Some(super::Shot { within: (800, 600), page: 1 }),
            scroll: sk_ui::Scroll::default(),
            chosen: None,
            dragging: false,
            editing: false,
            unsaved: false,
            page_text: None,
            finding: None,
            found: (0, 0),
            zoom: 1.0,
            pan: (0.0, 0.0),
            playing: true,
            delay_ms: 0,
            shown_at: None,
            as_on_disk: None,
            reading: None,
        }
    }

    /// **A draft on screen says so, and the finished render says it is not one.**
    ///
    /// The drawing scales a stand-in up to fill the box the finished page will
    /// occupy, and draws a finished one at one to one — so which of the two is on
    /// screen has to be a fact about the *image*, not about what will be asked
    /// for next. `coarse` is the second of those, and clearing it as the draft
    /// arrives is what drew a third-size page in the middle of the panel and then
    /// jumped it to full size when the render landed.
    #[test]
    fn a_draft_on_screen_is_marked_as_one_and_the_render_unmarks_it() {
        let mut look = paged(3);
        look.coarse = true;
        look.asked = Some(Shot { within: (334, 196), page: 1 });

        look.took(ImageKey(1), (139, 196), 1);
        assert!(look.drafted, "the draft is a stand-in and has to be drawn like one");
        assert!(!look.coarse, "and the next ask is the full size");
        assert!(look.held.is_empty(), "a draft is not remembered");

        look.asked = Some(Shot { within: (1004, 590), page: 1 });
        look.took(ImageKey(2), (417, 590), 1);
        assert!(!look.drafted, "the finished page is not a stand-in");
        assert_eq!(look.held.len(), 1, "and it is remembered");
    }

    /// A page taken off the held map is a finished one, whatever came before it.
    #[test]
    fn a_held_page_is_never_a_draft() {
        let mut look = paged(3);
        look.held.insert(2, (ImageKey(9), (417, 590)));
        look.coarse = true;
        look.drafted = true;
        look.page = 2;
        assert!(look.show_held((1004, 590)));
        assert!(!look.drafted, "only full renders are held");
    }

    /// A look holding `body` as its text, for the selection arithmetic.
    fn texted(body: &str) -> QuickLook {
        let mut look = paged(0);
        look.shows = Shows::Text;
        look.text = Some(Text::of(body, crate::highlight::Language::Plain));
        look
    }

    /// **What is copied is what the highlight covers**, on one line and across
    /// several, and the lines are joined by newlines and not by the platform's
    /// separator — there is one platform and its separator is a newline.
    #[test]
    fn copying_a_selection_gives_back_exactly_what_it_spans() {
        let mut look = texted("first line\nsecond line\nthird line\n");

        // Part of one line.
        look.chosen =
            Some(Chosen { anchor: Spot { line: 1, byte: 0 }, caret: Spot { line: 1, byte: 6 } });
        assert_eq!(look.chosen_text().as_deref(), Some("second"));

        // Across three, from the middle of the first to the middle of the last.
        look.chosen =
            Some(Chosen { anchor: Spot { line: 0, byte: 6 }, caret: Spot { line: 2, byte: 5 } });
        assert_eq!(look.chosen_text().as_deref(), Some("line\nsecond line\nthird"));

        // **Dragged upward is the same selection.** The ends are an anchor and a
        // caret, not a start and an end, so this is the case that a `start..end`
        // model gets wrong by producing nothing at all.
        look.chosen =
            Some(Chosen { anchor: Spot { line: 2, byte: 5 }, caret: Spot { line: 0, byte: 6 } });
        assert_eq!(look.chosen_text().as_deref(), Some("line\nsecond line\nthird"));
    }

    /// An empty selection is nothing to copy, and a click without a drag makes
    /// one — so `Ctrl+C` there must not quietly copy the whole file.
    #[test]
    fn a_click_without_a_drag_selects_nothing_and_copies_nothing() {
        let mut look = texted("one\ntwo\n");
        look.chosen =
            Some(Chosen { anchor: Spot { line: 0, byte: 2 }, caret: Spot { line: 0, byte: 2 } });
        assert!(!look.has_selection());
        assert_eq!(look.chosen_text(), None);
    }

    /// `Ctrl+A` takes the whole of what was *read* — a screenful, not a file.
    #[test]
    fn select_all_covers_every_line_that_was_read() {
        let mut look = texted("alpha\nbeta\ngamma");
        look.select_all();
        assert!(look.has_selection());
        assert_eq!(look.chosen_text().as_deref(), Some("alpha\nbeta\ngamma"));

        // And a preview of nothing has nothing to select, rather than a selection
        // of one empty line.
        let mut empty = texted("");
        empty.select_all();
        assert!(!empty.has_selection());
    }

    /// A double click takes the word under it and a triple takes the line.
    #[test]
    fn a_double_click_takes_a_word_and_a_triple_takes_the_line() {
        let mut look = texted("let holiday_2024 = 7;\nsecond line here\n");

        look.select_run(Spot { line: 0, byte: 8 }, false);
        assert_eq!(look.chosen_text().as_deref(), Some("holiday_2024"));

        // The whole line, from wherever on it the third click landed.
        look.select_run(Spot { line: 0, byte: 8 }, true);
        assert_eq!(look.chosen_text().as_deref(), Some("let holiday_2024 = 7;"));
        look.select_run(Spot { line: 1, byte: 0 }, true);
        assert_eq!(look.chosen_text().as_deref(), Some("second line here"));
    }

    /// **A wrapped line is one paragraph.** The third click takes the line the
    /// file has, not the row the panel drew — otherwise the gesture would take a
    /// different amount of text in a narrow window than in a wide one.
    #[test]
    fn a_triple_click_takes_the_whole_line_a_wrap_notwithstanding() {
        let mut look = texted("a much longer line than the panel is wide\nnext\n");
        look.text.as_mut().expect("text").wrap(10);
        assert!(look.text.as_ref().expect("text").rows().len() > 1, "the line did wrap");
        look.select_run(Spot { line: 0, byte: 30 }, true);
        assert_eq!(
            look.chosen_text().as_deref(),
            Some("a much longer line than the panel is wide")
        );
    }

    /// A click on a line that is not there selects nothing rather than panicking
    /// — the case a page turn produces, one line below.
    #[test]
    fn a_run_on_a_line_that_is_gone_is_no_selection() {
        let mut look = texted("one\n");
        look.select_run(Spot { line: 9, byte: 0 }, false);
        assert!(!look.has_selection());
    }

    /// A selection that outlives the text it was made in copies nothing rather
    /// than panicking on a slice.
    ///
    /// **The case is a page turn.** `←` replaces the whole look, but a `Text` that
    /// arrives late on a look whose selection was set by `Ctrl+A` on the previous
    /// one would index past the end — and a byte range into a `String` is a panic,
    /// not a wrong answer.
    #[test]
    fn a_selection_past_the_end_of_the_text_copies_what_is_there() {
        let mut look = texted("short\n");
        look.chosen =
            Some(Chosen { anchor: Spot { line: 0, byte: 0 }, caret: Spot { line: 40, byte: 900 } });
        assert_eq!(look.chosen_text().as_deref(), Some("short"));
    }

    /// **`↓` stops at the end and `↑` at the beginning**, where the count is
    /// known — ADR 0019's clamp.
    #[test]
    fn a_page_turn_stops_at_both_ends_of_a_known_count() {
        let mut look = paged(3);
        assert!(look.turn(1) && look.page == 2);
        assert!(look.turn(1) && look.page == 3);
        assert!(!look.turn(1), "a turn past the last page reported a move");
        assert_eq!(look.page, 3);
        assert!(look.turn(-1) && look.page == 2);
        assert!(look.turn(-1) && look.page == 1);
        assert!(!look.turn(-1), "a turn before the first page reported a move");
        assert_eq!(look.page, 1);
    }

    /// **An unknown count does not clamp**, because the only way to find the end
    /// is to ask past it — a machine with a renderer but no `pdfinfo`.
    #[test]
    fn an_unknown_count_lets_a_turn_ask_past_the_end() {
        let mut look = paged(0);
        // A page on screen, which is the state reaching the end is reached *from*:
        // `↓` past the last page is somebody reading, not a first request.
        look.coarse = false;
        look.took(sk_gpu::ImageKey(1), (848, 1200), 1);
        assert!(look.turn(1) && look.page == 2);
        assert!(look.turn(1) && look.page == 3);

        // And the child's answer settles it: the count arrives, the page comes
        // back to the last real one, and the shot follows so the page already on
        // screen is not rendered a second time.
        look.no_such_page(2);
        assert_eq!((look.page, look.pages), (2, 2));
        assert!(!look.wants((800, 600)), "the page on screen was asked for again");
        assert!(look.refusal.is_none(), "reaching the end left a message on screen");
    }

    /// Where the child cannot say how long the document is either, the step back
    /// is one page and the count stays unknown.
    #[test]
    fn no_such_page_without_a_count_steps_back_one() {
        let mut look = paged(0);
        look.turn(1);
        look.no_such_page(0);
        assert_eq!((look.page, look.pages), (1, 0));
    }

    /// A resize asks again, and so does a page turn — one field, one comparison.
    #[test]
    fn a_resize_and_a_page_turn_are_both_a_new_question() {
        let mut look = paged(9);
        assert!(!look.wants((800, 600)), "nothing changed and it asked anyway");
        assert!(look.wants((900, 600)), "a resize did not ask again");
        look.turn(1);
        assert!(look.wants((800, 600)), "a page turn did not ask again");
    }

    /// What the control says between its arrows, in both shapes.
    #[test]
    fn the_page_label_says_the_total_only_when_there_is_one() {
        let mut look = paged(14);
        look.turn(2);
        assert_eq!(look.page_label(), "3 / 14");
        let unknown = paged(0);
        assert_eq!(unknown.page_label(), "1");
    }

    /// **Every refusal that has a sentence has one in the catalogue.**
    ///
    /// The keys are returned from `Refusal::message` as bare literals rather than
    /// asked for through `lookup`, so `check-invariants.py` — which matches the
    /// call and not the constant — cannot see them. A typo in one would draw
    /// `preview.no_renderer` in the panel, which is the exact failure that gate
    /// exists to stop, one indirection out of its reach.
    ///
    /// The `match` is exhaustive so a new refusal cannot be added without
    /// deciding here whether it has a sentence.
    #[test]
    fn every_refusal_with_a_message_has_one_in_the_catalogue() {
        for refusal in [
            Refusal::None,
            Refusal::NotItsKind,
            Refusal::Unreadable,
            Refusal::Undecodable,
            Refusal::NoRenderer,
            Refusal::TooLarge,
            Refusal::NoSuchPage,
            Refusal::Interrupted,
        ] {
            let wanted = match refusal {
                Refusal::None | Refusal::NotItsKind | Refusal::NoSuchPage => false,
                Refusal::Unreadable
                | Refusal::Undecodable
                | Refusal::NoRenderer
                | Refusal::TooLarge
                | Refusal::Interrupted => true,
            };
            let Some(key) = refusal.message() else {
                assert!(!wanted, "{refusal:?} lost its sentence");
                continue;
            };
            assert!(wanted, "{refusal:?} gained a sentence without one being wanted");
            assert_ne!(
                sk_model::i18n::lookup(key),
                key,
                "`{key}` is not in the catalogue, so the panel would draw the key"
            );
        }
    }

    /// A long file stops at the panel's worth and says it did.
    #[test]
    fn a_long_file_is_cut_and_says_so() {
        let body = "line\n".repeat(LINES + 50);
        let text = Text::of(&body, crate::highlight::Language::Plain);
        assert_eq!(text.lines.len(), LINES);
        assert!(text.truncated, "a file that was cut did not say so");
        // And one that fits does not claim to have been.
        assert!(!Text::of("short\n", crate::highlight::Language::Plain).truncated);
    }

    /// Typing, deleting and the caret's arithmetic, over the text a preview
    /// holds.
    ///
    /// One test rather than six, because the interesting cases are the seams
    /// between them: a newline typed in the middle of a line, a backspace at the
    /// start of one, and a selection replaced by a character.
    #[test]
    fn the_editor_types_deletes_and_joins_lines() {
        let mut text = super::Text::of("one\ntwo\nthree\n", crate::highlight::Language::Plain);
        assert_eq!(text.lines, ["one", "two", "three"]);

        // A character in the middle of a line.
        let at = text.insert(Spot { line: 1, byte: 1 }, "X");
        assert_eq!(at, Spot { line: 1, byte: 2 });
        assert_eq!(text.lines, ["one", "tXwo", "three"]);

        // A newline splits it, and the caret lands at the start of the new line.
        let at = text.insert(Spot { line: 1, byte: 2 }, "\n");
        assert_eq!(at, Spot { line: 2, byte: 0 });
        assert_eq!(text.lines, ["one", "tX", "wo", "three"]);

        // Backspace at the start of a line joins it to the one above, and the
        // caret lands where the join happened.
        let from = text.before(Spot { line: 2, byte: 0 });
        assert_eq!(from, Spot { line: 1, byte: 2 });
        let at = text.remove(from, Spot { line: 2, byte: 0 });
        assert_eq!(at, Spot { line: 1, byte: 2 });
        assert_eq!(text.lines, ["one", "tXwo", "three"]);

        // A range across two lines goes in one piece.
        let at = text.remove(Spot { line: 0, byte: 1 }, Spot { line: 2, byte: 2 });
        assert_eq!(at, Spot { line: 0, byte: 1 });
        assert_eq!(text.lines, ["oree"]);

        // And the spans keep up with the lines, or the drawing reads past the
        // end of one.
        assert_eq!(text.spans.len(), text.lines.len());
        assert_eq!(text.spans[0].first().map(|span| span.to), Some(4));
        assert_eq!(text.body(), "oree\n");
    }

    /// A character is a character, not a byte.
    ///
    /// The caret walks by `char_indices`, so a backspace after `é` takes the
    /// whole letter and not the second half of it — which would leave a string
    /// that is no longer UTF-8 and panic the next time anything sliced it.
    #[test]
    fn the_caret_walks_characters_and_not_bytes() {
        let mut text = super::Text::of("café\n", crate::highlight::Language::Plain);
        let end = Spot { line: 0, byte: text.lines[0].len() };
        let back = text.before(end);
        assert_eq!(back.byte, end.byte - 2, "é is two bytes");
        let at = text.remove(back, end);
        assert_eq!(text.lines, ["caf"]);
        assert_eq!(at, Spot { line: 0, byte: 3 });

        // And past both ends it stands still rather than wrapping or panicking.
        let start = Spot { line: 0, byte: 0 };
        assert_eq!(text.before(start), start);
        let last = Spot { line: 0, byte: 3 };
        assert_eq!(text.after(last), last);
    }

    /// An empty file can be typed into, which is the one buffer with no line to
    /// put a character on.
    #[test]
    fn an_empty_file_takes_the_first_character() {
        let mut text = super::Text::of("", crate::highlight::Language::Plain);
        assert!(text.lines.is_empty());
        let at = text.insert(Spot { line: 0, byte: 0 }, "hello");
        assert_eq!(at, Spot { line: 0, byte: 5 });
        assert_eq!(text.lines, ["hello"]);
        assert_eq!(text.body(), "hello\n");
    }

    /// A long line is cut into rows, at spaces, and every row is a slice of it.
    ///
    /// The defect: the panel elided a long line with `…` and there was no way to
    /// see or reach what was past the right edge — a paragraph could be read only
    /// as far as the panel was wide.
    #[test]
    fn a_long_line_wraps_at_words_and_the_rows_cover_it() {
        let mut text = Text::of(
            "the quick brown fox jumps
short
",
            crate::highlight::Language::Plain,
        );
        text.wrap(10);
        let rows: Vec<&str> =
            text.rows().iter().map(|row| &text.lines[row.line][row.from..row.to]).collect();
        assert_eq!(rows, ["the quick ", "brown fox ", "jumps", "short"]);
        // Every row of a line, joined, is that line — no character is drawn twice
        // and none is dropped.
        for (line, body) in text.lines.iter().enumerate() {
            let joined: String = text
                .rows()
                .iter()
                .filter(|row| row.line == line)
                .map(|row| &body[row.from..row.to])
                .collect();
            assert_eq!(&joined, body, "line {line}");
        }

        // A word longer than a whole row is cut, because the alternative is a row
        // wider than the panel.
        let mut long = Text::of(
            "abcdefghijklmno
",
            crate::highlight::Language::Plain,
        );
        long.wrap(6);
        let cut: Vec<&str> =
            long.rows().iter().map(|row| &long.lines[row.line][row.from..row.to]).collect();
        assert_eq!(cut, ["abcdef", "ghijkl", "mno"]);
    }

    /// `Up` and `Down` walk drawn rows, and the column is kept across a wrap.
    #[test]
    fn the_caret_walks_wrapped_rows_and_not_lines() {
        let mut look = texted(
            "the quick brown fox jumps
short
",
        );
        look.editing = true;
        look.text.as_mut().expect("text").wrap(10);
        look.put_caret(Spot { line: 0, byte: 2 });

        // Down from the first row of a wrapped line lands on its second row, at
        // the same column — not on the next line of the file.
        look.move_caret(Step::Down, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 12 });
        look.move_caret(Step::Down, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 22 });
        // And the row after the last row of that line is the next line.
        look.move_caret(Step::Down, false);
        assert_eq!(look.caret(), Spot { line: 1, byte: 2 });
        look.move_caret(Step::Up, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 22 });

        // Home and End are about the drawn row too, which is what makes them
        // usable on a paragraph.
        look.move_caret(Step::LineStart, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 20 });
        look.move_caret(Step::LineEnd, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 25 });
    }

    /// `Ctrl ←` and `Ctrl →` cross words, and `Shift` selects as they go.
    #[test]
    fn the_caret_crosses_words_and_selects_while_it_does() {
        let mut look = texted(
            "one two  three
",
        );
        look.editing = true;
        look.put_caret(Spot { line: 0, byte: 14 });
        look.move_caret(Step::WordBack, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 9 });
        look.move_caret(Step::WordBack, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 4 });

        // Extending from here selects the word, which is what `Ctrl+Shift+→` is
        // for and what could not be done at all before.
        look.move_caret(Step::WordOn, true);
        assert_eq!(look.chosen_text().as_deref(), Some("two"));
        look.move_caret(Step::WordOn, true);
        assert_eq!(look.chosen_text().as_deref(), Some("two  three"));

        // The ends of the file, from anywhere in it.
        look.move_caret(Step::Start, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 0 });
        look.move_caret(Step::End, false);
        assert_eq!(look.caret(), Spot { line: 0, byte: 14 });
    }

    /// Discard puts back what is on disk, and only while something is unwritten.
    #[test]
    fn a_discard_puts_back_the_file_and_a_saved_one_has_nothing_to_put() {
        let mut look = texted(
            "one
two
",
        );
        look.start_editing();
        assert!(!look.discard(), "there was nothing to discard yet");

        look.put_caret(Spot { line: 0, byte: 3 });
        assert!(look.type_in("X"));
        assert!(look.unsaved);
        assert_eq!(
            look.text.as_ref().expect("text").body(),
            "oneX
two
"
        );

        assert!(look.discard());
        assert!(!look.unsaved);
        assert_eq!(
            look.text.as_ref().expect("text").body(),
            "one
two
"
        );

        // What a *save* leaves behind is the new text, so a discard after it puts
        // back what was last written rather than what was first opened.
        assert!(look.type_in("Y"));
        let after = look.text.as_ref().expect("text").body();
        look.saved_as(after.clone());
        assert!(!look.discard());
        assert!(look.type_in("Z"));
        assert!(look.discard());
        assert_eq!(look.text.as_ref().expect("text").body(), after);
    }
}
