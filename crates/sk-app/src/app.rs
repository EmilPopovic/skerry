//! The frame loop.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sk_cmd::Registry;
use sk_desktop::{Applications, Launcher, MimeDatabase};
use sk_gpu::{DrawList, FrameStats, Renderer};
use sk_model::{Problem, ProblemKind, SortColumn};
use sk_plat::{Button, Content, Event, Point, Selection, Size, Window, WindowConfig};
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

/// How far the pointer must travel before a press becomes a rubber band.
///
/// Without it, every click in empty space flickers a one-pixel band.
const BAND_SLOP: f32 = 3.0;

/// How to start.
pub struct Options {
    /// The directory to open.
    pub path: PathBuf,
    /// Whether to start in the dark palette.
    pub dark: bool,
    /// Whether the statistics overlay is on from the first frame.
    pub stats: bool,
    /// Ask for a client-drawn window frame even where the compositor offers one.
    pub own_frame: bool,
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
        self.pane.stop();
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
    desktop_rx: Receiver<std::sync::Arc<Desktop>>,
    paste_tx: std::sync::mpsc::Sender<Pasted>,
    paste_rx: Receiver<Pasted>,
    launched_tx: std::sync::mpsc::Sender<Launched>,
    launched_rx: Receiver<Launched>,
    watcher: sk_fs::Watcher,
    changes: Receiver<sk_fs::Change>,
}

impl Background {
    fn start(path: PathBuf) -> Result<Self, Problem> {
        let waker = Waker::new().map_err(|error| {
            Problem::plain(ProblemKind::OutOfResources, format!("eventfd: {error}"))
        })?;
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs { pool: Pool::new(), wake: waker.handle(), sender };
        let mut pane = Pane::new(path);
        pane.reload(&jobs);

        let desktop_rx = read_desktop_databases(&jobs, &waker);
        let (paste_tx, paste_rx) = mpsc::channel();
        let (launched_tx, launched_rx) = mpsc::channel();
        let (watch_sender, changes) = mpsc::channel();
        let watcher = sk_fs::Watcher::new(watch_sender, waker.handle()).map_err(|error| {
            Problem::plain(ProblemKind::OutOfResources, format!("inotify: {error}"))
        })?;
        watcher.watch(pane.path.clone());

        Ok(Self {
            waker,
            jobs,
            updates,
            pane,
            desktop_rx,
            paste_tx,
            paste_rx,
            launched_tx,
            launched_rx,
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
    crate::layout::Tool::ALL
        .map(|tool| commands.find(tool.command()).is_some_and(|id| commands.enabled(id, app)))
}

/// Seconds since the epoch, for the `Modified` column's sense of "today".
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
    /// Visible to `crate::commands`, which is where every action on it lives.
    pub(crate) pane: Pane,
    jobs: Jobs,
    waker: Waker,
    updates: Receiver<Message>,
    watcher: sk_fs::Watcher,
    changes: Receiver<sk_fs::Change>,
    /// The directory the watcher is currently on.
    ///
    /// Compared against the pane's path every pass rather than retargeted by
    /// each navigation command, so a new way of navigating cannot forget to do
    /// it — the same reasoning as the `Waker` that `pump` now owns.
    watch_target: PathBuf,
    /// When the last change-driven read was started.
    refreshed: Instant,
    /// A change arrived inside [`REFRESH_FLOOR`] and is still owed a read.
    refresh_owed: bool,
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
    /// Launches that have started, or failed to.
    launched_rx: Receiver<Launched>,
    launched_tx: std::sync::mpsc::Sender<Launched>,
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
    /// The navigation button under the pointer.
    hovered_tool: Option<crate::layout::Tool>,
    /// The hit regions the last frame produced.
    hits: crate::view::Hits,
    /// The path being typed over the breadcrumbs, while one is.
    typing_path: Option<crate::field::Field>,
    /// The context menu, while one is open.
    menu: Option<crate::menu::Menu>,
    /// A menu a command asked for, and where it goes.
    ///
    /// A command's `run` is `fn(&mut App)` and the rows come from the registry,
    /// which is not part of an `App` — so the keyboard's way of opening a menu
    /// leaves the request here and [`App::handle`] builds it, which is the one
    /// place a menu is ever constructed.
    menu_wanted: Option<(Point, &'static [&'static [&'static str]])>,
    /// Bytes read from a peer's clipboard, once the job that read them is done.
    paste_rx: Receiver<Pasted>,
    paste_tx: std::sync::mpsc::Sender<Pasted>,
    layout: Layout,
    frames: Frames,
    last: FrameStats,
    pending: Vec<Event>,

    adapter: String,
    backend: String,
    logical: Size,
    physical: (u32, u32),
    scale_120: u32,

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
    /// The offset within the scrollbar thumb where a drag began.
    thumb_grab: Option<f32>,
    last_frame: Instant,
    started: Instant,
    first_frame_reported: bool,
    open_time: (u128, u128, u128),
}

impl App {
    /// Opens a window, a device and a directory.
    pub fn new(options: Options) -> Result<Self, Problem> {
        let window_started = Instant::now();
        let window = Window::open(&WindowConfig {
            own_frame: options.own_frame,
            ..WindowConfig::default()
        })?;
        let window_us = window_started.elapsed().as_micros();

        let background = Background::start(options.path)?;
        // Open by default, which is what every file manager a person has met
        // does. Reading what goes in it is a job, started with the others.
        let sidebar =
            crate::sidebar::Sidebar::new(&background.jobs, background.waker.handle(), true);
        let Background {
            waker,
            jobs,
            updates,
            pane,
            desktop_rx,
            paste_tx,
            paste_rx,
            launched_tx,
            launched_rx,
            watcher,
            changes,
        } = background;
        let watch_target = pane.path.clone();

        let Drawing { renderer, fonts, physical, gpu_us, font_us } = open_drawing(&window)?;
        let theme = if options.dark { Theme::dark() } else { Theme::light() };
        let logical = window.logical();
        let scale_120 = window.scale_120();
        let capabilities = describe(&window);
        report_globals(&window);
        let measurer = Measurer::new(waker.handle());
        let edits = crate::edit::Edits::new(waker.handle());
        let now = Instant::now();
        Ok(Self {
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
            pane,
            jobs,
            waker,
            updates,
            watcher,
            changes,
            watch_target,
            refreshed: Instant::now(),
            refresh_owed: false,
            watching: true,
            capabilities,
            measurer,
            sidebar,
            edits,
            desktop: None,
            desktop_rx,
            launcher: Launcher::default(),
            launched_rx,
            launched_tx,
            open_owed: None,
            notice: None,
            window_state: sk_plat::WindowState::default(),
            hovered_button: None,
            hovered_crumb: None,
            hovered_tool: None,
            hits: crate::view::Hits::default(),
            typing_path: None,
            menu: None,
            menu_wanted: None,
            paste_rx,
            paste_tx,
            frames: Frames::new(),
            last: FrameStats::default(),
            pending: Vec::with_capacity(64),
            logical,
            physical,
            scale_120,
            dirty: true,
            closing: false,
            show_keys: false,
            show_stats: options.stats,
            typed: String::new(),
            filtering: false,
            typed_at: now,
            pointer: Point::default(),
            band: None,
            thumb_grab: None,
            last_frame: now,
            started: options.started,
            first_frame_reported: false,
            open_time: (window_us, gpu_us, font_us),
        })
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
            self.drain_launches();
            self.drain_edits();
            if self.sidebar.drain() {
                self.dirty = true;
            }
            self.drain_sizes();
            self.advance();
            if self.dirty {
                self.render(&commands, turn);
                self.dirty = false;
            }
        }
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
            self.refresh_owed.then(|| REFRESH_FLOOR.saturating_sub(self.refreshed.elapsed())),
            self.notice.as_ref().map(|(_, at)| NOTICE_WINDOW.saturating_sub(at.elapsed())),
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
            let Self { updates, pane, jobs, dirty, .. } = self;
            while let Ok(message) = updates.try_recv() {
                if pane.apply(message, jobs) {
                    *dirty = true;
                    landed = true;
                }
            }
        }
        if landed && self.edits.name_after_load.is_some() && !self.pane.loading {
            self.name_when_it_arrives();
        }
    }

    /// Applies change notifications, at most one read per [`REFRESH_FLOOR`].
    fn drain_changes(&mut self) {
        let mut owed = self.refresh_owed;
        while let Ok(change) = self.changes.try_recv() {
            if change == sk_fs::Change::Mounts {
                // A disk plugged in or unmounted. Only the sidebar's Volumes
                // section is affected, and re-reading the listing for it would
                // be a refresh of a directory nothing happened to.
                self.sidebar.reload(&self.jobs);
                continue;
            }
            // Every one of the other three means the same thing to a listing:
            // read it again. `Vanished` reads it again too, and the read fails
            // with a message that says the directory is gone — which is the
            // honest thing to show, rather than the entries it used to have.
            owed = true;
        }
        if !owed {
            return;
        }
        if self.refreshed.elapsed() < REFRESH_FLOOR {
            self.refresh_owed = true;
            return;
        }
        self.refresh_owed = false;
        self.refreshed = Instant::now();
        self.pane.refresh(&self.jobs);
        self.dirty = true;
    }

    /// Takes delivery of folder totals.
    fn drain_sizes(&mut self) {
        if self.measurer.drain() {
            self.dirty = true;
        }
    }

    /// Applies whatever came back from a clipboard read.
    fn drain_paste(&mut self) {
        let Ok(pasted) = self.paste_rx.try_recv() else { return };
        match pasted {
            Pasted::Folder(path) => self.pane.open(path, &self.jobs),
            Pasted::NotAFolder => {
                self.notice = Some((
                    sk_model::i18n::lookup("error.clipboard_not_a_folder").to_owned(),
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
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        if self.pane.scroll.advance(elapsed) {
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
        if self.watch_target != self.pane.path {
            self.watch_target = self.pane.path.clone();
            self.watcher.watch(self.watch_target.clone());
        } else if !self.watcher.armed() && !self.pane.loading && self.pane.problem.is_none() {
            // The watch is gone but the directory is not. That is what an
            // atomic replace looks like — a `git checkout` across a branch that
            // deletes and recreates it, `rsync --delete`, a `mv` over the top —
            // and `IN_IGNORED` disarms the watch for good. Without this the
            // listing refreshed exactly once and was then permanently stale,
            // saying "not watching for changes" until the user navigated away
            // and back. Rate-limited by the same floor as the read it follows.
            if self.refreshed.elapsed() >= REFRESH_FLOOR {
                self.watcher.watch(self.watch_target.clone());
            }
        }
        // Read every pass rather than once at arm time: the thread establishes the
        // watch asynchronously, so the answer is not known when `watch` returns.
        let armed = self.watcher.armed();
        if armed != self.watching {
            self.watching = armed;
            self.dirty = true;
        }
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
            Event::PointerMoved { position, .. } => self.moved(*position),
            Event::PointerLeft => {
                // Every hover, and each of them taken: `||` short-circuits, so a
                // pointer leaving a row left a breadcrumb lit and a window button
                // shaded until it came back.
                let gone = [
                    self.pane.hover.take().is_some(),
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
                Button::Primary => self.release(),
                Button::Secondary if *pressed => self.open_menu(*position),
                _ => {}
            },
            Event::Scroll { delta, precise, .. } => {
                self.pane.scroll.by(delta.y, *precise);
                self.dirty = true;
            }
            Event::KeyUp(_) | Event::Woken => {}
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
        // An open menu takes the keyboard first, and gives back everything it
        // has no use for.
        if self.menu.is_some() && self.menu_key(event, commands) {
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
        // The filter takes the keyboard while it is open, before the registry
        // sees it: a filter you cannot type the letter `h` into because `Ctrl+H`
        // is bound is not a filter. Bare characters only, so every chord still
        // reaches its command.
        if self.filtering && self.filter_key(event) {
            return;
        }
        if let Some(command) = commands.resolve(event) {
            // A disabled command consumes its key and does nothing, which is the
            // same thing a dimmed menu item does. Falling through to type-ahead
            // instead would make `Alt ←` with no history type an arrow.
            commands.run(command, self);
            self.dirty = true;
            return;
        }
        if let Some(text) = keys::type_ahead(event) {
            self.typed.push_str(text);
            self.typed_at = Instant::now();
            if self.pane.jump_to(&self.typed) {
                self.reveal_cursor();
            }
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
            &self.layout,
            &self.theme.metrics,
            naming.row,
            self.pane.scroll.offset(),
        )
    }

    /// Puts the caret where a click inside the name field landed.
    ///
    /// Measured by shaping, not by dividing the width by the character count: a
    /// click halfway along `étude` is not halfway along its bytes. The text
    /// starts six logical pixels inside the box, which is where
    /// `view::name_field_at` draws it — the one number the two share.
    fn place_caret(&mut self, x: f32) {
        let Some(naming) = self.edits.naming.as_mut() else { return };
        let field = crate::view::name_field(
            &self.layout,
            &self.theme.metrics,
            naming.row,
            self.pane.scroll.offset(),
        );
        let scale = self.window.scale();
        // The colour is not read: only the weight and the size decide an advance.
        let style = sk_ui::TextStyle::new(self.theme.metrics.text_size, self.theme.palette.text);
        naming.field.caret = sk_ui::caret_offset(
            &mut self.cache,
            &mut self.fonts,
            &naming.field.text,
            style,
            style.size * scale,
            (x - field.x - 6.0) * scale,
        );
        self.dirty = true;
    }

    /// The scroll extent, and the folder totals for what is on screen.
    ///
    /// Once per frame, and bounded by the viewport rather than by the directory:
    /// keyed by path rather than by row, so a new listing shows what is already
    /// known instead of starting again.
    fn settle_rows(&mut self) {
        let content = self.pane.listing.rows() as f32 * self.theme.metrics.row_height;
        self.pane.scroll.set_extent(content, self.layout.body.height);
        let visible = sk_ui::visible_rows(
            self.pane.scroll.offset(),
            self.layout.body.height,
            self.theme.metrics.row_height,
            self.pane.listing.rows(),
        );
        let Self { measurer, pane, jobs, .. } = self;
        measurer.update(pane, visible, jobs);
    }

    /// Puts the caret where a click in the path field landed.
    fn place_path_caret(&mut self, x: f32) {
        let Some(field) = self.typing_path.as_mut() else { return };
        let area = crate::view::path_field(&self.layout, &self.theme.metrics);
        let scale = self.window.scale();
        let style = sk_ui::TextStyle::new(self.theme.metrics.path_size, self.theme.palette.text);
        field.caret = sk_ui::caret_offset(
            &mut self.cache,
            &mut self.fonts,
            &field.text,
            style,
            style.size * scale,
            (x - area.x - crate::view::PATH_INSET) * scale,
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

    /// Opens the path bar for typing, or closes it.
    ///
    /// **The whole path, with the caret at the end.** A path bar that opens empty
    /// is a path bar you cannot use to go one directory sideways, which is most
    /// of what typing a path is for.
    pub(crate) fn toggle_path_entry(&mut self) {
        // Two fields with a caret in them is one field too many, and the name
        // field commits when the focus leaves it — the same thing a click
        // outside it does.
        if self.edits.naming.is_some() {
            self.commit_name();
        }
        self.typing_path = match self.typing_path {
            Some(_) => None,
            None => Some(crate::field::Field::new(self.pane.path.to_string_lossy().into_owned())),
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
            if path.is_absolute() { path.to_path_buf() } else { self.pane.path.join(path) }
        });
        if path != self.pane.path {
            self.pane.open(path, &self.jobs);
        }
    }

    /// Feeds one key to the open filter. Returns whether it was consumed.
    fn filter_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        let mut wanted = self.pane.filter.clone();
        match event.key {
            sk_plat::Key::Named(sk_plat::Named::Backspace) if event.mods.none() => {
                if wanted.pop().is_none() {
                    // Backspace on an empty filter closes it, which is what
                    // every search field a person has met does.
                    self.filtering = false;
                    self.dirty = true;
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
        if self.pane.set_filter(wanted, &self.jobs) {
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
        let parent = self.pane.path.clone();
        self.edits.new_directory(parent, &self.jobs);
        self.edits.name_after_load = Some(String::new());
    }

    /// Starts renaming whatever the cursor is on.
    pub(crate) fn start_rename(&mut self) {
        let Some(id) = self.pane.focused() else { return };
        let Some(row) = self.pane.listing.row_of(id) else { return };
        let name = self.pane.listing.name(id).to_owned();
        self.pane.scroll.reveal(
            row as f32 * self.theme.metrics.row_height,
            (row + 1) as f32 * self.theme.metrics.row_height,
            self.layout.body.height,
        );
        self.edits.naming = Some(crate::edit::Naming {
            row,
            path: self.pane.path.join(&name),
            field: crate::field::Field::new(name),
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

    /// Moves what is selected to the trash.
    pub(crate) fn trash_selection(&mut self) {
        let paths = self.selected_paths();
        // One at a time: the queue with honest progress is M3's and this is not
        // it. What a person selects here is a handful of files, and each one is
        // its own undo entry — so the first goes now and the rest are owed.
        let Some(first) = paths.first().cloned() else { return };
        // The cursor leaves the doomed row now rather than when the listing
        // comes back: a person deleting four files presses Delete four times,
        // and a cursor that lands nowhere makes the second press do nothing at
        // all — `enabled` reads `focused`.
        self.step_off_the_cursor();
        if paths.len() > 1 {
            self.notice = Some((
                sk_model::i18n::format("notice.one_at_a_time", &[&paths.len()]),
                Instant::now(),
            ));
        }
        self.edits.trash(first, &self.jobs);
    }

    /// Moves the cursor to the row after the one it is on, or the one before it
    /// where there is no after.
    fn step_off_the_cursor(&mut self) {
        let Some(row) = self.pane.cursor_row() else { return };
        let last = self.pane.listing.rows().saturating_sub(1);
        let next = if row < last { row + 1 } else { row.saturating_sub(1) };
        if next != row {
            self.pane.cursor_to(next, false);
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

    /// Shows or hides the list of places.
    pub(crate) fn toggle_sidebar(&mut self) {
        self.sidebar.open = !self.sidebar.open;
        self.sidebar.hover = None;
        self.relayout();
        self.dirty = true;
    }

    /// Opens the filter, or closes and clears it if it is already open.
    pub(crate) fn toggle_filter(&mut self) {
        if self.filtering {
            self.filtering = false;
            self.pane.set_filter(String::new(), &self.jobs);
        } else {
            self.filtering = true;
        }
        self.dirty = true;
    }

    /// Opens the focused entry: a directory in this window, a file in whatever
    /// the desktop says owns it.
    pub(crate) fn open_focused(&mut self) {
        if self.pane.open_focused(&self.jobs) {
            return;
        }
        let Some(id) = self.pane.focused() else { return };
        let path = self.pane.path.join(self.pane.listing.name(id));
        self.launch(path);
    }

    /// Puts the selected paths on the clipboard.
    pub(crate) fn copy_selection(&mut self) {
        let paths = self.selected_paths();
        let Some(first) = paths.first().cloned() else { return };
        if !self.window.capabilities().clipboard {
            self.notice =
                Some((sk_model::i18n::lookup("error.no_clipboard").to_owned(), Instant::now()));
            return;
        }
        let count = paths.len();
        self.window.set_selection(Selection::Clipboard, Content::paths(&paths));
        // The primary selection too, so a middle click in a terminal pastes what
        // was just copied. Hyprland discards this silently unless
        // `misc:middle_click_paste` is on, which is a documented limitation with
        // no fallback and no way to detect.
        self.window.set_selection(Selection::Primary, Content::paths(&paths));
        let message = if count == 1 {
            let name = first.file_name().map(|name| name.to_string_lossy().into_owned());
            sk_model::i18n::format("notice.copied_one", &[&name.unwrap_or_default()])
        } else {
            sk_model::i18n::format("notice.copied", &[&count])
        };
        self.notice = Some((message, Instant::now()));
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
        let sender = self.paste_tx.clone();
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
        let listing = &self.pane.listing;
        let mut paths: Vec<PathBuf> = self
            .pane
            .selection
            .in_order(listing.order())
            .map(|id| self.pane.path.join(listing.name(id)))
            .collect();
        if let Some(id) = self.pane.focused().filter(|_| paths.is_empty()) {
            paths.push(self.pane.path.join(listing.name(id)));
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
        let sender = self.launched_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(self.pane.lane(), sk_util::Cancel::new(), move |_| {
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

    /// Starts renaming the row a just-finished operation asked for.
    ///
    /// A folder is made and then named, and there is a directory read between the
    /// two: this is the far side of it.
    fn name_when_it_arrives(&mut self) {
        if self.edits.name_after_load.take().is_none() {
            return;
        }
        self.start_rename();
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
                    self.pane.select_after(name.to_string_lossy().into_owned());
                }
                self.pane.refresh(&self.jobs);
                message
            }
            crate::edit::Landed::Failed(message) => message,
        };
        self.notice = Some((message, Instant::now()));
        self.dirty = true;
    }

    /// Takes delivery of a launch: its notice, and the child to reap.
    fn drain_launches(&mut self) {
        while let Ok(launched) = self.launched_rx.try_recv() {
            if let Some(child) = launched.child {
                self.launcher.adopt(child);
            }
            self.notice = Some((launched.notice, Instant::now()));
            self.dirty = true;
        }
    }

    /// Goes to the parent directory.
    pub(crate) fn go_up(&mut self) {
        self.pane.go_up(&self.jobs);
    }

    /// Goes back to where the last navigation came from.
    pub(crate) fn go_back(&mut self) {
        self.pane.go_back(&self.jobs);
    }

    /// Undoes a Back.
    pub(crate) fn go_forward(&mut self) {
        self.pane.go_forward(&self.jobs);
    }

    /// Goes to `$HOME`.
    pub(crate) fn go_home(&mut self) {
        self.pane.go_home(&self.jobs);
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
        self.pane.refresh(&self.jobs);
    }

    /// Moves the cursor by `delta` rows, extending the selection if asked.
    pub(crate) fn move_cursor(&mut self, delta: isize, extend: bool) {
        self.pane.move_cursor(delta, extend);
        self.reveal_cursor();
    }

    /// Moves the cursor by a screenful.
    pub(crate) fn move_page(&mut self, direction: isize) {
        let rows = (self.layout.body.height / self.theme.metrics.row_height).floor().max(1.0);
        self.pane.move_cursor(direction * rows as isize, false);
        self.reveal_cursor();
    }

    /// Moves the cursor to the first row.
    pub(crate) fn go_top(&mut self) {
        self.pane.cursor_to(0, false);
        self.pane.scroll.to(0.0);
    }

    /// Moves the cursor to the last row.
    pub(crate) fn go_bottom(&mut self) {
        let last = self.pane.listing.rows().saturating_sub(1);
        self.pane.cursor_to(last, false);
        self.reveal_cursor();
    }

    /// Clears the selection, the type-ahead buffer and any overlay.
    ///
    /// One key that means "never mind", whatever is going on. Escape doing three
    /// different things depending on state is worse than Escape doing all of
    /// them.
    pub(crate) fn clear(&mut self) {
        self.pane.selection.clear();
        self.typed.clear();
        self.filtering = false;
        self.pane.set_filter(String::new(), &self.jobs);
        self.show_keys = false;
        self.show_stats = false;
    }

    /// Shows or hides dotfiles.
    pub(crate) fn toggle_hidden(&mut self) {
        self.pane.toggle_hidden(&self.jobs);
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
    }

    /// Sorts by `column`, reversing if it is already the sort column.
    pub(crate) fn sort_by(&mut self, column: SortColumn) {
        self.pane.sort_by(column, &self.jobs);
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

    /// What the pane is showing.
    #[expect(clippy::unnecessary_debug_formatting, reason = "the quoting is the point")]
    fn listing_state(&mut self) -> String {
        let cursor = self
            .pane
            .selection
            .cursor()
            .map(|id| self.pane.listing.name(id).to_owned())
            .unwrap_or_default();
        let hover = self.pane.hover.map_or(-1, |row| i64::try_from(row).unwrap_or(-1));
        let hover_name = self
            .pane
            .hover
            .and_then(|row| self.pane.listing.at(row))
            .map(|id| self.pane.listing.name(id).to_owned())
            .unwrap_or_default();
        let (known, running) = self.measurer.stats();
        let (jobs, panics) = self.jobs.pool.stats();
        let naming = self.edits.naming.as_ref().map(|it| it.field.text.clone()).unwrap_or_default();
        let typing_path =
            self.typing_path.as_ref().map(|field| field.text.clone()).unwrap_or_default();
        let naming_row =
            self.edits.naming.as_ref().map_or(-1, |it| i64::try_from(it.row).unwrap_or(-1));
        format!(
            "path={:?} rows={} hidden={} sort={:?} descending={} cursor={cursor:?} \
selected={} scroll={:.1} loading={} watching={} notice={:?} \
hover={hover} hover_name={hover_name:?} \
naming={naming:?} naming_row={naming_row} path_entry={typing_path:?} undo={} redo={} busy={} \
filtering={} filter={:?} sizes={known}/{running} jobs={jobs}/{panics}",
            self.pane.path,
            self.pane.listing.rows(),
            self.pane.show_hidden,
            self.pane.sort.column,
            self.pane.sort.descending,
            self.pane.selection.count(),
            self.pane.scroll.offset(),
            self.pane.loading,
            self.watching,
            self.notice.as_ref().map(|(message, _)| message.as_str()).unwrap_or_default(),
            self.edits.history.can_undo(),
            self.edits.history.can_redo(),
            self.edits.busy(),
            self.filtering,
            self.pane.filter,
        )
    }

    /// Where everything is, so a scenario can aim at it without a copy of the
    /// metrics it is testing.
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
        let place_hover = self.sidebar.hover.map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let place_name = self
            .sidebar
            .hover
            .and_then(|index| self.sidebar.at(index))
            .map(|entry| entry.label.clone())
            .unwrap_or_default();
        let crumb_hover = self.hovered_crumb.map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let menu = self.menu.as_ref();
        let menu_rows = menu.map_or(0, |menu| menu.items.len());
        let menu_active = menu
            .and_then(|menu| menu.active)
            .map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let menu_id = menu.and_then(super::menu::Menu::active_id).unwrap_or_default();
        // The middle of each menu row, so a scenario can aim at one by name
        // rather than by arithmetic over constants it would have to copy.
        let menu_aim = menu.map_or_else(String::new, |menu| {
            menu.items
                .iter()
                .zip(&self.hits.menu)
                .map(|(item, area)| {
                    format!("{}@{:.0}:{:.0}", item.id, area.x + 20.0, area.y + area.height / 2.0)
                })
                .collect::<Vec<_>>()
                .join(",")
        });
        // The middle of the crumb one level up, which is the one a test clicks.
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
menu={menu_rows} menu_active={menu_active} menu_id={menu_id:?} menu_aim={menu_aim:?} \
tool_w={tool_w:.1} tool_hover={tool_hover} \
sidebar_w={:.1} sidebar_top={:.1} places={} places_loaded={} \
place_hover={place_hover} place_name={place_name:?} \
logical={:.0}x{:.0} scale_120={} \
frame_p50_us={} frame_p99_us={} frames={} over={} keys={} stats={}",
            self.layout.body.y,
            self.layout.body.height,
            self.theme.metrics.row_height,
            self.layout.name.x,
            self.layout.size.x,
            self.layout.size.width,
            self.window_state.maximized,
            self.hits.crumbs.len(),
            self.layout.sidebar.width,
            self.layout.sidebar.y,
            self.sidebar.len(),
            self.sidebar.loads,
            self.logical.width,
            self.logical.height,
            self.scale_120,
            self.frames.percentile(0.5),
            self.frames.percentile(0.99),
            self.frames.budget().0,
            self.frames.budget().1,
            self.show_keys,
            self.show_stats,
        )
    }

    fn reveal_cursor(&mut self) {
        let Some(row) = self.pane.cursor_row() else { return };
        let height = self.theme.metrics.row_height;
        self.pane.scroll.reveal(
            row as f32 * height,
            (row + 1) as f32 * height,
            self.layout.body.height,
        );
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
        if self.menu.is_some() {
            self.menu_press(position, commands);
            return;
        }
        // A press outside the field commits, which is what every other text
        // field on the desktop does.
        if self.edits.naming.is_some() {
            if self.naming_field().contains(position.x, position.y) {
                self.place_caret(position.x);
                return;
            }
            self.commit_name();
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
        // Through the registry, not through a method: a dimmed button must do
        // nothing when clicked, and `enabled` is the only place that knows.
        if let Some(index) = self.place_at(position) {
            if let Some(entry) = self.sidebar.at(index) {
                let path = entry.path.clone();
                if path != self.pane.path {
                    self.pane.open(path, &self.jobs);
                }
            }
            self.dirty = true;
            return;
        }
        if let Some(tool) = self.layout.tool_at(position.x, position.y) {
            if let Some(id) = commands.find(tool.command()) {
                commands.run(id, self);
            }
            self.dirty = true;
            return;
        }
        if let Some(button) = self.layout.button_at(position.x, position.y) {
            if let Some(id) = commands.find(button.command()) {
                commands.run(id, self);
            }
            return;
        }
        // The path field before the drag region, for the same reason a
        // breadcrumb is: a press in a text field belongs to the caret, and a
        // press that moved the window instead would make the field unusable.
        if self.typing_path.is_some() {
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
        if let Some(depth) = self.crumb_at(position) {
            self.go_to_crumb(depth);
            return;
        }
        if self.layout.drag.contains(position.x, position.y) {
            // A double click on a title bar toggles maximize everywhere else, so
            // it does here.
            if clicks >= 2 {
                self.toggle_maximized();
            } else {
                // Nothing to fall through to: the title bar holds no rows, and
                // the breadcrumbs were tested above. A refused grab is a press
                // that does nothing, which is what it looked like anyway.
                let _ = self.window.start_move();
            }
            return;
        }
        if self.layout.header.contains(position.x, position.y) {
            if let Some(column) = self.column_at(position.x) {
                self.pane.sort_by(column, &self.jobs);
                self.dirty = true;
            }
            return;
        }
        if !self.layout.body.contains(position.x, position.y) {
            return;
        }
        if let Some(grab) = self.thumb_at(position) {
            self.thumb_grab = Some(grab);
            return;
        }

        let Some(row) = self.row_under(position) else {
            // Empty space: clear, and be ready to rubber-band.
            self.pane.selection.clear();
            self.pane.selection.set_cursor(None);
            self.band = Some((position, position));
            self.dirty = true;
            return;
        };
        let Some(id) = self.pane.listing.at(row) else { return };
        if shift {
            self.pane.selection.extend_to(id, self.pane.listing.order());
        } else if ctrl {
            self.pane.selection.toggle(id);
        } else {
            self.pane.selection.replace_with(id);
        }
        if clicks >= 2 {
            // `App::open_focused`, not `Pane::open_focused`: the pane's one
            // enters directories and returns false for anything else, so a
            // double click on a document used to do nothing at all — no notice,
            // no error — while `Enter` on the same row opened it.
            self.open_focused();
        }
        self.dirty = true;
    }

    fn release(&mut self) {
        // Both, not either: `||` short-circuits, so a release that ended a band
        // left a thumb grab behind. They cannot both be set today, which makes
        // this the kind of latent bug that becomes a stuck scrollbar the first
        // time that stops being true.
        let ended = self.band.take().is_some();
        let released = self.thumb_grab.take().is_some();
        if ended || released {
            self.dirty = true;
        }
    }

    /// What the pointer should look like at a point.
    ///
    /// **The resize band is invisible without this.** Six logical pixels that do
    /// something on a press and nothing to look at is a feature discoverable
    /// only by trying it, and the first thing a person tries is dragging the
    /// edge of a window.
    fn cursor_at(&self, position: Point) -> sk_plat::Cursor {
        if self.thumb_grab.is_some() {
            return sk_plat::Cursor::Grabbing;
        }
        if let Some(edge) = self.layout.edge_at(position.x, position.y) {
            return sk_plat::Cursor::Resize(edge);
        }
        if self.crumb_at(position).is_some()
            || self.layout.button_at(position.x, position.y).is_some()
            || self.layout.tool_at(position.x, position.y).is_some()
            || self.place_at(position).is_some()
        {
            return sk_plat::Cursor::Hand;
        }
        if self.layout.header.contains(position.x, position.y)
            && self.column_at(position.x).is_some()
        {
            return sk_plat::Cursor::Hand;
        }
        sk_plat::Cursor::Arrow
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

    /// Navigates to the prefix of the current path a breadcrumb names.
    fn go_to_crumb(&mut self, depth: usize) {
        let mut path = std::path::PathBuf::from("/");
        let names: Vec<_> = self
            .pane
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
        self.pane.open(path, &self.jobs);
        self.dirty = true;
    }

    /// The compositor's own window menu, which is where "always on top" and
    /// "move to workspace" live. Ours to offer and not ours to draw.
    /// Asks for the context menu the pointer is over.
    ///
    /// **The menu acts on what it points at.** A secondary click on a row the
    /// selection does not hold takes the selection first, which is what every
    /// file manager does and the only behaviour under which "Move to Trash" is
    /// safe to have on a menu at all.
    fn open_menu(&mut self, position: Point) {
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
        if self.edits.naming.is_some() {
            self.commit_name();
        }
        let row = self.row_under(position);
        if let Some(id) = row.and_then(|row| self.pane.listing.at(row))
            && !self.pane.selection.contains(id)
        {
            self.pane.selection.replace_with(id);
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
        let at = match self.pane.cursor_row() {
            Some(row) => {
                let top = self.layout.body.y + row as f32 * height - self.pane.scroll.offset();
                Point::new(self.layout.name.x, (top + height).max(self.layout.body.y))
            }
            None => Point::new(self.layout.body.x + 8.0, self.layout.body.y + 8.0),
        };
        let groups = if self.pane.cursor_row().is_some() {
            crate::menu::ON_ROW
        } else {
            crate::menu::ON_BACKGROUND
        };
        self.menu_wanted = Some((at, groups));
    }

    /// Which listing row is under a point, or `None` outside the list.
    fn row_under(&self, position: Point) -> Option<usize> {
        if !self.layout.body.contains(position.x, position.y) {
            return None;
        }
        self.layout
            .row_at(position.y, self.pane.scroll.offset(), self.theme.metrics.row_height)
            .filter(|row| *row < self.pane.listing.rows())
    }

    /// Which menu row is under a point, if a menu is open.
    fn menu_at(&self, position: Point) -> Option<usize> {
        self.menu.as_ref()?;
        self.hits.menu.iter().position(|area| area.contains(position.x, position.y))
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
        let Some(menu) = self.menu.as_mut() else { return false };
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.menu = None,
            Key::Named(Named::Down) if bare => menu.step(true),
            Key::Named(Named::Up) if bare => menu.step(false),
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
                    .resolve(event)
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

    fn moved(&mut self, position: Point) {
        self.pointer = position;
        // An open menu is over everything, so nothing under it hovers: a row
        // lighting up behind a menu reads as a click that will land there.
        if self.menu.is_some() {
            let row = self.menu_at(position);
            if self.menu.as_mut().is_some_and(|menu| menu.hover(row)) {
                self.dirty = true;
            }
            let cursor = if row.is_some() { sk_plat::Cursor::Hand } else { sk_plat::Cursor::Arrow };
            self.window.set_cursor(cursor);
            return;
        }
        let over = self.layout.button_at(position.x, position.y);
        if self.hovered_button != over {
            self.hovered_button = over;
            self.dirty = true;
        }
        let crumb = self.crumb_at(position);
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
        self.window.set_cursor(self.cursor_at(position));
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
        // **Inside the list, not merely at the same height as it.** `row_at` is
        // told a `y` and answers about rows; the sidebar is at the same `y` as
        // every row of the list, so hovering a place lit the row beside it.
        let row = self
            .layout
            .body
            .contains(position.x, position.y)
            .then(|| {
                self.layout.row_at(
                    position.y,
                    self.pane.scroll.offset(),
                    self.theme.metrics.row_height,
                )
            })
            .flatten()
            .filter(|row| *row < self.pane.listing.rows());
        if self.pane.hover != row {
            self.pane.hover = row;
            self.dirty = true;
        }
    }

    fn select_band(&mut self, from: Point, to: Point) {
        let height = self.theme.metrics.row_height;
        let offset = self.pane.scroll.offset();
        let top = from.y.min(to.y);
        let bottom = from.y.max(to.y);
        let first = ((top - self.layout.body.y + offset) / height).floor().max(0.0) as usize;
        let last = ((bottom - self.layout.body.y + offset) / height).floor().max(0.0) as usize;
        self.pane.selection.clear();
        let order = self.pane.listing.order();
        for row in first..=last.min(order.len().saturating_sub(1)) {
            if let Some(id) = order.get(row) {
                self.pane.selection.toggle(*id);
            }
        }
    }

    /// The scrollbar thumb's rectangle, if there is one.
    /// Where the scrollbar thumb is, in window coordinates.
    ///
    /// The geometry comes from `sk_ui::thumb`, which is also what draws it. The
    /// two used to be separate copies of the same arithmetic and had already
    /// drifted into two spellings of one clamp, one of which panicked.
    fn thumb(&self) -> Option<(f32, f32)> {
        let content = self.pane.listing.rows() as f32 * self.theme.metrics.row_height;
        let (position, height) =
            sk_ui::thumb(self.pane.scroll.offset(), content, self.layout.body.height)?;
        Some((self.layout.body.y + position, height))
    }

    fn thumb_at(&self, position: Point) -> Option<f32> {
        let (top, height) = self.thumb()?;
        // A generous grab region: the thumb is eight logical pixels wide and a
        // pointer is not, so the hit area is the right edge of the pane.
        let inside_x = position.x >= self.layout.body.right() - 20.0;
        let inside_y = position.y >= top && position.y < top + height;
        (inside_x && inside_y).then_some(position.y - top)
    }

    fn drag_thumb(&mut self, position: Point, grab: f32) {
        let Some((_, thumb)) = self.thumb() else { return };
        let track = self.layout.body.height;
        let travel = (track - thumb).max(1.0);
        let content = self.pane.listing.rows() as f32 * self.theme.metrics.row_height;
        let extent = (content - track).max(0.0);
        let fraction = ((position.y - grab - self.layout.body.y) / travel).clamp(0.0, 1.0);
        self.pane.scroll.to(fraction * extent);
    }

    fn column_at(&self, x: f32) -> Option<SortColumn> {
        for (column, area) in [
            (SortColumn::Name, self.layout.name),
            (SortColumn::Size, self.layout.size),
            (SortColumn::Modified, self.layout.modified),
        ] {
            if area.width > 0.0 && x >= area.x && x < area.right() {
                return Some(column);
            }
        }
        None
    }

    fn render(&mut self, commands: &Registry<Self>, turn: Instant) {
        let physical = self.window.physical();
        if physical != self.renderer.size() {
            self.renderer.resize(physical);
            self.physical = physical;
        }
        let scale = self.window.scale();
        self.settle_rows();

        // Asked before the borrow below splits `self` apart: the answer is
        // whether each navigation command can run, and only the registry knows
        // that — rule 2 again, so the dimming and the click cannot disagree.
        let tools = tool_state(commands, self);

        self.list.begin(physical);

        let Self {
            list,
            fonts,
            cache,
            theme,
            pane,
            layout,
            frames,
            last,
            renderer,
            jobs,
            show_keys,
            show_stats,
            typed,
            band,
            adapter,
            backend,
            scale_120,
            watching,
            notice,
            capabilities,
            hovered_button,
            hovered_crumb,
            hovered_tool,
            sidebar,
            edits,
            filtering,
            desktop,
            window_state,
            hits,
            menu,
            typing_path,
            ..
        } = self;

        // Built only when the overlay is up, which is why it may allocate.
        let rows = show_keys.then(|| crate::commands::overlay_rows(commands));
        let chrome = Chrome {
            keys: rows.as_deref(),
            stats: *show_stats,
            typed: (!typed.is_empty()).then_some(typed.as_str()),
            band: *band,
            watching: *watching,
            notice: notice.as_ref().map(|(message, _)| message.as_str()),
            hovered_button: *hovered_button,
            hovered_crumb: *hovered_crumb,
            hovered_tool: *hovered_tool,
            places: &sidebar.sections,
            hovered_place: sidebar.hover,
            naming: edits.naming.as_ref(),
            tools,
            filtering: *filtering,
            types: desktop.as_deref().map(|it| &it.mime),
            window: *window_state,
            menu: menu.as_ref(),
            path_entry: typing_path.as_ref(),
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
            view::draw(&mut painter, pane, layout, now, &chrome, &mut diagnostics, hits);
        }
        // Everything the loop did this turn, up to the submission. The renderer's
        // own wall time is not usable here because it contains the wait for a
        // presentation slot, which is the compositor's pacing and not a cost —
        // so its CPU share is added from `FrameStats` instead.
        let turn_us = turn.elapsed().as_micros() as u64;

        let background = theme.palette.background;
        let stats = renderer.draw(list, fonts, background);
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
