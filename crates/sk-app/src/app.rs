//! The frame loop.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sk_cmd::Registry;
use sk_gpu::{DrawList, FrameStats, Renderer};
use sk_model::{Flags, Problem, ProblemKind, SortColumn};
use sk_plat::{Button, Event, Point, Size, Window, WindowConfig};
use sk_text::{Fonts, LayoutCache};
use sk_ui::{Painter, Theme};
use sk_util::{Pool, Waker};

use crate::frames::Frames;
use crate::keys;
use crate::layout::Layout;
use crate::pane::{Jobs, Message, Pane};
use crate::view::{self, Chrome, Diagnostics};

/// How long a type-ahead buffer stays live after the last keystroke.
///
/// One second: long enough to type `doc` deliberately, short enough that
/// coming back to the keyboard starts a fresh jump rather than extending an
/// abandoned one.
const TYPE_AHEAD_WINDOW: Duration = Duration::from_secs(1);

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
    /// When the process started, for the cold-start measurement.
    pub started: Instant,
}

/// The application.
pub struct App {
    window: Window,
    renderer: Renderer,
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
        let window = Window::open(&WindowConfig::default())?;
        let window_us = window_started.elapsed().as_micros();

        // The pool and the directory read start **before** the GPU. Opening a
        // Vulkan instance costs 67–100 ms on this machine and the read costs
        // single-digit milliseconds, so overlapping them does not make the first
        // frame sooner — it makes the first frame contain the directory instead
        // of an empty list.
        let waker = Waker::new()
            .map_err(|error| Problem::plain(ProblemKind::NoDisplay, format!("eventfd: {error}")))?;
        let (sender, updates) = mpsc::channel();
        let jobs = Jobs { pool: Pool::new(), wake: waker.handle(), sender };
        let mut pane = Pane::new(options.path);
        pane.reload(&jobs);

        let (watch_sender, changes) = mpsc::channel();
        let watcher = sk_fs::Watcher::new(watch_sender, waker.handle())
            .map_err(|error| Problem::plain(ProblemKind::NoDisplay, format!("inotify: {error}")))?;
        watcher.watch(pane.path.clone());
        let watch_target = pane.path.clone();

        let gpu_started = Instant::now();
        let physical = window.physical();
        // SAFETY: the handles come from `window`, which this struct owns and
        // which is declared after `renderer` — so the renderer is dropped first.
        let renderer = unsafe { Renderer::new(window.raw_handles(), physical) }?;
        let gpu_us = gpu_started.elapsed().as_micros();

        let font_started = Instant::now();
        let fonts = Fonts::discover()?;
        let font_us = font_started.elapsed().as_micros();
        if !fonts.missing().is_empty() {
            eprintln!("skerry: font chain is short — no {}", fonts.missing().join(", "));
        }

        let theme = if options.dark { Theme::dark() } else { Theme::light() };
        let logical = window.logical();
        let scale_120 = window.scale_120();
        let now = Instant::now();
        Ok(Self {
            adapter: renderer.adapter().to_owned(),
            backend: renderer.backend().to_owned(),
            layout: Layout::compute(logical, &theme.metrics),
            window,
            renderer,
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
    pub fn run(&mut self) {
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
            self.drain_platform(&commands);
            self.drain_jobs();
            self.drain_changes();
            self.advance();
            if self.dirty {
                self.render(&commands);
                self.dirty = false;
            }
        }
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
        if self.watch_target != self.pane.path {
            self.watch_target = self.pane.path.clone();
            self.watcher.watch(self.watch_target.clone());
        }
        // Read every pass rather than once at arm time: the thread establishes the
        // watch asynchronously, so the answer is not known when `watch` returns.
        let armed = self.watcher.armed();
        if armed != self.watching {
            self.watching = armed;
            self.dirty = true;
        }
    }

    fn handle(&mut self, event: &Event, commands: &Registry<Self>) {
        match event {
            Event::Resized { logical, physical, scale_120 } => {
                self.logical = *logical;
                self.physical = *physical;
                self.scale_120 = *scale_120;
                self.layout = Layout::compute(*logical, &self.theme.metrics);
                self.renderer.resize(*physical);
                self.dirty = true;
            }
            Event::CloseRequested => self.closing = true,
            Event::Focused(_) => self.dirty = true,
            Event::KeyDown(key) => self.key(key, commands),
            Event::PointerMoved { position, .. } => self.moved(*position),
            Event::PointerLeft => {
                if self.pane.hover.take().is_some() {
                    self.dirty = true;
                }
            }
            Event::Button { button, pressed, position, clicks, mods } => {
                if *button == Button::Primary {
                    if *pressed {
                        self.press(*position, *clicks, mods.ctrl, mods.shift);
                    } else {
                        self.release();
                    }
                }
            }
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

    /// Opens the focused entry, if it is something that opens.
    pub(crate) fn open_focused(&mut self) {
        self.pane.open_focused(&self.jobs);
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

    /// Re-reads the current directory.
    pub(crate) fn reload(&mut self) {
        self.pane.reload(&self.jobs);
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
        self.layout = Layout::compute(self.logical, &self.theme.metrics);
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
        if clicks >= 2 && self.pane.listing.flags(id).has(Flags::DIR) {
            self.pane.open_focused(&self.jobs);
        }
        self.dirty = true;
    }

    fn release(&mut self) {
        if self.band.take().is_some() || self.thumb_grab.take().is_some() {
            self.dirty = true;
        }
    }

    fn moved(&mut self, position: Point) {
        self.pointer = position;
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
            self.dirty = true;
            return;
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

    fn render(&mut self, commands: &Registry<Self>) {
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

        // Folder sizes are measured for what is on screen, so the cost is bounded
        // by the viewport rather than by the directory. Asked for here, once per
        // frame, rather than by each thing that scrolls.
        let visible = sk_ui::visible_rows(
            self.pane.scroll.offset(),
            self.layout.body.height,
            self.theme.metrics.row_height,
            self.pane.listing.rows(),
        );
        self.pane.measure_visible(visible, &self.jobs);

        let build_started = Instant::now();
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
        };
        let atlas = renderer.atlas_stats();
        let (hits, misses, _) = cache.stats();
        let mut diagnostics = Diagnostics {
            frames,
            last: *last,
            atlas,
            shaping: (cache.len(), hits, misses),
            pool: {
                let (finished, panics) = jobs.pool.stats();
                (jobs.pool.width(), finished, panics)
            },
            adapter,
            backend,
            physical,
            scale_120: *scale_120,
        };

        {
            let mut painter = Painter { list, fonts, cache, theme, scale };
            view::draw(&mut painter, pane, layout, now, &chrome, &mut diagnostics);
        }
        let build_us = build_started.elapsed().as_micros() as u64;

        let background = theme.palette.background;
        let stats = renderer.draw(list, fonts, background);
        self.last = stats;
        self.frames.record(build_us + stats.cpu_us);

        if !self.first_frame_reported {
            self.first_frame_reported = true;
            let (window_us, gpu_us, font_us) = self.open_time;
            eprintln!(
                "skerry: first frame in {:.1} ms — window {:.1}, gpu {:.1}, fonts {:.1}, \
                 frame {:.1}; {} on {}, surface {}×{} at {}%",
                self.started.elapsed().as_secs_f64() * 1000.0,
                window_us as f64 / 1000.0,
                gpu_us as f64 / 1000.0,
                font_us as f64 / 1000.0,
                (build_us + stats.cpu_us) as f64 / 1000.0,
                self.adapter,
                self.backend,
                physical.0,
                physical.1,
                self.scale_120 * 100 / 120,
            );
        }
    }
}
