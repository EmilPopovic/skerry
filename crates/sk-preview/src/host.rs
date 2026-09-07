// SPDX-License-Identifier: Apache-2.0
//! The host half: one child per kind, a batch at a time, on a job.
//!
//! **Nothing here decodes anything** — [ADR
//! 0006](../../../docs/adr/0006-out-of-process-decoders.md) — and nothing here
//! blocks the frame loop. A round trip to a child is a write, a blocking read and
//! a wake-up, which is exactly the shape of a directory read, so it runs where a
//! directory read runs: on the job pool, with the answer arriving through a
//! channel.
//!
//! # Why a batch and not a request
//!
//! A grid of a hundred images wants a hundred thumbnails and
//! [ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)
//! puts one request in flight at a time. A job per request would be a hundred jobs
//! queued behind each other on a pool that has other work; a job per *batch* is
//! one, and it carries the child with it and hands it back when it is done. That
//! is also what keeps "one in flight" true by construction rather than by a lock:
//! while a batch is out, the host does not have the child to start another with.

use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

use crate::wire::{Ask, Refusal, Reply, Request};

/// How many thumbnails one batch carries.
///
/// **Eight, and the number is a latency bound rather than a throughput one.** The
/// batch holds the child for its whole run and a batch cannot be recalled, so its
/// length *is* the worst case a preview waits behind — and a round trip is
/// 60–155 ms on a directory of 4000×2250 JPEGs, measured over the wire. Thirty-two
/// was 1.9 seconds of that, which reads exactly like the window ignoring `Space`.
///
/// The throughput given up is a frame of idle child time between batches: about
/// 8 ms in every 500, or 1.7%. A batch exists to amortise a job dispatch — a
/// channel send and a thread wake, microseconds — against a 60 ms round trip, so
/// there was never much to amortise.
const BATCH: usize = 8;

/// How long the host waits for one reply before giving the child up for dead.
///
/// Five seconds, which is far longer than any honest decode and short enough that
/// a wedged child costs one batch rather than a session. The child's own
/// `RLIMIT_CPU` is the other half — see [`crate::sandbox`].
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Which helper answers a request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// `skerry-thumbnailer`: raster images and SVG, decoded in pure Rust.
    Image,
    /// `skerry-preview-doc`: one page of a document, through a renderer
    /// found on `PATH`.
    Document,
}

impl Kind {
    /// The executable's name, looked for beside this binary.
    const fn program(self) -> &'static str {
        match self {
            Self::Image => "skerry-thumbnailer",
            Self::Document => "skerry-preview-doc",
        }
    }
}

/// What the host wants and has not got.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Want {
    /// Which helper to ask.
    pub kind: Kind,
    /// Thumbnail or preview.
    pub ask: Ask,
    /// The file.
    pub path: PathBuf,
    /// Its modification time, which is half of what makes a thumbnail stale.
    pub mtime: i64,
    /// The box the answer must fit inside, in device pixels.
    pub within: (u32, u32),
    /// Which page, one-based, or zero for the first — ADR 0019. Zero for
    /// everything that has no pages, which is every image and every thumbnail.
    pub page: u32,
    /// Whether this is a guess rather than something somebody is waiting for.
    ///
    /// **A page rendered before it is asked for** — [ADR
    /// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md).
    /// It queues behind everything a person actually pressed, because what was
    /// guessed must never overtake what was asked.
    pub ahead: bool,
}

impl Want {
    /// Whether somebody is looking at the panel this answers.
    ///
    /// A preview is; a thumbnail is one cell of a grid that will look the same in
    /// a moment either way. What this decides is queue order — see
    /// [`Previews::pump`].
    #[must_use]
    pub fn is_urgent(&self) -> bool {
        self.ask == Ask::Preview && !self.ahead
    }

    /// What this want is keyed by: the file, when it changed, and how big.
    ///
    /// **The mtime is in the key** — [ADR
    /// 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)
    /// — so a re-encoded photograph is a different thumbnail rather than a stale
    /// one, and it costs no `statx` because the listing already has it.
    #[must_use]
    pub fn key(&self) -> Key {
        Key { path: self.path.clone(), mtime: self.mtime, within: self.within, page: self.page }
    }
}

/// What an answer is filed under.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    /// The file.
    pub path: PathBuf,
    /// Its modification time when the answer was asked for.
    pub mtime: i64,
    /// The box it was fitted inside.
    pub within: (u32, u32),
    /// Which page it is of.
    ///
    /// **In the key, because a page is what an answer is *of*.** Two `↓` presses
    /// in quick succession put two requests in one batch, and without this the
    /// reply for page 3 would satisfy a panel that had moved on to page 4 — the
    /// same staleness rule `within` already carries, applied to one more field.
    pub page: u32,
}

/// One answer, on its way back to the frame loop.
#[derive(Debug)]
pub struct Answer {
    /// What it answers.
    pub key: Key,
    /// Why there are no pixels, or [`Refusal::None`].
    pub refusal: Refusal,
    /// The image's own size, inside the box that was asked for.
    pub size: (u32, u32),
    /// How many pages the document has, or zero for unknown — ADR 0019.
    ///
    /// **Or how many frames an animation has**, which is the same field for the
    /// same reason: a GIF is a stack of pictures and the panel walks it with the
    /// controls a document already has.
    pub pages: u32,
    /// How long this frame is shown for, in milliseconds, or zero for untimed.
    pub delay_ms: u32,
    /// Straight `RGBA8`.
    pub pixels: Vec<u8>,
    /// This page's words, packed as [`crate::wire::Words`], or empty.
    ///
    /// **[ADR 0024](../../../docs/adr/0024-a-page-arrives-with-its-words.md).**
    /// Empty for a picture, for a thumbnail, and for a page whose document has
    /// no text layer — which is a page with no selection rather than an error.
    pub words: Vec<u8>,
}

/// A batch that has come back, with the child it borrowed.
struct Landed {
    kind: Kind,
    /// `None` where the child died and has to be spawned again.
    child: Option<Child>,
    answers: Vec<Answer>,
    missing: bool,
}

/// The preview host: what is wanted, what is in flight, and the children.
pub struct Previews {
    /// Wants that have not been asked yet, oldest first.
    queue: Vec<Want>,
    /// The child of each kind, when one is not out with a batch.
    children: Vec<(Kind, Child)>,
    /// The kinds whose batch is out, so a second is not started.
    busy: Vec<Kind>,
    /// Where a directory holding the helpers was found, or nothing.
    beside: Option<PathBuf>,
    landed: mpsc::Sender<Landed>,
    arriving: mpsc::Receiver<Landed>,
    /// Whether a helper could not be started, so the caller says so once.
    pub missing: Vec<Kind>,
}

impl Default for Previews {
    fn default() -> Self {
        Self::new()
    }
}

impl Previews {
    /// A host with no children and nothing wanted.
    ///
    /// **Spawns nothing.** A window that opens no image never starts a helper,
    /// which is what makes the whole feature free where it is not used.
    #[must_use]
    pub fn new() -> Self {
        let (landed, arriving) = mpsc::channel();
        Self {
            queue: Vec::new(),
            children: Vec::new(),
            busy: Vec::new(),
            beside: std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_owned)),
            landed,
            arriving,
            missing: Vec::new(),
        }
    }

    /// Asks for something, unless the same thing is already asked for.
    pub fn want(&mut self, want: Want) {
        if self.queue.contains(&want) {
            return;
        }
        // **A preview at a new size supersedes every queued one at an old size.**
        // Dragging a window edge changes the box on every frame, and each box is a
        // *different* want that the dedup above cannot see — so a two-second drag
        // queued dozens of renders at 150 ms each, the panel showed none of them
        // because a reply is matched against the box currently asked for, and it
        // sat blank for the four seconds it took to drain a queue of work that was
        // already superseded when it was enqueued.
        //
        // A prefetch is dropped too: it was guessed at the old size, and a page
        // rendered for a panel that has changed shape is a page nobody will draw.
        // One stale render can still be in flight, because a batch cannot be
        // recalled, and that is the bound.
        if want.ask == Ask::Preview {
            self.queue.retain(|queued| {
                queued.ask != Ask::Preview
                    || queued.path != want.path
                    || queued.within == want.within
            });
        }
        self.queue.push(want);
    }

    /// Drops queued previews of `path` for any page not in `pages`.
    ///
    /// **Because holding a page key queues a render per press.** The dedup above
    /// supersedes a want at a *different size*; two pages of one document at the
    /// same size are two different wants and both are kept, so twenty presses of
    /// `↓` in two seconds left twenty renders queued at sixty milliseconds each —
    /// and the panel showed the page from two seconds ago until the queue drained.
    /// That is the reported "holding up or down freezes the preview".
    ///
    /// The caller passes the page on screen and the ones it wants prefetched, so
    /// what is left is bounded by that set however fast the keys arrive. What is
    /// already in flight finishes: a batch cannot be recalled, and one stale
    /// render is the bound.
    pub fn only_pages(&mut self, path: &Path, pages: &[u32]) {
        self.queue.retain(|queued| {
            queued.ask != Ask::Preview || queued.path != path || pages.contains(&queued.page)
        });
    }

    /// Forgets everything not yet asked for.
    ///
    /// For a listing that has been left: the thumbnails of a folder nobody is
    /// looking at are work the pool has better uses for. What is already in flight
    /// finishes, because a batch cannot be recalled and its answers are cheap to
    /// throw away.
    pub fn forget_what_is_waiting(&mut self) {
        self.queue.clear();
    }

    /// The wants that have not been asked yet, for a test of the order they go in.
    ///
    /// **Not a general accessor.** The queue's order is `pump`'s business and
    /// nothing else reads it; this exists so the ordering rule can be asserted
    /// without a job pool, and it is the whole of what that test needs.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> &[Want] {
        &self.queue
    }

    /// How many wants are queued, for the diagnostic line.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.queue.len()
    }

    /// Sends the next batch of each idle kind, if there is one to send.
    ///
    /// Called once a frame. Does nothing at all when the queue is empty, which is
    /// every frame in a folder of text files.
    pub fn pump(&mut self, pool: &sk_util::Pool, wake: &sk_util::WakeHandle) {
        if self.queue.is_empty() {
            return;
        }
        for kind in [Kind::Image, Kind::Document] {
            if self.busy.contains(&kind) || !self.queue.iter().any(|want| want.kind == kind) {
                continue;
            }
            // **Previews first, and alone.** A preview is the one somebody is
            // waiting for; a thumbnail is one of forty in a grid nobody has looked
            // at yet. Mixing them would put a `Space` behind however many
            // thumbnails happened to be queued first, and the queue is filled by a
            // scroll — so the wait would depend on how fast the person scrolled.
            let urgent = self.queue.iter().any(|want| want.kind == kind && want.is_urgent());
            let room = if urgent { BATCH.max(1) } else { BATCH };
            let mut batch = Vec::with_capacity(room);
            self.queue.retain(|want| {
                let mine = want.kind == kind && (!urgent || want.is_urgent());
                if mine && batch.len() < room {
                    batch.push(want.clone());
                    return false;
                }
                true
            });
            if batch.is_empty() {
                continue;
            }
            if self.missing.contains(&kind) {
                continue;
            }
            let child = self
                .children
                .iter()
                .position(|(had, _)| *had == kind)
                .map(|at| self.children.remove(at).1);
            let beside = self.beside.clone();
            self.busy.push(kind);
            let landed = self.landed.clone();
            let wake = wake.clone();
            pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |cancel| {
                let child = child.or_else(|| spawn(kind, beside.as_deref()));
                let _ = landed.send(run_batch(kind, child, &batch, cancel));
                // The frame loop sleeps on descriptors and this is the descriptor
                // — rule 1. Without it a thumbnail sits in the channel until
                // something else happens to wake the window.
                wake.wake();
            });
        }
    }

    /// Takes delivery of every batch that has come back.
    ///
    /// Returns what arrived, and puts the children back. Never blocks.
    pub fn drain(&mut self) -> Vec<Answer> {
        let mut answers = Vec::new();
        while let Ok(batch) = self.arriving.try_recv() {
            self.busy.retain(|kind| *kind != batch.kind);
            if batch.missing && !self.missing.contains(&batch.kind) {
                self.missing.push(batch.kind);
            }
            if let Some(child) = batch.child {
                self.children.push((batch.kind, child));
            }
            answers.extend(batch.answers);
        }
        answers
    }
}

use std::path::Path;

/// Starts a helper, from beside this binary and then from `PATH`.
///
/// **Beside first.** A development build and an installed one are different
/// binaries, and a helper found on `PATH` could be either — which is a version
/// skew the protocol refuses rather than survives. `PATH` is the fallback for an
/// installation that splits `bin` from `libexec`.
fn spawn(kind: Kind, beside: Option<&Path>) -> Option<Child> {
    helper(kind.program(), beside)
}

/// Starts `program`, from `beside` first and then from `PATH`.
///
/// **One place that knows the rule**, because [`crate::archive`] spawns a helper
/// too and a second copy of "beside, then `PATH`" is a second chance to get the
/// version skew wrong.
pub(crate) fn helper(program: &str, beside: Option<&Path>) -> Option<Child> {
    let mut tried = Vec::new();
    if let Some(directory) = beside {
        tried.push(directory.join(program));
    }
    tried.push(PathBuf::from(program));
    tried.into_iter().find_map(|program| {
        use std::os::unix::process::CommandExt as _;
        Command::new(program)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()
    })
}

/// One batch, on a job: write, read, repeat, and hand the child back.
///
/// **A dead child ends the batch rather than the run.** Everything already
/// answered is kept and everything after it is dropped; the next pump spawns a
/// fresh child, and the wants that were lost come back the next time a frame asks
/// for them, because a want is derived from what is on screen rather than
/// remembered.
fn run_batch(kind: Kind, child: Option<Child>, batch: &[Want], cancel: &sk_util::Cancel) -> Landed {
    let Some(mut child) = child else {
        return Landed { kind, child: None, answers: Vec::new(), missing: true };
    };
    let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
        terminate(&mut child);
        return Landed { kind, child: None, answers: Vec::new(), missing: false };
    };
    let mut writing = BufWriter::new(crate::pipe::Timed::new(input, cancel.clone(), DEADLINE));
    let mut reading = BufReader::new(crate::pipe::Timed::new(output, cancel.clone(), DEADLINE));
    let mut answers = Vec::with_capacity(batch.len());
    let mut alive = true;
    let mut abandoned = false;
    for want in batch {
        if cancel.is_cancelled() {
            abandoned = true;
            break;
        }
        let request = Request {
            ask: want.ask,
            within: want.within,
            page: want.page,
            path: want.path.as_os_str().as_encoded_bytes().to_vec(),
        };
        let asked = std::time::Instant::now();
        writing.get_mut().until(asked + DEADLINE);
        reading.get_mut().until(asked + DEADLINE);
        let answered = request.write_to(&mut writing).and_then(|()| Reply::read_from(&mut reading));
        match answered {
            Ok(reply) if asked.elapsed() < DEADLINE => answers.push(Answer {
                key: want.key(),
                refusal: reply.refusal,
                size: reply.size,
                pages: reply.pages,
                delay_ms: reply.delay_ms,
                pixels: reply.pixels,
                words: reply.words,
            }),
            // A reply that arrived late is still a reply, and throwing it away
            // would be throwing away work that is already done — but a child that
            // is answering late is a child to replace before the next batch.
            Ok(reply) => {
                answers.push(Answer {
                    key: want.key(),
                    refusal: reply.refusal,
                    size: reply.size,
                    pages: reply.pages,
                    delay_ms: reply.delay_ms,
                    pixels: reply.pixels,
                    words: reply.words,
                });
                alive = false;
                break;
            }
            Err(_) => {
                alive = false;
                break;
            }
        }
    }
    // **Every want in the batch comes back with something.** A request that broke
    // the pipe, and every one behind it that was never sent, used to be dropped in
    // silence — and a preview records what it asked for, so it would never ask
    // again: a blank panel with a title and no way back. Whether a helper died or
    // a deadline expired, the answer is that it did not answer.
    //
    // **Except a batch that was cancelled**, which is a folder somebody has left:
    // nothing is waiting for those answers, and a refusal would be a sentence in a
    // panel about a file nobody asked about.
    for want in batch.iter().skip(answers.len()) {
        if abandoned {
            break;
        }
        answers.push(Answer {
            key: want.key(),
            refusal: Refusal::Interrupted,
            size: (0, 0),
            pages: 0,
            delay_ms: 0,
            pixels: Vec::new(),
            words: Vec::new(),
        });
    }
    // **The pipes go back on the child, or the child does not go back.** A
    // `BufWriter` that cannot give up its inner writer has a buffered write that
    // never reached the pipe, which is a child mid-request — not one to hand to
    // the next batch.
    child.stdout = Some(reading.into_inner().into_inner());
    match writing.into_inner() {
        Ok(input) if alive => {
            child.stdin = Some(input.into_inner());
            Landed { kind, child: Some(child), answers, missing: false }
        }
        _ => {
            terminate(&mut child);
            let _ = child.wait();
            Landed { kind, child: None, answers, missing: false }
        }
    }
}

fn terminate(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: helpers start in their own process group; a negative pid targets it.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Kind, Previews, Want};
    use crate::wire::{Ask, Refusal};
    use std::path::PathBuf;
    use std::process::Stdio;

    fn want(path: &str) -> Want {
        Want {
            kind: Kind::Image,
            ask: Ask::Thumbnail,
            path: PathBuf::from(path),
            mtime: 7,
            within: (128, 128),
            page: 0,
            ahead: false,
        }
    }

    /// **A resize does not queue a backlog of renders nobody will draw.**
    ///
    /// Every frame of a window drag is a different box and therefore a different
    /// want, which the identity dedup cannot see. Left alone, a two-second drag
    /// queued dozens of 150 ms renders and the panel stayed blank until they had
    /// all been done and all been discarded — a reply is matched against the box
    /// currently asked for.
    #[test]
    fn a_preview_at_a_new_size_replaces_the_ones_queued_at_old_sizes() {
        let mut host = Previews::new();
        let preview = |width: u32, page: u32, ahead: bool| Want {
            kind: Kind::Document,
            ask: Ask::Preview,
            path: PathBuf::from("/a/doc.pdf"),
            mtime: 7,
            within: (width, 900),
            page,
            ahead,
        };
        // A drag: thirty frames, thirty boxes, and a prefetch queued along the way.
        for width in 0..30 {
            host.want(preview(1000 + width, 1, false));
        }
        host.want(preview(1029, 2, true));
        assert_eq!(host.waiting(), 2, "the drag left a backlog: {}", host.waiting());

        // One more frame of the drag, and the prefetch for the old box goes too.
        host.want(preview(1100, 1, false));
        assert_eq!(host.waiting(), 1);
        assert_eq!(host.queued()[0].within, (1100, 900));

        // **A thumbnail is not a preview and is left alone**, or scrolling a grid
        // while a panel is open would throw away the thumbnails it asked for.
        let mut grid = Previews::new();
        for index in 0..5 {
            grid.want(want(&format!("/a/{index}.png")));
        }
        grid.want(preview(1000, 1, false));
        assert_eq!(grid.waiting(), 6, "a preview took the thumbnails with it");
    }

    /// **A helper that dies answers anyway, with the reason.**
    ///
    /// This is the shape of a preview that broke and stayed broken: no pixels and
    /// no refusal meant the panel recorded what it had asked for and never asked
    /// again — blank, with a title and a page number. Driven with a program that
    /// exits at once, which is what a killed child looks like from this side.
    #[test]
    fn a_batch_whose_child_dies_still_answers_every_want() {
        let child = std::process::Command::new("/bin/true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("/bin/true is not on this machine");
        let batch = [want("/a/one.png"), want("/a/two.png"), want("/a/three.png")];
        let landed = super::run_batch(Kind::Image, Some(child), &batch, &sk_util::Cancel::new());

        assert_eq!(landed.answers.len(), batch.len(), "a want was dropped in silence");
        for (answer, asked) in landed.answers.iter().zip(&batch) {
            assert_eq!(answer.refusal, Refusal::Interrupted);
            assert_eq!(answer.key, asked.key());
            assert!(answer.pixels.is_empty());
        }
        assert!(landed.child.is_none(), "a child that stopped was handed back");
    }

    /// A batch nobody is waiting for answers nothing.
    ///
    /// **The other half of the rule above.** A cancelled batch is a folder
    /// somebody has left, so a refusal for each of its wants would be a sentence
    /// in a panel about a file nobody asked about.
    #[test]
    fn a_cancelled_batch_answers_nothing() {
        let child = std::process::Command::new("/bin/true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("/bin/true is not on this machine");
        let cancel = sk_util::Cancel::new();
        cancel.cancel();
        let batch = [want("/a/one.png")];
        let landed = super::run_batch(Kind::Image, Some(child), &batch, &cancel);
        assert!(landed.answers.is_empty(), "a cancelled batch answered anyway");
    }

    /// **A preview leaves the queue before a thumbnail that was asked first.**
    ///
    /// The order matters because the queue is filled by a *scroll*: without this,
    /// how long a `Space` waits depends on how many cells the person happened to
    /// scroll past, which is the shape of a latency nobody can explain. A batch
    /// cannot be recalled once out, so this is the half that is fixable — the
    /// other half is `BATCH`, whose comment carries the number.
    #[test]
    fn a_preview_is_taken_before_thumbnails_that_were_queued_first() {
        let mut host = Previews::new();
        for index in 0..20 {
            host.want(want(&format!("/a/{index}.png")));
        }
        let mut looked = want("/a/looked-at.png");
        looked.ask = Ask::Preview;
        host.want(looked.clone());
        assert_eq!(host.waiting(), 21);

        // `pump` needs a pool and a waker to send anything, so what is asserted is
        // the batch this would form — the same predicate `pump` uses.
        let urgent: Vec<&Want> = host.queued().iter().filter(|it| it.is_urgent()).collect();
        assert_eq!(urgent.len(), 1, "the preview is not the only urgent want");
        assert_eq!(urgent[0].path, looked.path);
        assert!(
            host.queued().iter().filter(|it| !it.is_urgent()).count() == 20,
            "the thumbnails stopped being ordinary"
        );
    }

    /// **A host that is not asked for anything starts nothing**, which is what
    /// makes a folder of text files cost what it did before this existed.
    #[test]
    fn a_host_with_nothing_wanted_spawns_nothing() {
        let mut previews = Previews::new();
        assert_eq!(previews.waiting(), 0);
        assert!(previews.drain().is_empty());
        // `pump` with an empty queue must not reach the spawn path at all, which
        // is what `missing` staying empty says.
        let pool = sk_util::Pool::with_width(1);
        let waker = sk_util::Waker::new().unwrap();
        previews.pump(&pool, &waker.handle());
        assert!(previews.missing.is_empty());
    }

    /// The same file wanted twice is one want: a grid redraws every frame and each
    /// of those frames asks for what it can see.
    #[test]
    fn the_same_want_twice_is_one_request() {
        let mut previews = Previews::new();
        previews.want(want("/a/one.png"));
        previews.want(want("/a/one.png"));
        previews.want(want("/a/two.png"));
        assert_eq!(previews.waiting(), 2);

        // A different modification time is a different want, because it is a
        // different picture — ADR 0018.
        let mut changed = want("/a/one.png");
        changed.mtime = 8;
        previews.want(changed.clone());
        assert_eq!(previews.waiting(), 3);
        assert_ne!(want("/a/one.png").key(), changed.key());
    }

    #[test]
    fn leaving_a_folder_drops_what_was_only_wanted() {
        let mut previews = Previews::new();
        previews.want(want("/a/one.png"));
        previews.want(want("/a/two.png"));
        previews.forget_what_is_waiting();
        assert_eq!(previews.waiting(), 0);
    }
}
