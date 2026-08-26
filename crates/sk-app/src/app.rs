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
use crate::layout::{Frame, Layout};
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
fn describe(found: &sk_plat::Capabilities) -> String {
    [
        ("fractional", found.fractional_scale),
        ("viewport", found.viewporter),
        ("decorations", found.server_side_decorations),
        ("keyboard", found.keyboard),
        ("pointer", found.pointer),
        ("clipboard", found.clipboard),
        ("primary", found.primary_selection),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect::<Vec<_>>()
    .join(" ")
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

        // The pool and the directory read start **before** the GPU. Opening a
        // Vulkan instance costs 67–100 ms on this machine and the read costs
        // single-digit milliseconds, so overlapping them does not make the first
        // frame sooner — it makes the first frame contain the directory instead
        // of an empty list.
        let waker = Waker::new().map_err(|error| {
            Problem::plain(ProblemKind::OutOfResources, format!("eventfd: {error}"))
        })?;
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs { pool: Pool::new(), wake: waker.handle(), sender };
        let mut pane = Pane::new(options.path);
        pane.reload(&jobs);

        let desktop_rx = read_desktop_databases(&jobs, &waker);

        let (paste_tx, paste_rx) = mpsc::channel();
        let (launched_tx, launched_rx) = mpsc::channel();
        let (watch_sender, changes) = mpsc::channel();
        let watcher = sk_fs::Watcher::new(watch_sender, waker.handle()).map_err(|error| {
            Problem::plain(ProblemKind::OutOfResources, format!("inotify: {error}"))
        })?;
        watcher.watch(pane.path.clone());
        let watch_target = pane.path.clone();

        let gpu_started = Instant::now();
        let physical = window.physical();
        // SAFETY: the handles come from `window`, which this struct owns and
        // which is declared after `renderer` — so the renderer is dropped first.
        let renderer = unsafe { Renderer::new(window.raw_handles(), physical) }?;
        let gpu_us = gpu_started.elapsed().as_micros();

        // Beside font discovery for the same reason: both read a file that the
        // first frame would otherwise read.
        sk_ui::format::warm_timezone();

        let font_started = Instant::now();
        let fonts = Fonts::discover()?;
        let font_us = font_started.elapsed().as_micros();
        if !fonts.missing().is_empty() {
            eprintln!("skerry: font chain is short — no {}", fonts.missing().join(", "));
        }

        let theme = if options.dark { Theme::dark() } else { Theme::light() };
        let logical = window.logical();
        let scale_120 = window.scale_120();
        let capabilities = describe(window.capabilities());
        if std::env::var_os("SKERRY_GLOBALS").is_some() {
            // Every interface the compositor advertises, with its version.
            // ADR 0007 says capabilities are probed rather than compiled in; this
            // is the raw material the probe works from, and printing it is how a
            // "why did that not work" question gets an answer in one run.
            for (interface, version) in &window.capabilities().advertised {
                eprintln!("skerry: global {interface} v{version}");
            }
        }
        let measurer = Measurer::new(waker.handle());
        let now = Instant::now();
        Ok(Self {
            adapter: renderer.adapter().to_owned(),
            backend: renderer.backend().to_owned(),
            layout: Layout::compute_with(logical, &theme.metrics, frame_of(&window)),
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
            desktop: None,
            desktop_rx,
            launcher: Launcher::default(),
            launched_rx,
            launched_tx,
            open_owed: None,
            notice: None,
            window_state: sk_plat::WindowState::default(),
            hovered_button: None,
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
        let Self { updates, pane, jobs, dirty, .. } = self;
        while let Ok(message) = updates.try_recv() {
            if pane.apply(message, jobs) {
                *dirty = true;
            }
        }
    }

    /// Applies change notifications, at most one read per [`REFRESH_FLOOR`].
    fn drain_changes(&mut self) {
        let mut owed = self.refresh_owed;
        while let Ok(change) = self.changes.try_recv() {
            // Every one of the three means the same thing to a listing: read it
            // again. `Vanished` reads it again too, and the read fails with a
            // message that says the directory is gone — which is the honest
            // thing to show, rather than the entries it used to have.
            let _ = change;
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
                if self.pane.hover.take().is_some() || self.hovered_button.take().is_some() {
                    self.dirty = true;
                }
            }
            Event::Button { button, pressed, position, clicks, mods } => match button {
                Button::Primary if *pressed => {
                    self.press(*position, *clicks, mods.ctrl, mods.shift);
                }
                Button::Primary => self.release(),
                // The compositor's own window menu, which is where "always on
                // top" and "move to workspace" live. Ours to offer and not ours
                // to draw.
                Button::Secondary if *pressed => self.frame_menu(*position),
                _ => {}
            },
            Event::Scroll { delta, precise, .. } => {
                self.pane.scroll.by(delta.y, *precise);
                self.dirty = true;
            }
            Event::KeyUp(_) | Event::Woken => {}
        }
    }

    fn key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) {
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

    // ------------------------------------------------------------------
    // What the commands in `crate::commands` do. One method each, so the
    // registry holds function pointers and nothing else.
    // ------------------------------------------------------------------

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
            let pasted = sk_plat::decode_uri(first)
                .or_else(|| (!first.is_empty()).then(|| PathBuf::from(first)))
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
        self.layout =
            Layout::compute_with(self.logical, &self.theme.metrics, frame_of(&self.window));
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
    #[expect(clippy::unnecessary_debug_formatting, reason = "the quoting is the point")]
    pub(crate) fn dump(&mut self) {
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
        let frame = if self.window.needs_own_frame() { "own" } else { "server" };
        let frame_hover = match self.hovered_button {
            Some(crate::layout::Button::Minimize) => "minimize",
            Some(crate::layout::Button::Maximize) => "maximize",
            Some(crate::layout::Button::Close) => "close",
            None => "none",
        };
        let maximized = self.window_state.maximized;
        // The centre of the close button, so a test can aim at it without a copy
        // of the metrics it is testing.
        let close = self.layout.buttons[2];
        let close_x = close.x + close.width / 2.0;
        println!(
            "skerry-state path={:?} rows={} hidden={} sort={:?} descending={} cursor={cursor:?} \
selected={} scroll={:.1} loading={} watching={} notice={:?} \
hover={hover} hover_name={hover_name:?} \
body_top={:.1} body_height={:.1} row_h={:.1} name_x={:.1} size_x={:.1} size_w={:.1} \
frame={frame} frame_hover={frame_hover} maximized={maximized} close_x={close_x:.1} \
logical={:.0}x{:.0} scale_120={} \
frame_p50_us={} frame_p99_us={} frames={} over={} sizes={known}/{running} \
jobs={jobs}/{panics} keys={} stats={}",
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
            self.layout.body.y,
            self.layout.body.height,
            self.theme.metrics.row_height,
            self.layout.name.x,
            self.layout.size.x,
            self.layout.size.width,
            self.logical.width,
            self.logical.height,
            self.scale_120,
            self.frames.percentile(0.5),
            self.frames.percentile(0.99),
            self.frames.budget().0,
            self.frames.budget().1,
            self.show_keys,
            self.show_stats,
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
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

    fn press(&mut self, position: Point, clicks: u8, ctrl: bool, shift: bool) {
        self.pointer = position;
        // The frame comes first, and the resize border before the buttons: the
        // border overlaps the top-right corner where the close button is, and a
        // window whose corner cannot be grabbed is a window that cannot be
        // resized diagonally.
        if let Some(edge) = self.layout.edge_at(position.x, position.y, self.logical) {
            self.window.start_resize(edge);
            return;
        }
        if let Some(button) = self.layout.button_at(position.x, position.y) {
            match button {
                crate::layout::Button::Minimize => self.window.minimize(),
                crate::layout::Button::Maximize => {
                    self.window.set_maximized(!self.window_state.maximized);
                }
                crate::layout::Button::Close => self.window.request_close(),
            }
            return;
        }
        if self.layout.drag.contains(position.x, position.y) {
            // A double click on a title bar toggles maximize everywhere else, so
            // it does here.
            if clicks >= 2 {
                self.window.set_maximized(!self.window_state.maximized);
            } else {
                self.window.start_move();
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

        let row = self
            .layout
            .row_at(position.y, self.pane.scroll.offset(), self.theme.metrics.row_height)
            .filter(|row| *row < self.pane.listing.rows());
        let Some(row) = row else {
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

    /// The compositor's own window menu, which is where "always on top" and
    /// "move to workspace" live. Ours to offer and not ours to draw.
    fn frame_menu(&mut self, position: Point) {
        if self.layout.drag.contains(position.x, position.y) {
            self.window.show_window_menu(position);
        }
    }

    fn moved(&mut self, position: Point) {
        self.pointer = position;
        let over = self.layout.button_at(position.x, position.y);
        if self.hovered_button != over {
            self.hovered_button = over;
            self.dirty = true;
        }
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
        let row = self
            .layout
            .row_at(position.y, self.pane.scroll.offset(), self.theme.metrics.row_height)
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
    fn thumb(&self) -> Option<(f32, f32)> {
        let height = self.theme.metrics.row_height;
        let content = self.pane.listing.rows() as f32 * height;
        let track = self.layout.body.height;
        if content <= track || track <= 0.0 {
            return None;
        }
        let thumb = (track * track / content).max(28.0).min(track);
        let extent = (content - track).max(1.0);
        let position = (self.pane.scroll.offset() / extent).clamp(0.0, 1.0) * (track - thumb);
        Some((self.layout.body.y + position, thumb))
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs().cast_signed());

        let content = self.pane.listing.rows() as f32 * self.theme.metrics.row_height;
        self.pane.scroll.set_extent(content, self.layout.body.height);

        // Folder totals are applied to what is on screen and measured for what is
        // missing, once per frame. Bounded by the viewport rather than by the
        // directory, and keyed by path rather than by row — so a new listing
        // shows what is already known instead of starting again.
        let visible = sk_ui::visible_rows(
            self.pane.scroll.offset(),
            self.layout.body.height,
            self.theme.metrics.row_height,
            self.pane.listing.rows(),
        );
        {
            let Self { measurer, pane, jobs, .. } = self;
            measurer.update(pane, visible, jobs);
        }

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
            window_state,
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
            window: *window_state,
        };
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
            view::draw(&mut painter, pane, layout, now, &chrome, &mut diagnostics);
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
            let (window_us, gpu_us, font_us) = self.open_time;
            eprintln!(
                "skerry: first frame in {:.1} ms — window {:.1}, gpu {:.1}, fonts {:.1}, \
                 frame {:.1}; {} on {}, surface {}×{} at {}%; platform: {}",
                self.started.elapsed().as_secs_f64() * 1000.0,
                window_us as f64 / 1000.0,
                gpu_us as f64 / 1000.0,
                font_us as f64 / 1000.0,
                (turn_us + stats.cpu_us) as f64 / 1000.0,
                self.adapter,
                self.backend,
                physical.0,
                physical.1,
                self.scale_120 * 100 / 120,
                self.capabilities,
            );
        }
    }
}
