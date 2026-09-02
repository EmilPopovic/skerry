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
            text: None,
            refusal: None,
            page: 1,
            pages: 0,
            asked: None,
            scroll: sk_ui::Scroll::default(),
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
    pub(crate) const fn shot(&self, within: (u32, u32)) -> Shot {
        Shot { within, page: self.page }
    }

    /// Whether this look answers to `key` — the file, when it changed, the box
    /// it was asked at and the page.
    ///
    /// **All four, because a reply can be stale in four ways.** Two `↓` presses
    /// put two requests in one batch, and a page-3 reply arriving after the panel
    /// has moved to page 4 must not be drawn as page 4.
    pub(crate) fn answers_to(&self, key: &sk_preview::Key) -> bool {
        self.is_of(&key.path, key.mtime)
            && self.asked == Some(Shot { within: key.within, page: key.page })
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
        moved
    }

    /// Puts the page back after a child said there is no such page.
    ///
    /// **Not a refusal with a sentence.** The only way to ask for a page past the
    /// end is to press `↓` on the last one, and a panel that answered that with a
    /// message would be scolding somebody for reaching the end: the page it has
    /// stays on screen and the count is now known.
    pub(crate) fn no_such_page(&mut self, pages: u32) {
        if pages > 0 {
            self.pages = pages;
            self.page = self.page.min(pages).max(1);
        } else if self.page > 1 {
            self.page -= 1;
        }
        // **And nothing is re-rendered.** The page we came back to is the one
        // already on screen, so the shot that failed becomes the shot for it —
        // clearing `asked` instead would send a fresh request for a page we have,
        // which is a spawn and a render for no change at all.
        if let Some(shot) = &mut self.asked {
            shot.page = self.page;
        }
    }

    /// The sentence to draw when there is no picture and no text.
    pub(crate) fn message(&self) -> Option<&'static str> {
        self.refusal.and_then(Refusal::message)
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

    use super::{LINES, QuickLook, Text, Tone};
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
            asked: Some(super::Shot { within: (800, 600), page: 1 }),
            scroll: sk_ui::Scroll::default(),
            reading: None,
        }
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
        ] {
            let wanted = match refusal {
                Refusal::None | Refusal::NotItsKind | Refusal::NoSuchPage => false,
                Refusal::Unreadable
                | Refusal::Undecodable
                | Refusal::NoRenderer
                | Refusal::TooLarge => true,
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
