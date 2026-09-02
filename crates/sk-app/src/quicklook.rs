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
/// **One variant, and the rest of this enum is the seam.** Syntax highlighting is
/// not written; what is written is everything around it, so adding it is a real
/// [`highlight`] and more variants here — not a change to how text is stored,
/// drawn, or scrolled. Each variant will name a *role* rather than a colour, so a
/// theme decides what a keyword looks like and a light theme is not a second
/// table of hexadecimal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tone {
    /// Ordinary text, in the body colour.
    Plain,
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

/// A file's text, cut into lines and each line into tones.
pub(crate) struct Text {
    /// The lines, without their terminators.
    pub(crate) lines: Vec<String>,
    /// One run list per line, the same length as `lines`.
    pub(crate) spans: Vec<Vec<Span>>,
    /// Whether the file went on past what was read.
    pub(crate) truncated: bool,
}

/// Cuts each line into tones.
///
/// **One span per line today, and that is the whole of the highlighter.** It runs
/// once, when the text lands, rather than per frame — highlighting is a pass over a
/// file and not a property of drawing it, and a version that ran while drawing
/// would be a syntax parser inside the 8 ms budget.
///
/// What a real one replaces is this function and this function only. The drawing
/// already asks for runs and colours each by its tone, so a highlighter that
/// returns three spans where this returns one needs no other change.
fn highlight(lines: &[String]) -> Vec<Vec<Span>> {
    lines.iter().map(|line| vec![Span { from: 0, to: line.len(), tone: Tone::Plain }]).collect()
}

impl Text {
    /// Cuts `body` into lines and tones.
    fn of(body: &str) -> Self {
        let mut lines: Vec<String> =
            body.lines().take(LINES).map(|line| line.replace('\t', TAB)).collect();
        let truncated = body.lines().nth(LINES).is_some() || body.len() >= TEXT_LIMIT;
        // A file with no newline at all is one line, and an empty file is none —
        // which draws as an empty panel rather than as a panel with one blank row.
        if lines.is_empty() && !body.is_empty() {
            lines.push(body.replace('\t', TAB));
        }
        let spans = highlight(&lines);
        Self { lines, spans, truncated }
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
            text: None,
            refusal: None,
            page: 1,
            pages: 0,
            asked: None,
            scroll: sk_ui::Scroll::default(),
            chosen: None,
            dragging: false,
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
            Some(body) => self.text = Some(Text::of(&body)),
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
        if self.shows != Shows::Page {
            return false;
        }
        let wanted = self.page.saturating_add_signed(delta).max(1);
        let wanted = if self.pages > 0 { wanted.min(self.pages) } else { wanted };
        let moved = wanted != self.page;
        self.page = wanted;
        // **A turn is a draft first, for the same reason opening one is.** `↓`
        // twice in a row is a person looking for something, and two 65 ms drafts
        // answer that better than one 150 ms page.
        self.coarse |= moved;
        moved
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
        if !self.coarse && self.shows == Shows::Page {
            self.held.insert(page, (key, size));
        }
        if page == self.page {
            self.image = Some((key, size));
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
        self.standby = None;
        self.coarse = false;
        self.asked = Some(Shot { within, page: self.page });
        true
    }

    /// The next page worth rendering before anybody asks for it.
    ///
    /// **Two ahead, and only once the page on screen is finished.** Guessing while
    /// the current page is still being drafted would put the guess in front of the
    /// thing being waited for — see [`sk_preview::Want::is_urgent`].
    pub(crate) fn next_ahead(&self) -> Option<u32> {
        if self.shows != Shows::Page || self.coarse || self.image.is_none() {
            return None;
        }
        (1..=AHEAD)
            .map(|step| self.page + step)
            .find(|page| (self.pages == 0 || *page <= self.pages) && !self.held.contains_key(page))
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

    /// Selects the whole file — `Ctrl+A`.
    ///
    /// The whole of what was *read*, which is a screenful and not a file: the
    /// panel says so with an ellipsis and the clipboard gets what is on screen.
    /// Copying bytes nobody has seen would be a paste nobody could predict.
    pub(crate) fn select_all(&mut self) {
        let Some(text) = &self.text else { return };
        let Some(last) = text.lines.len().checked_sub(1) else { return };
        self.chosen = Some(Chosen {
            anchor: Spot { line: 0, byte: 0 },
            caret: Spot { line: last, byte: text.lines[last].len() },
        });
    }

    /// Whether there is a selection with anything in it.
    pub(crate) fn has_selection(&self) -> bool {
        self.chosen.is_some_and(|chosen| !chosen.is_empty())
    }

    /// The selected text, lines joined by newlines.
    ///
    /// **`\n` and not the platform's separator**, because there is one platform
    /// and it is the one whose separator is `\n`.
    pub(crate) fn chosen_text(&self) -> Option<String> {
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

    use super::{Chosen, LINES, QuickLook, Spot, Text, Tone};
    use crate::previews::Shows;
    use sk_preview::Refusal;

    /// **Every line has runs covering exactly it.** The drawing slices a line by
    /// its spans, so a span past the end would be a panic and a gap would be text
    /// nobody sees — which is what a real highlighter has to keep true.
    #[test]
    fn every_line_is_covered_by_its_spans_exactly_once() {
        let text = Text::of("one\ntwo words\n\nlast");
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
        let text = Text::of("fn main() {}\n");
        assert!(text.spans.iter().flatten().all(|span| span.tone == Tone::Plain));
    }

    /// A tab is an indent, because the shaper gives it no advance of its own.
    #[test]
    fn a_tab_is_drawn_as_spaces_and_the_spans_still_cover_the_line() {
        let text = Text::of("\tindented\n\t\ttwice\n");
        assert_eq!(text.lines, ["    indented", "        twice"]);
        for (line, spans) in text.lines.iter().zip(&text.spans) {
            assert_eq!(spans.last().map(|span| span.to), Some(line.len()));
        }
    }

    #[test]
    fn a_file_with_no_newline_is_one_line_and_an_empty_one_is_none() {
        assert_eq!(Text::of("no terminator").lines, ["no terminator"]);
        assert!(Text::of("").lines.is_empty());
        assert!(!Text::of("").truncated);
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
            asked: Some(super::Shot { within: (800, 600), page: 1 }),
            scroll: sk_ui::Scroll::default(),
            chosen: None,
            dragging: false,
            reading: None,
        }
    }

    /// A look holding `body` as its text, for the selection arithmetic.
    fn texted(body: &str) -> QuickLook {
        let mut look = paged(0);
        look.shows = Shows::Text;
        look.text = Some(Text::of(body));
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
        let text = Text::of(&body);
        assert_eq!(text.lines.len(), LINES);
        assert!(text.truncated, "a file that was cut did not say so");
        // And one that fits does not claim to have been.
        assert!(!Text::of("short\n").truncated);
    }
}
