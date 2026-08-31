//! The frame loop.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sk_cmd::{Registry, Scope};
use sk_desktop::{Applications, Launcher, MimeDatabase};
use sk_gpu::{DrawList, FrameStats, Renderer};
use sk_model::{Problem, ProblemKind, SortColumn};
use sk_plat::{Button, Content, Event, Intent, Point, Selection, Size, Window, WindowConfig};
use sk_text::{Fonts, LayoutCache};
use sk_ui::{Painter, Theme};
use sk_util::{Pool, Waker};

use crate::frames::Frames;
use crate::keys;
use crate::layout::{Area, Frame, Layout};
use crate::pane::{Jobs, Message, Pane};
use crate::sizes::Measurer;
use crate::view::{self, Chrome, Diagnostics};

/// How long a type-ahead buffer stays live after the last keystroke.
///
/// One second: long enough to type `doc` deliberately, short enough that
/// coming back to the keyboard starts a fresh jump rather than extending an
/// abandoned one.
const TYPE_AHEAD_WINDOW: Duration = Duration::from_secs(1);

/// How far a press may move and still be a click rather than a drag.
///
/// Four logical pixels, a little under the double-click slop: a click is never
/// perfectly still, and a hand on a touchpad moves a pixel while the button is
/// down. Below this the title bar opens the path for typing; above it, the
/// compositor takes over and moves the window.
const DRAG_THRESHOLD: f32 = 4.0;

/// How long a message stays in the status line.
///
/// Four seconds: long enough to read a sentence, short enough that it is gone
/// before it becomes furniture.
const NOTICE_WINDOW: Duration = Duration::from_secs(4);

/// The soonest a change notification may cause another read.
///
/// The watcher already coalesces a burst into one notification every 16 ms; this
/// is the second floor, and it is about the *read* rather than the events. A
/// download writing continuously would otherwise re-read the directory sixty
/// times a second, which for two hundred thousand entries is a quarter of a
/// second of work per notification. A quarter-second floor is well under what
/// anyone notices and two orders of magnitude cheaper.
const REFRESH_FLOOR: Duration = Duration::from_millis(250);

/// The soonest the session file may be written again.
///
/// Two seconds. A scroll offset changes sixty times a second and none of those
/// changes is worth an `fsync`; two seconds is short enough that a crash loses
/// nothing anybody would notice and long enough that a wheel spinning for a
/// minute writes the file thirty times rather than three thousand.
const SESSION_FLOOR: Duration = Duration::from_secs(2);

/// A press on a selected row, before it is known whether it is a drag.
///
/// **The selection is not replaced until the release.** A plain press on a row
/// that is already selected has two meanings — "select only this one" and "pick
/// all of these up" — and which it was is not known until the pointer either
/// moves or does not. Deciding on the press is what makes dragging three files
/// drag one of them.
struct FilesDrag {
    /// Where the press was, in logical pixels.
    from: Point,
    /// What would be dragged, captured at the press.
    paths: Vec<PathBuf>,
    /// The entry pressed, for the selection this collapses to on a plain click.
    ///
    /// `None` for a drag another application started: there is no row of ours
    /// under it and nothing to collapse to.
    id: Option<sk_model::EntryId>,
    /// Set once the pointer has moved past the threshold.
    active: bool,
    /// The pane the press was in, so a drop back into it is not a move to where
    /// the files already are.
    ///
    /// `None` for a drag another application started: it came from outside, so
    /// every pane is somewhere else.
    origin: Option<crate::panes::PaneId>,
    /// Where a drop would land, and what is marked to promise it.
    landing: Landing,
    /// What the pointer has been resting on, and since when.
    ///
    /// Held still for long enough it opens: a folder row into the pane showing
    /// it, a tab into the strip holding it. That is what makes it possible to
    /// drop three folders deep, or into another tab, in a gesture that cannot let
    /// go on the way. Reset every time the target changes, so passing over a
    /// folder on the way to another one does not open it.
    resting: Option<(Spring, Instant)>,
    /// Where the pointer was when a folder last sprang open.
    ///
    /// **A spring needs a fresh aim.** Opening a folder puts a different listing
    /// under a pointer that has not moved, and whatever is now beneath it is
    /// something nobody pointed at — so without this, holding still descends one
    /// level every nine hundred milliseconds and the drop lands wherever the
    /// hand happened to stop. Measured: a file aimed at `alpha` ended up in
    /// `alpha/nested`.
    sprung_at: Option<Point>,
}

/// A permanent deletion waiting for an answer.
///
/// Holds the paths rather than re-reading the selection when the answer comes:
/// the question names what it named, and a selection that changed underneath —
/// a watch firing, a row arriving — must not change what gets deleted.
struct Erasing {
    paths: Vec<PathBuf>,
    on: Confirm,
}

/// Which button the keyboard is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Confirm {
    /// The safe one, and the one it starts on.
    Cancel,
    Delete,
}

/// Where a drop would land, and what the frame marks to say so.
///
/// One value rather than four fields, because all four answer "where would this
/// go", all four are recomputed by the same call, and a mark that agrees with
/// three of them is a promise the drop does not keep.
#[derive(Clone, Default, PartialEq, Eq)]
struct Landing {
    /// The directory that would take the files.
    onto: Option<PathBuf>,
    /// The folder row that directory is: which pane, and which display row in it.
    row: Option<(crate::panes::PaneId, usize)>,
    /// The tab that directory is: which leaf, and which of its tabs.
    tab: Option<(crate::panes::PaneId, usize)>,
    /// The pane whose own directory would take the drop, when the pointer is on
    /// neither. The whole list is ringed, because the target is the directory the
    /// pane is showing rather than anything drawn in it.
    into_pane: Option<crate::panes::PaneId>,
}

/// What a drag would open if the hand stopped on it.
#[derive(Clone, PartialEq, Eq)]
enum Spring {
    /// A folder row: the leaf showing it, and the directory it names.
    Into(crate::panes::PaneId, PathBuf),
    /// A tab that is not the one its leaf is showing.
    Tab(crate::panes::PaneId, usize),
}

/// Whether the pointer is still where a spring left it.
///
/// **A distance, not an equality.** A compositor re-sends a position that has
/// not changed with sub-pixel differences, so `==` on two `f32` pairs said the
/// pointer had moved when a hand had been perfectly still — and the second
/// spring fired anyway. The same four logical pixels a press uses to tell a
/// click from a drag, for the same reason: a hand on a touchpad is never
/// exactly still.
fn still_there(sprung_at: Option<Point>, position: Point) -> bool {
    sprung_at.is_some_and(|at| {
        (position.x - at.x).abs().max((position.y - at.y).abs()) <= DRAG_THRESHOLD
    })
}

/// How long a drag must rest on a folder before the pane opens it.
///
/// **Nine hundred milliseconds**, which is the long end of what other file
/// managers use — GNOME's is around a second and macOS's is adjustable from
/// about half of one. Short is the dangerous direction here: a folder that opens
/// while the pointer is merely crossing it moves the target out from under a
/// gesture already in flight, and the drop then lands somewhere nobody chose.
const SPRING: Duration = Duration::from_millis(900);

/// How far the pointer must travel before a press becomes a rubber band.
///
/// Without it, every click in empty space flickers a one-pixel band.
const BAND_SLOP: f32 = 3.0;

/// The zoom steps, as factors on the list's metrics.
///
/// A table rather than a multiplier, so every step is a size somebody chose, and
/// so that stepping out and back lands exactly where it started rather than a
/// pixel off — which is what repeated multiplication does.
const ZOOM: [f32; 7] = [0.7, 0.85, 1.0, 1.15, 1.3, 1.5, 1.75];
/// Which of them is 1.0.
pub(crate) const ZOOM_DEFAULT: usize = 2;

/// How to start.
pub struct Options {
    /// The directory to open.
    pub path: PathBuf,
    /// Whether the saved session may decide the window's shape.
    ///
    /// `false` when a path was named on the command line: `skerry /etc` is an
    /// explicit request for one directory, and restoring four tabs over it would
    /// answer a question nobody asked.
    pub restore: bool,
    /// Whether to start in the dark palette.
    ///
    /// `None` — the ordinary case — means nothing was said, so the session
    /// decides and the default decides where there is no session. `--light` is
    /// the only thing that says otherwise, and it wins over the file.
    pub dark: Option<bool>,
    /// Whether the statistics overlay is on from the first frame.
    pub stats: bool,
    /// Ask for a client-drawn window frame even where the compositor offers one.
    pub own_frame: bool,
    /// Force a scale rather than taking the display server's.
    ///
    /// `--scale`. Wayland states a scale and this is ignored there; X11 has no
    /// protocol to state one, so under a bare `Xvfb` with no resource database
    /// this is the only way to run at anything but 100% — which makes it the
    /// only way to test a fractional scale headlessly.
    pub scale_120: Option<u32>,
    /// Whether to open with the list of places showing.
    ///
    /// `None` for the same reason [`Options::dark`] is: `--no-sidebar` is an
    /// instruction and the absence of it is not.
    pub sidebar: Option<bool>,
    /// When the process started, for the cold-start measurement.
    pub started: Instant,
}

impl Drop for App {
    /// Stops every folder walk before the pool is dropped.
    ///
    /// Dropping the pool joins its workers, and a worker three minutes into a
    /// directory tree would hold the window open for all of it. `Drop::drop` runs
    /// before the fields are dropped, which is the ordering this relies on.
    fn drop(&mut self) {
        self.measurer.stop();
        self.panes.focused().stop();
    }
}

/// The platform capabilities that were found, as one line.
///
/// [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md) says
/// capabilities are probed and never compiled in. A probe nobody can see is a
/// probe nobody can debug, so the answer goes on screen under `F2`.
fn describe(window: &Window) -> String {
    let found = window.capabilities();
    [
        ("fractional", found.fractional_scale),
        ("viewport", found.viewporter),
        ("decorations", found.server_side_decorations),
        // Stated rather than inferred from the absence above: "why has this
        // window a title bar here and not there" is a question that should be
        // answerable from one line of a bug report.
        ("own-frame", window.needs_own_frame()),
        ("keyboard", found.keyboard),
        ("pointer", found.pointer),
        ("cursor-shape", found.cursor_shape),
        ("gestures", found.gestures),
        ("input-method", found.input_method),
        ("clipboard", found.clipboard),
        ("primary", found.primary_selection),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name.to_owned()))
    .chain(gpu_note(found))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Which GPU the compositor said it renders on, and what that resolved to.
///
/// The ninth probed capability, and the only one whose absence is fatal: with no
/// answer the adapter is chosen by power preference, which on a hybrid laptop is
/// the wrong GPU and a protocol error at start-up. When the probe misses, the
/// program dies before it can say anything else — so what it knew has to be on
/// the line that is printed first.
fn gpu_note(found: &sk_plat::Capabilities) -> Option<String> {
    let device = found.render_device?;
    Some(match found.render_device_ids() {
        Some((vendor, device)) => format!("compositor-gpu={vendor:04x}:{device:04x}"),
        None => format!("compositor-gpu=dev:{device}(unresolved)"),
    })
}

/// The freedesktop databases, read together on one job.
struct Desktop {
    mime: MimeDatabase,
    apps: Applications,
}

/// Starts the job that reads `globs2` and `mimeinfo.cache`.
///
/// A few thousand lines across two files, read off the loop so the first frame
/// does not wait for them and the UI thread never opens one.
fn read_desktop_databases(
    jobs: &Jobs,
    waker: &sk_util::Waker,
) -> Receiver<std::sync::Arc<Desktop>> {
    let (sender, receiver) = mpsc::channel();
    let wake = waker.handle();
    jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
        let loaded =
            std::sync::Arc::new(Desktop { mime: MimeDatabase::load(), apps: Applications::load() });
        if sender.send(loaded).is_ok() {
            wake.wake();
        }
    });
    receiver
}

/// Everything that starts before the GPU does.
///
/// The pool and the directory read start **before** the GPU on purpose: opening
/// a Vulkan instance costs 67–100 ms on this machine and 193–220 ms on the
/// NVIDIA proprietary driver, while the read costs single-digit milliseconds. So
/// overlapping them does not make the first frame sooner — it makes the first
/// frame contain the directory instead of an empty list.
struct Background {
    waker: Waker,
    jobs: Jobs,
    updates: Receiver<Message>,
    pane: Pane,
    sidebar: crate::sidebar::Sidebar,
    desktop_rx: Receiver<std::sync::Arc<Desktop>>,
    replies: Replies,
    watcher: sk_fs::Watcher,
    changes: Receiver<sk_fs::Change>,
}

impl Background {
    fn start(path: PathBuf) -> Result<Self, Problem> {
        let waker = Waker::new().map_err(|error| {
            Problem::plain(ProblemKind::OutOfResources, format!("eventfd: {error}"))
        })?;
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs { pool: std::sync::Arc::new(Pool::new()), wake: waker.handle(), sender };
        let mut pane = Pane::new(path);
        pane.reload(&jobs);

        // Open by default, which is what every file manager a person has met
        // does. Reading what goes in it is a job, started with the others.
        let sidebar = crate::sidebar::Sidebar::new(&jobs, waker.handle(), true);
        let desktop_rx = read_desktop_databases(&jobs, &waker);
        let (watch_sender, changes) = mpsc::channel();
        let watcher = sk_fs::Watcher::new(watch_sender, waker.handle()).map_err(|error| {
            Problem::plain(ProblemKind::OutOfResources, format!("inotify: {error}"))
        })?;
        watcher.watch(vec![pane.path.clone()]);

        Ok(Self {
            waker,
            jobs,
            updates,
            pane,
            sidebar,
            desktop_rx,
            replies: Replies::new(),
            watcher,
            changes,
        })
    }
}

/// The renderer and the faces, and what each of them cost to open.
struct Drawing {
    renderer: Renderer,
    fonts: Fonts,
    physical: (u32, u32),
    gpu_us: u128,
    font_us: u128,
}

/// Opens the GPU and finds the faces.
///
/// The two most expensive things in a cold start after the Vulkan loader itself,
/// which is why each is timed separately and reported on the first frame.
fn open_drawing(window: &Window) -> Result<Drawing, Problem> {
    let gpu_started = Instant::now();
    let physical = window.physical();
    // The GPU the *compositor* is on, when it and the machine disagree about
    // which that is. On a hybrid laptop a buffer from the wrong one cannot be
    // imported and the connection dies at start-up.
    let render_device = window.capabilities().render_device_ids();
    // SAFETY: the handles come from `window`, which the caller owns and declares
    // after its renderer — so the renderer is dropped first.
    let renderer = unsafe { Renderer::new(window.raw_handles(), physical, render_device) }?;
    let gpu_us = gpu_started.elapsed().as_micros();

    // Beside font discovery for the same reason: both read a file that the first
    // frame would otherwise read.
    sk_ui::format::warm_timezone();

    let font_started = Instant::now();
    let fonts = Fonts::discover()?;
    let font_us = font_started.elapsed().as_micros();
    if !fonts.missing().is_empty() {
        eprintln!("skerry: font chain is short — no {}", fonts.missing().join(", "));
    }
    Ok(Drawing { renderer, fonts, physical, gpu_us, font_us })
}

/// Prints every interface the compositor advertises, when asked.
///
/// [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md) says
/// capabilities are probed rather than compiled in; this is the raw material the
/// probe works from, and printing it is how a "why did that not work" question
/// gets an answer in one run.
fn report_globals(window: &Window) {
    if std::env::var_os("SKERRY_GLOBALS").is_none() {
        return;
    }
    for (interface, version) in &window.capabilities().advertised {
        eprintln!("skerry: global {interface} v{version}");
    }
}

/// Whether each navigation button can run, in `Tool::ALL`'s order.
///
/// From the registry, because the button and the chord are the same action and a
/// second copy of "is there anywhere to go back to" is a second copy that drifts.
fn tool_state(commands: &Registry<App>, app: &App) -> [bool; 4] {
    crate::layout::Tool::ALL.map(|tool| runnable(commands, app, tool.command()))
}

/// Whether the command behind a pointer target can run right now.
///
/// The one question behind both halves of a dimmed control: what it looks like,
/// and what the pointer says about it. Asking it twice in two places is how an
/// interface comes to disagree with itself.
fn runnable(commands: &Registry<App>, app: &App, id: &str) -> bool {
    commands.find(id).is_some_and(|command| commands.enabled(command, app))
}

/// `~` and `~/x` to a path under `$HOME`, or `None` for anything else.
///
/// Only the current user's home: `~other` is a shell feature that needs
/// `getpwnam`, and a path bar that silently treated `~other` as a relative
/// directory of that name would be worse than one that does not expand it.
fn expand_home(typed: &str) -> Option<PathBuf> {
    let rest = typed.strip_prefix('~')?;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    match rest {
        "" | "/" => Some(home),
        _ => Some(home.join(rest.strip_prefix('/')?)),
    }
}

/// Which field is claiming the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Claim {
    /// The filter over the listing.
    Filter,
    /// A name being typed over a row.
    Naming,
    /// A path being typed over the breadcrumbs.
    Path,
    /// The command palette's query.
    Palette,
    /// The subtree search's query.
    Search,
}

/// Seconds since the epoch, for the `Modified` column's sense of "today".
fn clock_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs().cast_signed())
}

/// What frame this window has to draw, and what belongs in it.
///
/// Every part of it comes from the compositor: whether it decorates, whether the
/// window has free edges, and which of the three actions it offers. Nothing here
/// is assumed, because every one of them differs between the three compositors
/// in [docs/06](../../../docs/06-portability.md).
fn frame_of(window: &Window) -> Frame {
    if !window.needs_own_frame() {
        return Frame::None;
    }
    let state = window.state();
    Frame::Own {
        edges: !(state.maximized || state.fullscreen || state.tiled),
        minimize: state.can_minimize,
        maximize: state.can_maximize,
    }
}

/// Gathers the numbers the diagnostics overlay and the status line read.
///
/// Split out of `render` only because it is a struct literal with ten borrowed
/// fields and `render` has a hundred-line limit, which is a limit worth keeping.
#[allow(clippy::too_many_arguments, reason = "a struct literal with one call site")]
fn diagnostics_for<'a>(
    frames: &'a mut Frames,
    last: FrameStats,
    renderer: &Renderer,
    cache: &LayoutCache,
    jobs: &Jobs,
    adapter: &'a str,
    backend: &'a str,
    physical: (u32, u32),
    scale_120: u32,
    capabilities: &'a str,
) -> Diagnostics<'a> {
    let (hits, misses, _) = cache.stats();
    let (finished, panics) = jobs.pool.stats();
    Diagnostics {
        frames,
        last,
        atlas: renderer.atlas_stats(),
        shaping: (cache.len(), hits, misses),
        pool: (jobs.pool.width(), finished, panics),
        adapter,
        backend,
        physical,
        scale_120,
        capabilities,
    }
}

/// What a launch job came back with.
///
/// The child comes back too, because it still has to be reaped and only the
/// frame loop owns the [`Launcher`] that does that.
struct Launched {
    notice: String,
    child: Option<std::process::Child>,
}

/// Where a pasted location went, and whether it was one.
enum Pasted {
    /// A directory to open.
    Folder(PathBuf),
    /// The clipboard held something else, or nothing.
    NotAFolder,
    /// Paths to copy or move in, and whether to move them.
    ///
    /// Read on a job like everything else that touches the clipboard: the peer
    /// writes into a pipe at its own pace, and a read on the frame loop is a
    /// frame loop waiting on another process.
    Files(Vec<PathBuf>, bool),
    /// Paths dropped by another application, and the directory to put them in.
    ///
    /// Always a copy: a peer that asked for a move removes its own originals,
    /// and doing it here would delete somebody else's files on a confirmation
    /// this program cannot get — see `sk_plat::drag`.
    Dropped(Vec<PathBuf>, PathBuf),
    /// The clipboard held nothing that names a file.
    NotFiles,
    /// Text to insert into whichever field still has the keyboard.
    Text(String),
}

/// The three channels a job answers the frame loop on.
///
/// Together because they are three of the same thing with the same lifetime: a
/// command starts a job, the job sends one message, and a `drain_` method takes
/// delivery in the next pass. Held as one field so that adding a fourth is one
/// line rather than four in three places.
struct Replies {
    paste_tx: std::sync::mpsc::Sender<Pasted>,
    paste_rx: Receiver<Pasted>,
    content_tx: std::sync::mpsc::Sender<Copied>,
    content_rx: Receiver<Copied>,
    launched_tx: std::sync::mpsc::Sender<Launched>,
    launched_rx: Receiver<Launched>,
    siblings_tx: std::sync::mpsc::Sender<Siblings>,
    siblings_rx: Receiver<Siblings>,
    facts_tx: std::sync::mpsc::Sender<sk_fs::facts::Facts>,
    facts_rx: Receiver<sk_fs::facts::Facts>,
    openers_tx: std::sync::mpsc::Sender<crate::openers::Openers>,
    openers_rx: Receiver<crate::openers::Openers>,
    /// A search report, and the query it is an answer to.
    found_tx: std::sync::mpsc::Sender<(String, sk_index::Found)>,
    found_rx: Receiver<(String, sk_index::Found)>,
    /// What a job loading or building an index has to say.
    index_tx: std::sync::mpsc::Sender<crate::indexing::Note>,
    index_rx: Receiver<crate::indexing::Note>,
}

/// What a breadcrumb separator's directory turned out to hold.
struct Siblings {
    /// Which separator asked, so a stale answer to a menu nobody is waiting for
    /// any more is dropped rather than opened.
    depth: usize,
    /// The directory that was read.
    parent: PathBuf,
    /// Where the separator was when it was pressed.
    at: f32,
    /// The subdirectories, unsorted.
    rows: Vec<String>,
}

impl Replies {
    fn new() -> Self {
        let (paste_tx, paste_rx) = mpsc::channel();
        let (content_tx, content_rx) = mpsc::channel();
        let (launched_tx, launched_rx) = mpsc::channel();
        let (siblings_tx, siblings_rx) = mpsc::channel();
        let (facts_tx, facts_rx) = mpsc::channel();
        let (found_tx, found_rx) = mpsc::channel();
        let (index_tx, index_rx) = mpsc::channel();
        let (openers_tx, openers_rx) = mpsc::channel();
        Self {
            paste_tx,
            paste_rx,
            content_tx,
            content_rx,
            launched_tx,
            launched_rx,
            siblings_tx,
            siblings_rx,
            facts_tx,
            facts_rx,
            openers_tx,
            openers_rx,
            found_tx,
            found_rx,
            index_tx,
            index_rx,
        }
    }
}

/// A file's text, once the job that read it has finished.
struct Copied {
    /// What it was read from, for the notice.
    name: String,
    /// The text, or why there is none.
    outcome: Result<String, Problem>,
}

/// The application.
///
/// **The field order is load-bearing.** `renderer` is declared before `window`
/// because Rust drops fields in declaration order, and the drawing surface holds
/// raw pointers into the compositor objects the window owns. With the window
/// first, a clean exit freed the `wl_surface` and then asked the renderer to tear
/// down a surface built on it: measured, that is a segmentation fault on the way
/// out, and it hid for as long as it did because every test killed the process
/// with a signal instead of letting it close.
pub struct App {
    renderer: Renderer,
    window: Window,
    fonts: Fonts,
    cache: LayoutCache,
    list: DrawList,
    theme: Theme,
    /// The pane tree: the splits, and the tabs inside each of them.
    ///
    /// Visible to `crate::commands`, which is where every action on it lives.
    pub(crate) panes: crate::panes::Panes,
    jobs: Jobs,
    waker: Waker,
    updates: Receiver<Message>,
    watcher: sk_fs::Watcher,
    changes: Receiver<sk_fs::Change>,
    /// The directories the watcher was last asked for: one per visible pane.
    ///
    /// A background tab is not watched. It is re-read when it is shown, and the
    /// watch budget Spike C measured is per-user and shared — so the count is
    /// bounded by the splits on screen rather than by every directory a session
    /// has ever opened.
    watched: Vec<PathBuf>,
    /// The directory the focused pane is on.
    ///
    /// Compared against the pane's path every pass rather than retargeted by
    /// each navigation command, so a new way of navigating cannot forget to do
    /// it — the same reasoning as the `Waker` that `pump` now owns.
    watch_target: PathBuf,
    /// When the last change-driven read was started.
    refreshed: Instant,
    /// Directories a change arrived for inside [`REFRESH_FLOOR`], still owed a
    /// read.
    refresh_owed: Vec<PathBuf>,
    /// When the sidebar last read the mount table, and whether it owes a read.
    ///
    /// The same floor the listing's refresh keeps, for the same reason: `snap`
    /// brings up thirty squashfs mounts at login and a card reader announces one
    /// per slot, and each delivery takes the highlight out from under the
    /// pointer.
    remounted: Instant,
    mounts_owed: bool,
    /// Whether the watcher managed to arm on the current directory.
    watching: bool,
    /// The platform capabilities found at start-up, as one line.
    capabilities: String,
    /// Folder totals, kept by path so they survive every listing.
    measurer: Measurer,
    /// The list of places down the left.
    sidebar: crate::sidebar::Sidebar,
    /// The undo stack, the operation in flight, and the name being typed.
    edits: crate::edit::Edits,

    /// The freedesktop databases, once the job that reads them has finished.
    ///
    /// `None` for the first few milliseconds. Loaded on a job because `globs2`
    /// and `mimeinfo.cache` are files, and rule 1 does not have an exception for
    /// files that are small.
    /// `Arc`, not `Box`, because a launch job borrows it: resolving a type
    /// opens the file and resolving a handler reads `.desktop` files across
    /// every XDG data directory, and neither belongs on the frame loop.
    desktop: Option<std::sync::Arc<Desktop>>,
    desktop_rx: Receiver<std::sync::Arc<Desktop>>,
    launcher: Launcher,
    /// An open asked for before the databases arrived.
    open_owed: Option<PathBuf>,
    /// Something to tell the person, shown in the status line.
    notice: Option<(String, Instant)>,
    /// What the compositor last said the window is, when the window draws its
    /// own frame and so has to know.
    window_state: sk_plat::WindowState,
    /// The window button under the pointer.
    hovered_button: Option<crate::layout::Button>,
    /// The path component under the pointer.
    hovered_crumb: Option<usize>,
    /// The last command a chord resolved to and could not run.
    refused: Option<&'static str>,
    /// The last key the window was given, as the registry sees it.
    ///
    /// **Because "nothing happened" has two causes and they look identical.** A
    /// chord that resolves to nothing and a chord that never arrived both leave
    /// no notice, no job and no frame worth looking at; `refused` covers only the
    /// third case, where a command was found and declined to run. Measured: two
    /// chords on `i` were unreachable in a window while resolving correctly in
    /// the registry's own test, and there was no way to tell which half was
    /// wrong.
    last_key: String,
    /// The breadcrumb separator under the pointer.
    hovered_separator: Option<usize>,
    /// The sibling menu a separator opened, once its directory has been read.
    crumb_menu: Option<crate::crumbs::CrumbMenu>,
    /// A separator whose directory is still being read, so a second press on it
    /// does not queue a second read.
    crumb_pending: Option<usize>,
    /// The navigation button under the pointer.
    hovered_tool: Option<crate::layout::Tool>,
    /// How far the list is zoomed, as a step on [`ZOOM`].
    ///
    /// A step rather than a factor, so `Ctrl +` and a pinch land on the same
    /// sizes and a round trip out and back is exactly where it started.
    zoom: usize,
    /// The session file as it was last written, so an unchanged one is not
    /// rewritten.
    session_text: String,
    /// When it was last considered, which is the floor's clock.
    session_at: Instant,
    /// Where the last layout put the panes, and what the pointer is on.
    ///
    /// Copies rather than lookups, for the same reason `layout` is a field: a
    /// hit test that reads one borrows a field and not the whole `App`, and
    /// every one of the hundred sites that used to read `layout.body` still
    /// reads a field.
    placement: crate::panes::Placement,
    /// The hit regions the last frame produced.
    hits: crate::view::Hits,
    /// The shape last asked of the pointer.
    ///
    /// A diagnostic: no compositor lets a client read the cursor back, so what
    /// the program asked for is the only half a test can see — and it is the half
    /// that decides whether the interface agrees with itself about what is
    /// clickable.
    cursor: sk_plat::Cursor,
    /// The keyboard hint in the corner of the path bar.
    ///
    /// Built from the registry once, because it names a chord and a hardcoded
    /// one drifts: this string said `F2 stats` for two commits after `F2` became
    /// Rename. Held rather than formatted per frame — it never changes, and the
    /// path bar draws it in every one.
    hint: String,
    /// The path being typed over the breadcrumbs, while one is.
    typing_path: Option<crate::field::Field>,
    /// The command palette, while it is open.
    palette: Option<crate::palette::Palette>,
    /// The subtree search, while it is open.
    search: Option<crate::search::Search>,
    /// The persistent index this window has loaded, and any build in flight.
    indexes: crate::indexing::Indexes,
    /// The Properties panel, while it is open.
    properties: Option<crate::properties::Properties>,
    /// The Open With list, while it is open.
    openers: Option<crate::openers::Openers>,
    /// A press on a tab that has not moved far enough to be a drag: which leaf,
    /// which tab, and where the press was.
    tab_press: Option<(crate::panes::PaneId, usize, Point)>,
    /// Whether that press has become a drag.
    tab_dragging: bool,
    /// What a second `Shift+Delete` would delete for good, and when the first
    /// one was pressed.
    ///
    /// Rule 3 says permanent deletion is "an explicit, differently-shaped
    /// action". This is the shape: the first press arms and says what it would
    /// do, the second does it, and anything else at all disarms. It expires with
    /// the notice that describes it, so the offer never outlives the sentence.
    /// The permanent deletion waiting to be confirmed, while one is.
    ///
    /// **A question, not a notice.** This used to be a sentence in the status
    /// line armed for as long as that sentence happened to stay up — so a copy
    /// finishing, or four seconds passing, silently withdrew an offer the person
    /// was still reading, and the highest-stakes action in the program was drawn
    /// in the same dim grey as a row count. It is a panel now, and it goes away
    /// when it is answered and not before.
    erasing: Option<Erasing>,
    /// Whether the paths on the clipboard were cut rather than copied.
    ///
    /// Cleared by a copy and by a paste, so a second paste after a cut copies
    /// rather than moving something that is no longer where the clipboard says.
    cutting: bool,
    /// The context menu, while one is open.
    menu: Option<crate::menu::Menu>,
    /// A menu a command asked for, and where it goes.
    ///
    /// A command's `run` is `fn(&mut App)` and the rows come from the registry,
    /// which is not part of an `App` — so the keyboard's way of opening a menu
    /// leaves the request here and [`App::handle`] builds it, which is the one
    /// place a menu is ever constructed.
    menu_wanted: Option<(Point, &'static [&'static [&'static str]])>,
    /// What the jobs a command starts answer on.
    replies: Replies,
    layout: Layout,
    frames: Frames,
    last: FrameStats,
    pending: Vec<Event>,

    adapter: String,
    backend: String,
    logical: Size,
    physical: (u32, u32),
    scale_120: u32,
    /// The scale the very first frame was drawn at.
    ///
    /// Kept only so a scenario can assert about it. `wp_fractional_scale_v1`
    /// sends its first `preferred_scale` at a moment the protocol does not pin
    /// down, and a compositor that waits for the first buffer would have this
    /// report 120 where the output is at 125% — one frame at the wrong size,
    /// which is the row docs/08-roadmap.md owes a 1×1 pre-map probe for.
    first_frame_scale_120: Option<u32>,

    dirty: bool,
    closing: bool,
    show_keys: bool,
    show_stats: bool,
    typed: String,
    /// Whether the filter field has the keyboard.
    filtering: bool,
    typed_at: Instant,
    pointer: Point,
    /// Where a rubber band started, and where it is now.
    band: Option<(Point, Point)>,
    /// A press on an already-selected row that may become a drag of files.
    files_drag: Option<FilesDrag>,
    /// Whether the display server has already reported this gesture's drop.
    ///
    /// **A drag ends twice.** The pointer's button comes up and the data device
    /// says `drop`, in an order neither protocol fixes, and both mean the same
    /// gesture. Whichever arrives first does the work; this is how the other one
    /// knows not to do it again.
    dropped: bool,
    /// The offset within the scrollbar thumb where a drag began.
    thumb_grab: Option<f32>,
    /// The field a press landed in, while the button is still down.
    ///
    /// A drag in a field selects text, and the field it started in is the field it
    /// stays in: dragging out of the box and back must not move the selection to
    /// whatever the pointer passed over.
    text_drag: Option<Claim>,
    /// Where a press on the window's own title bar landed, until it is decided.
    ///
    /// **The same press is a drag and a click**, and which one it is is not known
    /// until the pointer either moves or comes up: moving starts the compositor's
    /// window move, and coming up opens the path for typing. `xdg_toplevel.move`
    /// takes the last button serial, which is still this press's, so starting the
    /// move a few milliseconds later is the same grab.
    title_press: Option<Point>,
    /// Whether the path field was opened by the click that just happened on the
    /// title bar, so the second click of a double click can take it back.
    path_from_title: bool,
    last_frame: Instant,
    started: Instant,
    first_frame_reported: bool,
    open_time: (u128, u128, u128),
}

impl App {
    /// Opens a window, a device and a directory.
    pub fn new(options: &Options) -> Result<Self, Problem> {
        // **Read before the window is opened**, because the size it asks for is
        // in the file. One `openat` and one read of a few hundred bytes, before
        // there is a frame loop for it to be a rule 1 violation of.
        let session = options.restore.then(crate::session::load).flatten();
        let window_started = Instant::now();
        let window = Window::open(&WindowConfig {
            own_frame: options.own_frame,
            scale_120: options.scale_120,
            logical: session.as_ref().map_or(WindowConfig::default().logical, |it| it.window),
            ..WindowConfig::default()
        })?;
        let window_us = window_started.elapsed().as_micros();
        report_globals(&window);
        // The directory the command line named, read while the GPU is opening —
        // and thrown away where a session replaces it, which costs one read of a
        // directory somebody was about to look at anyway.
        let background = Background::start(options.path.clone())?;
        let drawing = open_drawing(&window)?;
        let mut app = Self::assemble(options, window, window_us, background, drawing);
        if let Some(session) = session {
            app.restore_session(&session, options);
        }
        Ok(app)
    }

    /// Puts the window back the way the file says it was.
    fn restore_session(&mut self, session: &crate::session::Session, options: &Options) {
        // What was asked for on the command line wins over what was saved: a
        // flag is an instruction for this run and the file is a memory of the
        // last one.
        let dark = options.dark.unwrap_or(session.dark);
        if dark != self.theme.dark {
            self.theme = if dark { Theme::dark() } else { Theme::light() };
        }
        self.sidebar.open = options.sidebar.unwrap_or(session.sidebar);
        self.zoom = session.zoom.min(ZOOM.len() - 1);
        self.panes = crate::panes::Panes::restore(&session.tree, &self.jobs);
        for pane in self.panes.every_mut() {
            pane.show_hidden = session.hidden;
        }
        // `apply_zoom` rebuilds the metrics from the palette just chosen and
        // relays out, which is also what puts the restored tree on screen.
        self.apply_zoom();
        self.settle_panes();
    }

    /// What the window is doing, for the session file.
    fn session(&self) -> crate::session::Session {
        crate::session::Session {
            dark: self.theme.dark,
            sidebar: self.sidebar.open,
            hidden: self.panes.focused().show_hidden,
            zoom: self.zoom,
            window: (self.logical.width as u32, self.logical.height as u32),
            tree: self.panes.describe(),
        }
    }

    /// Writes the session where [`crate::session::path`] says, if it has moved.
    ///
    /// **Compared rather than tracked.** Every state worth saving is reachable
    /// from twenty places, and a `dirty` flag each of them has to remember to set
    /// is a flag one of them will not. Rendering the file is a few dozen lines of
    /// string and one allocation per tab; comparing it is a `str` compare. The
    /// floor is what keeps that off the frames that matter — a scroll changes
    /// sixty times a second and none of those is a reason to write a file.
    fn keep_the_session(&mut self, now: bool) {
        if !now && self.session_at.elapsed() < SESSION_FLOOR {
            return;
        }
        self.session_at = Instant::now();
        let text = crate::session::render(&self.session());
        if text == self.session_text {
            return;
        }
        self.session_text.clone_from(&text);
        if now {
            // **Written here rather than on a job**, and only here: this is the
            // last thing the process does, and a job queued now races the pool's
            // shutdown. There is no frame left to block.
            crate::session::write(&text);
            return;
        }
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            crate::session::write(&text);
        });
    }

    /// Puts the window, the jobs and the renderer together into one `App`.
    ///
    /// Split from [`App::new`] because the three things it is given are acquired
    /// in an order that matters — the window first, so its socket is connected
    /// while the directory is being read — and assembling them is a different
    /// question from acquiring them.
    // Almost all of it is one struct literal with one call site, and the fields
    // are the state — splitting it would be two functions that each hold half an
    // `App` and one that cannot be read without the other.
    #[expect(clippy::too_many_lines, reason = "a struct literal with one call site")]
    fn assemble(
        options: &Options,
        window: Window,
        window_us: u128,
        background: Background,
        drawing: Drawing,
    ) -> Self {
        let Background {
            waker,
            jobs,
            updates,
            pane,
            sidebar,
            desktop_rx,
            replies,
            watcher,
            changes,
        } = background;
        let watch_target = pane.path.clone();

        let Drawing { renderer, fonts, physical, gpu_us, font_us } = drawing;
        let theme = if options.dark.unwrap_or(true) { Theme::dark() } else { Theme::light() };
        let logical = window.logical();
        let scale_120 = window.scale_120();
        let capabilities = describe(&window);
        let (measurer, edits, now) = (
            Measurer::new(waker.handle()),
            crate::edit::Edits::new(waker.handle()),
            Instant::now(),
        );
        let mut app = Self {
            adapter: renderer.adapter().to_owned(),
            backend: renderer.backend().to_owned(),
            layout: Layout::compute_with(logical, &theme.metrics, frame_of(&window), true),
            renderer,
            window,
            fonts,
            // Two generations of 4096 runs: a 4K screen at the smallest useful
            // row height holds about 2,200 rows, so one full screen of three
            // columns fits in one generation with room for a scroll.
            cache: LayoutCache::new(4096),
            list: DrawList::default(),
            theme,
            panes: crate::panes::Panes::new(pane),
            jobs,
            waker,
            updates,
            watcher,
            changes,
            watch_target,
            watched: Vec::new(),
            refreshed: Instant::now(),
            refresh_owed: Vec::new(),
            remounted: Instant::now(),
            mounts_owed: false,
            watching: true,
            capabilities,
            zoom: ZOOM_DEFAULT,
            session_text: String::new(),
            session_at: now,
            placement: crate::panes::Placement::default(),
            measurer,
            sidebar,
            edits,
            desktop: None,
            desktop_rx,
            launcher: Launcher::default(),
            open_owed: None,
            notice: None,
            window_state: sk_plat::WindowState::default(),
            hovered_button: None,
            hovered_crumb: None,
            refused: None,
            last_key: String::new(),
            hovered_separator: None,
            crumb_menu: None,
            crumb_pending: None,
            hovered_tool: None,
            hits: crate::view::Hits::default(),
            cursor: sk_plat::Cursor::Arrow,
            hint: String::new(),
            typing_path: None,
            palette: None,
            search: None,
            indexes: crate::indexing::Indexes::default(),
            properties: None,
            openers: None,
            tab_press: None,
            tab_dragging: false,
            erasing: None,
            cutting: false,
            menu: None,
            menu_wanted: None,
            replies,
            frames: Frames::new(),
            last: FrameStats::default(),
            pending: Vec::with_capacity(64),
            logical,
            physical,
            scale_120,
            first_frame_scale_120: None,
            dirty: true,
            closing: false,
            show_keys: false,
            show_stats: options.stats,
            typed: String::new(),
            filtering: false,
            typed_at: now,
            pointer: Point::default(),
            band: None,
            files_drag: None,
            dropped: false,
            thumb_grab: None,
            text_drag: None,
            title_press: None,
            path_from_title: false,
            last_frame: now,
            started: options.started,
            first_frame_reported: false,
            open_time: (window_us, gpu_us, font_us),
        };
        // The tree has to be laid out before the first frame, not at the first
        // resize: a window that opens at the size it asked for gets no resize at
        // all, and every pane would be a rectangle of zeros until one arrived.
        // `--no-sidebar` is the only thing that says so before a session does.
        app.sidebar.open = options.sidebar.unwrap_or(true);
        app.divide();
        app
    }

    /// Runs until the window closes.
    ///
    /// Returns why, if it was not the user's doing: a compositor that goes away
    /// takes the connection with it, and every later dispatch fails the same way,
    /// so there is nothing to recover to.
    pub fn run(&mut self) -> Option<String> {
        // The registry lives here rather than in `App`, so that running a command
        // can take `&mut App` without the registry being part of what is
        // borrowed. It is built once; a command's `run` is a plain function
        // pointer, so there is nothing to keep warm.
        let commands = crate::commands::registry();
        self.hint = crate::commands::hint(&commands);
        if !commands.problems().is_empty() {
            // A conflicting binding is a bug and a test fails on it, but a
            // release build that meets one should say so rather than silently
            // giving a key to whichever command registered first.
            for problem in commands.problems() {
                eprintln!("skerry: keymap: {problem}");
            }
        }
        while !self.closing && !self.window.close_requested() {
            self.window.pump(Some(&self.waker), self.timeout());
            // **The clock starts here, not at the painter.** Everything between
            // this line and the submitted frame is work the user waited for:
            // taking delivery of a 10 MB listing, writing 2,048 metadata rows,
            // scheduling folder totals. Timing only the immediate-mode pass
            // reports sub-millisecond frames on either side of the two most
            // expensive things the loop does — docs/12-measurement.md, and the
            // reason this is a parameter rather than an `Instant::now()` inside
            // `render`.
            let turn = Instant::now();
            self.drain_platform(&commands);
            self.drain_jobs();
            self.drain_changes();
            self.drain_desktop();
            self.drain_paste();
            self.drain_content();
            self.drain_launches();
            self.drain_facts();
            self.drain_openers();
            self.drain_found();
            self.drain_index();
            if self.edits.drain_progress() {
                self.dirty = true;
            }
            self.drain_edits();
            if self.sidebar.drain() {
                self.dirty = true;
            }
            self.drain_sizes();
            self.advance();
            if self.dirty {
                // Recorded before the frame rather than after: what a scenario
                // asks about is the scale this frame was drawn at.
                if self.first_frame_scale_120.is_none() {
                    self.first_frame_scale_120 = Some(self.scale_120);
                }
                self.render(&commands, turn);
                self.dirty = false;
            }
            // **After the frame, never before it.** Rendering the file is
            // cheap and comparing it is cheaper, but neither belongs between
            // the input that caused this turn and the pixels that answer it.
            self.keep_the_session(false);
        }
        self.keep_the_session(true);
        self.window.failure().map(str::to_owned)
    }

    /// How long the loop may sleep.
    ///
    /// `None` means "until something happens", which is the case that makes idle
    /// CPU zero. A pending frame is `ZERO`; an animation is also `ZERO` because
    /// `PresentMode::Fifo` does the pacing; and a live type-ahead buffer gets the
    /// remainder of its window so the overlay disappears on time.
    fn timeout(&self) -> Option<Duration> {
        if self.dirty {
            return Some(Duration::ZERO);
        }
        // A read that the floor deferred owes a wake-up, or it would wait for
        // whatever the user does next.
        let deadlines = [
            (!self.typed.is_empty())
                .then(|| TYPE_AHEAD_WINDOW.saturating_sub(self.typed_at.elapsed())),
            (!self.refresh_owed.is_empty())
                .then(|| REFRESH_FLOOR.saturating_sub(self.refreshed.elapsed())),
            self.mounts_owed.then(|| REFRESH_FLOOR.saturating_sub(self.remounted.elapsed())),
            self.notice.as_ref().map(|(_, at)| NOTICE_WINDOW.saturating_sub(at.elapsed())),
            // A drag resting on a folder owes a wake-up, or the folder opens only
            // when the pointer next moves — which is the one thing a person who
            // is holding still is not doing.
            self.spring_due(),
        ];
        deadlines.into_iter().flatten().min()
    }

    fn drain_platform(&mut self, commands: &Registry<Self>) {
        // The queue is moved out and back rather than collected, so a frame with
        // forty events does not allocate.
        let mut pending = std::mem::take(&mut self.pending);
        pending.clear();
        pending.extend(self.window.drain_events());
        for event in &pending {
            self.handle(event, commands);
        }
        self.pending = pending;
    }

    fn drain_jobs(&mut self) {
        let mut landed = false;
        {
            let Self { updates, panes, jobs, dirty, .. } = self;
            while let Ok(message) = updates.try_recv() {
                // Routed by generation, which is unique across the process: a
                // reply for a tab that has been closed matches no pane and is
                // dropped, and a reply for a background tab lands there without
                // touching the frame.
                // A background tab's listing lands without a frame; a split's
                // other pane is on screen and needs one.
                let drawn = panes.is_showing(message.generation);
                let Some(pane) = panes.by_generation_mut(message.generation) else { continue };
                if pane.apply(message, jobs) && drawn {
                    *dirty = true;
                    landed = true;
                }
            }
        }
        if landed {
            self.settle_naming();
            if self.edits.name_after_load.is_some() && !self.panes.focused().loading {
                self.name_when_it_arrives();
            }
        }
    }

    /// Applies change notifications, at most one read per [`REFRESH_FLOOR`].
    ///
    /// A notification names the directory it happened in, so a change under one
    /// split refreshes that pane and leaves the other alone — including the
    /// case where both are showing the same directory, which is two panes and
    /// one watch and has to refresh both.
    fn drain_changes(&mut self) {
        let mut owed = std::mem::take(&mut self.refresh_owed);
        let mut mounted = self.mounts_owed;
        while let Ok(change) = self.changes.try_recv() {
            match change {
                // A disk plugged in or unmounted. Only the sidebar's Volumes
                // section is affected, and re-reading a listing for it would be
                // a refresh of a directory nothing happened to.
                //
                // **Coalesced, like the listing's own refresh.** `snap` brings
                // up thirty squashfs mounts at login and a card reader announces
                // one per slot; a job each would parse `/proc/self/mountinfo`
                // thirty times and take the highlight out from under the pointer
                // on every delivery.
                sk_fs::Change::Mounts => mounted = true,
                // The kernel dropped events and cannot say for which directory,
                // so every visible listing is suspect.
                sk_fs::Change::Overflowed => owed.extend(self.watched.iter().cloned()),
                // `Vanished` re-reads too, and the read fails with a message
                // that says the directory is gone — which is the honest thing to
                // show, rather than the entries it used to have.
                sk_fs::Change::Touched(path) | sk_fs::Change::Vanished(path) => owed.push(path),
            }
        }
        if mounted {
            self.mounts_owed = self.remounted.elapsed() < REFRESH_FLOOR;
            if !self.mounts_owed {
                self.remounted = Instant::now();
                self.sidebar.reload(&self.jobs);
            }
        }
        if owed.is_empty() {
            return;
        }
        if self.refreshed.elapsed() < REFRESH_FLOOR {
            self.refresh_owed = owed;
            return;
        }
        self.refreshed = Instant::now();
        let Self { panes, jobs, dirty, .. } = self;
        for pane in panes.visible_mut() {
            if owed.contains(&pane.path) {
                pane.refresh(jobs);
                *dirty = true;
            }
        }
    }

    /// Takes delivery of folder totals.
    fn drain_sizes(&mut self) {
        if self.measurer.drain() {
            self.dirty = true;
        }
    }

    /// Applies whatever came back from a clipboard read.
    fn drain_paste(&mut self) {
        self.drain_siblings();
        let Ok(pasted) = self.replies.paste_rx.try_recv() else { return };
        match pasted {
            Pasted::Folder(path) => self.panes.focused_mut().open(path, &self.jobs),
            Pasted::NotAFolder => {
                self.notice = Some((
                    sk_model::i18n::lookup("error.clipboard_not_a_folder").to_owned(),
                    Instant::now(),
                ));
            }
            Pasted::Files(paths, moving) => {
                // Cleared here rather than where the cut was made: a paste is
                // what consumes it, and a second paste of the same clipboard has
                // to copy — the originals are not where the clipboard says any
                // more.
                self.cutting = false;
                let into = self.panes.focused().path.clone();
                self.edits.paste_all(paths, into, moving, &self.jobs);
            }
            Pasted::Dropped(paths, into) => {
                // The bytes are in hand, so the drag is over: `wl_data_offer`
                // wants `finish` *after* the read, and a source told earlier
                // stops serving before it has served.
                self.window.finish_drop();
                if !paths.is_empty() {
                    self.edits.paste_all(paths, into, false, &self.jobs);
                }
            }
            Pasted::Text(text) => {
                if let Some(field) = self.focused_field() {
                    field.insert(&text);
                }
            }
            Pasted::NotFiles => {
                self.notice = Some((
                    sk_model::i18n::lookup("error.clipboard_no_files").to_owned(),
                    Instant::now(),
                ));
            }
        }
        self.dirty = true;
    }

    /// Takes delivery of the freedesktop databases, once.
    fn drain_desktop(&mut self) {
        if let Ok(loaded) = self.desktop_rx.try_recv() {
            self.desktop = Some(loaded);
            if let Some(path) = self.open_owed.take() {
                self.launch(path);
            }
            self.dirty = true;
        }
    }

    fn advance(&mut self) {
        self.spring_open();
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        if self.panes.focused_mut().scroll.advance(elapsed) {
            self.dirty = true;
        }
        if !self.typed.is_empty() && self.typed_at.elapsed() > TYPE_AHEAD_WINDOW {
            self.typed.clear();
            self.dirty = true;
        }
        // A configure can change what the window *is* without changing its size
        // — activation, tiling, a maximise granted at the same dimensions — and
        // that changes what the frame draws. Read every pass rather than only on
        // `Resized`, which is the event that does not fire for any of them.
        let shell = self.window.state();
        if shell != self.window_state {
            self.window_state = shell;
            self.relayout();
            self.dirty = true;
        }
        if self.watch_target != self.panes.focused().path {
            // **Every field belongs to the directory it was opened in.** This is
            // the one place that notices a navigation whatever caused it — a
            // chord, a breadcrumb, a sidebar row, a pasted path — and each of
            // the four had a way of surviving one: `Ctrl+L` then `Alt ←` left
            // the path bar holding the path you had just left, and `Enter`
            // then took you back to it.
            self.abandon_fields();
            self.watch_target = self.panes.focused().path.clone();
        }
        self.retarget_watches();
        if self.watched.iter().any(|path| !self.watcher.armed(path))
            && !self.panes.focused().loading
            && self.panes.focused().problem.is_none()
        {
            // The watch is gone but the directory is not. That is what an
            // atomic replace looks like — a `git checkout` across a branch that
            // deletes and recreates it, `rsync --delete`, a `mv` over the top —
            // and `IN_IGNORED` disarms the watch for good. Without this the
            // listing refreshed exactly once and was then permanently stale,
            // saying "not watching for changes" until the user navigated away
            // and back. Rate-limited by the same floor as the read it follows.
            if self.refreshed.elapsed() >= REFRESH_FLOOR {
                self.watcher.watch(self.watched.clone());
            }
        }
        // Read every pass rather than once at arm time: the thread establishes the
        // watch asynchronously, so the answer is not known when `watch` returns.
        let armed = self.watcher.armed(&self.watch_target);
        if armed != self.watching {
            self.watching = armed;
            self.dirty = true;
        }
        self.aim_the_input_method();
        self.launcher.reap();
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > NOTICE_WINDOW) {
            self.notice = None;
            self.dirty = true;
        }
    }

    fn handle(&mut self, event: &Event, commands: &Registry<Self>) {
        match event {
            Event::Resized { logical, physical, scale_120 } => {
                self.logical = *logical;
                self.physical = *physical;
                self.scale_120 = *scale_120;
                self.relayout();
                self.renderer.resize(*physical);
                self.dirty = true;
            }
            Event::CloseRequested => self.closing = true,
            Event::Focused(_) => self.dirty = true,
            Event::KeyDown(key) => self.key(key, commands),
            Event::PointerMoved { position, .. } => self.moved(*position, commands),
            // A drag from somewhere else — or from here, because a drag that
            // starts in this window and stays in it comes back through the same
            // events as anybody else's.
            Event::DragEntered { position } | Event::DragMoved { position } => {
                self.drag_over(*position);
            }
            Event::DragLeft => {
                if self.files_drag.take().is_some() {
                    self.dirty = true;
                }
            }
            Event::Dropped { position } => self.take_drop(*position),
            Event::PointerLeft => {
                // Every hover, and each of them taken: `||` short-circuits, so a
                // pointer leaving a row left a breadcrumb lit and a window button
                // shaded until it came back.
                let gone = [
                    self.panes.focused_mut().hover.take().is_some(),
                    self.hovered_button.take().is_some(),
                    self.hovered_crumb.take().is_some(),
                    self.hovered_tool.take().is_some(),
                    self.sidebar.hover.take().is_some(),
                ];
                if gone.contains(&true) {
                    self.dirty = true;
                }
            }
            Event::Button { button, pressed, position, clicks, mods } => match button {
                Button::Primary if *pressed => {
                    self.press(*position, *clicks, mods.ctrl, mods.shift, commands);
                }
                Button::Primary => {
                    let position = *position;
                    // **A drag that ends inside this window is ours to finish.**
                    // The display server is told to let go: it cannot arrange a
                    // drop onto a surface the pointer is already on, and a source
                    // left alive outlives the gesture — measured, one compositor
                    // sends a single `enter` after the button is already up and
                    // then nothing at all.
                    //
                    // **Any pane's list, not the focused one's.** This asked
                    // about `placement.here`, so a drop into the *other* half of
                    // a split released a button the server still thought was
                    // dragging: the files moved and the gesture never ended,
                    // leaving a drag over the window that nothing could clear.
                    let inside = self.pane_under(position).is_some();
                    if inside && self.window.dragging_out() {
                        self.window.cancel_drag();
                    }
                    if !std::mem::take(&mut self.dropped) {
                        self.drop_files(mods.ctrl);
                    }
                    self.release();
                }
                Button::Secondary if *pressed => self.open_menu(*position),
                _ => {}
            },
            Event::Scroll { delta, precise, mods } => {
                // The palette takes the wheel while it is open. Without this the
                // twelfth result of fifty is unreachable by pointer alone, and
                // the listing scrolls behind a panel covering it.
                if mods.ctrl && self.palette.is_none() && self.menu.is_none() {
                    // What `Ctrl` and a wheel do in every list and every
                    // browser, and the pointer's half of what a pinch does.
                    self.zoom_by(if delta.y < 0.0 { 1 } else { -1 });
                } else if let Some(palette) = self.palette.as_mut() {
                    palette.wheel(delta.y);
                } else if self.menu.is_some() {
                    // A menu taller than the window scrolls; one that fits does
                    // not move, and neither does the list behind it.
                    let room = self.layout.window.height - crate::menu::MARGIN * 2.0;
                    if let Some(menu) = self.menu.as_mut() {
                        menu.wheel(delta.y, room);
                    }
                } else {
                    self.panes.focused_mut().scroll.by(delta.y, *precise);
                }
                self.dirty = true;
            }
            // **Three fingers, not two.** Two-finger movement on a touchpad is
            // scroll and arrives as `Scroll`; a compositor that also reported it
            // as a swipe would navigate while the list moved.
            Event::Swiped { direction, fingers } if *fingers >= 3 => {
                let command = match direction {
                    sk_plat::Swipe::Left => "go.back",
                    sk_plat::Swipe::Right => "go.forward",
                    sk_plat::Swipe::Up => "go.parent",
                    sk_plat::Swipe::Down => "go.open",
                };
                if let Some(id) = commands.find(command)
                    && commands.enabled(id, self)
                {
                    commands.run(id, self);
                }
            }
            // An input method's change goes to the field with the keyboard, and
            // nowhere at all when none has it: a candidate window over a file
            // list is a candidate window whose text has nothing to go into.
            Event::Composing(change) => {
                if let Some(field) = self.focused_field() {
                    field.compose(change);
                    self.dirty = true;
                }
            }
            Event::Pinched { out } => {
                self.zoom_by(if *out { 1 } else { -1 });
            }
            Event::Swiped { .. } | Event::KeyUp(_) | Event::Woken => {}
        }
        self.settle_menu(commands);
    }

    /// Builds the menu somebody asked for.
    ///
    /// Every event ends here, which is what makes this the only construction
    /// site: the pointer and the keyboard both leave a request rather than a
    /// menu, so the two paths cannot drift into two different menus.
    fn settle_menu(&mut self, commands: &Registry<Self>) {
        let Some((at, groups)) = self.menu_wanted.take() else { return };
        self.menu = crate::menu::build(groups, commands, at, |id| {
            commands.find(id).is_some_and(|command| commands.enabled(command, self))
        });
        self.dirty = true;
    }

    fn key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) {
        // Any key ends the double-click window: a field opened with `Ctrl+L` was
        // not opened by a click on the title bar, and a double click in it belongs
        // to the caret.
        self.path_from_title = false;
        // An open menu takes the keyboard first, and gives back everything it
        // has no use for.
        if self.menu.is_some() && self.menu_key(event, commands) {
            return;
        }
        // **The question comes first, and there is no key that dismisses it.**
        // A name that is already taken has four answers and none of them is
        // "never mind": the queue is stopped mid-way with files still to place,
        // and a stray key that closed the panel would leave it stopped with
        // nothing on screen saying so. `Escape` is bound to Stop, which is the
        // answer a person means by it.
        if self.edits.asking() && self.clash_key(event, commands) {
            return;
        }
        // Beside it, and for the same reason: the only irreversible thing this
        // program does is waiting for an answer, and a key that fell through
        // would be a key acting on a selection the question names.
        if self.erasing.is_some() && self.erasing_key(event, commands) {
            return;
        }
        // The Open With list takes the keyboard while it is up: arrows move,
        // Enter launches, and anything else closes it — the same contract the
        // context menu has, and for the same reason.
        if self.openers.is_some() && self.openers_key(event, commands) {
            return;
        }
        // The Properties panel is a statement, not a field: any key dismisses
        // it, and dismisses only. The diagnostic report passes through for the
        // reason it passes through an open menu — it is the only way the harness
        // can see the panel at all.
        if self.properties.is_some() {
            let diagnostic = commands
                .resolve(event, Scope::Global)
                .and_then(|id| commands.describe(id))
                .is_some_and(|command| command.id == "help.dump");
            if !diagnostic {
                self.properties = None;
                self.dirty = true;
                return;
            }
        }
        // Before the palette and before the list: a sibling menu is the most
        // specific thing on screen while it is open, and an arrow that moved the
        // list behind it would move a selection nobody can see.
        if (self.crumb_menu.is_some() || self.crumb_pending.is_some())
            && self.crumb_menu_key(event, commands)
        {
            return;
        }
        // The palette takes the keyboard next, and takes nearly all of it: it is
        // a modal field over the whole window, and a chord that ran a command
        // behind it would run one the palette was about to offer.
        if self.palette.is_some() && self.palette_key(event, commands) {
            return;
        }
        // The search takes it on the same terms and for the same reason: it is a
        // field over the window, and every letter belongs to the query.
        if self.search.is_some() && self.search_key(event, commands) {
            return;
        }
        // **A field-scoped command comes before the field's own editing keys.**
        // `Ctrl+A` over a listing is Select all and inside a field is Select the
        // whole name, and the registry is the only place that difference can live
        // if a keymap is ever to be rebindable — see `sk_cmd::Scope`. Everything a
        // field does that means nothing over a listing, `Shift ←` included, stays
        // in `Field::key`, because there is nothing there for a scope to settle.
        if self.field_scope() == Scope::Field
            && let Some(command) = commands.resolve(event, Scope::Field)
            && commands.scope_of(command) == Scope::Field
        {
            commands.run(command, self);
            self.dirty = true;
            return;
        }
        // The path bar takes the keyboard while it is open, for the same reason
        // the name field does and before it: it is the field with the caret in
        // it, and `Delete` in it must delete a character and not a file.
        if self.typing_path.is_some() && self.path_key(event) {
            return;
        }
        // A name being typed takes the keyboard before anything else, and takes
        // more of it than the filter does: `Delete` is bound to Move to Trash,
        // and a rename field where `Delete` deletes the file is a field nobody
        // will ever use twice.
        if self.edits.naming.is_some() && self.name_key(event) {
            return;
        }
        // **A field swallows everything that acts on files or on the listing.**
        // Anything it did not handle used to fall through to the registry and then
        // to the type-ahead — so `Ctrl+Backspace` in a rename field, which was not
        // a text operation yet, jumped the cursor to another row and changed the
        // selection under the name being typed. `Ctrl+Shift+N` made a folder,
        // `Delete` was only safe because the field claimed it first, and `Ctrl+Z`
        // walked back the last operation on disk.
        //
        // Two things still get through, and the line is what they act on: a
        // command in [`Group::Window`], because a window that ignores its own
        // close chord reads as frozen, and the diagnostic report, for the reason
        // the menu lets it through — a report that cannot be taken while a field
        // is open cannot describe one.
        if self.field_scope() == Scope::Field {
            if let Some(command) = commands.resolve(event, Scope::Global)
                && commands
                    .describe(command)
                    .is_some_and(|it| it.group == sk_cmd::Group::Window || it.id == "help.dump")
            {
                commands.run(command, self);
            }
            self.dirty = true;
            return;
        }
        // The filter takes the keyboard while it is open, before the registry
        // sees it: a filter you cannot type the letter `h` into because `Ctrl+H`
        // is bound is not a filter. Bare characters only, so every chord still
        // reaches its command.
        if self.filtering && self.filter_key(event) {
            return;
        }
        self.last_key = format!(
            "{}{}{}{}{:?}",
            if event.mods.ctrl { "ctrl+" } else { "" },
            if event.mods.alt { "alt+" } else { "" },
            if event.mods.shift { "shift+" } else { "" },
            if event.mods.logo { "super+" } else { "" },
            event.key
        );
        if let Some(command) = commands.resolve(event, Scope::Global) {
            // A disabled command consumes its key and does nothing, which is the
            // same thing a dimmed menu item does. Falling through to type-ahead
            // instead would make `Alt ←` with no history type an arrow.
            //
            // **Which one it was is remembered.** A chord that resolves to a
            // command whose `enabled` says no produces nothing at all — no
            // notice, no job, no frame worth looking at — and from outside the
            // process that is indistinguishable from a key that never arrived.
            // One `&'static str`, written on the path that already happened.
            if !commands.run(command, self) {
                self.refused = commands.describe(command).map(|it| it.id);
            }
            self.dirty = true;
            return;
        }
        if let Some(text) = keys::type_ahead(event) {
            self.typed.push_str(text);
            self.typed_at = Instant::now();
            if self.panes.focused_mut().jump_to(&self.typed) {
                self.reveal_cursor();
            }
            self.dirty = true;
        }
    }

    /// Which scope the keyboard is in.
    ///
    /// `Field` while something with a caret has it — a name, a path, the palette's
    /// query. **Not the filter**, which is a `String` with no caret and no
    /// selection: `Ctrl+A` while filtering selects the rows that matched, which is
    /// what filtering is for.
    fn field_scope(&self) -> Scope {
        let has_field =
            self.edits.naming.is_some() || self.typing_path.is_some() || self.palette.is_some();
        if has_field { Scope::Field } else { Scope::Global }
    }

    /// The field with the keyboard, in the order the key chain gives it away.
    fn focused_field(&mut self) -> Option<&mut crate::field::Field> {
        if self.palette.is_some() {
            return self.palette.as_mut().map(|palette| &mut palette.query);
        }
        if self.typing_path.is_some() {
            return self.typing_path.as_mut();
        }
        self.edits.naming.as_mut().map(|naming| &mut naming.field)
    }

    /// What an input method is composing in the field with the keyboard.
    fn field_preedit(&self) -> Option<&(String, Option<(usize, usize)>)> {
        if let Some(palette) = self.palette.as_ref() {
            return palette.query.preedit.as_ref();
        }
        if let Some(field) = self.typing_path.as_ref() {
            return field.preedit.as_ref();
        }
        self.edits.naming.as_ref().and_then(|naming| naming.field.preedit.as_ref())
    }

    /// Whether a field has the keyboard, for a command's `enabled`.
    #[must_use]
    pub fn in_a_field(&self) -> bool {
        self.field_scope() == Scope::Field
    }

    /// Selects everything in the field with the keyboard.
    pub(crate) fn select_all_text(&mut self) {
        if let Some(field) = self.focused_field() {
            field.select_all();
            self.dirty = true;
        }
    }

    /// Puts the field's selection on the clipboard as text.
    pub(crate) fn copy_text(&mut self) {
        let Some(selected) = self.focused_field().map(|field| field.selected().to_owned()) else {
            return;
        };
        if selected.is_empty() {
            return;
        }
        self.window.set_selection(Selection::Clipboard, Content::text(&selected));
    }

    /// Takes back the last edit in the field with the keyboard.
    pub(crate) fn undo_text(&mut self) {
        if self.focused_field().is_some_and(crate::field::Field::undo) {
            self.dirty = true;
        }
    }

    /// Does the last undone edit in the field again.
    pub(crate) fn redo_text(&mut self) {
        if self.focused_field().is_some_and(crate::field::Field::redo) {
            self.dirty = true;
        }
    }

    /// Inserts the clipboard's text into the field with the keyboard.
    ///
    /// **Read on a job**, like every other clipboard read: the peer writes into a
    /// pipe at its own pace, and a read on the frame loop is a frame loop waiting
    /// on another process. The field may be gone by the time the bytes arrive, and
    /// then the paste is dropped — the place it was going no longer exists.
    pub(crate) fn paste_text(&mut self) {
        let offered = self.window.clipboard().offered().to_vec();
        let Some(mime) = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "text/uri-list"]
            .into_iter()
            .find(|want| offered.iter().any(|have| have == want))
        else {
            return;
        };
        let Some(fd) = self.window.request_selection(Selection::Clipboard, mime) else { return };
        let sender = self.replies.paste_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let mut bytes = Vec::new();
            let mut file = std::fs::File::from(fd);
            let _ = std::io::Read::read_to_end(&mut file, &mut bytes);
            // The first line only: this is one line of text, and a filename
            // cannot hold a newline at all.
            let text = String::from_utf8_lossy(&bytes);
            let line = text.lines().next().unwrap_or("").to_owned();
            if !line.is_empty() && sender.send(Pasted::Text(line)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Copies the field's selection and removes it.
    pub(crate) fn cut_text(&mut self) {
        self.copy_text();
        if let Some(field) = self.focused_field() {
            field.backspace();
            self.dirty = true;
        }
    }

    /// Where the name field is, so a press inside it is not a press outside it.
    ///
    /// The same rectangle `view::name_field` draws, from the same numbers — the
    /// alternative is two rectangles that agree until one of them is edited.
    fn naming_field(&self) -> Area {
        let Some(naming) = self.edits.naming.as_ref() else { return Area::default() };
        crate::view::name_field(
            &self.placement.here,
            &self.theme.metrics,
            naming.row,
            self.panes.focused().scroll.offset(),
        )
    }

    /// Puts the caret where a click inside the name field landed.
    ///
    /// Measured by shaping, not by dividing the width by the character count: a
    /// click halfway along `étude` is not halfway along its bytes. The text
    /// starts six logical pixels inside the box, which is where
    /// `view::name_field_at` draws it — the one number the two share.
    fn place_caret(&mut self, x: f32) {
        let Some(row) = self.edits.naming.as_ref().map(|naming| naming.row) else { return };
        let area = crate::view::name_field(
            &self.placement.here,
            &self.theme.metrics,
            row,
            self.panes.focused().scroll.offset(),
        );
        let size = self.theme.metrics.text_size;
        self.place_caret_in(Claim::Naming, area, size, crate::view::NAME_INSET, x);
    }

    /// The scroll extent, and the folder totals for what is on screen.
    ///
    /// Once per frame, and bounded by the viewport rather than by the directory:
    /// keyed by path rather than by row, so a new listing shows what is already
    /// known instead of starting again.
    fn settle_rows(&mut self) {
        self.rebandif_needed();
        let cells = self.cells();
        self.panes
            .focused_mut()
            .scroll
            .set_extent(cells.content_height(), self.placement.here.body.height);
        let visible = cells.visible(self.panes.focused().scroll.offset());
        let Self { measurer, panes, jobs, .. } = self;
        measurer.update(panes.focused_mut(), visible, jobs);
    }

    /// Puts the caret where a click in the path field landed.
    fn place_path_caret(&mut self, x: f32) {
        let area = crate::view::path_field(&self.layout, &self.theme.metrics);
        let size = self.theme.metrics.path_size;
        self.place_caret_in(Claim::Path, area, size, crate::view::PATH_INSET, x);
    }

    /// A press on the path strip, beside the breadcrumbs. Returns whether it was.
    ///
    /// Two answers, because the same strip is two things. **Where the window draws
    /// its own frame** it is the title bar, and the press waits: the compositor
    /// takes the pointer the moment `start_move` is asked, so asking on the press
    /// would make every click a drag. **Where the compositor draws the frame**
    /// there is no drag region and nothing else wants the press, so it opens the
    /// path for typing straight away — which the address bar in every browser
    /// does, and which the breadcrumbs beside it already did.
    fn press_on_the_path_strip(&mut self, position: Point, clicks: u8) -> bool {
        if self.layout.drag.contains(position.x, position.y) {
            // A double click on a title bar toggles maximize everywhere else, so
            // it does here.
            if clicks >= 2 {
                self.toggle_maximized();
            } else {
                self.title_press = Some(position);
            }
            return true;
        }
        if self.layout.path.contains(position.x, position.y) {
            self.toggle_path_entry();
            return true;
        }
        false
    }

    /// A press inside an open text field: the caret goes there and a drag begins.
    ///
    /// Returns whether the press belonged to a field. **A press outside the name
    /// field commits it**, which is what every other text field on the desktop
    /// does — and the press then carries on to whatever it landed on.
    fn press_in_a_field(&mut self, position: Point) -> bool {
        if self.edits.naming.is_some() {
            if self.naming_field().contains(position.x, position.y) {
                self.place_caret(position.x);
                self.text_drag = Some(Claim::Naming);
                return true;
            }
            self.commit_name();
        }
        if self.typing_path.is_some()
            && crate::view::path_field(&self.layout, &self.theme.metrics)
                .contains(position.x, position.y)
        {
            self.place_path_caret(position.x);
            self.text_drag = Some(Claim::Path);
            return true;
        }
        false
    }

    /// Extends the field's selection to where the pointer is.
    ///
    /// The same measurement `place_caret_in` makes, and then the caret moves with
    /// the anchor left where the press put it — which is the whole of a drag.
    fn drag_text(&mut self, which: Claim, x: f32) {
        let (area, size, inset) = match which {
            Claim::Naming => {
                let Some(row) = self.edits.naming.as_ref().map(|naming| naming.row) else { return };
                let area = crate::view::name_field(
                    &self.placement.here,
                    &self.theme.metrics,
                    row,
                    self.panes.focused().scroll.offset(),
                );
                (area, self.theme.metrics.text_size, crate::view::NAME_INSET)
            }
            Claim::Path => (
                crate::view::path_field(&self.layout, &self.theme.metrics),
                self.theme.metrics.path_size,
                crate::view::PATH_INSET,
            ),
            Claim::Filter | Claim::Palette | Claim::Search => return,
        };
        let Some(offset) = self.text_offset_at(which, area, size, inset, x) else { return };
        if let Some(field) = self.field_for(which) {
            field.extend_to(offset);
            self.dirty = true;
        }
    }

    /// The field one claim names, for the two the pointer can drag in.
    fn field_for(&mut self, which: Claim) -> Option<&mut crate::field::Field> {
        match which {
            Claim::Path => self.typing_path.as_mut(),
            Claim::Naming => self.edits.naming.as_mut().map(|naming| &mut naming.field),
            Claim::Palette => self.palette.as_mut().map(|palette| &mut palette.query),
            Claim::Search => self.search.as_mut().map(|search| &mut search.query),
            Claim::Filter => None,
        }
    }

    /// Which byte offset in a field a logical `x` lands on.
    ///
    /// The one measurement, shared by the click that places a caret and the drag
    /// that extends a selection — two copies of it is how a drag selects a
    /// different span from the one the pointer is over.
    fn text_offset_at(
        &mut self,
        which: Claim,
        area: Area,
        size: f32,
        inset: f32,
        x: f32,
    ) -> Option<usize> {
        let scale = self.window.scale();
        let style = sk_ui::TextStyle::new(size, self.theme.palette.text);
        let px_size = size * scale;
        let (text, caret) = {
            let field = self.field_for(which)?;
            (field.text.clone(), field.caret)
        };
        let Self { cache, fonts, .. } = self;
        let before =
            sk_ui::advance_of(cache, fonts, &text[..caret.min(text.len())], style.weight, px_size)
                / scale;
        let scroll = crate::view::field_scroll(before, area.width - inset * 2.0);
        Some(sk_ui::caret_offset(
            fonts,
            &text,
            style,
            px_size,
            (x - area.x - inset + scroll) * scale,
        ))
    }

    /// Puts the caret in one of the two fields where a click landed.
    ///
    /// The text is scrolled by however much the frame the person is looking at
    /// scrolled it, so the click has to be read in the same coordinates the draw
    /// used — the arithmetic is `view::field_scroll`, once, for both.
    fn place_caret_in(&mut self, which: Claim, area: Area, size: f32, inset: f32, x: f32) {
        let Some(field) = (match which {
            Claim::Path => self.typing_path.as_mut(),
            Claim::Naming => self.edits.naming.as_mut().map(|naming| &mut naming.field),
            Claim::Palette => self.palette.as_mut().map(|palette| &mut palette.query),
            Claim::Search => self.search.as_mut().map(|search| &mut search.query),
            Claim::Filter => None,
        }) else {
            return;
        };
        let scale = self.window.scale();
        let style = sk_ui::TextStyle::new(size, self.theme.palette.text);
        let px_size = size * scale;
        let Self { cache, fonts, .. } = self;
        let before =
            sk_ui::advance_of(cache, fonts, field.before_caret(), style.weight, px_size) / scale;
        let scroll = crate::view::field_scroll(before, area.width - inset * 2.0);
        field.deselect();
        field.caret = sk_ui::caret_offset(
            fonts,
            &field.text,
            style,
            px_size,
            (x - area.x - inset + scroll) * scale,
        );
        self.dirty = true;
    }

    /// Feeds one key to the name being typed. Returns whether it was consumed.
    ///
    /// Enter commits and Escape abandons; everything else the field does itself.
    fn name_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        use sk_plat::{Key, Named};
        match event.key {
            Key::Named(Named::Enter) => self.commit_name(),
            Key::Named(Named::Escape) => {
                self.edits.stop_naming();
            }
            _ => {
                let Some(naming) = self.edits.naming.as_mut() else { return false };
                if !naming.field.key(event) {
                    return false;
                }
            }
        }
        self.dirty = true;
        true
    }

    /// Feeds one key to the path being typed. Returns whether it was consumed.
    ///
    /// Enter goes there and Escape puts the breadcrumbs back.
    fn path_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        use sk_plat::{Key, Named};
        match event.key {
            Key::Named(Named::Enter) => self.go_to_typed_path(),
            Key::Named(Named::Escape) => {
                self.typing_path = None;
            }
            _ => {
                let Some(field) = self.typing_path.as_mut() else { return false };
                if !field.key(event) {
                    return false;
                }
            }
        }
        self.dirty = true;
        true
    }

    /// Closes every field except the one about to open.
    ///
    /// **Two carets is one too many.** Three things in this window take the
    /// keyboard — the filter, a name and a path — and each was opened without
    /// asking what already had it: `Ctrl+F` then `Ctrl+L` left the filter
    /// narrowing the list while the path field ate every character.
    ///
    /// A name is committed rather than abandoned, which is what a click outside
    /// it already does. The menu is not here: it is a mode over the whole window
    /// rather than a field, `menu_key` has the keyboard while it is up, and
    /// closing a filter because somebody right-clicked would lose their filter.
    fn claim_keyboard(&mut self, keep: Claim) {
        // **The filter box loses the keyboard; the filter stays applied.** Two
        // things behind one flag: closing the box used to un-narrow the list, so
        // `F2` on a row that was found by searching renamed it in a list where it
        // had jumped to wherever it sits among all the others — usually screens
        // away from where it was aimed at.
        if keep != Claim::Filter && std::mem::take(&mut self.filtering) {
            self.dirty = true;
        }
        if keep != Claim::Naming && self.edits.naming.is_some() {
            self.commit_name();
        }
        if keep != Claim::Path && self.typing_path.take().is_some() {
            self.dirty = true;
        }
        if keep != Claim::Search && self.search.is_some() {
            self.close_search();
        }
        if keep != Claim::Palette && self.palette.take().is_some() {
            self.dirty = true;
        }
    }

    /// Closes every field and the menu, because the directory changed under them.
    ///
    /// A name is abandoned here and not committed: the subject is in a directory
    /// nobody is looking at any more, and `F2` then `Alt ←` then `Enter` renamed
    /// a file in it.
    fn abandon_fields(&mut self) {
        let closed = [
            self.edits.stop_naming(),
            self.typing_path.take().is_some(),
            self.menu.take().is_some(),
            self.palette.take().is_some(),
            std::mem::take(&mut self.filtering),
        ];
        if closed.contains(&true) {
            self.dirty = true;
        }
    }

    /// Opens the path bar for typing, or closes it.
    ///
    /// **The whole path, with the caret at the end.** A path bar that opens empty
    /// is a path bar you cannot use to go one directory sideways, which is most
    /// of what typing a path is for.
    pub(crate) fn toggle_path_entry(&mut self) {
        self.claim_keyboard(Claim::Path);
        self.typing_path = match self.typing_path {
            Some(_) => None,
            None => Some(crate::field::Field::new(
                self.panes.focused().path.to_string_lossy().into_owned(),
            )),
        };
        self.dirty = true;
    }

    /// Goes where the path bar says, and puts the breadcrumbs back.
    ///
    /// **Nothing is stat'ed here** — rule 1. A path that is not a directory is
    /// opened anyway and the read fails with a message that says so, which is
    /// the same answer a directory deleted a moment ago gives.
    fn go_to_typed_path(&mut self) {
        let Some(field) = self.typing_path.take() else { return };
        let typed = field.text.trim();
        if typed.is_empty() {
            return;
        }
        let path = expand_home(typed).unwrap_or_else(|| {
            let path = std::path::Path::new(typed);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.panes.focused().path.join(path)
            }
        });
        if path != self.panes.focused().path {
            self.panes.focused_mut().open(path, &self.jobs);
        }
    }

    /// Feeds one key to the open filter. Returns whether it was consumed.
    fn filter_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        let mut wanted = self.panes.focused().filter.clone();
        match event.key {
            sk_plat::Key::Named(sk_plat::Named::Backspace) if event.mods.none() => {
                // **An empty filter swallows Backspace rather than closing.**
                // Closing on it looks harmless and is not: key repeat does not
                // stop because a mode ended, so a held Backspace emptied the
                // filter, closed it, and handed the next repeat to the list —
                // where `Backspace` is Parent folder. Holding it walked out of
                // the directory being searched, one level per repeat. `Escape`
                // is the key that leaves, and it is the one every other field
                // here leaves on.
                if wanted.pop().is_none() {
                    return true;
                }
            }
            // Escape and Enter leave the field. Escape also clears, so "never
            // mind" is one key here as it is everywhere else.
            sk_plat::Key::Named(sk_plat::Named::Escape) => {
                self.filtering = false;
                wanted.clear();
            }
            sk_plat::Key::Named(sk_plat::Named::Enter) => {
                self.filtering = false;
                self.dirty = true;
                return false;
            }
            _ => match keys::typed_text(event) {
                Some(text) => wanted.push_str(text),
                // Arrows, Page Down, `Ctrl+A`: still the list's, so a filtered
                // list can be moved through and acted on without closing it.
                None => return false,
            },
        }
        if self.panes.focused_mut().set_filter(wanted, &self.jobs) {
            self.reveal_cursor();
        }
        self.dirty = true;
        true
    }

    // ------------------------------------------------------------------
    // What the commands in `crate::commands` do. One method each, so the
    // registry holds function pointers and nothing else.
    // ------------------------------------------------------------------

    /// What the compositor last said the window is.
    #[must_use]
    pub(crate) fn window_state(&self) -> sk_plat::WindowState {
        self.window_state
    }

    /// Minimises, where the compositor has such a thing.
    pub(crate) fn minimize_window(&mut self) {
        self.window.minimize();
    }

    /// Maximises, or restores if it is already maximized.
    pub(crate) fn toggle_maximized(&mut self) {
        self.window.set_maximized(!self.window_state.maximized);
    }

    /// Asks the window to close, as the title bar's button and `wl_toplevel`'s
    /// own close event both do.
    pub(crate) fn close_window(&mut self) {
        self.window.request_close();
    }

    /// Whether an operation is in flight, so a second one is not started.
    #[must_use]
    pub(crate) fn busy(&self) -> bool {
        self.edits.busy()
    }

    /// Whether there is anything to undo, and nothing in flight.
    #[must_use]
    pub(crate) fn can_undo(&self) -> bool {
        !self.edits.busy() && self.edits.history.can_undo()
    }

    /// Whether there is anything to redo, and nothing in flight.
    #[must_use]
    pub(crate) fn can_redo(&self) -> bool {
        !self.edits.busy() && self.edits.history.can_redo()
    }

    /// Makes a folder here, and names it once it exists.
    ///
    /// Created first and named second, which is what Windows and Nautilus both
    /// do: there has to be a row before there is somewhere to type. The name it
    /// gets in between is a real name — press Escape and the folder is called
    /// `New folder`, which is what a person expects and is one `Ctrl+Z` from
    /// gone.
    pub(crate) fn new_folder(&mut self) {
        let parent = self.panes.focused().path.clone();
        self.edits.new_directory(parent, &self.jobs);
        // The job picks the name — `New folder`, or `New folder 2` if that is
        // taken — so what is owed here is "name whatever it made", and the name
        // itself arrives with the outcome.
        self.edits.naming_owed = true;
    }

    /// Starts renaming whatever the cursor is on.
    pub(crate) fn start_rename(&mut self) {
        self.claim_keyboard(Claim::Naming);
        let Some(id) = self.panes.focused().focused() else { return };
        let Some(row) = self.panes.focused().listing.row_of(id) else { return };
        let name = self.panes.focused().listing.name(id).to_owned();
        let directory = self.panes.focused().listing.flags(id).has(sk_model::Flags::DIR);
        let (top, bottom) = self.cells().span(row);
        let body = self.placement.here.body.height;
        self.panes.focused_mut().scroll.reveal(top, bottom, body);
        self.edits.naming = Some(crate::edit::Naming {
            row,
            path: self.panes.focused().path.join(&name),
            // The whole name for a directory, which has no extension to keep, and
            // the part before the extension for a file — the edit a rename almost
            // always is. One gesture either way: a rename opens with something
            // selected, and typing replaces it.
            field: if directory {
                crate::field::Field::naming_all(name)
            } else {
                crate::field::Field::naming(name)
            },
        });
        self.dirty = true;
    }

    /// Commits the name being typed, if it changed.
    fn commit_name(&mut self) {
        let Some(naming) = self.edits.naming.take() else { return };
        self.dirty = true;
        let unchanged = naming
            .path
            .file_name()
            .is_some_and(|current| current.to_string_lossy() == naming.field.text);
        if unchanged || naming.field.text.is_empty() {
            return;
        }
        self.edits.rename(naming.path, naming.field.text, &self.jobs);
    }

    /// Copies everything selected into the folder it is already in.
    pub(crate) fn duplicate_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.edits.duplicate_all(paths, &self.jobs);
    }

    /// Deletes what is selected for good, on the second press.
    ///
    /// The first press arms and says how many and where; the second, inside the
    /// window the notice is up for, does it. Every other command disarms, which
    /// is what makes this an action a person cannot arrive at by accident.
    pub(crate) fn delete_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        // **The second press is the fast path, not the mechanism.** A person who
        // knows what they are doing presses Delete twice; the panel is what makes
        // that safe for everybody else, and it is what a pointer answers.
        if self.erasing.as_ref().is_some_and(|asking| asking.paths == paths) {
            self.erase_for_good();
            return;
        }
        self.erasing = Some(Erasing { paths, on: Confirm::Cancel });
        self.dirty = true;
    }

    /// Does what the panel is asking about, and closes it.
    pub(crate) fn erase_for_good(&mut self) {
        let Some(asking) = self.erasing.take() else { return };
        self.step_off_the_selection(&asking.paths);
        self.edits.erase_all(asking.paths, &self.jobs);
        self.dirty = true;
    }

    /// Closes the panel without deleting anything.
    pub(crate) fn keep_them(&mut self) {
        self.erasing = None;
        self.dirty = true;
    }

    /// Keys while the panel is up. Whether it took the event.
    ///
    /// **`Enter` is not the destructive one.** The answer a hand arrives at by
    /// reflex has to be the safe one, so `Enter` takes whichever button is on and
    /// the one that starts on is Cancel — and `Delete` again is the deliberate
    /// second press that was always the keyboard's way in.
    fn erasing_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.keep_them(),
            // **With or without `Shift`.** The chord that asked the question has
            // to be able to answer it, and `Shift+Delete` is what asked — a panel
            // that swallowed the second press would turn the keyboard's two-press
            // fast path into a key that does nothing at all.
            Key::Named(Named::Delete) => self.erase_for_good(),
            Key::Named(Named::Left | Named::Right | Named::Tab) if bare => {
                if let Some(asking) = self.erasing.as_mut() {
                    asking.on = match asking.on {
                        Confirm::Cancel => Confirm::Delete,
                        Confirm::Delete => Confirm::Cancel,
                    };
                }
            }
            Key::Named(Named::Enter) => match self.erasing.as_ref().map(|asking| asking.on) {
                Some(Confirm::Delete) => self.erase_for_good(),
                _ => self.keep_them(),
            },
            _ => {
                // The diagnostic report passes through, for the reason it passes
                // through an open menu: it is the only way the harness sees this.
                let diagnostic = commands
                    .resolve(event, Scope::Global)
                    .and_then(|id| commands.describe(id))
                    .is_some_and(|command| command.id == "help.dump");
                return !diagnostic;
            }
        }
        self.dirty = true;
        true
    }

    /// What the delete-for-good question is showing, for the diagnostic line.
    ///
    /// The topmost panel's rectangle, where to click the two buttons, and which
    /// one the keyboard is on. The rectangle is here so a scenario can aim *into*
    /// a panel rather than guessing from a button and landing past the last row —
    /// a point where nothing hovers either way, which is how the first version of
    /// the hover scenario passed against the bug it was written for.
    fn question_state(&self) -> (String, String, &'static str) {
        let panel_area = self.hits.panels.last().map_or_else(String::new, |a| {
            format!("{:.0}:{:.0}:{:.0}:{:.0}", a.x, a.y, a.width, a.height)
        });
        let erase_aim = [self.hits.erase_cancel, self.hits.erase_delete]
            .iter()
            .flatten()
            .map(|a| format!("{:.0}:{:.0}", a.x + a.width / 2.0, a.y + a.height / 2.0))
            .collect::<Vec<_>>()
            .join(",");
        let on = match self.erasing.as_ref().map(|asking| asking.on) {
            Some(Confirm::Delete) => "delete",
            Some(Confirm::Cancel) => "cancel",
            None => "",
        };
        (panel_area, erase_aim, on)
    }

    /// Whether the pointer is on something inside a panel that answers a click.
    ///
    /// The delete question's two buttons, and the name-taken question's answers
    /// and its checkbox — every one of them a target that a click does something
    /// with, and every one of them drawn with the default arrow over it until
    /// now. The panels with no targets of their own fall through to the plain
    /// arrow, which is what they already had.
    fn over_a_panel_button(&self, position: Point) -> bool {
        let on = |area: &crate::layout::Area| area.contains(position.x, position.y);
        [self.hits.erase_cancel, self.hits.erase_delete, self.hits.for_the_rest]
            .iter()
            .flatten()
            .any(on)
            || self.hits.answers.iter().any(on)
    }

    /// A press on the status strip's own controls. Whether it was taken.
    ///
    /// Only while an operation is running, which is the only time they are drawn:
    /// a hit area for a control nobody can see is a press that does something
    /// invisible.
    fn press_in_the_status(&mut self, position: Point) -> bool {
        if self.hits.pause.is_some_and(|a| a.contains(position.x, position.y)) {
            self.toggle_pause();
            return true;
        }
        if self.hits.cancel.is_some_and(|a| a.contains(position.x, position.y)) {
            self.cancel_operation();
            return true;
        }
        false
    }

    /// A press while the panel is up. Whether it was taken.
    ///
    /// **Always taken, and a press outside answers nothing.** The same rule the
    /// name-taken question follows: a question with no answer left on screen is
    /// worse than one that insists.
    fn press_in_the_erasing(&mut self, position: Point) -> bool {
        if self.erasing.is_none() {
            return false;
        }
        if self.hits.erase_cancel.is_some_and(|a| a.contains(position.x, position.y)) {
            self.keep_them();
        } else if self.hits.erase_delete.is_some_and(|a| a.contains(position.x, position.y)) {
            self.erase_for_good();
        }
        true
    }

    /// Moves what is selected to the trash.
    pub(crate) fn trash_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        // The cursor leaves the doomed rows now rather than when the listing
        // comes back: a person deleting four files presses Delete four times, and
        // a cursor that lands nowhere makes the second press do nothing at all —
        // `enabled` reads `focused`.
        self.step_off_the_selection(&paths);
        self.edits.trash_all(paths, &self.jobs);
    }

    /// Moves the cursor to the first row that is not going.
    ///
    /// Below the selection where there is anything below it, and above where
    /// there is not — which is where the eye already is, and which leaves the
    /// next press of Delete with something to act on.
    fn step_off_the_selection(&mut self, going: &[PathBuf]) {
        let doomed: Vec<&std::ffi::OsStr> =
            going.iter().filter_map(|path| path.file_name()).collect();
        let survives = |app: &Self, row: usize| {
            app.panes.focused().listing.at(row).is_some_and(|id| {
                let name = app.panes.focused().listing.name(id);
                !doomed.iter().any(|going| *going == name)
            })
        };
        let rows = self.panes.focused().listing.rows();
        let from = self.panes.focused().cursor_row().unwrap_or(0);
        let next = (from + 1..rows)
            .find(|row| survives(self, *row))
            .or_else(|| (0..from).rev().find(|row| survives(self, *row)));
        match next {
            Some(row) => self.panes.focused_mut().cursor_to(row, false),
            // Everything is going. Nothing to point at, and nothing to do.
            None => {
                self.panes.focused_mut().selection.set_cursor(None);
            }
        }
    }

    /// Takes the most recent change back.
    pub(crate) fn undo(&mut self) {
        self.edits.undo(&self.jobs);
    }

    /// Does the most recently undone change again.
    pub(crate) fn redo(&mut self) {
        self.edits.redo(&self.jobs);
    }

    /// Whether the list can be zoomed that way at all.
    pub(crate) fn can_zoom(&self, steps: isize) -> bool {
        let Ok(from) = isize::try_from(self.zoom) else { return false };
        let next = from.saturating_add(steps);
        next >= 0 && usize::try_from(next).is_ok_and(|next| next < ZOOM.len())
    }

    /// Whether the list is at anything other than its default size.
    pub(crate) fn zoomed(&self) -> bool {
        self.zoom != ZOOM_DEFAULT
    }

    /// Makes the list bigger or smaller by one step. Whether it moved.
    pub(crate) fn zoom_by(&mut self, steps: isize) -> bool {
        let Ok(from) = isize::try_from(self.zoom) else { return false };
        let Ok(next) = usize::try_from(from.saturating_add(steps)) else { return false };
        let next = next.min(ZOOM.len() - 1);
        if next == self.zoom {
            return false;
        }
        self.zoom = next;
        self.apply_zoom();
        true
    }

    pub(crate) fn zoom_in(&mut self) {
        self.zoom_by(1);
    }

    pub(crate) fn zoom_out(&mut self) {
        self.zoom_by(-1);
    }

    pub(crate) fn zoom_reset(&mut self) {
        if self.zoom != ZOOM_DEFAULT {
            self.zoom = ZOOM_DEFAULT;
            self.apply_zoom();
        }
    }

    /// Rebuilds the metrics and everything measured from them.
    ///
    /// The theme is rebuilt from scratch rather than scaled again, because
    /// scaling an already-scaled value drifts: eight steps out and eight back
    /// left the rows a pixel taller than they started.
    fn apply_zoom(&mut self) {
        let base = if self.theme.dark { Theme::dark() } else { Theme::light() };
        self.theme.metrics = base.metrics.zoomed(ZOOM[self.zoom]);
        self.relayout();
        // The layout cache needs no clearing: every key carries the pixel size
        // it was shaped at, so a zoom misses on every run and the old ones age
        // out with the next generation.
        self.dirty = true;
    }

    /// Opens a tab on the directory this one is showing.
    pub(crate) fn new_tab(&mut self) {
        let path = self.panes.focused().path.clone();
        self.panes.open_tab(path, &self.jobs);
        self.settle_panes();
    }

    /// Closes the showing tab, and the window when it was the last one.
    pub(crate) fn close_tab(&mut self) {
        if self.panes.close_tab() {
            self.settle_panes();
        } else {
            self.close_window();
        }
    }

    pub(crate) fn reopen_tab(&mut self) {
        if self.panes.reopen(&self.jobs) {
            self.settle_panes();
        }
    }

    pub(crate) fn next_tab(&mut self) {
        if self.panes.step_tab(1) {
            self.settle_panes();
        }
    }

    pub(crate) fn previous_tab(&mut self) {
        if self.panes.step_tab(-1) {
            self.settle_panes();
        }
    }

    pub(crate) fn split_beside(&mut self) {
        self.panes.split(crate::panes::Axis::Beside, &self.jobs);
        self.settle_panes();
    }

    pub(crate) fn split_below(&mut self) {
        self.panes.split(crate::panes::Axis::Below, &self.jobs);
        self.settle_panes();
    }

    pub(crate) fn close_pane(&mut self) {
        if self.panes.close_leaf() {
            self.settle_panes();
        }
    }

    pub(crate) fn next_pane(&mut self) {
        if self.panes.step_leaf(1) {
            self.settle_panes();
        }
    }

    /// Tells the input method whether a field has the keyboard, and where.
    ///
    /// Every pass rather than at each place the focus can move, for the same
    /// reason the watcher is retargeted here: a new way of opening a field
    /// cannot forget. `set_text_area` sends nothing when the answer is
    /// unchanged, so the cost is a comparison.
    fn aim_the_input_method(&mut self) {
        let area = self.in_a_field().then(|| {
            let caret = self.caret_rectangle();
            sk_plat::TextArea {
                x: caret.x as i32,
                y: caret.y as i32,
                width: caret.width as i32,
                height: caret.height as i32,
            }
        });
        self.window.set_text_area(area);
    }

    /// Where the caret of the field with the keyboard is, in logical pixels.
    ///
    /// The *field's* box rather than the caret's exact position: a candidate
    /// window is placed beside the text being edited, and a box is what the
    /// protocol asks for.
    fn caret_rectangle(&self) -> Area {
        // The palette's own field rectangle is recorded by the frame that drew
        // it, which is the only thing that knows how wide the panel came out.
        if let Some(field) = self.hits.palette_field.filter(|_| self.palette.is_some()) {
            return field;
        }
        if self.typing_path.is_some() {
            return crate::view::path_field(&self.layout, &self.theme.metrics);
        }
        self.naming_field()
    }

    /// Watches exactly the directories that are on screen.
    ///
    /// Called after anything that changes which panes those are — a navigation,
    /// a split, a tab — rather than by each of them, so a new way of changing
    /// the tree cannot forget: the same reasoning as the `watch_target` check
    /// this sits beside.
    fn retarget_watches(&mut self) {
        let wanted: Vec<PathBuf> = self.panes.visible().map(|pane| pane.path.clone()).collect();
        if wanted != self.watched {
            self.watched.clone_from(&wanted);
            self.watcher.watch(wanted);
        }
    }

    /// A press on the sidebar, a navigation button or a window button.
    ///
    /// Each goes through the registry rather than through a method: a dimmed
    /// button must do nothing when clicked, and `enabled` is the only place that
    /// knows whether it is dimmed.
    fn press_on_the_chrome(&mut self, position: Point, commands: &Registry<Self>) -> bool {
        if let Some(index) = self.place_at(position) {
            if let Some(entry) = self.sidebar.at(index) {
                let path = entry.path.clone();
                if path != self.panes.focused().path {
                    self.panes.focused_mut().open(path, &self.jobs);
                }
            }
            self.dirty = true;
            return true;
        }
        if let Some(tool) = self.layout.tool_at(position.x, position.y) {
            if let Some(id) = commands.find(tool.command()) {
                commands.run(id, self);
            }
            self.dirty = true;
            return true;
        }
        if let Some(button) = self.layout.button_at(position.x, position.y) {
            if let Some(id) = commands.find(button.command()) {
                commands.run(id, self);
            }
            return true;
        }
        false
    }

    /// A press on a divider, a tab, or a pane that does not have the keyboard.
    ///
    /// Returns whether the press was spent. A press in an unfocused pane's list
    /// is *not* spent: it moves the keyboard and then goes on to select the row
    /// it landed on, which is what a click into another pane means everywhere
    /// else on the desktop.
    fn press_in_the_tree(
        &mut self,
        position: Point,
        clicks: u8,
        commands: &Registry<Self>,
    ) -> bool {
        if let Some(divider) = self.divider_at(position) {
            self.placement.dragging = Some(divider);
            return true;
        }
        let Some(id) = self.leaf_at(position) else { return false };
        if self.panes.focus() != id {
            self.panes.focus_on(id);
            self.settle_panes();
        }
        if self.plus_at(position) {
            if let Some(command) = commands.find("tab.new") {
                commands.run(command, self);
            }
            return true;
        }
        let Some((index, closing)) = self.tab_at(position) else { return false };
        // The cross, and a double click anywhere on the tab — which is what a
        // person who has used a browser reaches for, and which is the same
        // action. Through the registry, like every other pointer route: a second
        // way of closing a tab must not be a second piece of code to keep in
        // step, which is rule 2.
        if closing || clicks >= 2 {
            self.panes.show_tab(id, index);
            self.settle_panes();
            // The tab this press started a possible drag on is about to stop
            // existing, and a drag of an index that is gone would move whatever
            // took its place.
            self.tab_press = None;
            if let Some(command) = commands.find("tab.close") {
                commands.run(command, self);
            }
        } else {
            if self.panes.show_tab(id, index) {
                self.settle_panes();
            }
            // A press on a tab may still become a drag. Remembered rather than
            // started: a click is never perfectly still, and a hand on a
            // touchpad moves a pixel while the button is down.
            self.tab_press = Some((id, index, position));
        }
        true
    }

    /// Moves a tab drag along, if one is in flight. Whether it took the pointer.
    ///
    /// **The overlay moves, not the tab.** A floating copy of a tab under the
    /// pointer is a second thing to draw and a second thing to get wrong, and
    /// where it would *land* is what a person is reading anyway.
    fn drag_a_tab(&mut self, position: Point) -> bool {
        let Some((_, _, from)) = self.tab_press else { return false };
        let moved = (position.x - from.x).abs().max((position.y - from.y).abs());
        if !self.tab_dragging && moved <= DRAG_THRESHOLD {
            return false;
        }
        self.tab_dragging = true;
        let drop = self.tab_drop_at(position);
        if self.placement.tab_drop != drop {
            self.placement.tab_drop = drop;
            self.dirty = true;
        }
        self.point(sk_plat::Cursor::Grabbing);
        true
    }

    /// Whether `position` is on the `+` at the end of a leaf's tabs.
    fn plus_at(&self, position: Point) -> bool {
        let Some(leaf) = self.leaf_layout_at(position) else { return false };
        let Some(count) = self.panes.leaf(leaf.id).map(|tabs| tabs.panes.len()) else {
            return false;
        };
        crate::panes::tab_plus(leaf.tabs, count).contains(position.x, position.y)
    }

    /// The layout of the leaf `position` is inside.
    fn leaf_layout_at(&self, position: Point) -> Option<crate::panes::PaneLayout> {
        self.placement
            .leaves
            .iter()
            .find(|leaf| leaf.area.contains(position.x, position.y))
            .copied()
    }

    /// Where a tab being dragged would land, if one is being dragged.
    fn tab_drop_at(&self, position: Point) -> Option<crate::panes::Drop> {
        let leaf = self.leaf_layout_at(position)?;
        let count = self.panes.leaf(leaf.id).map_or(0, |tabs| tabs.panes.len());
        Some(crate::panes::Drop::at(&leaf, count, position))
    }

    /// Finishes a tab drag: moves the tab where the overlay said it would go.
    fn drop_the_tab(&mut self, position: Point) {
        let Some((from, index, _)) = self.tab_press.take() else { return };
        let Some(drop) = self.tab_drop_at(position) else { return };
        // Dropping a tab back where it came from: the strip it is already in, at
        // the place it already occupies. Nothing to do, and doing it anyway
        // would move the keyboard and re-read a directory for a click.
        if drop == crate::panes::Drop::Strip(from, index)
            || drop == crate::panes::Drop::Strip(from, index + 1)
        {
            self.dirty = true;
            return;
        }
        // A leaf of one tab cannot be split with itself: taking the tab out
        // closes the leaf the split was going to happen to.
        let alone = self.panes.leaf(from).is_some_and(|tabs| tabs.panes.len() == 1);
        if alone && drop.leaf() == from {
            self.dirty = true;
            return;
        }
        let Some(pane) = self.panes.take_tab(from, index) else { return };
        match drop {
            crate::panes::Drop::Strip(id, at) => {
                // The index is from before the tab was taken out, so a move to
                // the right within one strip is one place further left now.
                let at = if id == from && at > index { at - 1 } else { at };
                self.panes.put_tab(id, at, pane);
            }
            crate::panes::Drop::Split(id, axis, before) => {
                self.panes.split_with(id, axis, before, pane);
            }
        }
        self.settle_panes();
    }

    /// The divider whose grab band covers `position`.
    fn divider_at(&self, position: Point) -> Option<crate::panes::SplitId> {
        self.placement
            .dividers
            .iter()
            .find(|divider| divider.area.contains(position.x, position.y))
            .map(|divider| divider.id)
    }

    /// The leaf `position` is inside.
    fn leaf_at(&self, position: Point) -> Option<crate::panes::PaneId> {
        self.placement
            .leaves
            .iter()
            .find(|leaf| leaf.area.contains(position.x, position.y))
            .map(|leaf| leaf.id)
    }

    /// The tab under `position`, and whether the press was on its cross.
    fn tab_at(&self, position: Point) -> Option<(usize, bool)> {
        let leaf =
            self.placement.leaves.iter().find(|leaf| leaf.area.contains(position.x, position.y))?;
        if !leaf.tabs.contains(position.x, position.y) {
            return None;
        }
        let count = self.panes.leaf(leaf.id)?.panes.len();
        crate::panes::tab_areas(leaf.tabs, count).enumerate().find_map(|(index, area)| {
            area.contains(position.x, position.y).then(|| {
                let closing = crate::panes::tab_close(area)
                    .is_some_and(|cross| cross.contains(position.x, position.y));
                (index, closing)
            })
        })
    }

    /// Moves the divider being dragged to where the pointer is.
    fn drag_the_divider(&mut self, id: crate::panes::SplitId, position: Point) {
        let Some(divider) = self.placement.dividers.iter().find(|it| it.id == id).copied() else {
            return;
        };
        // The ratio is of the split's own extent, which is the two panes either
        // side of this divider and not the window: a nested split's ratio is
        // measured inside its parent's half.
        let (first, second) = self.leaves_either_side(divider);
        let at = match divider.axis {
            crate::panes::Axis::Beside => {
                let span = second.right() - first.x;
                (position.x - first.x) / span.max(1.0)
            }
            crate::panes::Axis::Below => {
                let span = second.bottom() - first.y;
                (position.y - first.y) / span.max(1.0)
            }
        };
        self.panes.set_ratio(id, at);
        self.divide();
        self.dirty = true;
    }

    /// The outermost rectangles a divider divides.
    ///
    /// A divider's own band is between two *subtrees*, and the leaves in them
    /// may be several: the extent to measure a ratio against is the union, which
    /// is the first leaf that starts before the band and the last that ends
    /// after it.
    fn leaves_either_side(&self, divider: crate::panes::Divider) -> (Area, Area) {
        let band = divider.area;
        let (mut before, mut after) = (band, band);
        for leaf in &self.placement.leaves {
            let (starts_before, ends_after) = match divider.axis {
                crate::panes::Axis::Beside => {
                    (leaf.area.x < band.x, leaf.area.right() > band.right())
                }
                crate::panes::Axis::Below => {
                    (leaf.area.y < band.y, leaf.area.bottom() > band.bottom())
                }
            };
            let overlaps = match divider.axis {
                crate::panes::Axis::Beside => {
                    leaf.area.y < band.bottom() && leaf.area.bottom() > band.y
                }
                crate::panes::Axis::Below => {
                    leaf.area.x < band.right() && leaf.area.right() > band.x
                }
            };
            if !overlaps {
                continue;
            }
            if starts_before {
                before = before.union(leaf.area);
            }
            if ends_after {
                after = after.union(leaf.area);
            }
        }
        (before, after)
    }

    /// What every change to the tree owes the rest of the window.
    ///
    /// The geometry moved, the keyboard may be somewhere else, and a pane that
    /// has just become visible needs watching. One function rather than four
    /// lines at nine call sites, because the site that forgets one of them is a
    /// pane that draws where it no longer is.
    fn settle_panes(&mut self) {
        self.abandon_fields();
        self.divide();
        self.retarget_watches();
        self.dirty = true;
    }

    /// Shows or hides the list of places.
    pub(crate) fn toggle_sidebar(&mut self) {
        self.sidebar.open = !self.sidebar.open;
        self.sidebar.hover = None;
        self.relayout();
        self.dirty = true;
    }

    /// Opens the filter, or closes and clears it if it is already open.
    pub(crate) fn toggle_filter(&mut self) {
        self.claim_keyboard(Claim::Filter);
        if self.filtering {
            self.filtering = false;
            self.panes.focused_mut().set_filter(String::new(), &self.jobs);
        } else {
            self.filtering = true;
        }
        self.dirty = true;
    }

    /// Opens every selected directory in a tab, and stays where it is.
    ///
    /// **Enabled on the focused entry, run over the whole selection.** Whether
    /// any of two hundred thousand selected entries is a directory is a walk of
    /// the selection, and `enabled` runs every frame — so the question asked
    /// sixty times a second is the O(1) one, about the entry `Enter` would open.
    /// Directories sort first, so the topmost selected entry is a directory
    /// whenever one is selected at all, and the two answers differ only for a
    /// selection made with `Ctrl` from the bottom up. It differs by doing more
    /// than the row promised, never less.
    pub(crate) fn open_in_new_tab(&mut self) {
        let paths = self.selected_directories();
        if paths.is_empty() {
            return;
        }
        self.panes.open_tabs_behind(paths, &self.jobs);
        self.settle_panes();
    }

    /// Opens the focused directory in a new pane beside this one.
    pub(crate) fn open_beside(&mut self) {
        self.open_in_a_pane(crate::panes::Axis::Beside);
    }

    /// And below it.
    pub(crate) fn open_below(&mut self) {
        self.open_in_a_pane(crate::panes::Axis::Below);
    }

    /// **One pane, and not one per selected directory.** Three tabs are a queue
    /// and three panes are a shattered window, so this takes the entry `Enter`
    /// would open and nothing else — which is also what makes it agree exactly
    /// with the `enabled` that dims it.
    fn open_in_a_pane(&mut self, axis: crate::panes::Axis) {
        let Some(path) = self.focused_directory() else { return };
        self.panes.open_split(path, axis, &self.jobs);
        self.settle_panes();
    }

    /// Whether the entry `Enter` would open is a directory.
    #[must_use]
    pub(crate) fn focused_is_a_directory(&self) -> bool {
        let pane = self.panes.focused();
        pane.focused().is_some_and(|id| pane.listing.flags(id).has(sk_model::Flags::DIR))
    }

    /// That entry's path, when it is a directory.
    ///
    /// Kept apart from the predicate above rather than written as
    /// `focused_directory().is_some()`, because the predicate is a command's
    /// `enabled` and runs every frame: joining a path sixty times a second to
    /// answer a yes-or-no question is an allocation per frame for nothing.
    fn focused_directory(&self) -> Option<PathBuf> {
        let pane = self.panes.focused();
        let id = pane.focused()?;
        pane.listing.flags(id).has(sk_model::Flags::DIR).then(|| pane.path_of(id))
    }

    /// The selected directories in list order, or the focused one when nothing
    /// is selected.
    fn selected_directories(&self) -> Vec<PathBuf> {
        let pane = self.panes.focused();
        let is_directory = |id| pane.listing.flags(id).has(sk_model::Flags::DIR);
        let mut paths: Vec<PathBuf> = pane
            .selection
            .in_order(pane.listing.order())
            .filter(|id| is_directory(*id))
            .map(|id| pane.path_of(id))
            .collect();
        if let Some(id) = pane.focused().filter(|_| paths.is_empty()).filter(|id| is_directory(*id))
        {
            paths.push(pane.path_of(id));
        }
        paths
    }

    /// Opens the focused entry: a directory in this window, a file in whatever
    /// the desktop says owns it.
    pub(crate) fn open_focused(&mut self) {
        if self.panes.focused_mut().open_focused(&self.jobs) {
            return;
        }
        let Some(id) = self.panes.focused().focused() else { return };
        let path = self.panes.focused().path_of(id);
        self.launch(path);
    }

    /// Puts the selected paths on the clipboard, to be copied.
    pub(crate) fn copy_selection(&mut self) {
        self.publish_selection(Intent::Copy);
    }

    /// Puts them there to be moved.
    pub(crate) fn cut_selection(&mut self) {
        self.publish_selection(Intent::Cut);
    }

    /// Publishes the selection with the verb a peer will read.
    fn publish_selection(&mut self, intent: Intent) {
        let paths = self.selected_paths();
        let Some(first) = paths.first().cloned() else { return };
        if !self.window.capabilities().clipboard {
            self.notice =
                Some((sk_model::i18n::lookup("error.no_clipboard").to_owned(), Instant::now()));
            return;
        }
        let count = paths.len();
        self.window.set_selection(Selection::Clipboard, Content::paths(&paths, intent));
        // The primary selection too, so a middle click in a terminal pastes what
        // was just copied. Hyprland discards this silently unless
        // `misc:middle_click_paste` is on, which is a documented limitation with
        // no fallback and no way to detect.
        self.window.set_selection(Selection::Primary, Content::paths(&paths, intent));
        let (one, many) = match intent {
            Intent::Copy => ("notice.copied_one", "notice.copied"),
            Intent::Cut => ("notice.cut_one", "notice.cut"),
        };
        let message = if count == 1 {
            let name = first.file_name().map(|name| name.to_string_lossy().into_owned());
            sk_model::i18n::format(one, &[&name.unwrap_or_default()])
        } else {
            sk_model::i18n::format(many, &[&count])
        };
        self.notice = Some((message, Instant::now()));
        self.cutting = matches!(intent, Intent::Cut) && count > 0;
    }

    /// Puts the selected paths on the clipboard and marks them to be moved.
    ///
    /// Stops the operation in flight and drops the rest of the queue.
    pub(crate) fn cancel_operation(&mut self) {
        // **Whatever the strip is showing.** Stop has one meaning — stop the
        // operation that is running — and which operation that is, is not the
        // person's problem. An index build is a long operation and [rule
        // 7](../../../docs/03-principles.md) says a long operation is
        // cancellable, so it answers to the same command the copy does rather
        // than to a second one nobody would look for.
        if self.indexes.building().is_some() {
            self.indexes.stop();
            self.notice =
                Some((sk_model::i18n::lookup("notice.cancelled").to_owned(), Instant::now()));
            self.dirty = true;
            return;
        }
        if !self.edits.cancellable() {
            return;
        }
        self.edits.cancel();
        self.notice = Some((sk_model::i18n::lookup("notice.cancelled").to_owned(), Instant::now()));
        self.dirty = true;
    }

    /// Whether there is anything worth stopping.
    #[must_use]
    pub fn can_cancel(&self) -> bool {
        self.edits.cancellable() || self.indexes.building().is_some()
    }

    /// Whether there is anything worth holding still.
    ///
    /// A trash is one syscall with nothing between its start and its end to
    /// pause at, and a queue that is already paused is worth un-pausing.
    ///
    /// **Not the same set as [`Self::can_cancel`] any more.** An index build is
    /// cancellable and is not pausable: it is seconds long — 652 ms for half a
    /// million entries, measured — so a latch on it would be a control nobody
    /// has time to reach, and a Pause drawn on the strip that did nothing would
    /// be worse than no Pause at all.
    #[must_use]
    pub fn can_pause(&self) -> bool {
        self.edits.cancellable()
    }

    /// Holds the queue where it is, or lets it go.
    pub(crate) fn toggle_pause(&mut self) {
        let held = !self.edits.held();
        self.edits.hold(held);
        self.dirty = true;
    }

    /// Whether there is anything on the clipboard worth pasting.
    ///
    /// Asked of what the peer *offers*, not of what it holds: reading the bytes
    /// means a pipe and a job, and a command's `enabled` runs every frame. So the
    /// answer is "the clipboard offers something that could name a file", and a
    /// paste that then finds none says so in a notice.
    #[must_use]
    pub fn can_paste(&self) -> bool {
        let offered = self.window.clipboard().offered();
        Self::PASTE_TYPES.iter().any(|want| offered.iter().any(|have| have == want))
    }

    /// The clipboard types a paste will take, best first.
    ///
    /// `x-special/gnome-copied-files` leads because it is the only one that says
    /// whether the files were copied or **cut** — the rest name files and nothing
    /// else, and taking one of them turns somebody's cut into a copy.
    const PASTE_TYPES: [&str; 5] = [
        "x-special/gnome-copied-files",
        "text/uri-list",
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
    ];

    /// Copies or moves what is on the clipboard into the directory being shown.
    pub(crate) fn paste_files(&mut self) {
        let offered = self.window.clipboard().offered().to_vec();
        let Some(mime) =
            Self::PASTE_TYPES.into_iter().find(|want| offered.iter().any(|have| have == want))
        else {
            self.notice = Some((
                sk_model::i18n::lookup("error.clipboard_no_files").to_owned(),
                Instant::now(),
            ));
            return;
        };
        // **Not a silent return.** The display server can refuse a conversion —
        // the owner went away between the offer and the ask, or the type it
        // advertised is one it will not actually serve — and a paste that does
        // nothing with nothing to show for it is indistinguishable from a key
        // that never arrived.
        let Some(fd) = self.window.request_selection(Selection::Clipboard, mime) else {
            self.notice =
                Some((sk_model::i18n::format("error.clipboard_refused", &[&mime]), Instant::now()));
            return;
        };
        // Our own mark, for a clipboard we published ourselves. A peer's verb
        // arrives inside the bytes and overrides it below.
        let moving = self.cutting;
        let carries_verb = mime == "x-special/gnome-copied-files";
        let sender = self.replies.paste_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let mut bytes = Vec::new();
            let mut file = std::fs::File::from(fd);
            let _ = std::io::Read::read_to_end(&mut file, &mut bytes);

            // **The peer's verb wins where there is one.** A cut in Nautilus
            // arrives as `x-special/gnome-copied-files` opening with `cut`, and
            // pasting it as a copy leaves the original behind — which is a paste
            // that silently did half of what was asked.
            if carries_verb && let Some((intent, paths)) = sk_plat::decode_copied_files(&bytes) {
                let paths: Vec<PathBuf> =
                    paths.into_iter().filter(|path| path.symlink_metadata().is_ok()).collect();
                let moving = matches!(intent, sk_plat::Intent::Cut);
                let reply =
                    if paths.is_empty() { Pasted::NotFiles } else { Pasted::Files(paths, moving) };
                if sender.send(reply).is_ok() {
                    wake.wake();
                }
                return;
            }

            let text = String::from_utf8_lossy(&bytes);
            // Every line, not just the first: a copy of nine files is nine lines,
            // and a paste that takes one of them is a paste that silently drops
            // eight. `symlink_metadata`, so a broken link is still a thing to
            // move rather than a line to discard.
            let paths: Vec<PathBuf> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .filter_map(|line| sk_plat::decode_uri(line).or_else(|| Some(PathBuf::from(line))))
                .filter(|path| path.is_absolute() && path.symlink_metadata().is_ok())
                .collect();
            let reply =
                if paths.is_empty() { Pasted::NotFiles } else { Pasted::Files(paths, moving) };
            if sender.send(reply).is_ok() {
                wake.wake();
            }
        });
    }

    /// Puts the focused entry's full path on the clipboard as plain text.
    pub(crate) fn copy_path(&mut self) {
        let paths = self.selected_paths();
        let Some(path) = paths.first() else { return };
        let text = path.to_string_lossy().into_owned();
        self.window.set_selection(Selection::Clipboard, Content::text(&text));
        self.window.set_selection(Selection::Primary, Content::text(&text));
        self.notice =
            Some((sk_model::i18n::format("notice.copied_path", &[&text]), Instant::now()));
    }

    /// Puts the focused file's text on the clipboard.
    ///
    /// **The whole point is not opening it.** A path or a snippet you want in a
    /// message is two keys instead of starting an editor, waiting for it, selecting
    /// all, copying, and closing it again.
    ///
    /// The read is a job, because it is a read: rule 1 has no exemption for a
    /// small file, and a small file on an unresponsive mount takes as long to
    /// open as a large one. Bounded at [`sk_fs::TEXT_LIMIT`], and the job decides
    /// whether the bytes are really text — `enabled` only guesses from the name.
    pub(crate) fn copy_content(&mut self) {
        let Some(id) = self.panes.focused().focused() else { return };
        let name = self.panes.focused().listing.name(id).to_owned();
        let path = self.panes.focused().path.join(&name);
        let sender = self.replies.content_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let outcome = sk_fs::read_text(&path, sk_fs::TEXT_LIMIT);
            if sender.send(Copied { name, outcome }).is_ok() {
                wake.wake();
            }
        });
    }

    /// Whether the focused entry is worth offering to copy as text.
    ///
    /// **A guess from the name, and deliberately a generous one.** `by_name` is a
    /// hash lookup with no syscall behind it, which is what lets this run in an
    /// `enabled` predicate; `of` would read the first bytes of the file, and rule
    /// 1 has no exemption for that either. A name no glob matches — `notes`, or a
    /// `Makefile` on a machine with an old shared-mime-info — is offered rather
    /// than dimmed, and the read says so if it turns out not to be text. A name
    /// that *is* matched and is not text — `.png`, `.pdf` — is dimmed, because a
    /// menu row that cannot work is worth more dimmed than absent.
    #[must_use]
    pub(crate) fn focused_is_text(&self) -> bool {
        let Some(id) = self.panes.focused().focused() else { return false };
        // A directory, a device, a socket, a FIFO, or a link to nothing: the read
        // would refuse each of them, and dimming is a better way to say so than a
        // message after the fact.
        let flags = self.panes.focused().listing.flags(id);
        for refused in [sk_model::Flags::DIR, sk_model::Flags::SPECIAL, sk_model::Flags::BROKEN] {
            if flags.has(refused) {
                return false;
            }
        }
        let name = self.panes.focused().listing.name(id);
        self.desktop
            .as_deref()
            .and_then(|desktop| desktop.mime.by_name(name))
            .is_none_or(sk_desktop::is_text)
    }

    /// Takes delivery of a file's text and puts it on the clipboard.
    fn drain_content(&mut self) {
        let Ok(copied) = self.replies.content_rx.try_recv() else { return };
        let message = match copied.outcome {
            Ok(text) => {
                let size = sk_ui::format::size(text.len() as u64);
                // Both selections, like every other copy here: the clipboard for
                // `Ctrl+V` and the primary for a middle click.
                self.window.set_selection(Selection::Clipboard, Content::text(&text));
                self.window.set_selection(Selection::Primary, Content::text(&text));
                sk_model::i18n::format("notice.copied_content", &[&size, &copied.name])
            }
            Err(problem) => problem.message(),
        };
        self.notice = Some((message, Instant::now()));
        self.dirty = true;
    }

    /// Reads the clipboard and, if it names a folder, goes there.
    ///
    /// The read happens on a job: the peer writes to the pipe whenever it likes
    /// and may never write at all, and waiting for it here is rule 1's canonical
    /// failure with a different cause.
    pub(crate) fn paste_location(&mut self) {
        // The type the peer prefers, of the ones we can make sense of.
        let wanted = ["text/uri-list", "text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
        let offered = self.window.clipboard().offered().to_vec();
        let Some(mime) = wanted.iter().find(|want| offered.iter().any(|have| have == *want)) else {
            self.notice = Some((
                sk_model::i18n::lookup("error.clipboard_not_a_folder").to_owned(),
                Instant::now(),
            ));
            return;
        };
        let Some(fd) = self.window.request_selection(Selection::Clipboard, mime) else { return };
        let sender = self.replies.paste_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let mut bytes = Vec::new();
            let mut file = std::fs::File::from(fd);
            let _ = std::io::Read::read_to_end(&mut file, &mut bytes);
            let text = String::from_utf8_lossy(&bytes);
            let first = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
            // The `is_dir` happens here, not on the loop: this is a path another
            // application chose, and it can name a hung mount.
            // **Canonical, not as written.** The clipboard can hold a relative
            // path from somebody's terminal or one with `..` in it, and both
            // resolve — `is_dir` against this process's working directory, and
            // `Path::components` without folding `..` at all. The breadcrumb then
            // named a directory that was not the one being shown, and clicking a
            // component went somewhere else again.
            let pasted = sk_plat::decode_uri(first)
                .or_else(|| (!first.is_empty()).then(|| PathBuf::from(first)))
                .and_then(|path| std::fs::canonicalize(path).ok())
                .filter(|path| path.is_dir())
                .map_or(Pasted::NotAFolder, Pasted::Folder);
            if sender.send(pasted).is_ok() {
                wake.wake();
            }
        });
    }

    /// The full paths of what is selected, or of the cursor if nothing is.
    ///
    /// **In display order.** `Selection::iter` walks a bitset indexed by
    /// `EntryId`, which is `getdents64` order — so sorting by size and copying
    /// handed the peer the paths shuffled with respect to the list they were
    /// taken from, and nothing anywhere said so.
    fn selected_paths(&self) -> Vec<PathBuf> {
        let listing = &self.panes.focused().listing;
        let mut paths: Vec<PathBuf> = self
            .panes
            .focused()
            .selection
            .in_order(listing.order())
            .map(|id| self.panes.focused().path_of(id))
            .collect();
        if let Some(id) = self.panes.focused().focused().filter(|_| paths.is_empty()) {
            paths.push(self.panes.focused().path_of(id));
        }
        paths
    }

    /// Starts the application that owns `path`.
    ///
    /// Deferred if the databases have not arrived yet, which is a window of a few
    /// milliseconds after start-up. Dropping the request instead would make a
    /// double click do nothing exactly once, which is the kind of thing a person
    /// blames themselves for.
    fn launch(&mut self, path: PathBuf) {
        let Some(desktop) = self.desktop.clone() else {
            self.open_owed = Some(path);
            return;
        };
        // **All of it is a job.** Typing the file opens it when the name says
        // nothing; finding the handler reads `.desktop` files across every XDG
        // data directory and stats every `PATH` entry for each candidate; and
        // `spawn` is `fork` plus `exec`, which with a `pre_exec` set blocks the
        // parent on the child's exec-status pipe. Enter on an extensionless file
        // living on a stalled mount used to stop the window painting until the
        // mount answered.
        let sender = self.replies.launched_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
            let mime = desktop.mime.of(&path, false);
            let launched = match desktop.apps.default_for(&mime) {
                None => Launched {
                    notice: sk_model::i18n::format("error.no_handler", &[&mime]),
                    child: None,
                },
                Some(entry) => match Launcher::start(&entry, std::slice::from_ref(&path)) {
                    Ok(child) => Launched {
                        notice: sk_model::i18n::format("notice.opening", &[&entry.name]),
                        child: Some(child),
                    },
                    Err(sk_desktop::launch::Refused::NoTerminal) => Launched {
                        notice: sk_model::i18n::format("error.no_terminal", &[&entry.name]),
                        child: None,
                    },
                    Err(_) => Launched {
                        notice: sk_model::i18n::format("error.launch_failed", &[&entry.name]),
                        child: None,
                    },
                },
            };
            if sender.send(launched).is_ok() {
                wake.wake();
            }
        });
    }

    /// Asks which application should open the focused file.
    ///
    /// The list is what this machine's `mimeapps.list` and `.desktop` files say,
    /// so it is read on a job — resolving the type opens the file when the name
    /// says nothing, and resolving handlers reads every XDG data directory.
    pub(crate) fn open_with(&mut self) {
        if self.openers.take().is_some() {
            self.dirty = true;
            return;
        }
        let Some(path) = self.selected_paths().into_iter().next() else { return };
        let Some(desktop) = self.desktop.clone() else { return };
        let sender = self.replies.openers_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
            let mime = desktop.mime.of(&path, false);
            let rows = desktop.apps.handlers_for(&mime);
            let list = crate::openers::Openers::new(path, mime, rows);
            if sender.send(list).is_ok() {
                wake.wake();
            }
        });
    }

    /// One key to the Open With list. Returns whether it was consumed.
    fn openers_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.openers = None,
            Key::Named(Named::Down) if bare => {
                if let Some(list) = self.openers.as_mut() {
                    list.step(1);
                }
            }
            Key::Named(Named::Up) if bare => {
                if let Some(list) = self.openers.as_mut() {
                    list.step(-1);
                }
            }
            Key::Named(Named::Enter) => self.follow_openers(),
            _ => {
                // The diagnostic report passes through, for the reason it passes
                // through an open menu: it is the only way the harness sees this.
                let diagnostic = commands
                    .resolve(event, Scope::Global)
                    .and_then(|id| commands.describe(id))
                    .is_some_and(|command| command.id == "help.dump");
                if diagnostic {
                    return false;
                }
                self.openers = None;
            }
        }
        self.dirty = true;
        true
    }

    /// A key while a name-taken question is open. Whether it was taken.
    ///
    /// Arrows walk the four answers, `Space` toggles whether the answer covers
    /// the rest of the queue, `Enter` gives it and `Escape` stops.
    fn clash_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.answer_the_clash(crate::edit::Answer::Stop),
            Key::Named(Named::Down | Named::Right) if bare => self.edits.step_answer(1),
            Key::Named(Named::Up | Named::Left) if bare => self.edits.step_answer(-1),
            Key::Character(' ') if bare => self.edits.for_the_rest = !self.edits.for_the_rest,
            Key::Named(Named::Enter) => {
                let answer = self.edits.chosen_answer();
                self.answer_the_clash(answer);
            }
            _ => {
                // The diagnostic report passes through, for the reason it passes
                // through an open menu: it is the only way the harness sees this.
                let diagnostic = commands
                    .resolve(event, Scope::Global)
                    .and_then(|id| commands.describe(id))
                    .is_some_and(|command| command.id == "help.dump");
                return !diagnostic;
            }
        }
        self.dirty = true;
        true
    }

    /// Gives the answer and lets the queue go on.
    fn answer_the_clash(&mut self, answer: crate::edit::Answer) {
        self.edits.answer(answer, &self.jobs);
        if !self.edits.asking() && !self.edits.busy() {
            self.notice = Some((
                sk_model::i18n::lookup(match answer {
                    crate::edit::Answer::Stop => "notice.cancelled",
                    _ => "notice.done",
                })
                .to_owned(),
                Instant::now(),
            ));
        }
        self.panes.focused_mut().refresh(&self.jobs);
        self.dirty = true;
    }

    /// A press while a name-taken question is open. Whether it was taken.
    ///
    /// **Always taken.** Every other overlay here closes on a press that misses
    /// it; this one has no dismissal, so a press outside it does nothing at all
    /// rather than leaving a stopped queue with no way back to the question.
    fn press_in_the_clash(&mut self, position: Point) -> bool {
        if !self.edits.asking() {
            return false;
        }
        if let Some(row) = self.hits.answers.iter().position(|a| a.contains(position.x, position.y))
        {
            self.edits.answering = row;
            let answer = self.edits.chosen_answer();
            self.answer_the_clash(answer);
        } else if self.hits.for_the_rest.is_some_and(|a| a.contains(position.x, position.y)) {
            self.edits.for_the_rest = !self.edits.for_the_rest;
            self.dirty = true;
        }
        true
    }

    /// A press while the Open With list is up. Whether it was taken.
    fn press_in_the_openers(&mut self, position: Point) -> bool {
        if self.openers.is_none() {
            return false;
        }
        let row = self.hits.openers.iter().position(|area| area.contains(position.x, position.y));
        // A press anywhere but a row closes it and does nothing else, which is
        // the rule every overlay here follows.
        let Some(offset) = row else {
            self.openers = None;
            self.dirty = true;
            return true;
        };
        if let Some(list) = self.openers.as_mut() {
            list.hover(offset);
        }
        self.follow_openers();
        true
    }

    /// Takes delivery of the Open With list.
    fn drain_openers(&mut self) {
        let Ok(list) = self.replies.openers_rx.try_recv() else { return };
        if list.rows.is_empty() {
            self.notice =
                Some((sk_model::i18n::format("error.no_handler", &[&list.mime]), Instant::now()));
            self.dirty = true;
            return;
        }
        self.openers = Some(list);
        self.dirty = true;
    }

    /// Launches the highlighted application and closes the list.
    fn follow_openers(&mut self) {
        let Some(list) = self.openers.take() else { return };
        self.dirty = true;
        let Some(entry) = list.chosen().cloned() else { return };
        let path = list.path.clone();
        let sender = self.replies.launched_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
            let launched = match Launcher::start(&entry, std::slice::from_ref(&path)) {
                Ok(child) => Launched {
                    notice: sk_model::i18n::format("notice.opening", &[&entry.name]),
                    child: Some(child),
                },
                Err(sk_desktop::launch::Refused::NoTerminal) => Launched {
                    notice: sk_model::i18n::format("error.no_terminal", &[&entry.name]),
                    child: None,
                },
                Err(_) => Launched {
                    notice: sk_model::i18n::format("error.launch_failed", &[&entry.name]),
                    child: None,
                },
            };
            if sender.send(launched).is_ok() {
                wake.wake();
            }
        });
    }

    /// Opens the Properties panel over what is selected.
    ///
    /// A second press closes it, like every other overlay here. The facts are
    /// read on a job — `statx` on a hung mount is exactly what rule 1 exists for
    /// — so the panel appears with its title and fills in.
    pub(crate) fn toggle_properties(&mut self) {
        if self.properties.take().is_some() {
            self.dirty = true;
            return;
        }
        let paths = self.selected_paths();
        let paths = if paths.is_empty() {
            // Nothing selected means the folder being shown, which is what every
            // file manager's Properties does with an empty selection.
            vec![self.panes.focused().path.clone()]
        } else {
            paths
        };
        for path in &paths {
            let sender = self.replies.facts_tx.clone();
            let wake = self.waker.handle();
            let path = path.clone();
            self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
                let Some(facts) = sk_fs::facts::describe(&path) else { return };
                if sender.send(facts).is_ok() {
                    wake.wake();
                }
            });
        }
        self.properties = Some(crate::properties::Properties::new(paths));
        self.dirty = true;
    }

    /// A drag of files: where it would land, and what it is carrying.
    ///
    /// **Which pane as well as which row.** A scenario dragging between two
    /// panes cannot tell "the right pane's row 2" from "the left pane's row 2",
    /// and that is the whole thing it is asserting about.
    fn drag_report(&self) -> String {
        let row = self.drop_row().map_or(-1, |(_, row)| i64::try_from(row).unwrap_or(-1));
        let leaf = self
            .drop_row()
            .map(|(id, _)| id)
            .or_else(|| self.drop_tab().map(|(id, _)| id))
            .or_else(|| self.drop_pane())
            .and_then(|id| self.placement.leaves.iter().position(|leaf| leaf.id == id))
            .map_or(-1, |at| i64::try_from(at).unwrap_or(-1));
        let carrying = self
            .files_drag
            .as_ref()
            .filter(|drag| drag.active)
            .and_then(|drag| drag.paths.first())
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tab = self.drop_tab().map_or(-1, |(_, at)| i64::try_from(at).unwrap_or(-1));
        format!(
            "drop_row={row} drop_leaf={leaf} drop_tab={tab} drop_whole={} dragging={} \
carrying={carrying:?}",
            i32::from(self.drop_pane().is_some()),
            self.files_drag.as_ref().is_some_and(|drag| drag.active),
        )
    }

    /// The subtree search, which changes no listing while it is open and is
    /// otherwise invisible from outside: how many results, and which one is on.
    fn search_report(&self) -> String {
        let Some(search) = self.search.as_ref() else {
            return "searching=false search_query=\"\" search_hits=0 search_done=false \
search_active=\"\""
                .to_owned();
        };
        let active = search
            .chosen()
            .map(|hit| crate::panes::title_of(&hit.path).to_owned())
            .unwrap_or_default();
        format!(
            "searching=true search_query={:?} search_hits={} search_done={} \
search_active={active:?}",
            search.query.text,
            search.found.hits.len(),
            search.found.complete,
        )
    }

    /// Opens the search over everything under the folder being shown.
    pub(crate) fn open_search(&mut self) {
        if self.search.is_some() {
            self.close_search();
            return;
        }
        self.claim_keyboard(Claim::Search);
        let root = self.panes.focused().path.clone();
        // Asked for here rather than on the first keystroke, so the read is over
        // by the time anybody has typed enough to search for.
        self.want_index(&root);
        self.search = Some(crate::search::Search::new(root));
        self.dirty = true;
    }

    /// Closes it and stops the walk.
    pub(crate) fn close_search(&mut self) {
        if let Some(search) = self.search.take() {
            search.cancel.cancel();
        }
        self.dirty = true;
    }

    /// Starts a walk for whatever is typed now, stopping the one before it.
    ///
    /// **A walk per keystroke, and the old one is cancelled rather than left to
    /// finish.** A tree of a hundred thousand entries takes a second or two, and
    /// six characters typed at speed would otherwise put six walks on the pool at
    /// once — five of them for questions nobody is asking any more.
    fn search_again(&mut self) {
        let Some(search) = self.search.as_mut() else { return };
        let query = search.query.text.clone();
        if query == search.asked {
            return;
        }
        search.cancel.cancel();
        search.asked.clone_from(&query);
        search.found = sk_index::Found::default();
        search.active = 0;
        search.scroll = 0;
        if query.is_empty() {
            self.dirty = true;
            return;
        }
        let cancel = sk_util::Cancel::new();
        search.cancel = cancel.clone();
        let root = search.root.clone();
        let sender = self.replies.found_tx.clone();
        let wake = self.waker.handle();
        let lane = self.panes.focused().lane();
        // **The index if there is one, the walk if there is not.** ADR 0008's
        // third constraint is that a missing index is a performance event and
        // never a correctness one, so these two arms answer the same question
        // and the caller cannot tell which ran.
        let held = self.indexes.covering(&root).map(std::sync::Arc::clone);
        let pool = std::sync::Arc::clone(&self.jobs.pool);
        // Recorded now rather than on the reply, because the summary has to say
        // which of the two answered and a `Found` cannot: the whole point of ADR
        // 0008's third constraint is that the two are indistinguishable.
        if let Some(search) = self.search.as_mut() {
            search.indexed = held.as_ref().map(|it| it.built());
        }
        self.jobs.pool.spawn(lane, cancel, move |cancel| {
            let mut say = |found: &sk_index::Found| {
                if sender.send((query.clone(), found.clone())).is_ok() {
                    wake.wake();
                }
            };
            match held {
                Some(index) => {
                    let found = sk_index::store::search(&index, &query, &pool, cancel);
                    say(&found);
                }
                None => sk_index::find(&root, &query, cancel, &mut say),
            }
        });
        self.dirty = true;
    }

    /// Asks for the index covering `root`, unless it has already been asked for.
    ///
    /// **On a job, because it is a fifty-megabyte read.** Once per root rather
    /// than once per keystroke: the loaded index is what every search of that
    /// folder then scans, and re-reading it per character would be the walk this
    /// feature exists to replace, with extra steps.
    fn want_index(&mut self, root: &std::path::Path) {
        if self.indexes.covering(root).is_some() || self.indexes.asked_about(root) {
            return;
        }
        self.indexes.asking(root);
        let sender = self.replies.index_tx.clone();
        let wake = self.waker.handle();
        let root = root.to_path_buf();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let held = sk_index::store::load(&root).map(std::sync::Arc::new);
            if sender.send(crate::indexing::Note::Loaded(held)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Builds an index for the folder being shown, and saves it.
    ///
    /// [ADR 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md)
    /// is that nothing builds one on its own: a background indexer is a second
    /// lifecycle and the design most likely to be blamed for a machine feeling
    /// slow, so this is a thing a person asks for and can stop.
    pub(crate) fn build_index(&mut self) {
        let root = self.panes.focused().path.clone();
        let cancel = sk_util::Cancel::new();
        self.indexes.begins(&root, cancel.clone());
        let sender = self.replies.index_tx.clone();
        let wake = self.waker.handle();

        // **The counting pass, beside the build rather than before it.** This is
        // the copy engine's bargain and it is what gives the bar a denominator:
        // counting first would make every index wait on a walk, and estimating
        // from a partial count is the bar that reaches 40% and starts over.
        // Cheap enough to be worth doing twice because it reads names and does
        // not `statx` each entry — see `sk_index::store::count`.
        let counting = (sender.clone(), self.waker.handle(), root.clone());
        self.jobs.pool.spawn(sk_util::Lane::Parallel, cancel.clone(), move |cancel| {
            let (sender, wake, root) = counting;
            sk_index::store::count(&root, cancel, &mut |seen, complete| {
                if sender.send(crate::indexing::Note::Total(seen, complete)).is_ok() {
                    wake.wake();
                }
            });
        });

        self.jobs.pool.spawn(sk_util::Lane::Parallel, cancel, move |cancel| {
            let mut said = |count: usize| {
                if sender.send(crate::indexing::Note::Counting(count)).is_ok() {
                    wake.wake();
                }
            };
            let built = sk_index::store::build(&root, cancel, &mut said);
            // Saved before it is delivered, so the window that asked for it and
            // the next window to open the folder get the same answer.
            let held = built.map(|index| {
                let _ = sk_index::store::save(&index);
                std::sync::Arc::new(index)
            });
            if sender.send(crate::indexing::Note::Built(held)).is_ok() {
                wake.wake();
            }
        });
        self.dirty = true;
    }

    /// Deletes the index for the folder being shown.
    ///
    /// ADR 0008 asks for "a way to turn it off and delete it", and a filename
    /// index is a list of everything a person has. This is the delete half; not
    /// building one is the other.
    pub(crate) fn forget_index(&mut self) {
        let root = self.panes.focused().path.clone();
        self.indexes.stop();
        self.indexes.drop_for(&root);
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let _ = sk_index::store::forget(&root);
        });
        self.dirty = true;
    }

    /// Whether an index build is running.
    #[must_use]
    pub(crate) fn indexing(&self) -> bool {
        self.indexes.building().is_some()
    }

    /// Whether the folder being shown has an index loaded.
    #[must_use]
    pub(crate) fn indexed(&self) -> bool {
        self.indexes.covering(&self.panes.focused().path).is_some()
    }

    /// Takes delivery of index reports.
    fn drain_index(&mut self) {
        while let Ok(note) = self.replies.index_rx.try_recv() {
            if self.indexes.arrived(note) {
                self.dirty = true;
            }
        }
    }

    /// Takes delivery of search reports.
    fn drain_found(&mut self) {
        while let Ok((query, found)) = self.replies.found_rx.try_recv() {
            let Some(search) = self.search.as_mut() else { continue };
            if search.arrived(&query, found) {
                self.dirty = true;
            }
        }
    }

    /// Whether a search has found anything to put in the pane.
    #[must_use]
    pub(crate) fn search_has_results(&self) -> bool {
        self.search.as_ref().is_some_and(|search| !search.found.hits.is_empty())
    }

    /// Puts every result into the pane as rows.
    ///
    /// **The other half of what a search is for.** [`Self::follow_search`] is for
    /// one that was a way of getting somewhere; this is for one that was a way of
    /// finding a *set* of files — and once they are rows, every command in the
    /// registry already works on them, because a result carries the directory it
    /// came from and `Pane::path_of` is the only thing that ever needed to know.
    pub(crate) fn flatten_search(&mut self) {
        let Some(search) = self.search.as_ref() else { return };
        if search.found.hits.is_empty() {
            return;
        }
        let (query, listing) = (search.query.text.clone(), search.listing());
        self.close_search();
        self.panes.focused_mut().show_results(query, listing);
        self.settle_panes();
    }

    /// Goes to the highlighted result: its folder, with the cursor on it.
    fn follow_search(&mut self) {
        let Some(hit) = self.search.as_ref().and_then(crate::search::Search::chosen) else {
            return;
        };
        let (path, name) = (hit.path.clone(), hit.path.file_name().map(std::ffi::OsStr::to_owned));
        let Some(parent) = path.parent().map(std::path::Path::to_path_buf) else { return };
        self.close_search();
        // Already there: navigating would re-read the directory the results came
        // from, which is a listing that is already on screen.
        if parent == self.panes.focused().path {
            self.panes.focused_mut().refresh(&self.jobs);
        } else {
            self.panes.focused_mut().open(parent, &self.jobs);
        }
        if let Some(name) = name {
            self.panes.focused_mut().select_after(name.to_string_lossy().into_owned());
        }
        self.settle_panes();
    }

    /// A key while the search is open. Whether it was taken.
    fn search_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.close_search(),
            Key::Named(Named::Down) if bare => self.step_search(1),
            Key::Named(Named::Up) if bare => self.step_search(-1),
            // The registry holds this too, in the field scope. Both routes run
            // the same function: a chord the search answers itself and a palette
            // row that runs a command must not be two implementations.
            Key::Named(Named::Enter) if event.mods.ctrl => self.flatten_search(),
            Key::Named(Named::Enter) => self.follow_search(),
            _ => {
                let diagnostic = commands
                    .resolve(event, Scope::Global)
                    .and_then(|id| commands.describe(id))
                    .is_some_and(|command| command.id == "help.dump");
                if diagnostic {
                    return false;
                }
                // Everything else is typing, which is what a field does with it.
                let Some(search) = self.search.as_mut() else { return false };
                if !search.query.key(event) {
                    return false;
                }
                self.search_again();
                self.dirty = true;
                return true;
            }
        }
        self.dirty = true;
        true
    }

    fn step_search(&mut self, delta: isize) {
        if let Some(search) = self.search.as_mut() {
            search.step(delta);
        }
    }

    /// A press while the search is open. Whether it was taken.
    fn press_in_the_search(&mut self, position: Point) -> bool {
        if self.search.is_none() {
            return false;
        }
        if self.hits.search_all.is_some_and(|a| a.contains(position.x, position.y)) {
            self.flatten_search();
            return true;
        }
        if let Some(offset) =
            self.hits.search.iter().position(|a| a.contains(position.x, position.y))
        {
            if let Some(search) = self.search.as_mut() {
                search.hover(offset);
            }
            self.follow_search();
            return true;
        }
        if self.hits.search_field.is_some_and(|a| a.contains(position.x, position.y)) {
            return true;
        }
        // A press outside it closes it and does nothing else, which is the rule
        // every overlay here follows.
        self.close_search();
        true
    }

    /// Takes delivery of what a path turned out to be.
    fn drain_facts(&mut self) {
        while let Ok(facts) = self.replies.facts_rx.try_recv() {
            // A panel that has been closed, or a second one opened over the
            // first: the answer belongs to neither.
            let Some(panel) = self.properties.as_mut() else { continue };
            if !panel.paths.contains(&facts.path) {
                continue;
            }
            panel.facts.push(facts);
            self.dirty = true;
        }
        // A folder's real size is the measurer's answer, which it already has for
        // every folder on screen — so a panel over one asks the cache rather than
        // starting a second walk.
        if let Some(panel) = self.properties.as_mut()
            && panel.total.is_none()
            && let Some(facts) = panel.only()
            && facts.kind == sk_fs::facts::Kind::Directory
            && let Some(bytes) = self.measurer.total(&facts.path)
        {
            panel.total = Some(bytes);
            self.dirty = true;
        }
    }

    /// Opens the user's terminal in the folder this pane is showing.
    ///
    /// [11-non-goals.md](../../../docs/11-non-goals.md) declines an embedded
    /// terminal and names this as what covers the actual need — so it is one
    /// command, and it honours whatever `$TERMINAL` says.
    pub(crate) fn open_terminal(&mut self) {
        let at = self.panes.focused().path.clone();
        let sender = self.replies.launched_tx.clone();
        let wake = self.waker.handle();
        // A job for the same reason a launch is: `spawn` is `fork` plus `exec`
        // and the parent blocks on the child's exec-status pipe.
        self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
            let launched = match Launcher::terminal_in(&at) {
                Ok(child) => Launched {
                    notice: sk_model::i18n::lookup("notice.terminal").to_owned(),
                    child: Some(child),
                },
                Err(_) => Launched {
                    notice: sk_model::i18n::lookup("error.no_terminal_at_all").to_owned(),
                    child: None,
                },
            };
            if sender.send(launched).is_ok() {
                wake.wake();
            }
        });
    }

    /// Starts renaming the row a just-finished operation asked for.
    ///
    /// A folder is made and then named, and there is a directory read between the
    /// two: this is the far side of it.
    ///
    /// **By name, not on the cursor.** It used to call `start_rename` directly,
    /// which renames whatever the cursor is on — so a creation that failed left
    /// the debt outstanding and the next listing to land anywhere at all opened a
    /// rename field over a file nobody had chosen.
    fn name_when_it_arrives(&mut self) {
        let Some(wanted) = self.edits.name_after_load.take() else { return };
        let Some(row) = self.row_named(&wanted) else { return };
        let Some(id) = self.panes.focused().listing.at(row) else { return };
        self.panes.focused_mut().selection.replace_with(id);
        self.start_rename();
    }

    /// Which display row holds an entry with this name.
    ///
    /// A scan, and it runs when a listing lands or when one moves under an open
    /// field — never per frame. `EntryId` is an index into the listing it came
    /// from, so it is not an identity that survives a re-read; the name is.
    fn row_named(&self, name: &str) -> Option<usize> {
        (0..self.panes.focused().listing.rows()).find(|row| {
            self.panes
                .focused()
                .listing
                .at(*row)
                .is_some_and(|id| self.panes.focused().listing.name(id) == name)
        })
    }

    /// Puts an open name field back on its own row after the listing moved.
    ///
    /// A file that sorts above it landing renumbers every row below, and a field
    /// drawn over the wrong row renames a file nobody chose. If the name is gone
    /// altogether the field goes with it: one that captures the keyboard while
    /// being drawn nowhere is a window that looks frozen.
    fn settle_naming(&mut self) {
        let Some(naming) = self.edits.naming.as_ref() else { return };
        let Some(name) = naming.path.file_name().map(|name| name.to_string_lossy().into_owned())
        else {
            return;
        };
        let still_there = self
            .panes
            .focused()
            .listing
            .at(naming.row)
            .is_some_and(|id| self.panes.focused().listing.name(id) == name);
        if still_there {
            return;
        }
        match self.row_named(&name) {
            Some(row) => {
                if let Some(naming) = self.edits.naming.as_mut() {
                    naming.row = row;
                }
            }
            None => {
                self.edits.stop_naming();
            }
        }
        self.dirty = true;
    }

    /// Takes delivery of a finished mutation.
    fn drain_edits(&mut self) {
        let Some(landed) = self.edits.drain() else { return };
        let message = match landed {
            crate::edit::Landed::Notice(message, subject) => {
                // The listing does not know yet — the watcher will bring it in
                // within a quarter of a second — so the name to land the cursor
                // on is remembered rather than looked up.
                if let Some(path) = subject
                    && let Some(name) = path.file_name()
                {
                    let name = name.to_string_lossy().into_owned();
                    if std::mem::take(&mut self.edits.naming_owed) {
                        self.edits.name_after_load = Some(name.clone());
                    }
                    self.panes.focused_mut().select_after(name);
                }
                self.panes.focused_mut().refresh(&self.jobs);
                message
            }
            crate::edit::Landed::Failed(message) => {
                // **The debt dies with the operation.** A creation that failed
                // has nothing to name, and an outstanding one made the next
                // listing to land anywhere open a rename field over whatever the
                // cursor happened to be on.
                self.edits.naming_owed = false;
                message
            }
        };
        // The next one in the queue starts here rather than in `drain`: the
        // engine has no pool and this is where one is in reach.
        self.edits.next_in_queue(&self.jobs);
        self.notice = Some((message, Instant::now()));
        self.dirty = true;
    }

    /// Takes delivery of a launch: its notice, and the child to reap.
    fn drain_launches(&mut self) {
        while let Ok(launched) = self.replies.launched_rx.try_recv() {
            if let Some(child) = launched.child {
                self.launcher.adopt(child);
            }
            self.notice = Some((launched.notice, Instant::now()));
            self.dirty = true;
        }
    }

    /// Goes to the parent directory.
    pub(crate) fn go_up(&mut self) {
        self.panes.focused_mut().go_up(&self.jobs);
    }

    /// Goes back to where the last navigation came from.
    pub(crate) fn go_back(&mut self) {
        self.panes.focused_mut().go_back(&self.jobs);
    }

    /// Undoes a Back.
    pub(crate) fn go_forward(&mut self) {
        self.panes.focused_mut().go_forward(&self.jobs);
    }

    /// Goes to `$HOME`.
    pub(crate) fn go_home(&mut self) {
        self.panes.focused_mut().go_home(&self.jobs);
    }

    /// Re-reads the current directory, and re-measures every folder in it.
    ///
    /// The totals are the reason this forgets as well as reloads. A watch on a
    /// directory says nothing about its grandchildren, so a subtree that changed
    /// underneath keeps its old number until somebody asks — and `F5` is a person
    /// asking.
    pub(crate) fn reload(&mut self) {
        self.measurer.forget_all();
        // The places too: a volume can appear or vanish while a window is open,
        // and nothing here watches `/proc/self/mountinfo` yet.
        self.sidebar.reload(&self.jobs);
        // `refresh` rather than `reload`: the directory is the same one, so the
        // cursor and the selection are carried across by name. Pressing `F5`
        // should not cost the user what they had picked out.
        self.panes.focused_mut().refresh(&self.jobs);
    }

    /// Moves the cursor one step in a direction.
    ///
    /// **Not `move_cursor(±1)`.** In a grid, Up and Down move by a whole line and
    /// Left and Right by one entry; in Details there is one column, so all four
    /// come out as previous and next. [`crate::cells::Cells::step`] is the only
    /// thing that knows which, which is what keeps the two modes from needing two
    /// bindings — see [ADR
    /// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md).
    pub(crate) fn step_cursor(&mut self, step: crate::cells::Step, extend: bool) {
        let cells = self.cells();
        let Some(from) = self.panes.focused().cursor_row() else {
            // No cursor yet: a direction key puts one on the first entry, which
            // is what it does in every list.
            self.panes.focused_mut().cursor_to(0, extend);
            self.reveal_cursor();
            return;
        };
        let to = cells.step(from, step);
        self.panes.focused_mut().cursor_to(to, extend);
        self.reveal_cursor();
    }

    /// Lays the focused pane out the other way.
    ///
    /// **The cursor is revealed afterwards**, because the same entry is at a
    /// different height once the cells change shape: switching to a grid while
    /// looking at entry nine hundred and leaving the scroll offset where it was
    /// lands somewhere else entirely.
    pub(crate) fn set_mode(&mut self, mode: crate::cells::Mode) {
        if self.panes.focused().mode == mode {
            return;
        }
        self.panes.focused_mut().mode = mode;
        let cells = self.cells();
        let body = self.placement.here.body.height;
        self.panes.focused_mut().scroll.set_extent(cells.content_height(), body);
        self.reveal_cursor();
        // Nothing saves the session here: `keep_the_session` compares the
        // rendered file every frame rather than trusting twenty callers to
        // remember a flag, and this is one of the twenty.
        self.dirty = true;
    }

    /// Moves the cursor by a screenful.
    pub(crate) fn move_page(&mut self, direction: isize) {
        let page = isize::try_from(self.cells().page()).unwrap_or(1);
        self.panes.focused_mut().move_cursor(direction * page, false);
        self.reveal_cursor();
    }

    /// Moves the cursor to the first row.
    pub(crate) fn go_top(&mut self) {
        self.panes.focused_mut().cursor_to(0, false);
        self.panes.focused_mut().scroll.to(0.0);
    }

    /// Moves the cursor to the last row.
    pub(crate) fn go_bottom(&mut self) {
        let last = self.panes.focused().listing.rows().saturating_sub(1);
        self.panes.focused_mut().cursor_to(last, false);
        self.reveal_cursor();
    }

    /// Clears the selection, the type-ahead buffer and any overlay.
    ///
    /// One key that means "never mind", whatever is going on. Escape doing three
    /// different things depending on state is worse than Escape doing all of
    /// them.
    pub(crate) fn clear(&mut self) {
        // A drag in flight is the most specific thing Escape can mean, and
        // leaving it running would drop files on the next release with no
        // highlight to say where.
        self.files_drag = None;
        self.panes.focused_mut().selection.clear();
        self.typed.clear();
        self.filtering = false;
        self.panes.focused_mut().set_filter(String::new(), &self.jobs);
        self.show_keys = false;
        self.show_stats = false;
    }

    /// Shows or hides dotfiles.
    pub(crate) fn toggle_hidden(&mut self) {
        self.panes.focused_mut().toggle_hidden(&self.jobs);
    }

    /// Swaps the palette.
    pub(crate) fn toggle_theme(&mut self) {
        self.theme = if self.theme.dark { Theme::light() } else { Theme::dark() };
        self.relayout();
    }

    /// Recomputes every region, including whose frame this window is drawing.
    fn relayout(&mut self) {
        self.layout = Layout::compute_with(
            self.logical,
            &self.theme.metrics,
            frame_of(&self.window),
            self.sidebar.open,
        );
        self.divide();
    }

    /// Lays the pane tree out inside the content rectangle.
    ///
    /// Separate from [`App::relayout`] because the window's chrome changes when
    /// the window does and the tree changes when a split, a tab or the focus
    /// does — and running the wrong one leaves a pane drawing where it no longer
    /// is, which is a click that lands one pane over.
    fn divide(&mut self) {
        let Self { panes, layout, theme, placement, .. } = self;
        panes.layout(
            layout.content,
            &theme.metrics,
            &mut placement.leaves,
            &mut placement.dividers,
        );
        let focus = panes.focus();
        placement.here = placement
            .leaves
            .iter()
            .find(|leaf| leaf.id == focus)
            .or_else(|| placement.leaves.first())
            .copied()
            .unwrap_or_default();
    }

    /// Sorts by `column`, reversing if it is already the sort column.
    pub(crate) fn sort_by(&mut self, column: SortColumn) {
        self.panes.focused_mut().sort_by(column, &self.jobs);
    }

    /// Shows or hides the keyboard list.
    pub(crate) fn toggle_keys(&mut self) {
        self.show_keys = !self.show_keys;
    }

    /// Shows or hides the frame statistics.
    pub(crate) fn toggle_stats(&mut self) {
        self.show_stats = !self.show_stats;
    }

    /// Writes a diagnostic report to the log.
    ///
    /// [docs/04-architecture.md](../../../docs/04-architecture.md#observability)
    /// asks for the frame profiler to be dumpable with a keystroke, so that a
    /// person saying "it stuttered" becomes a timeline. This is that keystroke,
    /// and it carries the interface state beside the timings — which is also what
    /// makes an end-to-end test of the real window able to assert anything at
    /// all, since a test cannot look at a screen.
    ///
    /// One line, machine-readable, on the standard error stream. A file lands
    /// with the crash reporter in M8.
    // The path, the cursor and the notice are printed with `Debug` on purpose:
    // this line is machine-read, and `Debug`'s quoting and escaping is what makes
    // a filename containing a space or a quote parseable at the other end.
    pub(crate) fn dump(&mut self) {
        // Two halves and one line: the listing's state and the chrome's
        // geometry. Split because between them they name fifty things and one
        // function that names fifty things is a function nobody reads — and
        // joined because `tools/gui-test` parses `skerry-state` as one record.
        println!("skerry-state {} {}", self.listing_state(), self.chrome_state());
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    /// How the focused pane lays out, and what index it has.
    ///
    /// Split out of [`Self::listing_state`] because that function reached the
    /// hundred-line lint, and these two belong together: both answer "what shape
    /// is this pane in", which is exactly what the rest of the line cannot say —
    /// [rule 8](../../../docs/03-principles.md) makes a grid and a details list
    /// the same rows in the same order, and an indexed search and a walk give the
    /// same answer on purpose.
    fn shape_state(&self) -> String {
        let cells = self.cells();
        let index = self
            .indexes
            .covering(&self.panes.focused().path)
            .map_or(-1i64, |it| i64::try_from(it.len()).unwrap_or(-1));
        let indexing = i64::try_from(self.indexes.building().map_or(0, |(_, at)| at)).unwrap_or(0);
        let searched = self.search.as_ref().is_some_and(|it| it.indexed.is_some());
        format!(
            "mode={} grouping={} bands={} columns={} cell={:.1}x{:.1} \
index={index} indexing={indexing} search_indexed={searched}",
            cells.mode().token(),
            self.panes.focused().grouping.token(),
            cells.count(),
            cells.columns(),
            cells.cell().0,
            cells.cell().1,
        )
    }

    /// What the pane is showing.
    #[expect(clippy::unnecessary_debug_formatting, reason = "the quoting is the point")]
    fn listing_state(&mut self) -> String {
        let cursor = self
            .panes
            .focused()
            .selection
            .cursor()
            .map(|id| self.panes.focused().listing.name(id).to_owned())
            .unwrap_or_default();
        let hover = self.panes.focused().hover.map_or(-1, |row| i64::try_from(row).unwrap_or(-1));
        let hover_name = self
            .panes
            .focused()
            .hover
            .and_then(|row| self.panes.focused().listing.at(row))
            .map(|id| self.panes.focused().listing.name(id).to_owned())
            .unwrap_or_default();
        let (known, running) = self.measurer.stats();
        let (jobs, panics) = self.jobs.pool.stats();
        let naming = self.edits.naming.as_ref().map(|it| it.field.text.clone()).unwrap_or_default();
        let typing_path =
            self.typing_path.as_ref().map(|field| field.text.clone()).unwrap_or_default();
        // The caret and the selection, because a field's *text* says nothing about
        // either — and a click that fails to move the caret looks identical to one
        // that lands, from the text alone.
        let path_caret =
            self.typing_path.as_ref().map_or(-1, |field| i64::try_from(field.caret).unwrap_or(-1));
        let path_selected =
            self.typing_path.as_ref().map(|field| field.selected().to_owned()).unwrap_or_default();
        let naming_selected =
            self.edits.naming.as_ref().map(|it| it.field.selected().to_owned()).unwrap_or_default();
        let naming_row =
            self.edits.naming.as_ref().map_or(-1, |it| i64::try_from(it.row).unwrap_or(-1));
        let (queue_left, queue_total) = self.edits.queue_state();
        let cursor_row =
            self.panes.focused().cursor_row().map_or(-1, |row| i64::try_from(row).unwrap_or(-1));
        let progress = self.edits.progress_line().unwrap_or_default();
        let drag = self.drag_report();
        // The open question, which is the one overlay a scenario cannot see any
        // other way: it draws no row, changes no listing, and the whole point of
        // it is that nothing has happened yet.
        let asking = self.edits.asking();
        let clash = self
            .edits
            .clash
            .as_ref()
            .map(|it| crate::panes::title_of(&it.existing).to_owned())
            .unwrap_or_default();
        let clash_rest = self.edits.clash.as_ref().map_or(0, |it| it.rest);
        let answer = if asking { self.edits.chosen_answer().key() } else { "" };
        let for_the_rest = self.edits.for_the_rest;
        let search = self.search_report();
        let paused = self.edits.held();
        // The three settings a session restores that nothing else on this line
        // reports. `hidden` is already here, from the pane.
        let theme = if self.theme.dark { "dark" } else { "light" };
        let zoom = self.zoom;
        let sidebar_open = self.sidebar.open;
        // The counted total, and whether the count reached the end — which is
        // the whole condition on there being an ETA at all.
        let (total, total_done) = self.edits.counted();
        // The display server's own view, which is not the same question: one is
        // "this window is carrying something", the other is "the server has a
        // drag over this surface".
        // What the clipboard says it holds, which is the whole of what dims or
        // undims Paste — a command that silently does nothing is a command whose
        // `enabled` said no, and this is the only way to see that from outside.
        let offered = self.window.clipboard().offered().join(" ");
        // The type-ahead buffer. A chord whose modifier did not register arrives
        // as a bare letter and lands here, which is otherwise indistinguishable
        // from a key that was never delivered at all.
        let typed = self.typed.clone();
        let refused = self.refused.unwrap_or_default();
        let last_key = self.last_key.clone();
        // The query a pane is showing results for, empty when it is showing a
        // directory. Reported because from outside the two look identical: rows
        // are rows, which is the point.
        let results = self.panes.focused().results.clone().unwrap_or_default();
        // Whether this folder has an index and how big it is, because from
        // outside an indexed search and a walk are deliberately identical — which
        // is ADR 0008's third constraint and therefore the thing a test has to be
        // able to see in order to assert that both answer the same.
        let shape = self.shape_state();
        // How many the delete-for-good question is asking about, and which button
        // it is on. `0` when it is not up, which is not the same as asking about
        // nothing — the panel never opens on an empty selection.
        let erasing = self.erasing.as_ref().map_or(0, |asking| asking.paths.len());
        let (panel_area, erase_aim, erasing_on) = self.question_state();
        let drag_in = self.window.dragging().is_some();
        let drag_out = self.window.dragging_out();
        format!(
            "{shape} \
path={:?} results={results:?} erasing={erasing} erasing_on={erasing_on} erase_aim={erase_aim:?} panel_area={panel_area:?} rows={} hidden={} sort={:?} descending={} \
cursor={cursor:?} cursor_row={cursor_row} \
selected={} scroll={:.1} loading={} watching={} notice={:?} \
hover={hover} hover_name={hover_name:?} \
naming={naming:?} naming_row={naming_row} naming_selected={naming_selected:?} \
path_entry={typing_path:?} path_caret={path_caret} path_selected={path_selected:?} \
undo={} redo={} busy={} queue={queue_left}/{queue_total} progress={progress:?} \
asking={asking} clash={clash:?} clash_rest={clash_rest} answer={answer:?} \
for_the_rest={for_the_rest} paused={paused} total={total} total_done={total_done} \
theme={theme} zoom={zoom} sidebar_open={sidebar_open} {search} \
filtering={} filter={:?} {drag} \
drag_in={drag_in} drag_out={drag_out} offered={offered:?} typed={typed:?} \
refused={refused:?} last_key={last_key:?} \
sizes={known}/{running} jobs={jobs}/{panics}",
            self.panes.focused().path,
            self.panes.focused().listing.rows(),
            self.panes.focused().show_hidden,
            self.panes.focused().sort.column,
            self.panes.focused().sort.descending,
            self.panes.focused().selection.count(),
            self.panes.focused().scroll.offset(),
            self.panes.focused().loading,
            self.watching,
            self.notice.as_ref().map(|(message, _)| message.as_str()).unwrap_or_default(),
            self.edits.history.can_undo(),
            self.edits.history.can_redo(),
            self.edits.busy(),
            self.filtering,
            self.panes.focused().filter,
        )
    }

    /// The open menu, for a scenario: how many rows, which is highlighted, which
    /// are dimmed, and where each one can be clicked.
    ///
    /// "Dimmed rather than absent" is a claim about the interface and not an
    /// implementation detail, so it is reported. The aim points are the middle of
    /// each row, so a scenario names a command instead of copying the menu's own
    /// constants — which would then be a second copy to keep right.
    fn menu_report(&self) -> String {
        let menu = self.menu.as_ref();
        let rows = menu.map_or(0, |menu| menu.items.len());
        let active = menu
            .and_then(|menu| menu.active)
            .map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let id = menu.and_then(super::menu::Menu::active_id).unwrap_or_default();
        let dimmed = menu.map_or_else(String::new, |menu| {
            menu.items
                .iter()
                .filter(|item| !item.enabled)
                .map(|item| item.id)
                .collect::<Vec<_>>()
                .join(",")
        });
        let aim = menu.map_or_else(String::new, |menu| {
            menu.items
                .iter()
                .zip(&self.hits.menu)
                // A row scrolled out of a menu taller than the window has an
                // empty rectangle, and an aim at it would be an aim at nothing.
                .filter(|(_, area)| area.width > 0.0)
                .map(|(item, area)| {
                    format!("{}@{:.0}:{:.0}", item.id, area.x + 20.0, area.y + area.height / 2.0)
                })
                .collect::<Vec<_>>()
                .join(",")
        });
        format!(
            "menu={rows} menu_active={active} menu_id={id:?} menu_aim={aim:?} menu_dim={dimmed:?}"
        )
    }

    /// The open palette, reported the way the menu is.
    ///
    /// `-1` results rather than `0` when it is closed, because a query that
    /// matches nothing is a real state and has to be distinguishable from no
    /// palette at all.
    fn palette_report(&self) -> String {
        let palette = self.palette.as_ref();
        let rows = palette.map_or(-1, |it| i64::try_from(it.shown.len()).unwrap_or(-1));
        let query = palette.map(|it| it.query.text.clone()).unwrap_or_default();
        let id = palette.and_then(crate::palette::Palette::active_id).unwrap_or_default();
        let scroll = palette.map_or(-1, |it| i64::try_from(it.scroll).unwrap_or(-1));
        let aim = palette.map_or_else(String::new, |palette| {
            let (first, _) = palette.window();
            self.hits
                .palette
                .iter()
                .enumerate()
                .filter_map(|(offset, area)| {
                    let row = palette.row(first + offset)?;
                    Some(format!(
                        "{}@{:.0}:{:.0}",
                        row.id,
                        area.x + 20.0,
                        area.y + area.height / 2.0
                    ))
                })
                .collect::<Vec<_>>()
                .join(",")
        });
        format!(
            "palette={rows} palette_query={query:?} palette_id={id:?} \
             palette_scroll={scroll} palette_aim={aim:?}"
        )
    }

    /// Where everything is, so a scenario can aim at it without a copy of the
    /// metrics it is testing.
    /// How many separators are drawn, which is hovered, and where each one is.
    ///
    /// The aim points are the middle of every drawn one, so a scenario names a
    /// depth instead of copying the path bar's own constants — which would then
    /// be a second copy to keep right.
    fn separator_state(&self) -> (usize, i64, String) {
        let drawn = self.hits.separators.iter().filter(|area| area.width > 0.0).count();
        let hovered = self.hovered_separator.map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let aim = self
            .hits
            .separators
            .iter()
            .enumerate()
            .filter(|(_, area)| area.width > 0.0)
            .map(|(depth, area)| {
                format!(
                    "{depth}@{:.1}:{:.1}",
                    area.x + area.width / 2.0,
                    area.y + area.height / 2.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        (drawn, hovered, aim)
    }

    /// The middle of every drawn menu row, by the directory it names.
    ///
    /// So a scenario clicks a name instead of counting rows and copying this
    /// file's constants — the same trade the context menu and the palette make.
    fn crumb_menu_aim(&self) -> String {
        let Some(menu) = &self.crumb_menu else { return String::new() };
        let (_, drawn) = menu.window();
        drawn
            .iter()
            .enumerate()
            .filter_map(|(offset, name)| {
                let area = self.hits.crumb_menu.get(offset)?;
                Some(format!(
                    "{name}@{:.1}:{:.1}",
                    area.x + area.width / 2.0,
                    area.y + area.height / 2.0
                ))
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The open sibling menu: how many rows, which separator, what is chosen.
    fn crumb_menu_state(&self) -> (i64, i64, String) {
        let Some(menu) = &self.crumb_menu else { return (-1, -1, String::new()) };
        let active = menu.active.and_then(|row| menu.rows.get(row)).cloned().unwrap_or_default();
        (
            i64::try_from(menu.rows.len()).unwrap_or(-1),
            i64::try_from(menu.depth).unwrap_or(-1),
            active,
        )
    }

    /// The shape of the pane tree, and where a pointer would aim at it.
    ///
    /// Every value a scenario needs to assert that a split is a split: how many
    /// panes and tabs there are, which has the keyboard, what each is showing,
    /// and the middle of every tab and every divider so a click is arithmetic
    /// over what the frame drew rather than over the harness's own metrics.
    fn panes_report(&self) -> String {
        let leaf = self.panes.focused_leaf();
        let focus = self.panes.focus();
        let order = self.placement.leaves.iter().position(|it| it.id == focus).unwrap_or(0);
        let paths = self
            .panes
            .visible()
            .map(|pane| crate::panes::title_of(&pane.path).to_owned())
            .collect::<Vec<_>>()
            .join(",");
        let strip = self.placement.here.tabs;
        let count = leaf.panes.len();
        let plus = crate::panes::tab_plus(strip, count);
        let plus_aim =
            format!("{:.0}:{:.0}", plus.x + plus.width / 2.0, plus.y + plus.height / 2.0);
        let tab_aim = crate::panes::tab_areas(strip, count)
            .map(|area| format!("{:.0}:{:.0}", area.x + 20.0, area.y + area.height / 2.0))
            .collect::<Vec<_>>()
            .join(",");
        let cross_aim = crate::panes::tab_areas(strip, count)
            .filter_map(crate::panes::tab_close)
            .map(|area| {
                format!("{:.0}:{:.0}", area.x + area.width / 2.0, area.y + area.height / 2.0)
            })
            .collect::<Vec<_>>()
            .join(",");
        // What the overlay is saying while a tab is being dragged, so a
        // scenario can read the promise before it tests the result.
        let tab_drop = match self.placement.tab_drop {
            None => "none",
            Some(crate::panes::Drop::Strip(..)) => "strip",
            Some(crate::panes::Drop::Split(..)) => "split",
        };
        let divider_aim = self
            .placement
            .dividers
            .iter()
            .map(|divider| {
                format!(
                    "{:.0}:{:.0}",
                    divider.area.x + divider.area.width / 2.0,
                    divider.area.y + divider.area.height / 2.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        // One point in each leaf's strip, in layout order, so a scenario can aim
        // at a pane that does not have the keyboard — which `tab_aim` cannot,
        // because it answers about the focused one.
        let strip_aim = self
            .placement
            .leaves
            .iter()
            .map(|leaf| {
                format!("{:.0}:{:.0}", leaf.tabs.x + 8.0, leaf.tabs.y + leaf.tabs.height / 2.0)
            })
            .collect::<Vec<_>>()
            .join(",");
        let widths = self
            .placement
            .leaves
            .iter()
            .map(|leaf| format!("{:.0}x{:.0}", leaf.area.width, leaf.area.height))
            .collect::<Vec<_>>()
            .join(",");
        // Each leaf's list, so a scenario can put the pointer inside a pane that
        // does not have the keyboard — which `body_top` and `name_x` cannot,
        // because they answer about the focused one.
        let bodies = self
            .placement
            .leaves
            .iter()
            .map(|leaf| {
                format!(
                    "{:.0}:{:.0}:{:.0}:{:.0}",
                    leaf.body.x, leaf.body.y, leaf.body.width, leaf.body.height
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "panes={} pane_focus={order} pane_sizes={widths:?} pane_paths={paths:?} \
tabs={} tab={} tab_aim={tab_aim:?} tab_cross={cross_aim:?} plus_aim={plus_aim:?} \
tab_drop={tab_drop} strip_aim={strip_aim:?} divider_aim={divider_aim:?} pane_body={bodies:?}",
            self.placement.leaves.len(),
            leaf.panes.len(),
            leaf.active,
        )
    }

    /// What the two panels are showing: the Properties title and its rows as
    /// `label=value` pairs, and the Open With list's length and highlight.
    ///
    /// Enough for a scenario to assert about either without depending on what
    /// this machine happens to have installed or on where the panel drew.
    fn overlay_report(&self) -> (String, String, usize, String) {
        let facts = self.properties.as_ref().map_or_else(String::new, |panel| {
            panel
                .rows(clock_seconds())
                .iter()
                .map(|row| format!("{}={}", row.label, row.value))
                .collect::<Vec<_>>()
                .join("|")
        });
        let properties =
            self.properties.as_ref().map_or_else(String::new, crate::properties::Properties::title);
        let openers = self.openers.as_ref().map_or(0, |list| list.rows.len());
        let opener = self
            .openers
            .as_ref()
            .and_then(|list| list.chosen())
            .map(|entry| entry.name.clone())
            .unwrap_or_default();
        (properties, facts, openers, opener)
    }

    fn chrome_state(&mut self) -> String {
        let frame = if self.window.needs_own_frame() { "own" } else { "server" };
        let frame_hover = match self.hovered_button {
            Some(crate::layout::Button::Minimize) => "minimize",
            Some(crate::layout::Button::Maximize) => "maximize",
            Some(crate::layout::Button::Close) => "close",
            None => "none",
        };
        let close = self.layout.buttons[2];
        let close_x = close.x + close.width / 2.0;
        // The four navigation buttons are laid out from x=0, this wide each.
        let tool_w = self.layout.tools[0].width;
        let tool_hover = match self.hovered_tool {
            Some(crate::layout::Tool::Back) => "back",
            Some(crate::layout::Tool::Forward) => "forward",
            Some(crate::layout::Tool::Parent) => "parent",
            Some(crate::layout::Tool::Reload) => "reload",
            None => "none",
        };
        let cursor_shape = match self.cursor {
            sk_plat::Cursor::Arrow => "arrow".to_owned(),
            sk_plat::Cursor::Hand => "hand".to_owned(),
            sk_plat::Cursor::Text => "text".to_owned(),
            sk_plat::Cursor::Grabbing => "grabbing".to_owned(),
            sk_plat::Cursor::Dragging => "dragging".to_owned(),
            sk_plat::Cursor::Resize(edge) => format!("resize-{edge:?}").to_lowercase(),
            sk_plat::Cursor::Divider { horizontal: true } => "divider-h".to_owned(),
            sk_plat::Cursor::Divider { horizontal: false } => "divider-v".to_owned(),
        };
        let place_hover = self.sidebar.hover.map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let place_name = self
            .sidebar
            .hover
            .and_then(|index| self.sidebar.at(index))
            .map(|entry| entry.label.clone())
            .unwrap_or_default();
        let crumb_hover = self.hovered_crumb.map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        // Where the path field's box is, so a scenario can drag inside it by
        // arithmetic over what the frame drew rather than over its own copy of the
        // metrics.
        let box_of = crate::view::path_field(&self.layout, &self.theme.metrics);
        let path_field =
            format!("{:.1},{:.1},{:.1},{:.1}", box_of.x, box_of.y, box_of.width, box_of.height);
        // What an input method is composing, if anything. The field it is in is
        // whichever has the keyboard, so one value answers for all three.
        let composing = self.field_preedit().map(|(text, _)| text.clone()).unwrap_or_default();
        let (properties, facts, openers, opener) = self.overlay_report();
        let panes_report = self.panes_report();
        let menu_report = self.menu_report();
        let palette_report = self.palette_report();
        // The middle of the crumb one level up, which is the one a test clicks.
        let (separators, separator_hover, separator_aim) = self.separator_state();
        let (crumb_menu, crumb_menu_depth, crumb_menu_active) = self.crumb_menu_state();
        let crumb_menu_aim = self.crumb_menu_aim();
        let parent_crumb_x = self
            .hits
            .crumbs
            .len()
            .checked_sub(2)
            .and_then(|index| self.hits.crumbs.get(index))
            .map_or(0.0, |area| area.x + area.width / 2.0);
        format!(
            "body_top={:.1} body_height={:.1} row_h={:.1} name_x={:.1} size_x={:.1} size_w={:.1} \
frame={frame} frame_hover={frame_hover} maximized={} close_x={close_x:.1} \
crumbs={} crumb_hover={crumb_hover} parent_crumb_x={parent_crumb_x:.1} \
separators={separators} separator_hover={separator_hover} separator_aim={separator_aim:?} \
crumb_menu={crumb_menu} crumb_menu_depth={crumb_menu_depth} crumb_menu_active={crumb_menu_active:?} \
crumb_menu_aim={crumb_menu_aim:?} \
{panes_report} {menu_report} {palette_report} \
properties={properties:?} facts={facts:?} openers={openers} opener={opener:?} \
composing={composing:?} \
tool_w={tool_w:.1} tool_hover={tool_hover} pointer={cursor_shape} \
sidebar_w={:.1} sidebar_top={:.1} places={} places_loaded={} \
place_hover={place_hover} place_name={place_name:?} \
path_field={path_field} \
logical={:.0}x{:.0} scale_120={} first_frame_scale_120={} \
frame_p50_us={} frame_p99_us={} frames={} over={} keys={} stats={}",
            self.placement.here.body.y,
            self.placement.here.body.height,
            self.theme.metrics.row_height,
            self.placement.here.name.x,
            self.placement.here.size.x,
            self.placement.here.size.width,
            self.window_state.maximized,
            self.hits.crumbs.len(),
            self.layout.sidebar.width,
            self.layout.sidebar.y,
            self.sidebar.len(),
            self.sidebar.loads,
            self.logical.width,
            self.logical.height,
            self.scale_120,
            self.first_frame_scale_120.unwrap_or(0),
            self.frames.percentile(0.5),
            self.frames.percentile(0.99),
            self.frames.budget().0,
            self.frames.budget().1,
            self.show_keys,
            self.show_stats,
        )
    }

    /// Where the focused pane's entries are, this frame.
    ///
    /// [ADR 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md):
    /// nothing outside [`crate::cells`] computes an entry's position, so every
    /// hit test, reveal, page and band goes through here.
    pub(crate) fn cells(&self) -> crate::groups::Bands {
        self.cells_of(&self.placement.here, self.panes.focused())
    }

    /// As [`Self::cells`], for a pane that is not the focused one.
    ///
    /// **Bands rather than cells**, because grouping is above the cell geometry
    /// and every one of these questions has to answer for both — see [ADR
    /// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md).
    /// An ungrouped pane is one band with no heading, so this is one code path
    /// rather than a branch at each of the fifteen call sites.
    fn cells_of(
        &self,
        layout: &crate::panes::PaneLayout,
        pane: &crate::pane::Pane,
    ) -> crate::groups::Bands {
        crate::groups::Bands::new(
            pane.mode,
            layout.body,
            self.theme.metrics.row_height,
            &pane.bands,
            pane.listing.rows(),
        )
    }

    /// Cuts the focused pane into headings, or stops.
    pub(crate) fn set_grouping(&mut self, grouping: crate::groups::Grouping) {
        if self.panes.focused().grouping == grouping {
            return;
        }
        self.panes.focused_mut().grouping = grouping;
        self.rebandif_needed();
        self.reveal_cursor();
        self.dirty = true;
    }

    /// Keeps the focused pane's band edges current, re-sorting when they move.
    ///
    /// **Called every frame, and it almost never does anything.** The edges come
    /// from the local calendar, so they move once a day — at midnight, while the
    /// window is open, "today" has to stop meaning yesterday. Comparing six
    /// numbers per frame is what buys that, and the alternative is a timer, which
    /// [rule 1](../../../docs/03-principles.md) does not allow.
    fn rebandif_needed(&mut self) {
        let wanted = match self.panes.focused().grouping {
            crate::groups::Grouping::None => sk_model::Banding::default(),
            crate::groups::Grouping::Time => {
                sk_model::Banding::new(&sk_ui::format::day_bands(clock_seconds()))
            }
        };
        if self.panes.focused().sort.bands == wanted {
            return;
        }
        self.panes.focused_mut().sort.bands = wanted;
        self.panes.focused_mut().reorder(&self.jobs);
    }

    fn reveal_cursor(&mut self) {
        let Some(row) = self.panes.focused().cursor_row() else { return };
        let (top, bottom) = self.cells().span(row);
        let height = self.placement.here.body.height;
        self.panes.focused_mut().scroll.reveal(top, bottom, height);
    }

    fn press(
        &mut self,
        position: Point,
        clicks: u8,
        ctrl: bool,
        shift: bool,
        commands: &Registry<Self>,
    ) {
        self.pointer = position;
        // First, and it takes every press: the question has no dismissal, so a
        // press that fell through to the list behind it would move a selection
        // the queue is about to act on.
        if self.press_in_the_clash(position) {
            return;
        }
        if self.press_in_the_erasing(position) {
            return;
        }
        if self.press_in_the_status(position) {
            return;
        }
        if self.palette.is_some() {
            self.palette_press(position, commands);
            return;
        }
        if self.press_in_the_search(position) {
            return;
        }
        if self.properties.take().is_some() {
            self.dirty = true;
            return;
        }
        if self.press_in_the_openers(position) {
            return;
        }
        if self.press_in_the_crumb_menu(position) {
            return;
        }
        if self.menu.is_some() {
            self.menu_press(position, commands);
            return;
        }
        if self.press_in_a_field(position) {
            return;
        }
        // The frame comes first, and the resize border before the buttons: the
        // border overlaps the top-right corner where the close button is, and a
        // window whose corner cannot be grabbed is a window that cannot be
        // resized diagonally.
        // Only if the compositor took it: a press with no serial behind it is
        // not a grab the protocol accepts, and swallowing it would make the
        // top six pixels of the list a dead band.
        if let Some(edge) = self.layout.edge_at(position.x, position.y)
            && self.window.start_resize(edge)
        {
            return;
        }
        if self.press_on_the_chrome(position, commands) {
            return;
        }
        // The path field before the drag region, for the same reason a
        // breadcrumb is: a press in a text field belongs to the caret, and a
        // press that moved the window instead would make the field unusable.
        if self.typing_path.is_some() {
            // The second click of a double click on the title bar. The first one
            // opened this field on its release, so taking it back is what makes
            // "double click to maximise" survive "click to type a path".
            if clicks >= 2 && self.path_from_title {
                self.typing_path = None;
                self.path_from_title = false;
                self.toggle_maximized();
                self.dirty = true;
                return;
            }
            let field = crate::view::path_field(&self.layout, &self.theme.metrics);
            if field.contains(position.x, position.y) {
                self.place_path_caret(position.x);
            } else {
                self.typing_path = None;
                self.dirty = true;
            }
            return;
        }
        // A breadcrumb before the drag region, because the breadcrumbs are *in*
        // the drag region: the path bar is the title bar, and a component that
        // moved the window instead of navigating would be a component nobody
        // could click.
        if let Some(depth) = self.separator_at(position) {
            self.open_crumb_menu(depth);
            return;
        }
        if let Some(depth) = self.crumb_at(position) {
            self.go_to_crumb(depth);
            return;
        }
        if self.press_on_the_path_strip(position, clicks) {
            return;
        }
        // The tree before the header and the list, because both of those are
        // questions about *which* pane — and the answer can change here.
        if self.press_in_the_tree(position, clicks, commands) {
            return;
        }
        if self.placement.here.header.contains(position.x, position.y) {
            if let Some(column) = self.column_at(position.x) {
                self.panes.focused_mut().sort_by(column, &self.jobs);
                self.dirty = true;
            }
            return;
        }
        if !self.placement.here.body.contains(position.x, position.y) {
            return;
        }
        if let Some(grab) = self.thumb_at(position) {
            self.thumb_grab = Some(grab);
            return;
        }

        let Some(row) = self.row_under(position) else {
            // Empty space: clear, and be ready to rubber-band.
            self.panes.focused_mut().selection.clear();
            self.panes.focused_mut().selection.set_cursor(None);
            self.band = Some((position, position));
            self.dirty = true;
            return;
        };
        let Some(id) = self.panes.focused().listing.at(row) else { return };
        self.press_on_a_row(id, position, clicks, ctrl, shift);
        if clicks >= 2 {
            // `App::open_focused`, not `Pane::open_focused`: the pane's one
            // enters directories and returns false for anything else, so a
            // double click on a document used to do nothing at all — no notice,
            // no error — while `Enter` on the same row opened it.
            self.open_focused();
        }
        self.dirty = true;
    }

    /// Follows a drag of files, and finds what would take the drop.
    fn drag_files_to(&mut self, position: Point) {
        let Some(drag) = &self.files_drag else { return };
        let moved = (position.x - drag.from.x).abs().max((position.y - drag.from.y).abs());
        if !drag.active && moved <= DRAG_THRESHOLD {
            return;
        }
        let Some(drag) = &mut self.files_drag else { return };
        let starting = !drag.active;
        drag.active = true;
        if starting && !drag.paths.is_empty() {
            // **Handed to the display server**, which is what makes it possible
            // to drop these anywhere but here. It comes back through
            // `Event::DragEntered` while the pointer is still over this window,
            // so the internal case is the same code as everybody else's — and
            // where the server will not take it, the pointer keeps driving and
            // an internal drop still works.
            let content = Content::paths(&drag.paths, Intent::Copy);
            let actions = [sk_plat::DragAction::Copy, sk_plat::DragAction::Move];
            let _ = self.window.start_drag(content, &actions);
        }
        let Some(drag) = &self.files_drag else { return };
        // **The card is drawn at the pointer, so a pointer that has moved is a
        // frame that is out of date** — whether or not what it is over changed.
        // This used to be marked only when the *target* changed, which is a card
        // that jumps from one folder row to the next instead of following the
        // hand. Before the spring's early return, because that return is taken
        // for movements up to the click threshold and four pixels is visible.
        if !drag.paths.is_empty() {
            self.dirty = true;
        }
        // **Nothing is recomputed while the pointer sits where a spring left
        // it.** Opening a folder puts a different listing under a pointer that
        // has not moved: the row beneath it is one the listing moved there, and
        // treating it as a target means both springing into it and *dropping*
        // into it. Measured — a file aimed at `alpha` landed in `alpha/nested`,
        // and suppressing only the spring left the drop doing it anyway. What
        // the spring already set stands until a hand moves.
        if still_there(drag.sprung_at, position) {
            return;
        }
        let landing = self.landing_at(position);
        // **Restarted whenever the target changes**, so passing over four folders
        // on the way to a fifth opens none of them.
        let resting = self.spring_from(&landing);
        let Some(drag) = &mut self.files_drag else { return };
        drag.sprung_at = None;
        if drag.landing != landing {
            drag.landing = landing;
            self.dirty = true;
        }
        match (&drag.resting, resting) {
            (Some((held, _)), Some(next)) if *held == next => {}
            (_, next) => drag.resting = next.map(|spring| (spring, Instant::now())),
        }
    }

    /// Where a drop at `position` would land.
    ///
    /// Three answers, in order: the tab under the pointer, the folder row under
    /// it, or the directory a *different* pane is showing.
    ///
    /// A target is a **directory that is not part of what is being dragged**:
    /// dropping a folder into itself is a request nobody means, and the engine
    /// underneath would follow the loop until it ran out of depth.
    fn landing_at(&self, position: Point) -> Landing {
        let Some(drag) = self.files_drag.as_ref() else { return Landing::default() };
        let held = |path: &std::path::Path| drag.paths.iter().any(|it| path.starts_with(it));
        if let Some((leaf, index, path)) = self.tab_under(position) {
            // **A tab is the directory it holds, not the pane it sits over.** So
            // a drop on tab 2 lands in tab 2 whether or not the strip has
            // switched to it yet, and a hand that has read the labels can aim at
            // one. The exception is the tab this drag started on, which is a move
            // to where the files already are.
            let home = drag.origin == Some(leaf)
                && self.panes.leaf(leaf).is_some_and(|tabs| tabs.active == index);
            if home || held(&path) {
                return Landing::default();
            }
            return Landing { onto: Some(path), tab: Some((leaf, index)), ..Landing::default() };
        }
        if let Some((leaf, row, path)) = self.folder_under(position)
            && !held(&path)
        {
            return Landing { onto: Some(path), row: Some((leaf, row)), ..Landing::default() };
        }
        // **A pane other than the one this started in takes the drop into the
        // directory it is showing.** That is the whole gesture a split exists
        // for: pick files up on the left, put them down on the right, with no
        // folder to aim at. The origin pane is excluded because dropping files
        // back where they came from is a move to where they already are, and the
        // ring would promise something that would then do nothing.
        let into_pane = self.pane_under(position).filter(|id| Some(*id) != drag.origin);
        Landing { into_pane, ..Landing::default() }
    }

    /// What a landing would open if the hand stayed on it.
    ///
    /// A folder row and a tab, and nothing else: a pane's own directory is
    /// already open and there is nothing to enter, and neither is the tab its
    /// leaf is already showing.
    fn spring_from(&self, landing: &Landing) -> Option<Spring> {
        if let Some((leaf, index)) = landing.tab {
            let showing = self.panes.leaf(leaf).is_some_and(|tabs| tabs.active == index);
            return (!showing).then_some(Spring::Tab(leaf, index));
        }
        let (leaf, _) = landing.row?;
        Some(Spring::Into(leaf, landing.onto.clone()?))
    }

    /// Opens what the drag has been resting on, if it has rested long enough.
    ///
    /// **The origin is forgotten with it.** A pane that has sprung is no longer
    /// showing the directory the files came from, so the rule that stops a drop
    /// landing where the files already are no longer applies to it — and without
    /// this, springing inside the pane you started in leaves a pane that takes no
    /// drop at all. Which is the whole of the tab case: tab 1 and tab 2 are the
    /// same leaf.
    fn spring_open(&mut self) {
        let Some(drag) = self.files_drag.as_ref() else { return };
        if !drag.active {
            return;
        }
        let Some((spring, since)) = drag.resting.clone() else { return };
        if since.elapsed() < SPRING {
            return;
        }
        let opened = match spring {
            Spring::Into(leaf, path) => self.spring_into(leaf, path),
            // **Nothing is frozen for a tab.** A folder that springs puts a
            // different listing under a still hand, which is why that one stops
            // recomputing until the hand moves; a strip that switches leaves
            // every tab exactly where it was, so the tab under the pointer is
            // still the target it was a moment ago, release included.
            Spring::Tab(leaf, index) => self.panes.reveal_tab(leaf, index).then_some(leaf),
        };
        let Some(leaf) = opened else { return };
        let Some(drag) = self.files_drag.as_mut() else { return };
        drag.resting = None;
        if drag.origin == Some(leaf) {
            drag.origin = None;
        }
        self.settle_panes();
        self.dirty = true;
    }

    /// Opens a folder into the pane that was showing its row, and makes that
    /// pane the target. The leaf, if it is still there.
    fn spring_into(
        &mut self,
        leaf: crate::panes::PaneId,
        path: PathBuf,
    ) -> Option<crate::panes::PaneId> {
        self.panes.leaf_mut(leaf)?.showing_mut().open(path, &self.jobs);
        let pointer = self.pointer;
        let drag = self.files_drag.as_mut()?;
        // The row that was the target has gone with the listing it was in, and
        // **the pane that opened is the target now** — set here rather than left
        // to the next motion, because the gesture this exists for is one where
        // the pointer is deliberately not moving. Without it a release straight
        // after a spring found no target at all and collapsed into a click.
        drag.landing = Landing { into_pane: Some(leaf), ..Landing::default() };
        drag.sprung_at = Some(pointer);
        Some(leaf)
    }

    /// How long until a resting drag opens what it is over.
    fn spring_due(&self) -> Option<Duration> {
        let drag = self.files_drag.as_ref().filter(|drag| drag.active)?;
        let (_, since) = drag.resting.as_ref()?;
        Some(SPRING.saturating_sub(since.elapsed()))
    }

    /// The pane whose list is under `position`, whichever pane has the keyboard.
    ///
    /// The body rather than the whole leaf: a tab answers for the directory it
    /// holds — see [`Self::tab_under`] — and the header sorts, so a file dropped
    /// on the strip is not a file dropped on this.
    fn pane_under(&self, position: Point) -> Option<crate::panes::PaneId> {
        self.placement
            .leaves
            .iter()
            .find(|leaf| leaf.body.contains(position.x, position.y))
            .map(|leaf| leaf.id)
    }

    /// The tab under `position`, whichever leaf's strip it is in, and the
    /// directory that tab is showing.
    ///
    /// The directory comes from the tab rather than from the leaf: the two differ
    /// for every tab but the one on top, and that difference is the whole point
    /// of aiming at one.
    fn tab_under(&self, position: Point) -> Option<(crate::panes::PaneId, usize, PathBuf)> {
        let leaf = self.leaf_at(position)?;
        let (index, _) = self.tab_at(position)?;
        let path = self.panes.leaf(leaf)?.panes.get(index)?.path.clone();
        Some((leaf, index, path))
    }

    /// The folder row under `position` in any pane, and the path it names.
    fn folder_under(&self, position: Point) -> Option<(crate::panes::PaneId, usize, PathBuf)> {
        let leaf =
            self.placement.leaves.iter().find(|it| it.body.contains(position.x, position.y))?;
        let pane = self.panes.leaf(leaf.id)?.showing();
        let row =
            self.cells_of(leaf, pane).index_at(position.x, position.y, pane.scroll.offset())?;
        let id = pane.listing.at(row)?;
        if !pane.listing.flags(id).has(sk_model::Flags::DIR) {
            return None;
        }
        Some((leaf.id, row, pane.path_of(id)))
    }

    /// Follows a drag over this window, whoever started it.
    ///
    /// Ours and a peer's are the same gesture from here: the compositor drives
    /// both once the drag has begun, and the only difference is whether
    /// [`Self::files_drag`] holds what is being carried.
    fn drag_over(&mut self, position: Point) {
        self.pointer = position;
        if self.files_drag.is_none() {
            // **Only where the window still has one.** A compositor brackets a
            // drop with `leave, drop, leave` — measured — so a motion arriving
            // after the drop would otherwise start a second, empty drag that
            // never ends and leaves the highlight lit.
            if self.window.dragging().is_none() {
                return;
            }
            // A peer's. Nothing is held here, so the highlight is all there is.
            self.files_drag = Some(FilesDrag {
                from: position,
                paths: Vec::new(),
                id: None,
                active: true,
                origin: None,
                landing: Landing::default(),
                resting: None,
                sprung_at: None,
            });
        }
        self.drag_files_to(position);
        // Whether this window would take it, and what it would do. Said every
        // time: a target that accepts once and then goes quiet gets a refusal
        // cursor over rows it would in fact accept.
        let wanted = self.drop_target().is_some().then_some("text/uri-list");
        self.window.accept_drag(wanted, sk_plat::DragAction::Copy);
    }

    /// The directory a drop would land in.
    ///
    /// Whatever [`Self::landing_at`] found — a tab, a folder row, or a
    /// *different* pane — or, for a peer's drag only, the directory this pane is
    /// showing.
    ///
    /// **A peer's drag can land anywhere in the list; ours cannot.** Files from
    /// another application belong in the directory being shown, and requiring a
    /// folder under the pointer would refuse every drop onto a row or onto empty
    /// space, which is most of the window. Our own selection dropped on the pane
    /// it came from is a move to where it already is, so inside that one pane it
    /// still takes a folder.
    fn drop_target(&self) -> Option<PathBuf> {
        let drag = self.files_drag.as_ref()?;
        if let Some(onto) = &drag.landing.onto {
            return Some(onto.clone());
        }
        if let Some(id) = drag.landing.into_pane {
            return self.panes.leaf(id).map(|leaf| leaf.showing().path.clone());
        }
        drag.paths.is_empty().then(|| self.panes.focused().path.clone())
    }

    /// Takes a drop from another application.
    ///
    /// Ours never reaches here with anything to read: [`Self::drop_files`] has
    /// already moved what this window was carrying, and a drag that came from
    /// this window has its paths in hand rather than in a pipe.
    fn take_drop(&mut self, position: Point) {
        self.dropped = true;
        self.drag_over(position);
        let ours = self.files_drag.as_ref().is_some_and(|drag| !drag.paths.is_empty());
        if ours {
            // Ours landed here. The paths are in hand, so the offer is closed
            // rather than read — but it is still closed, or the display server
            // holds the pointer waiting for a confirmation.
            self.window.finish_drop();
            self.drop_files(false);
            return;
        }
        if self.window.dragging_out() {
            // Ours, and the button release has already moved them. The offer is
            // still closed, or the display server holds the pointer.
            self.window.finish_drop();
            self.files_drag = None;
            self.dirty = true;
            return;
        }
        let into = self.drop_target().unwrap_or_else(|| self.panes.focused().path.clone());
        self.files_drag = None;
        self.dirty = true;
        let Some(fd) = self.window.take_drop("text/uri-list") else { return };
        let sender = self.replies.paste_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let mut bytes = Vec::new();
            let mut file = std::fs::File::from(fd);
            let _ = std::io::Read::read_to_end(&mut file, &mut bytes);
            let text = String::from_utf8_lossy(&bytes);
            let paths: Vec<PathBuf> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .filter_map(sk_plat::decode_uri)
                .filter(|path| path.symlink_metadata().is_ok())
                .collect();
            // **A copy, always.** A peer that asked for a move deletes its own
            // originals; taking that on ourselves would delete somebody else's
            // files on a confirmation this crate cannot get — see `sk_plat::drag`.
            // Sent even when it names nothing, so the drag is always closed:
            // an offer left open holds a source that is waiting on us.
            let reply = if paths.is_empty() {
                Pasted::Dropped(Vec::new(), into)
            } else {
                Pasted::Dropped(paths, into)
            };
            if sender.send(reply).is_ok() {
                wake.wake();
            }
        });
    }

    /// Which pane's row a drag of files would land on, for the highlight.
    #[must_use]
    pub(crate) fn drop_row(&self) -> Option<(crate::panes::PaneId, usize)> {
        self.files_drag.as_ref().filter(|drag| drag.active).and_then(|drag| drag.landing.row)
    }

    /// Which tab it would land in instead, for the highlight on that tab.
    #[must_use]
    pub(crate) fn drop_tab(&self) -> Option<(crate::panes::PaneId, usize)> {
        self.files_drag.as_ref().filter(|drag| drag.active).and_then(|drag| drag.landing.tab)
    }

    /// Which whole pane would take it instead, for the ring around its list.
    #[must_use]
    pub(crate) fn drop_pane(&self) -> Option<crate::panes::PaneId> {
        self.files_drag.as_ref().filter(|drag| drag.active).and_then(|drag| drag.landing.into_pane)
    }

    /// Finishes a drag of files, or collapses it back into a plain click.
    ///
    /// **Move, and `Ctrl` copies.** GNOME and KDE decide this per drop from the
    /// device each side is on, so the same gesture moves or copies depending on
    /// something nobody can see. Stated instead: a drag moves, `Ctrl` copies, and
    /// `Ctrl+Z` takes either back. A drop into another pane can now cross
    /// devices — the engine handles that, and the rule does not change with it.
    fn drop_files(&mut self, copying: bool) {
        // Asked before the drag is taken, because the answer for a drop into
        // another pane is the directory *that pane* is showing and the drag is
        // what says which pane that is.
        let landing = self.drop_target();
        let Some(drag) = self.files_drag.take() else { return };
        let Some(into) = landing.filter(|_| drag.active) else {
            // Never moved, or moved and landed on nothing: the press was a plain
            // click after all, and the selection it deferred happens now.
            if !drag.active
                && let Some(id) = drag.id
            {
                self.panes.focused_mut().selection.replace_with(id);
            }
            self.dirty = true;
            return;
        };
        self.edits.paste_all(drag.paths, into, !copying, &self.jobs);
        self.dirty = true;
    }

    /// Chooses what a press on a row selects, and whether it may become a drag.
    ///
    /// **A plain press on an already-selected row changes nothing yet.** It
    /// means either "just this one" or "pick all of these up", and which it was
    /// is not known until the pointer moves or does not — so the selection is
    /// deferred to the release. Deciding here is what made dragging three files
    /// drag one of them.
    fn press_on_a_row(
        &mut self,
        id: sk_model::EntryId,
        position: Point,
        clicks: u8,
        ctrl: bool,
        shift: bool,
    ) {
        self.dropped = false;
        if shift {
            let pane = self.panes.focused_mut();
            pane.selection.extend_to(id, pane.listing.order());
            return;
        }
        if ctrl {
            self.panes.focused_mut().selection.toggle(id);
            return;
        }
        if !self.panes.focused().selection.contains(id) || clicks >= 2 {
            self.panes.focused_mut().selection.replace_with(id);
        }
        if clicks < 2 {
            self.files_drag = Some(FilesDrag {
                from: position,
                paths: self.selected_paths(),
                id: Some(id),
                active: false,
                origin: Some(self.panes.focus()),
                landing: Landing::default(),
                resting: None,
                sprung_at: None,
            });
        }
    }

    fn release(&mut self) {
        if std::mem::take(&mut self.tab_dragging) {
            let at = self.pointer;
            self.placement.tab_drop = None;
            self.drop_the_tab(at);
        }
        self.tab_press = None;
        // Both, not either: `||` short-circuits, so a release that ended a band
        // left a thumb grab behind. They cannot both be set today, which makes
        // this the kind of latent bug that becomes a stuck scrollbar the first
        // time that stops being true.
        let ended = self.band.take().is_some();
        let released = self.thumb_grab.take().is_some();
        let dragged = self.text_drag.take().is_some();
        let divided = self.placement.dragging.take().is_some();
        // A press on the title bar that never moved: the person clicked the path
        // strip, which is a request to type a path.
        if self.title_press.take().is_some() {
            self.toggle_path_entry();
            self.path_from_title = self.typing_path.is_some();
        }
        if ended || released || dragged || divided {
            self.dirty = true;
        }
    }

    /// What the pointer should look like at a point.
    ///
    /// **The resize band is invisible without this.** Six logical pixels that do
    /// something on a press and nothing to look at is a feature discoverable
    /// only by trying it, and the first thing a person tries is dragging the
    /// edge of a window.
    fn cursor_at(&self, position: Point, commands: &Registry<Self>) -> sk_plat::Cursor {
        // **Before everything, including the resize band.** While files are
        // being carried the pointer is a payload looking for a destination, and
        // every other shape would be an answer to a question nobody is asking —
        // the window's edge does not resize while a hand is full.
        if self.files_drag.as_ref().is_some_and(|drag| drag.active) {
            return sk_plat::Cursor::Dragging;
        }
        if self.thumb_grab.is_some() {
            return sk_plat::Cursor::Grabbing;
        }
        if let Some(edge) = self.layout.edge_at(position.x, position.y) {
            return sk_plat::Cursor::Resize(edge);
        }
        // **A panel is opaque to the shape as well as to the hover.** Under one,
        // the column header's hand and the divider's arrows used to show through
        // — a pointer promising something a click on a panel will not do. And on
        // the panel's own two buttons it stayed the default arrow, which is a
        // target saying it is not one.
        //
        // After the resize band, which is the window's edge and wins over
        // anything drawn inside it.
        if self.hits.panels.iter().any(|panel| panel.contains(position.x, position.y)) {
            return if self.over_a_panel_button(position) {
                sk_plat::Cursor::Hand
            } else {
                sk_plat::Cursor::Arrow
            };
        }
        // Before every other shape, and including while one is being dragged:
        // a divider under the pointer is the most specific thing there, and a
        // drag that leaves the band must not change shape halfway.
        if let Some(id) = self.placement.dragging.or_else(|| self.divider_at(position)) {
            let horizontal = self
                .placement
                .dividers
                .iter()
                .find(|divider| divider.id == id)
                .is_some_and(|divider| divider.axis == crate::panes::Axis::Beside);
            return sk_plat::Cursor::Divider { horizontal };
        }
        // A dimmed navigation button is not clickable, and Back is dimmed for the
        // first navigation of every session — so it is the first thing a pointer
        // lands on that used to promise a click and do nothing. Asked of the
        // registry, which is where the dimming comes from.
        let over_tool = self
            .layout
            .tool_at(position.x, position.y)
            .is_some_and(|tool| runnable(commands, self, tool.command()));
        // The status strip's Pause and Stop are here rather than with the panel
        // buttons above: the strip is not a panel, and they are drawn only while
        // there is an operation to stop.
        let on_the_strip = [self.hits.pause, self.hits.cancel]
            .iter()
            .flatten()
            .any(|area| area.contains(position.x, position.y));
        if over_tool
            || on_the_strip
            || self.crumb_at(position).is_some()
            || self.layout.button_at(position.x, position.y).is_some()
            || self.place_at(position).is_some()
        {
            return sk_plat::Cursor::Hand;
        }
        // Over a button that is drawn and dimmed, and nothing else is under it.
        if self.layout.tool_at(position.x, position.y).is_some() {
            return sk_plat::Cursor::Arrow;
        }
        if self.placement.here.header.contains(position.x, position.y)
            && self.column_at(position.x).is_some()
        {
            return sk_plat::Cursor::Hand;
        }
        // A field with a caret in it is not a thing to click, it is text to put
        // the caret into — and both of these fields already accept a click that
        // does exactly that.
        let over_field = self.typing_path.is_some()
            && crate::view::path_field(&self.layout, &self.theme.metrics)
                .contains(position.x, position.y)
            || self.edits.naming.is_some() && self.naming_field().contains(position.x, position.y);
        if over_field {
            return sk_plat::Cursor::Text;
        }
        sk_plat::Cursor::Arrow
    }

    /// Sets the pointer's shape, and remembers what was asked for.
    ///
    /// Remembered because **the shape itself cannot be read back** — no
    /// compositor exposes it — so the only observable half is what the program
    /// asked for, and that is the half that is ours to be right about. It goes on
    /// the diagnostic line, which is how `cursor-says-what-is-clickable` can
    /// assert about it at all.
    fn point(&mut self, cursor: sk_plat::Cursor) {
        self.cursor = cursor;
        self.window.set_cursor(cursor);
    }

    /// Which sidebar row is under a point, if any.
    ///
    /// The regions come from the last frame, for the same reason the
    /// breadcrumbs' do: a row's height is a metric but its position depends on
    /// how many sections came before it, and that is known where it is drawn.
    fn place_at(&self, position: Point) -> Option<usize> {
        self.hits.places.iter().position(|area| area.contains(position.x, position.y))
    }

    /// Which path component is under a point, if any.
    ///
    /// The last component is excluded: it is where you already are, and a
    /// breadcrumb that navigates to the current directory is a reload wearing a
    /// disguise. The regions come from the last frame, which is the only place
    /// the shaped width of a name is known.
    fn crumb_at(&self, position: Point) -> Option<usize> {
        let last = self.hits.crumbs.len().checked_sub(1)?;
        self.hits.crumbs[..last]
            .iter()
            .position(|area| area.width > 0.0 && area.contains(position.x, position.y))
    }

    /// Lights the separator and the menu row under the pointer.
    ///
    /// Both, because a menu hanging from a separator is one thing on screen and
    /// a pointer moving from the chevron into the list should not lose the
    /// highlight on the way.
    fn hover_the_path_bar(&mut self, position: Point) {
        let separator = self.separator_at(position);
        if self.hovered_separator != separator {
            self.hovered_separator = separator;
            self.dirty = true;
        }
        if let Some(offset) = self.crumb_menu_at(position)
            && let Some(menu) = &mut self.crumb_menu
            && menu.hover(offset)
        {
            self.dirty = true;
        }
    }

    /// A press while a separator's menu is open. Whether it was taken.
    ///
    /// A press on a row follows it; a press anywhere else closes the menu and
    /// **falls through**, so the same click also does whatever it was aimed at.
    /// The separator itself is left alone, because the branch that owns it
    /// toggles — closing here would make a second press reopen the menu it had
    /// just shut.
    fn press_in_the_crumb_menu(&mut self, position: Point) -> bool {
        if self.crumb_menu.is_none() && self.crumb_pending.is_none() {
            return false;
        }
        let Some(row) = self.crumb_menu_at(position) else {
            // The separator itself is left alone: the branch that owns it
            // toggles, and closing here would make a second press reopen what it
            // had just shut.
            if self.separator_at(position).is_some() {
                return false;
            }
            // **Closes, and only closes.** A press that dismisses a menu must not
            // also act behind it, which is the rule the context menu follows and
            // the reason a row under this panel does not light up either.
            self.close_crumb_menu();
            return true;
        };
        if let Some(menu) = &mut self.crumb_menu {
            menu.hover(row);
        }
        self.follow_crumb_menu();
        true
    }

    /// Which drawn row of the sibling menu is under a point, as an offset from
    /// the first drawn.
    fn crumb_menu_at(&self, position: Point) -> Option<usize> {
        self.hits.crumb_menu.iter().position(|area| area.contains(position.x, position.y))
    }

    /// Which breadcrumb separator is under a point, if any.
    fn separator_at(&self, position: Point) -> Option<usize> {
        self.hits
            .separators
            .iter()
            .position(|area| area.width > 0.0 && area.contains(position.x, position.y))
    }

    /// The path a breadcrumb at `depth` names: the current path, that deep.
    fn path_to_crumb(&self, depth: usize) -> PathBuf {
        let mut path = PathBuf::from("/");
        let names = self.panes.focused().path.components().filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name.to_owned()),
            _ => None,
        });
        for name in names.take(depth) {
            path.push(name);
        }
        path
    }

    /// Opens the menu of directories inside the component left of a separator.
    ///
    /// **Read on a job**, because rule 1 gives the UI thread no filesystem
    /// syscalls at all and a separator over a hung mount is exactly the case
    /// that rule is for. The menu appears when the answer lands; until then the
    /// chevron stays lit and a second press on it does nothing.
    fn open_crumb_menu(&mut self, depth: usize) {
        if self.crumb_menu.as_ref().is_some_and(|menu| menu.depth == depth) {
            self.close_crumb_menu();
            return;
        }
        // The menu takes the keyboard while it is up, the way the context menu
        // does — so anything else holding it lets go first.
        self.menu = None;
        self.palette = None;
        self.crumb_menu = None;
        self.crumb_pending = Some(depth);
        self.dirty = true;
        let parent = self.path_to_crumb(depth);
        let at = self.hits.separators.get(depth).map_or(0.0, |area| area.x);
        let show_hidden = self.panes.focused().show_hidden;
        let sender = self.replies.siblings_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |cancel| {
            let rows = crate::crumbs::children(&parent, show_hidden, cancel).unwrap_or_default();
            if sender.send(Siblings { depth, parent, at, rows }).is_ok() {
                wake.wake();
            }
        });
    }

    /// Takes delivery of a separator's directory listing.
    fn drain_siblings(&mut self) {
        let Ok(found) = self.replies.siblings_rx.try_recv() else { return };
        // A menu nobody is waiting for any more: the separator was pressed
        // again, or something else took the keyboard while the read was running.
        if self.crumb_pending != Some(found.depth) {
            return;
        }
        self.crumb_pending = None;
        // The component of the current path that follows this one, which is the
        // row to mark and to open on.
        let along = self
            .panes
            .focused()
            .path
            .strip_prefix(&found.parent)
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|part| part.as_os_str().to_string_lossy().into_owned());
        self.crumb_menu = Some(crate::crumbs::CrumbMenu::new(
            found.depth,
            found.parent,
            found.at,
            found.rows,
            along.as_deref(),
        ));
        self.dirty = true;
    }

    /// One key, while a separator's menu is up. Whether it was taken.
    ///
    /// **Everything is taken.** A menu of directory names is modal in the same
    /// way the context menu is: a chord that reached the list behind it would act
    /// on a selection the menu is covering.
    fn crumb_menu_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.close_crumb_menu(),
            Key::Named(Named::Enter) => self.follow_crumb_menu(),
            Key::Named(Named::Down) if bare => self.step_crumb_menu(1),
            Key::Named(Named::Up) if bare => self.step_crumb_menu(-1),
            _ => {
                // **The diagnostic report passes through every mode**, the same
                // as it does through the context menu: a report that cannot be
                // taken while this is open cannot describe it, and it is the only
                // way the GUI harness can see one. Asked of the registry rather
                // than matched on a key, so rebinding it does not quietly make
                // the menu opaque again.
                let diagnostic = commands
                    .resolve(event, Scope::Global)
                    .and_then(|id| commands.describe(id))
                    .is_some_and(|command| command.id == "help.dump");
                if diagnostic {
                    return false;
                }
                // Anything else dismisses, and dismisses only: a key that closes
                // a menu must not also act behind it.
                self.close_crumb_menu();
            }
        }
        self.dirty = true;
        true
    }

    fn step_crumb_menu(&mut self, delta: isize) {
        if let Some(menu) = &mut self.crumb_menu {
            menu.step(delta);
            self.dirty = true;
        }
    }

    /// Closes the sibling menu, and forgets a read still in flight.
    fn close_crumb_menu(&mut self) {
        if self.crumb_menu.take().is_some() || self.crumb_pending.take().is_some() {
            self.dirty = true;
        }
    }

    /// Goes where the highlighted row of the sibling menu points.
    fn follow_crumb_menu(&mut self) {
        let Some(menu) = &self.crumb_menu else { return };
        let Some(path) = menu.active.and_then(|row| menu.path_of(row)) else { return };
        self.close_crumb_menu();
        self.panes.focused_mut().open(path, &self.jobs);
        self.dirty = true;
    }

    /// Navigates to the prefix of the current path a breadcrumb names.
    fn go_to_crumb(&mut self, depth: usize) {
        let mut path = std::path::PathBuf::from("/");
        let names: Vec<_> = self
            .panes
            .focused()
            .path
            .components()
            .filter_map(|part| match part {
                std::path::Component::Normal(name) => Some(name.to_owned()),
                _ => None,
            })
            .collect();
        // `depth` counts the root as component zero, so it is also how many
        // named components to keep.
        for name in names.iter().take(depth) {
            path.push(name);
        }
        self.panes.focused_mut().open(path, &self.jobs);
        self.dirty = true;
    }

    /// Asks for the context menu the pointer is over.
    ///
    /// **The menu acts on what it points at.** A secondary click on a row the
    /// selection does not hold takes the selection first, which is what every
    /// file manager does and the only behaviour under which "Move to Trash" is
    /// safe to have on a menu at all.
    fn open_menu(&mut self, position: Point) {
        // Whatever else this press does, it closes the menu that is up: every
        // path out of this function is a different menu or none, and leaving the
        // old one behind made a click on the sidebar look like it had opened the
        // list's menu.
        if self.menu.take().is_some() {
            self.dirty = true;
        }
        // A secondary click inside a text field belongs to the field. Without
        // this the path bar — which is the title bar — answered one with the
        // compositor's window menu, over a field with a caret in it.
        if self.typing_path.is_some()
            && crate::view::path_field(&self.layout, &self.theme.metrics)
                .contains(position.x, position.y)
        {
            return;
        }
        // The title bar's menu is the compositor's, which is where "always on
        // top" and "move to workspace" live. Ours to offer and not ours to draw.
        if self.layout.drag.contains(position.x, position.y) {
            self.window.show_window_menu(position);
            return;
        }
        // **Not over the sidebar.** Every action on the background menu is about
        // the directory the list is showing, and a sidebar row is a different
        // directory — a menu offering New folder over `Downloads` would make it
        // somewhere else. What a place's own menu should offer is that place in
        // a new tab and Pin, and a command that runs against `&mut App` cannot
        // be told which place: it needs a target, which the registry has no
        // shape for yet.
        if self.layout.sidebar.contains(position.x, position.y) {
            return;
        }
        if self.edits.naming.is_some() {
            self.commit_name();
        }
        let row = self.row_under(position);
        if let Some(id) = row.and_then(|row| self.panes.focused().listing.at(row))
            && !self.panes.focused().selection.contains(id)
        {
            self.panes.focused_mut().selection.replace_with(id);
        }
        let groups = if row.is_some() { crate::menu::ON_ROW } else { crate::menu::ON_BACKGROUND };
        self.menu_wanted = Some((position, groups));
    }

    /// Opens the context menu from the keyboard, or closes an open one.
    ///
    /// At the cursor row rather than at the pointer, because a keyboard user's
    /// attention is on the cursor and the pointer may be on another monitor.
    pub(crate) fn toggle_menu(&mut self) {
        if self.menu.take().is_some() {
            self.dirty = true;
            return;
        }
        let height = self.theme.metrics.row_height;
        let at = match self.panes.focused().cursor_row() {
            Some(row) => {
                let top = self.placement.here.body.y + row as f32 * height
                    - self.panes.focused().scroll.offset();
                Point::new(
                    self.placement.here.name.x,
                    (top + height).max(self.placement.here.body.y),
                )
            }
            None => Point::new(self.placement.here.body.x + 8.0, self.placement.here.body.y + 8.0),
        };
        let groups = if self.panes.focused().cursor_row().is_some() {
            crate::menu::ON_ROW
        } else {
            crate::menu::ON_BACKGROUND
        };
        self.menu_wanted = Some((at, groups));
    }

    /// Which listing row is under a point, or `None` outside the list.
    fn row_under(&self, position: Point) -> Option<usize> {
        self.cells().index_at(position.x, position.y, self.panes.focused().scroll.offset())
    }

    /// Which drawn palette result is under a point, if the palette is open.
    ///
    /// An offset from the first drawn row, which is what the hit rectangles are
    /// built from.
    fn palette_at(&self, position: Point) -> Option<usize> {
        self.palette.as_ref()?;
        self.hits.palette.iter().position(|area| area.contains(position.x, position.y))
    }

    /// A press while the palette is open: a result, the field, or dismiss.
    ///
    /// **A dismissing press does nothing else**, for the reason a menu's does
    /// not: the click that closes a modal surface must not also land on the row
    /// underneath it.
    fn palette_press(&mut self, position: Point, commands: &Registry<Self>) {
        if let Some(offset) = self.palette_at(position) {
            self.run_palette_row(offset, commands);
            return;
        }
        if let Some(area) = self.hits.palette_field.filter(|a| a.contains(position.x, position.y)) {
            let size = self.theme.metrics.path_size;
            self.place_caret_in(Claim::Palette, area, size, crate::palette::INSET, position.x);
            return;
        }
        self.palette = None;
        self.dirty = true;
    }

    /// Which menu row is under a point, if a menu is open.
    fn menu_at(&self, position: Point) -> Option<usize> {
        self.menu.as_ref()?;
        self.hits.menu.iter().position(|area| area.contains(position.x, position.y))
    }

    /// Asks the registry again which rows of an open menu can run.
    ///
    /// **Every frame, for the reason the navigation buttons' dimming is.** The
    /// answers were taken once when the menu was built, so a menu opened while an
    /// operation was in flight went on showing Rename and Undo dimmed after it
    /// landed — and a click on a row that had become runnable was dropped
    /// silently, because `menu_press` filters on the stale answer.
    fn settle_menu_state(&mut self, commands: &Registry<Self>) {
        let Some(menu) = self.menu.as_ref() else { return };
        // On the stack rather than in a `Vec`: this runs in every frame a menu is
        // open, and no menu in the program has half this many rows.
        let mut wanted = [false; 32];
        let rows = menu.items.len().min(wanted.len());
        for (slot, item) in wanted.iter_mut().zip(&menu.items) {
            *slot = commands.find(item.id).is_some_and(|id| commands.enabled(id, self));
        }
        let Some(menu) = self.menu.as_mut() else { return };
        let mut moved = false;
        for (item, enabled) in menu.items.iter_mut().zip(wanted).take(rows) {
            moved |= item.enabled != enabled;
            item.enabled = enabled;
        }
        // A highlight cannot sit on a row that has stopped being runnable.
        if menu.active.is_some_and(|row| !menu.items.get(row).is_some_and(|item| item.enabled)) {
            menu.active = None;
            moved = true;
        }
        if moved {
            self.dirty = true;
        }
    }

    /// A press while a menu is open: it runs a row, or it dismisses.
    ///
    /// **A dismissing press does nothing else.** The click that closes a menu
    /// must not also land on the row underneath it — which is what every menu on
    /// the desktop does, and the difference between closing a menu and trashing
    /// the file the menu was over.
    fn menu_press(&mut self, position: Point, commands: &Registry<Self>) {
        let chosen = self
            .menu_at(position)
            .and_then(|row| self.menu.as_ref()?.items.get(row))
            .filter(|item| item.enabled)
            .map(|item| item.id);
        self.menu = None;
        self.dirty = true;
        if let Some(id) = chosen.and_then(|id| commands.find(id)) {
            commands.run(id, self);
        }
    }

    /// One key to an open menu. Returns whether it was consumed.
    ///
    /// Anything else closes the menu and falls through, so the menu never traps
    /// the keyboard: `F1` from an open menu opens the keyboard list, the way it
    /// does from everywhere else.
    fn menu_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        // Read before the menu is borrowed: a highlight that moves has to be
        // brought into view, and how much view there is is the window's.
        let room = self.layout.window.height - crate::menu::MARGIN * 2.0;
        let Some(menu) = self.menu.as_mut() else { return false };
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.menu = None,
            Key::Named(Named::Down) if bare => {
                menu.step(true);
                menu.reveal(room);
            }
            Key::Named(Named::Up) if bare => {
                menu.step(false);
                menu.reveal(room);
            }
            Key::Named(Named::Enter) => {
                let chosen = menu.active_id();
                self.menu = None;
                if let Some(id) = chosen.and_then(|id| commands.find(id)) {
                    commands.run(id, self);
                }
            }
            _ => {
                // **The diagnostic report passes through every mode, this one
                // included.** A report that cannot be taken while a menu is open
                // cannot describe the menu, and it is the only way the GUI
                // harness can see one at all. Asked of the registry rather than
                // matched on the key, so rebinding the report does not quietly
                // make the menu opaque again.
                let diagnostic = commands
                    .resolve(event, Scope::Global)
                    .and_then(|id| commands.describe(id))
                    .is_some_and(|command| command.id == "help.dump");
                if diagnostic {
                    return false;
                }
                // Anything else dismisses, and dismisses only: a key that
                // closes a menu must not also act behind it, which is the rule
                // the pointer already follows.
                self.menu = None;
                self.dirty = true;
            }
        }
        self.dirty = true;
        true
    }

    /// Opens the palette, or closes it if it is already open.
    pub(crate) fn toggle_palette(&mut self) {
        if self.palette.take().is_some() {
            self.dirty = true;
            return;
        }
        self.claim_keyboard(Claim::Palette);
        // Built here rather than held, so what it lists is what can run *now* —
        // the same question a menu row asks when it opens.
        let commands = crate::commands::registry();
        self.palette = Some(crate::palette::open(&commands, |id| {
            commands.find(id).is_some_and(|command| commands.enabled(command, self))
        }));
        self.dirty = true;
    }

    /// The palette's keys: the field, the arrows, `Enter` and `Escape`.
    ///
    /// Returns whether it consumed the press. Almost everything is consumed —
    /// see [`App::palette_key`]'s caller — and the two exceptions are the
    /// diagnostic report, for the reason the menu lets it through, and the chord
    /// that opened it, so the same key closes it again.
    fn palette_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        let opener = commands
            .resolve(event, Scope::Global)
            .and_then(|id| commands.describe(id))
            .map(|command| command.id);
        if opener == Some("help.dump") {
            return false;
        }
        if opener == Some("view.palette") {
            self.palette = None;
            self.dirty = true;
            return true;
        }
        let Some(palette) = self.palette.as_mut() else { return false };
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.palette = None,
            Key::Named(Named::Down) if bare => palette.step(true),
            Key::Named(Named::Up) if bare => palette.step(false),
            Key::Named(Named::Enter) => {
                let chosen = palette.active_id();
                self.palette = None;
                if let Some(id) = chosen.and_then(|id| commands.find(id)) {
                    commands.run(id, self);
                }
            }
            _ => {
                // Every other press goes to the field, which is what makes this
                // a search box rather than a list with a text box beside it.
                if palette.query.key(event) {
                    palette.refilter();
                } else {
                    return false;
                }
            }
        }
        self.dirty = true;
        true
    }

    /// Runs the palette result a click landed on.
    ///
    /// `offset` counts from the first *drawn* row, because that is what the hit
    /// rectangles are built from — a palette scrolled down four rows would
    /// otherwise run the fourth command above the one the pointer was on.
    fn run_palette_row(&mut self, offset: usize, commands: &Registry<Self>) {
        let chosen = self.palette.as_ref().and_then(|palette| {
            let (first, _) = palette.window();
            palette.row(first + offset).filter(|row| row.enabled).map(|row| row.id)
        });
        self.palette = None;
        self.dirty = true;
        if let Some(id) = chosen.and_then(|id| commands.find(id)) {
            commands.run(id, self);
        }
    }

    /// Lights whatever overlay is under the pointer. Whether one took it.
    ///
    /// **In z-order**, and each one answers alone: an overlay is over the window,
    /// so nothing beneath it hovers — a row lighting up behind a panel reads as a
    /// click that will land there, and it will not.
    fn hover_an_overlay(&mut self, position: Point) -> bool {
        // The palette is over everything the menu is over, and over the menu, so
        // it answers first and nothing beneath it hovers.
        if self.palette.is_some() {
            let offset = self.palette_at(position);
            let (first, _) = self.palette.as_ref().map_or((0, 0), crate::palette::Palette::window);
            let clickable = offset
                .and_then(|offset| self.palette.as_ref()?.row(first + offset))
                .is_some_and(|row| row.enabled);
            let hovered = offset.map(|offset| first + offset);
            if self.palette.as_mut().is_some_and(|palette| palette.hover(hovered)) {
                self.dirty = true;
            }
            let over_field =
                self.hits.palette_field.is_some_and(|a| a.contains(position.x, position.y));
            self.point(match (clickable, over_field) {
                (true, _) => sk_plat::Cursor::Hand,
                (false, true) => sk_plat::Cursor::Text,
                (false, false) => sk_plat::Cursor::Arrow,
            });
            return true;
        }
        // An open menu is over everything, so nothing under it hovers: a row
        // lighting up behind a menu reads as a click that will land there.
        if self.menu.is_some() {
            let row = self.menu_at(position);
            // **Only where a click would do something.** `menu_at` answers where
            // the pointer is and not whether anything is there to run, so a
            // dimmed row was reported as a hand — the interface promising a click
            // that `menu_press` then dropped.
            let clickable = row
                .and_then(|row| self.menu.as_ref()?.items.get(row))
                .is_some_and(|item| item.enabled);
            if self.menu.as_mut().is_some_and(|menu| menu.hover(row)) {
                self.dirty = true;
            }
            self.point(if clickable { sk_plat::Cursor::Hand } else { sk_plat::Cursor::Arrow });
            return true;
        }
        // A separator's menu is over the list, so nothing under it hovers: a row
        // lighting up behind a panel reads as a click that will land there, and
        // the click does not — a press outside this menu closes it and does
        // nothing else, which is the rule the context menu already follows.
        //
        // **The return is the whole of it.** Nothing needs putting out first: the
        // pointer has to cross to the chevron to open this, which clears whatever
        // it was lighting on the way, and after that no branch that sets a hover
        // is reached at all.
        //
        // The chevrons keep hovering, because moving between them while one is
        // open is how a person changes their mind about which depth they meant.
        if self.crumb_menu.is_some() || self.crumb_pending.is_some() {
            self.hover_the_path_bar(position);
            let over_row = self.crumb_menu_at(position).is_some();
            let over_chevron = self.separator_at(position).is_some();
            self.point(if over_row || over_chevron {
                sk_plat::Cursor::Hand
            } else {
                sk_plat::Cursor::Arrow
            });
            return true;
        }
        false
    }

    fn moved(&mut self, position: Point, commands: &Registry<Self>) {
        self.pointer = position;
        // A press on the title bar that has now moved is a window drag after all.
        // The threshold is what separates the two: a click is never perfectly
        // still, and a hand on a touchpad moves a pixel while the button is down.
        if let Some(from) = self.title_press {
            let moved = (position.x - from.x).abs().max((position.y - from.y).abs());
            if moved > DRAG_THRESHOLD {
                self.title_press = None;
                // A refused grab is a press that does nothing, which is what it
                // looked like anyway.
                let _ = self.window.start_move();
            }
            return;
        }
        // A drag inside a field selects text. Before the palette's branch, because
        // a drag that started in a field belongs to that field until the button
        // comes up — and after nothing, because it is the most specific thing the
        // pointer can be doing.
        if let Some(which) = self.text_drag {
            self.drag_text(which, position.x);
            return;
        }
        if self.files_drag.is_some() {
            self.drag_files_to(position);
            return;
        }
        if self.hover_an_overlay(position) {
            return;
        }
        let over = self.layout.button_at(position.x, position.y);
        if self.hovered_button != over {
            self.hovered_button = over;
            self.dirty = true;
        }
        let crumb = self.crumb_at(position);
        self.hover_the_path_bar(position);
        if self.hovered_crumb != crumb {
            self.hovered_crumb = crumb;
            self.dirty = true;
        }
        let tool = self.layout.tool_at(position.x, position.y);
        if self.hovered_tool != tool {
            self.hovered_tool = tool;
            self.dirty = true;
        }
        let place = self.place_at(position);
        if self.sidebar.hover != place {
            self.sidebar.hover = place;
            self.dirty = true;
        }
        if self.drag_a_tab(position) {
            return;
        }
        // A divider being dragged owns the pointer until the button comes up.
        if let Some(id) = self.placement.dragging {
            self.drag_the_divider(id, position);
            return;
        }
        let divider = self.divider_at(position);
        if self.placement.hovered_divider != divider {
            self.placement.hovered_divider = divider;
            self.dirty = true;
        }
        let tab = self.leaf_at(position).zip(self.tab_at(position)).map(|(id, (at, _))| (id, at));
        if self.placement.hovered_tab != tab {
            self.placement.hovered_tab = tab;
            self.dirty = true;
        }
        let plus = self.plus_at(position);
        if self.placement.hovered_plus != plus {
            self.placement.hovered_plus = plus;
            self.dirty = true;
        }
        let cursor = self.cursor_at(position, commands);
        self.point(cursor);
        if let Some(grab) = self.thumb_grab {
            self.drag_thumb(position, grab);
            self.dirty = true;
            return;
        }
        if let Some((start, _)) = self.band {
            self.band = Some((start, position));
            if (position.y - start.y).abs() > BAND_SLOP || (position.x - start.x).abs() > BAND_SLOP
            {
                self.select_band(start, position);
            }
            // Falls through to the hover update rather than returning: returning
            // froze the highlight on whatever row the drag started over, so the
            // band and the hover disagreed for the length of the gesture.
            self.dirty = true;
        }
        // **Inside the list, not merely at the same height as it.** `row_at` is
        // told a `y` and answers about rows; the sidebar is at the same `y` as
        // every row of the list, so hovering a place lit the row beside it.
        //
        // **And not under a panel.** A question, a menu or the Properties panel
        // over the list left the row beneath it lighting up as the pointer
        // crossed it — a highlight tracking a pointer that was reading something
        // else, on rows a click could not reach. Every panel records its
        // rectangle; a card that follows the pointer deliberately does not, so a
        // drag still sees the rows under it.
        let under_a_panel =
            self.hits.panels.iter().any(|panel| panel.contains(position.x, position.y));
        let in_the_list =
            !under_a_panel && self.placement.here.body.contains(position.x, position.y);
        let row = in_the_list
            .then(|| {
                self.cells().index_at(position.x, position.y, self.panes.focused().scroll.offset())
            })
            .flatten();
        if self.panes.focused().hover != row {
            self.panes.focused_mut().hover = row;
            self.dirty = true;
        }
    }

    fn select_band(&mut self, from: Point, to: Point) {
        let cells = self.cells();
        let offset = self.panes.focused().scroll.offset();
        let rect = crate::layout::Area {
            x: from.x.min(to.x),
            y: from.y.min(to.y),
            width: (to.x - from.x).abs(),
            height: (to.y - from.y).abs(),
        };
        let mut touched = Vec::new();
        cells.touching(rect, offset, |index| touched.push(index));
        let pane = self.panes.focused_mut();
        pane.selection.clear();
        for row in touched {
            if let Some(id) = pane.listing.at(row) {
                pane.selection.toggle(id);
            }
        }
    }

    /// Where the scrollbar thumb is, in window coordinates.
    ///
    /// The geometry comes from `sk_ui::thumb`, which is also what draws it. The
    /// two used to be separate copies of the same arithmetic and had already
    /// drifted into two spellings of one clamp, one of which panicked.
    fn thumb(&self) -> Option<(f32, f32)> {
        let content = self.cells().content_height();
        let (position, height) = sk_ui::thumb(
            self.panes.focused().scroll.offset(),
            content,
            self.placement.here.body.height,
        )?;
        Some((self.placement.here.body.y + position, height))
    }

    /// Where in the thumb a press landed, as a fraction of its height.
    fn thumb_at(&self, position: Point) -> Option<f32> {
        let (top, height) = self.thumb()?;
        // A generous grab region: the thumb is eight logical pixels wide and a
        // pointer is not, so the hit area is the right edge of the pane.
        let inside_x = position.x >= self.placement.here.body.right() - 20.0;
        let inside_y = position.y >= top && position.y < top + height;
        (inside_x && inside_y).then_some(position.y - top)
    }

    fn drag_thumb(&mut self, position: Point, grab: f32) {
        let Some((_, thumb)) = self.thumb() else { return };
        let track = self.placement.here.body.height;
        let travel = (track - thumb).max(1.0);
        let content = self.cells().content_height();
        let extent = (content - track).max(0.0);
        let fraction = ((position.y - grab - self.placement.here.body.y) / travel).clamp(0.0, 1.0);
        self.panes.focused_mut().scroll.to(fraction * extent);
    }

    fn column_at(&self, x: f32) -> Option<SortColumn> {
        // The header's own geometry, asked rather than repeated: in a grid the
        // three labels are a strip at the left rather than three columns, and a
        // second copy of that arithmetic here would sort by whatever Details
        // would have had under the pointer.
        let columns = crate::view::sort_columns(
            &self.placement.here,
            &self.theme.metrics,
            self.panes.focused().mode,
        );
        for (column, area, ..) in columns {
            if area.width > 0.0 && x >= area.x && x < area.right() {
                return Some(column);
            }
        }
        None
    }

    /// Everything that has to settle before the frame can be described.
    ///
    /// Separate from [`Self::render`] because all of it wants `&mut self` whole,
    /// and the description below splits `self` into its fields — so anything
    /// left here would have to be answered twice or borrowed around.
    fn before_frame(&mut self, commands: &Registry<Self>) -> ((u32, u32), f32, [bool; 4]) {
        let physical = self.window.physical();
        if physical != self.renderer.size() {
            self.renderer.resize(physical);
            self.physical = physical;
        }
        let scale = self.window.scale();
        self.settle_rows();
        // Asked here for the same reason: the answer is whether each navigation
        // command can run, and only the registry knows that — rule 2 again, so
        // the dimming and the click cannot disagree.
        let tools = tool_state(commands, self);
        self.settle_menu_state(commands);
        (physical, scale, tools)
    }

    // The same shape `assemble` is, and settled the same way: almost all of it
    // is one struct literal with one call site, over fields the destructure has
    // already split the borrow of. Two halves would each hold part of an `App`
    // and neither could be read without the other.
    #[expect(clippy::too_many_lines, reason = "a struct literal with one call site")]
    fn render(&mut self, commands: &Registry<Self>, turn: Instant) {
        let (physical, scale, tools) = self.before_frame(commands);
        self.list.begin(physical);

        let Self {
            list,
            fonts,
            cache,
            theme,
            panes,
            layout,
            frames,
            last,
            renderer,
            jobs,
            show_keys,
            show_stats,
            typed,
            band,
            files_drag,
            pointer,
            adapter,
            backend,
            scale_120,
            watching,
            notice,
            capabilities,
            hovered_button,
            hovered_crumb,
            hovered_separator,
            crumb_menu,
            hovered_tool,
            sidebar,
            edits,
            filtering,
            desktop,
            window_state,
            hits,
            hint,
            menu,
            typing_path,
            palette,
            erasing,
            search,
            properties,
            openers,
            placement,
            indexes,
            ..
        } = self;

        // Built only when the overlay is up, which is why it may allocate.
        let rows = show_keys.then(|| crate::commands::overlay_rows(commands));
        // An index build has a count and no total — the total is what the walk is
        // going to find out — so it takes the sentence and not the bar. A climbing
        // count is honest; a bar that guesses at a denominator is the indeterminate
        // spinner [rule 7](../../../docs/03-principles.md) refuses.
        let indexing = indexes.building().map(|(_, count)| {
            let done = sk_ui::format::grouped(count);
            // Once the counting pass has landed, the sentence says the
            // denominator the bar is drawn from — a bar with no number beside it
            // is a bar a person has to trust rather than read.
            indexes.counted().map_or_else(
                || sk_model::i18n::format("index.building", &[&done]),
                |total| {
                    let all = sk_ui::format::grouped(total);
                    sk_model::i18n::format("index.building_of", &[&done, &all])
                },
            )
        });
        let progress = edits.progress_line().or(indexing);
        let doomed: Option<(Vec<String>, usize, bool)> = erasing.as_ref().map(|asking| {
            let names = asking
                .paths
                .iter()
                .take(crate::view::ERASE_NAMES)
                .map(|path| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .collect();
            (names, asking.paths.len(), asking.on == Confirm::Delete)
        });
        let dropping = files_drag.as_ref().filter(|drag| drag.active);
        // Only for a drag this window started: a peer's carries paths we have
        // not read and names we would have to invent.
        let carrying = dropping.filter(|drag| !drag.paths.is_empty()).map(|drag| {
            let first = drag.paths.first();
            crate::view::Carrying {
                at: *pointer,
                name: first
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                count: drag.paths.len(),
                directory: first.is_some_and(|path| path.is_dir()),
            }
        });
        let chrome = Chrome {
            keys: rows.as_deref(),
            stats: *show_stats,
            typed: (!typed.is_empty()).then_some(typed.as_str()),
            band: *band,
            watching: *watching,
            // Progress wins over a notice while an operation is running: what a
            // person wants to see during a copy is the copy, and the notice that
            // preceded it has already been read.
            notice: progress.as_deref().or(notice.as_ref().map(|(message, _)| message.as_str())),
            hovered_button: *hovered_button,
            hovered_crumb: *hovered_crumb,
            hovered_separator: *hovered_separator,
            crumb_menu: crumb_menu.as_ref(),
            hovered_tool: *hovered_tool,
            places: &sidebar.sections,
            hovered_place: sidebar.hover,
            drop_row: dropping.and_then(|drag| drag.landing.row),
            drop_tab: dropping.and_then(|drag| drag.landing.tab),
            drop_pane: dropping.and_then(|drag| drag.landing.into_pane),
            carrying: carrying.as_ref(),
            search: search.as_ref(),
            clash: edits.clash.as_ref(),
            erasing: doomed.as_ref().map(|(names, total, on)| (names.as_slice(), *total, *on)),
            answering: edits.answering,
            for_the_rest: edits.for_the_rest,
            // The copy's bar if one is running, the index build's otherwise. Both
            // are a real fraction of a counted total or nothing at all.
            progress: edits.fraction().or_else(|| indexes.fraction()),
            // Only where there is something a latch can hold. See `can_pause`.
            pausable: edits.busy(),
            paused: edits.held(),
            naming: edits.naming.as_ref(),
            tools,
            filtering: *filtering,
            types: desktop.as_deref().map(|it| &it.mime),
            window: *window_state,
            menu: menu.as_ref(),
            path_entry: typing_path.as_ref(),
            palette: palette.as_ref(),
            properties: properties.as_ref(),
            openers: openers.as_ref(),
            hint,
            leaves: &placement.leaves,
            dividers: &placement.dividers,
            hovered_divider: placement.hovered_divider,
            hovered_tab: placement.hovered_tab,
            hovered_plus: placement.hovered_plus,
            tab_drop: placement.tab_drop,
        };
        let now = clock_seconds();
        let mut diagnostics = diagnostics_for(
            frames,
            *last,
            renderer,
            cache,
            jobs,
            adapter,
            backend,
            physical,
            *scale_120,
            capabilities,
        );

        {
            let mut painter = Painter { list, fonts, cache, theme, scale };
            view::draw(&mut painter, panes, layout, now, &chrome, &mut diagnostics, hits);
        }
        self.submit(turn, physical);
    }

    /// Hands the frame to the GPU and records what the turn cost.
    ///
    /// **The clock stops before the submission, not after it.** The renderer's
    /// own wall time contains the wait for a presentation slot, which is the
    /// compositor's pacing rather than a cost of ours — so the turn's time is
    /// taken here and the renderer's CPU share is added from `FrameStats`.
    fn submit(&mut self, turn: Instant, physical: (u32, u32)) {
        let turn_us = turn.elapsed().as_micros() as u64;
        let background = self.theme.palette.background;
        let stats = self.renderer.draw(&mut self.list, &mut self.fonts, background);
        self.last = stats;
        self.frames.record(turn_us + stats.cpu_us);

        if !self.first_frame_reported {
            self.first_frame_reported = true;
            self.report_first_frame(turn_us + stats.cpu_us, physical);
        }
    }

    /// The cold-start attribution, once.
    ///
    /// Every number in it has been the answer to a real question: which of the
    /// four stages the 80 ms budget went into, which adapter was chosen on a
    /// hybrid machine, and whether this compositor decorates.
    fn report_first_frame(&self, frame_us: u64, physical: (u32, u32)) {
        let (window_us, gpu_us, font_us) = self.open_time;
        eprintln!(
            "skerry: first frame in {:.1} ms — window {:.1}, gpu {:.1}, fonts {:.1}, \
             frame {:.1}; {} on {}, surface {}×{} at {}%; platform: {}",
            self.started.elapsed().as_secs_f64() * 1000.0,
            window_us as f64 / 1000.0,
            gpu_us as f64 / 1000.0,
            font_us as f64 / 1000.0,
            frame_us as f64 / 1000.0,
            self.adapter,
            self.backend,
            physical.0,
            physical.1,
            self.scale_120 * 100 / 120,
            self.capabilities,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::expand_home;
    use std::path::Path;

    /// `~other` is a shell feature that needs `getpwnam`. Expanding it as this
    /// user's home would send a person somewhere they did not ask for, and
    /// treating it as a relative directory called `~other` at least fails with a
    /// message that names what it looked for.
    #[test]
    fn only_this_users_home_is_expanded() {
        // SAFETY: single-threaded test, and the value is restored below.
        let saved = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/home/tester") };

        assert_eq!(expand_home("~").as_deref(), Some(Path::new("/home/tester")));
        assert_eq!(expand_home("~/").as_deref(), Some(Path::new("/home/tester")));
        assert_eq!(
            expand_home("~/Downloads/x").as_deref(),
            Some(Path::new("/home/tester/Downloads/x"))
        );
        assert_eq!(expand_home("~other").as_deref(), None);
        assert_eq!(expand_home("~other/files").as_deref(), None);
        assert_eq!(expand_home("/etc").as_deref(), None);
        assert_eq!(expand_home("relative").as_deref(), None);

        match saved {
            // SAFETY: as above.
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
