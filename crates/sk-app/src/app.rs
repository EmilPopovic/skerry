// SPDX-License-Identifier: Apache-2.0
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
    copying: bool,
    directory: bool,
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

/// Which column boundary a drag on a header has hold of.
///
/// Named by the column *left* of the line, because that is the one whose width
/// the drag sets: everything right of it shifts to follow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ColumnEdge {
    Name,
    Size,
    Modified,
}

/// How far either side of a column boundary a press takes hold of it.
///
/// Four logical pixels each way, which is eight of target — the same band the
/// pane divider uses, and narrow enough that the middle of a header label is
/// still a press that sorts. A boundary is aimed at deliberately; a header is
/// clicked in passing, and the click that sorts must not become a drag that
/// resizes because the pointer was three pixels off.
const COLUMN_GRAB: f32 = 4.0;

/// Which command a key event resolves to in the global scope, by id.
///
/// **One place, because five overlays ask the same question.** Each of them lets
/// exactly one chord through — the diagnostic report, or the chord that opened
/// it — and each used to spell the three-call chain out.
fn named(commands: &Registry<App>, event: &sk_plat::KeyEvent) -> Option<&'static str> {
    commands
        .resolve(event, Scope::Global)
        .and_then(|id| commands.describe(id))
        .map(|command| command.id)
}

/// What a mount, unmount or eject had to say.
struct Mounted {
    /// What to put in the status line, already looked up.
    notice: String,
    /// Where the volume landed, when it was a mount that worked and the press
    /// asked to go there.
    open: Option<PathBuf>,
    /// The chain, for the log, which is never the sentence — rule 8.
    detail: String,
}

/// Which of the two things that are not mounting is being done.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Unmount,
    Eject,
}

/// What a mount should do once it has worked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mounting {
    /// Go there, which is what a press on the row means.
    AndOpen,
    /// Say so and stay put, which is what the command means when it was reached
    /// from a menu over a row somebody was not looking at.
    AndSaySo,
}

/// The type a file manager is the default for.
const FOLDERS: &str = "inode/directory";

/// Makes this the desktop's file manager, or stops being it. Whether it worked,
/// and what to say about it.
///
/// **Two files and one decision.** `mimeapps.list` says what a *running* desktop
/// opens a folder with; the D-Bus service file says what to *start* when none of
/// this is running. A person who had one and not the other would have a file
/// manager that answers the desktop about half the time, which is not a state
/// worth being able to choose — so the row writes both or neither, and takes the
/// first back when the second is refused.
///
/// **Runs on a job**: two reads and two writes, plus a bus call.
fn handle_folders(yes: bool) -> (bool, String) {
    let done = if yes { become_the_file_manager() } else { stop_being_the_file_manager() };
    match done {
        Ok((listed, started, key)) => {
            (true, sk_model::i18n::format(key, &[&listed.display(), &started.display()]))
        }
        Err(said) => (false, said),
    }
}

/// Both files, the service file first.
///
/// **First because it is the half that can be refused on somebody else's
/// behalf**: a service file naming another program is that person's choice, and
/// finding it only after `mimeapps.list` had been written is the half-state
/// [`handle_folders`] exists to rule out.
///
/// **A bus that would not reload is said, not swallowed**: until it does, the
/// desktop keeps starting whichever file manager it found first, so a row that
/// only said *written* would be telling the person something untrue until they
/// log in again.
fn become_the_file_manager() -> Result<(PathBuf, PathBuf, &'static str), String> {
    let started = sk_desktop::activation::announce().map_err(activation_said)?;
    let key = if sk_desktop::activation::rescan() {
        "settings.handler_written"
    } else {
        "settings.handler_written_next_login"
    };
    match folder_association(true) {
        Ok(listed) => Ok((listed, started, key)),
        Err(said) => {
            let _ = sk_desktop::activation::withdraw();
            Err(said)
        }
    }
}

/// Neither file names this program any more, `mimeapps.list` first.
///
/// **A service file naming another program is success here, not a refusal.**
/// Turning the row off asks that nothing start Skerry, and a file that starts
/// something else already says so — deleting it to prove the point would be the
/// hijack [`sk_desktop::activation`] refuses, in reverse.
fn stop_being_the_file_manager() -> Result<(PathBuf, PathBuf, &'static str), String> {
    use sk_desktop::activation::Refused;
    let listed = folder_association(false)?;
    let started = match sk_desktop::activation::withdraw() {
        Ok(path) => path,
        Err(Refused::Taken(_)) => sk_desktop::activation::place().unwrap_or_default(),
        Err(why) => {
            let _ = folder_association(true);
            return Err(activation_said(why));
        }
    };
    Ok((listed, started, "settings.handler_cleared"))
}

/// The half of [`handle_folders`] a running desktop reads. The file, or what to
/// say instead.
fn folder_association(yes: bool) -> Result<PathBuf, String> {
    let id = crate::service::DESKTOP_ID;
    let done = if yes {
        sk_desktop::mimeapps::set_default(FOLDERS, id)
    } else {
        sk_desktop::mimeapps::clear_default(FOLDERS, id)
    };
    done.map_err(|why| match why {
        sk_desktop::mimeapps::Refused::Unreadable(line) => {
            sk_model::i18n::format("error.mimeapps_unreadable", &[&line])
        }
        _ => sk_model::i18n::lookup("error.mimeapps_refused").to_owned(),
    })
}

/// What to say when the service file was not written or not removed.
fn activation_said(why: sk_desktop::activation::Refused) -> String {
    use sk_desktop::activation::Refused;
    let place = sk_desktop::activation::place().unwrap_or_default();
    match why {
        Refused::Taken(program) => {
            sk_model::i18n::format("error.activation_taken", &[&place.display(), &program])
        }
        Refused::Unnameable => {
            sk_model::i18n::format("error.activation_unnameable", &[&place.display()])
        }
        Refused::NoDataHome | Refused::NotWritten => {
            sk_model::i18n::lookup("error.activation_refused").to_owned()
        }
    }
}

/// A permanent deletion waiting for an answer.
///
/// Holds the paths rather than re-reading the selection when the answer comes:
/// the question names what it named, and a selection that changed underneath —
/// a watch firing, a row arriving — must not change what gets deleted.
struct Erasing {
    paths: Vec<PathBuf>,
    on: Confirm,
    /// Whether this is the permanent deletion, or a Move to Trash that somebody
    /// asked to be asked about.
    ///
    /// **One panel and one flag, not two panels.** Both ask the same question
    /// about the same list with the same two buttons; what differs is the
    /// sentence under the names and whether the action is on the undo stack —
    /// and a second copy of this machinery would be a second place for the
    /// keyboard's two-press fast path to be got wrong.
    forever: bool,
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

/// A pinned place being dragged into a different order.
///
/// **The pinned rows only**, which is the whole of what an order means here: the
/// well-known directories are in the order
/// [`sk_desktop::user_dirs`](sk_desktop::user_dirs) puts them and the volumes are
/// in the order they are mounted, and neither is a choice somebody made.
#[derive(Clone, Copy, PartialEq, Debug)]
struct PlaceDrag {
    /// Which pinned place was taken hold of, as an index into
    /// [`crate::sidebar::Sidebar::pinned`].
    from: usize,
    /// Where it would land, as a gap: `0` is above the first row and `n` below
    /// the last.
    at: usize,
    /// Where the press was, so four pixels of hand tremor is not a reorder.
    started: Point,
    /// Whether it has moved far enough to be a drag rather than a click.
    active: bool,
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

/// How much one step of zoom magnifies a picture.
///
/// **A ratio rather than a table.** The list's zoom is seven chosen sizes —
/// stepping out and back has to land exactly where it started — and a picture's
/// is continuous, because a pinch is continuous and a person zooming a photograph
/// is looking for a size rather than a step. 1.15 is `sk_plat`'s own pinch step,
/// so one notch of the wheel and one step of a pinch cover the same distance.
const PINCH_STEP: f32 = 1.15;

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
    /// Open independently with a separately locked session checkpoint.
    pub new_window: bool,
    /// The bus this process took its name on, where there was one.
    ///
    /// **Taken before the window** — `sk_app::take_the_name` — because a second
    /// instance has to hand over and exit without opening one. What arrives here
    /// is the connection that owns the name, which the service thread then
    /// answers on.
    pub bus: Option<sk_desktop::bus::Bus>,
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
pub(crate) struct Desktop {
    /// **Behind its own `Arc`**, because a sort job needs it and a job cannot
    /// borrow the `App` that holds this — grouping by kind bands on
    /// `sk_desktop::Category`, which is a question `globs2` answers. One
    /// reference count, rather than the whole bundle in [`crate::pane::Jobs`]
    /// when only this half is ever read there.
    pub(crate) mime: std::sync::Arc<MimeDatabase>,
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
        let loaded = std::sync::Arc::new(Desktop {
            mime: std::sync::Arc::new(MimeDatabase::load()),
            apps: Applications::load(),
        });
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
        let jobs = Jobs {
            pool: std::sync::Arc::new(Pool::new()),
            wake: waker.handle(),
            sender,
            mime: None,
        };
        let mut pane = Pane::new(path);
        pane.reload(&jobs);

        // Open by default, which is what every file manager a person has met
        // does. Reading what goes in it is a job, started with the others.
        let sidebar = crate::sidebar::Sidebar::new(
            &jobs,
            waker.handle(),
            true,
            crate::sidebar::DEFAULT_WIDTH,
        );
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

/// Measures text with the window's own faces, for the markdown layout.
///
/// **Logical pixels in and out.** Everything above `sk-plat` works in logical
/// pixels and the shaper works in device ones, so the conversion happens here —
/// once, in the one place the layout touches the font stack.
struct Shaper<'a> {
    cache: &'a mut sk_text::LayoutCache,
    fonts: &'a mut sk_text::Fonts,
    scale: f32,
}

impl crate::mdlayout::Measure for Shaper<'_> {
    fn width_of(&mut self, text: &str, weight: sk_text::Weight, size: f32) -> f32 {
        sk_ui::advance_of(self.cache, self.fonts, text, weight, size * self.scale) / self.scale
    }
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
    Files(Vec<PathBuf>, bool, u64, PathBuf),
    KdeIntent(bool, u64, PathBuf),
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
    openers_tx: std::sync::mpsc::Sender<(crate::openers::Asker, crate::openers::Handlers)>,
    openers_rx: Receiver<(crate::openers::Asker, crate::openers::Handlers)>,
    /// A search report, and the query it is an answer to.
    found_tx: std::sync::mpsc::Sender<(String, sk_index::Found)>,
    found_rx: Receiver<(String, sk_index::Found)>,
    /// What a job loading or building an index has to say.
    index_tx: std::sync::mpsc::Sender<crate::indexing::Note>,
    index_rx: Receiver<crate::indexing::Note>,
    /// What somebody on the bus asked this window to show.
    asked_tx: std::sync::mpsc::Sender<crate::service::Asked>,
    asked_rx: Receiver<crate::service::Asked>,
    /// What a write to the list of places did, and whether it worked.
    places_tx: std::sync::mpsc::Sender<(crate::sidebar::Edit, PathBuf, bool)>,
    places_rx: Receiver<(crate::sidebar::Edit, PathBuf, bool)>,
    /// What a place opened in the sidebar's tree turned out to hold.
    opened_tx: std::sync::mpsc::Sender<(PathBuf, Vec<String>)>,
    opened_rx: Receiver<(PathBuf, Vec<String>)>,
    /// Whether this machine's `mimeapps.list` names us for folders, and what a
    /// change to it had to say.
    ///
    /// **Both directions on one channel**, because the answer to *did it work*
    /// is the same shape as the answer to *what is it now*: the state, and a
    /// sentence when there is one to say.
    handler_tx: std::sync::mpsc::Sender<(bool, Option<String>)>,
    handler_rx: Receiver<(bool, Option<String>)>,
    /// What a mount, unmount or eject did: the sentence, and where to go.
    ///
    /// **The sentence rather than the error**, because the job is where the
    /// catalogue lookup belongs — the frame loop takes delivery of something it
    /// can put on screen. See [`Mounted`].
    mounted_tx: std::sync::mpsc::Sender<Mounted>,
    mounted_rx: Receiver<Mounted>,
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
        let (places_tx, places_rx) = mpsc::channel();
        let (opened_tx, opened_rx) = mpsc::channel();
        let (handler_tx, handler_rx) = mpsc::channel();
        let (mounted_tx, mounted_rx) = mpsc::channel();
        let (asked_tx, asked_rx) = mpsc::channel();
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
            asked_tx,
            asked_rx,
            places_tx,
            places_rx,
            opened_tx,
            opened_rx,
            handler_tx,
            handler_rx,
            mounted_tx,
            mounted_rx,
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

/// One row of a menu as the accessibility tree reads it: what it says, where
/// it is, and whether it is the one the keys act on.
pub(crate) type A11yRow = (String, Area, bool);

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
    watch_failure: Option<sk_fs::watch::Failure>,
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
    hovered_crumb: Option<(crate::panes::PaneId, usize)>,
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
    hovered_separator: Option<(crate::panes::PaneId, usize)>,
    /// The sibling menu a separator opened, once its directory has been read.
    crumb_menu: Option<crate::crumbs::CrumbMenu>,
    /// A separator whose directory is still being read, so a second press on it
    /// does not queue a second read.
    crumb_pending: Option<usize>,
    /// The navigation button under the pointer.
    hovered_tool: Option<(crate::panes::PaneId, crate::layout::Tool)>,
    /// The quick look's control under the pointer, while one is open.
    hovered_look: Option<crate::view::LookControl>,
    /// The switcher button under the pointer.
    hovered_mode: Option<(crate::panes::PaneId, crate::cells::Mode)>,
    /// How many scroll events have arrived, how many of them said a finger sent
    /// them, and how many gestures have ended.
    ///
    /// **Three counters for the diagnostic line, and they earned their place.** A
    /// flick that moved the list but threw nothing could have been any of three
    /// things — no `axis_source`, no `axis_stop`, or a velocity of zero — and
    /// nothing on screen tells them apart. `zwlr_virtual_pointer_v1` is the only
    /// way the harness can send a scroll at all, so what it does and does not
    /// forward is a fact worth being able to read.
    scrolls: (u64, u64, u64),
    /// How many lifts actually threw the list.
    ///
    /// The answer to the only question the other counters leave open: everything a
    /// fling needs can be present and the velocity still come out at nothing.
    flings: u64,
    /// The device clock on the last scroll event, or zero if it carried none.
    ///
    /// The third of the three things that decide whether a flick can throw, and
    /// the one that is invisible everywhere else: `axis_value120` and
    /// `axis_discrete` have no `time` field in the protocol, so a compositor that
    /// forwards only those leaves every frame of a gesture stamped zero.
    scroll_time: u32,
    /// How far the list is zoomed, as a step on [`ZOOM`].
    ///
    /// A step rather than a factor, so `Ctrl +` and a pinch land on the same
    /// sizes and a round trip out and back is exactly where it started.
    zoom: usize,
    /// The session file as it was last written, so an unchanged one is not
    /// rewritten.
    session_writer: crate::session::WriteBack,
    session_lock: Option<std::fs::File>,
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
    /// How each folder was last laid out.
    views: crate::views::Views,
    /// The file as last written, so a save that would change nothing does not.
    views_text: String,
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
    /// A submenu a hover asked for: which row of the open menu, and where the
    /// pointer was. The same shape as `menu_wanted` and for the same reason —
    /// building rows needs the registry, and a pointer's motion does not carry
    /// it — so `settle_menu` opens it after the event.
    submenu_wanted: Option<(usize, Point)>,
    /// The token of the job finding the open menu's applications, so closing
    /// the menu can stop a job whose answer nobody is waiting for.
    menu_job: Option<sk_util::Cancel>,
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
    /// The file being looked at, if any. `Space` opens and closes it.
    quick_look: Option<crate::quicklook::QuickLook>,
    /// The preview host: which children are running, and what is wanted.
    ///
    /// **Spawns nothing until something is asked for**, so a session in a folder
    /// of text files costs exactly what it did before previews existed.
    previews: sk_preview::Previews,
    /// What the atlas knows each thumbnail by.
    thumbs: crate::previews::Thumbs,
    /// Whether the history strip is showing its list or only its header.
    ///
    /// Collapsed by default and remembered across the process, which is
    /// [rule 6](../../../docs/03-principles.md): a panel a person opened is part
    /// of how the window was left.
    history_open: bool,
    /// The history row under the pointer, if the list is open and it is over one.
    hovered_edit: Option<usize>,
    /// The history list's own scroll, because it has more rows than it draws.
    history_scroll: sk_ui::Scroll,
    /// How wide the list's columns are, and what a drag on a header boundary
    /// changes.
    ///
    /// **One set for the window, not one per pane.** A width is a decision about
    /// how much of a name a person wants to read, and it does not change because
    /// they looked at a different folder — the per-folder file holds what a view
    /// *is* (a grid, a grouping) and this is how the chrome around it is sized.
    /// Kept in the session, so a drag survives the process like every other part
    /// of the layout.
    columns: crate::layout::Columns,
    /// The column boundary being dragged, and the leaf whose header it started in.
    column_drag: Option<(crate::panes::PaneId, ColumnEdge)>,
    /// Whether the sidebar's own divider has been taken hold of.
    aside_drag: bool,
    /// A pinned place being dragged into a different order.
    place_drag: Option<PlaceDrag>,
    /// The place a pointer acted on, for the one command that acts on it.
    ///
    /// **The target `open_menu` said the registry had no shape for.** A command
    /// runs against `&mut App` and takes no argument, so a place's menu rows and
    /// a press on a twisty need somewhere to say *which* place — and that is a
    /// field, exactly as the selection is the field that tells `file.trash` what
    /// to trash
    /// ([ADR 0021](../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md)).
    /// It outlives the gesture by exactly one command, which is the command the
    /// gesture was for; a menu clears it through [`App::close_menu`], so a target
    /// cannot outlive the menu it was read from.
    place_target: Option<PathBuf>,
    /// Where a rubber band started, and where it is now.
    band: Option<(Point, Point)>,
    /// A press on an already-selected row that may become a drag of files.
    files_drag: Option<FilesDrag>,
    /// The well-known names this process holds, for the startup line.
    ///
    /// Empty is a session with no bus, or one where another file manager already
    /// answers — both of which are ordinary, and neither of which is visible
    /// anywhere else: ADR 0023 and ADR 0007.
    on_the_bus: String,
    /// Whether a `ShowItemProperties` is still waiting for its listing.
    ///
    /// The panel is about the selection and the selection is about rows that do
    /// not exist until the read lands, so the request outlives the navigation by
    /// exactly one listing.
    properties_owed: bool,
    /// Where a pan of a zoomed picture last had the pointer.
    look_pan: Option<Point>,
    /// Whether a drag is selecting the words of a page.
    page_drag: bool,
    /// What the config file said about the keymap, if there was one.
    ///
    /// **Held rather than re-read.** A registry is built whenever one is needed —
    /// the frame loop's, the palette's — and reading a file on the palette's path
    /// would be a syscall on the frame loop, which is rule 1's whole subject.
    keymap: crate::config::Config,
    /// How many times the session file has been written.
    ///
    /// **A diagnostic that costs nothing when unread**, and it answers the one
    /// question `no-cpu-when-idle` cannot otherwise ask: whether a tick of CPU
    /// on a window nobody touched was this loop waking up to save. See
    /// [12-measurement.md](../../../docs/12-measurement.md#a-test-that-keeps-poking-cannot-see-what-happens-when-the-poking-stops).
    saves: u32,
    /// Whether a session save was turned away by [`SESSION_FLOOR`] and is still
    /// owed.
    ///
    /// **A wake-up with an owner.** See [`Self::keep_the_session`].
    session_owed: bool,
    /// Which settings the file named and which this window has changed.
    ///
    /// **[ADR 0027](../../../docs/adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md).**
    /// The values themselves live where they are used — the theme in the
    /// palette, `hidden` on every pane — because a second copy of a live value
    /// is a second copy to keep in step. What has nowhere else to live is
    /// *provenance*, which is what the precedence rule and the panel's third
    /// column are both about.
    marks: crate::settings::Marks,
    /// The settings panel, while it is open.
    panel: Option<crate::settings::Panel>,
    /// Whether Move to Trash asks before it moves anything.
    ///
    /// **Off by default, because rule 3 is the safety net.** Every trash is one
    /// `Ctrl+Z`, so the question is friction rather than protection — but a
    /// person who wants it is not wrong, and asking is one line of state and the
    /// panel [`Erasing`] already draws.
    confirm_trash: bool,
    /// Whether the display server has already reported this gesture's drop.
    ///
    /// **A drag ends twice.** The pointer's button comes up and the data device
    /// says `drop`, in an order neither protocol fixes, and both mean the same
    /// gesture. Whichever arrives first does the work; this is how the other one
    /// knows not to do it again.
    dropped: bool,
    /// Which page each document was last left on, most recent first.
    ///
    /// **Because reopening a manual at page one is losing somebody's place.** A
    /// quick look records the page as it turns and reads it back when the same
    /// file is opened again, and the list goes in the session file so it survives
    /// the process — [rule 6](../../../docs/03-principles.md). Bounded by
    /// [`crate::session::MOST_PAGES`], oldest dropped.
    pages_left_at: Vec<(std::path::PathBuf, u32)>,
    /// Raises asked for, and raises there was a way to ask for.
    ///
    /// **Two numbers because they differ by display server.** X11 always has a
    /// way — `_NET_ACTIVE_WINDOW` — and Wayland has one only with a token from
    /// whoever asked, so the second number is the capability and the first is the
    /// request. Neither says the window came forward: that is the compositor's.
    raises: (u32, u32),
    /// The accessibility tree's publisher, where a desktop has an accessibility
    /// bus. `None` is the common case and costs one method call at startup.
    a11y: Option<sk_desktop::atspi::Accessibility>,
    /// Drops whose read was started, whose bytes came back, and which were acted
    /// on — the three counts in the diagnostic report's `drops`.
    ///
    /// **Because the three failures look identical from outside.** A drag in from
    /// another application that leaves nothing behind is a read that never
    /// finished, a message that never got drained, or an operation that was
    /// refused; `1/0/0`, `1/1/0` and `1/1/1` say which, and none of them costs a
    /// syscall to record.
    drops_started: u32,
    drops_delivered: std::sync::Arc<std::sync::atomic::AtomicU32>,
    drops_drained: u32,
    /// When the last drop's read began, and how long it took to come back —
    /// which is what separates a read that is slow from a message that was never
    /// drained. Both counts can read `1/1/0` for either.
    drops_asked: Option<Instant>,
    drops_took: std::sync::Arc<std::sync::atomic::AtomicU32>,
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
    /// until the pointer moves: moving starts the compositor's window move, and
    /// coming up without moving is a click on a title bar, which does nothing.
    /// `xdg_toplevel.move` takes the last button serial, which is still this
    /// press's, so starting the move a few milliseconds later is the same grab.
    title_press: Option<Point>,
    last_frame: Instant,
    started: Instant,
    first_frame_reported: bool,
    open_time: (u128, u128, u128),
}

impl App {
    /// Opens a window, a device and a directory.
    pub fn new(mut options: Options) -> Result<Self, Problem> {
        // Taken out because a connection cannot be copied, and because the field
        // exists to carry it exactly this far.
        let mut bus = options.bus.take();
        let options = &options;
        // **Read before the window is opened**, because the size it asks for is
        // in the file. One `openat` and one read of a few hundred bytes, before
        // there is a frame loop for it to be a rule 1 violation of.
        // **Read whatever the command line named**, because a named directory
        // says nothing about a theme. ADR 0027's six settings come out of this
        // file every time; the *layout* — the tree, the columns, the window —
        // comes out of it only when nobody asked for a particular folder, which
        // is what `restore` means.
        let slot = crate::session::claim(options.new_window).map_err(|error| {
            Problem::new(sk_model::ProblemKind::Unreadable, "session".to_owned(), error.to_string())
        })?;
        let session = slot
            .as_ref()
            .and_then(|(file, _)| std::fs::read_to_string(file).ok())
            .and_then(|text| crate::session::parse(&text));
        // **Beside the session read and for the same reason** — ADR 0025. One
        // `openat` and a few hundred bytes, before there is a frame loop for it
        // to be a rule 1 violation of; and it has to be before the first registry
        // is built, which is what every chord in the window resolves against.
        let keymap = crate::config::Config::read();
        let window_started = Instant::now();
        let window = Window::open(&WindowConfig {
            own_frame: options.own_frame,
            scale_120: options.scale_120,
            // The layout half, so a named directory opens at the default size
            // rather than at whatever the last session happened to be — see the
            // split below.
            logical: session
                .as_ref()
                .filter(|_| options.restore)
                .map_or(WindowConfig::default().logical, |it| it.window),
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
        if let Some((file, lock)) = slot {
            app.session_writer = crate::session::WriteBack::at(file);
            app.session_lock = Some(lock);
        }
        // **The file's values before the session's**, which is ADR 0027's
        // precedence rule in the order the two reads happen: this is what a
        // fresh window does, and the session below moves only what this window
        // was told to move.
        app.marks = keymap.marks;
        let from_file = keymap.values;
        app.keymap = keymap;
        app.take_values(&from_file);
        if let Some(session) = &session {
            app.take_settings(session, options);
        }
        // **The palette may have changed twice by now**, and the metrics are
        // built from it. `restore_layout` does this too; doing it here as well is
        // one relayout before the first frame and is what keeps a window with no
        // layout to restore drawing in the theme it was told to.
        app.apply_zoom();
        // Whether anything at all has an opinion about the theme, which is what
        // the desktop's preference below is the fallback for.
        let stated = app.marks.touched(crate::settings::Setting::Theme);
        if options.restore
            && let Some(session) = session
        {
            app.restore_layout(&session);
        }
        // **The desktop's own light-or-dark, and only when nothing else has
        // said.** A flag is an instruction for this run and the session file is
        // what this window was last left as; both are about *this* program, and
        // either outranks a preference about the desktop. What is left is a fresh
        // install, which is the case the M7 bar is written about.
        // **And the config file counts as having said**, which is the line ADR
        // 0027 added to this rule: a person who wrote `theme = light` has said
        // something about *this* program, and the desktop's preference is about
        // the desktop.
        let asked = (!stated
            && options.dark.is_none()
            && !app.marks.is_named(crate::settings::Setting::Theme))
        .then(|| bus.as_mut().and_then(crate::service::desktop_prefers_dark))
        .flatten();
        if asked.is_some_and(|dark| dark != app.theme.dark) {
            app.theme = if asked == Some(true) { Theme::dark() } else { Theme::light() };
        }
        // **The name was taken before the window existed** — a second instance has
        // to hand over and exit without flashing one — so what arrives here is the
        // connection that already owns it, and the service thread answers on it
        // from here until the process ends.
        // **Accessibility, before the first frame.** The registry asks its
        // questions the moment `Embed` returns, so the thread has to exist before
        // there is anything to describe — it answers from an application with a
        // name and no children until the first frame publishes. ADR 0026.
        app.a11y = sk_desktop::atspi::Accessibility::start("Skerry");
        if let Some(bus) = bus {
            app.on_the_bus = bus.names().join(" ");
            crate::service::serve(bus, app.replies.asked_tx.clone(), app.waker.handle());
        }
        Ok(app)
    }

    /// Puts the six settings where the session says, for the ones it may speak
    /// for.
    ///
    /// **The session speaks for a setting only where this window changed it** —
    /// ADR 0027. `self` already holds what the config file said, so an unchanged
    /// setting is left at the file's answer, and a session written before the
    /// `touched` line existed leaves all six alone — which is what a first
    /// upgrade should do.
    ///
    /// Separate from [`Self::restore_layout`] because the two answer to
    /// different things: a directory named on the command line says which folder
    /// to open and nothing whatever about a theme.
    fn take_settings(&mut self, session: &crate::session::Session, options: &Options) {
        use crate::settings::Setting;
        self.marks.absorb(&session.touched);
        let changed = |setting| session.touched.touched(setting);

        // What was asked for on the command line wins over what was saved: a
        // flag is an instruction for this run and the file is a memory of the
        // last one.
        let dark = options.dark.unwrap_or(if changed(Setting::Theme) {
            session.dark
        } else {
            self.theme.dark
        });
        if dark != self.theme.dark {
            self.theme = if dark { Theme::dark() } else { Theme::light() };
        }
        self.sidebar.open = options.sidebar.unwrap_or(if changed(Setting::Sidebar) {
            session.sidebar
        } else {
            self.sidebar.open
        });
        if changed(Setting::SidebarWidth) {
            self.sidebar.width =
                session.aside.clamp(crate::sidebar::NARROWEST, crate::sidebar::WIDEST);
        }
        if changed(Setting::View) {
            self.views.defaults.layout = session.layout;
        }
        if changed(Setting::ConfirmTrash) {
            self.confirm_trash = session.confirm_trash;
        }
        if changed(Setting::Hidden) {
            for pane in self.panes.every_mut() {
                pane.show_hidden = session.hidden;
            }
        }
    }

    /// Puts the window back the way the file says it was.
    ///
    /// Runs after [`Self::take_settings`], and only when no directory was named.
    fn restore_layout(&mut self, session: &crate::session::Session) {
        // **Read before the panes are replaced**, because that is where the
        // answer for it currently lives and the replacement discards it.
        let hidden = self.values().hidden;
        // **Opened before the sections have arrived**, which is why this is two
        // steps: the tree's shape is a list of paths and the rows it hangs under
        // are still being read on a job. Each read is started here rather than
        // when the sections land, because the answer is keyed by path and the
        // order the two arrive in does not matter.
        self.pages_left_at.clone_from(&session.pages);
        self.sidebar.open_these(&session.expanded);
        for path in session.expanded.clone() {
            self.read_a_place(path);
        }
        self.history_open = session.history;
        self.zoom = session.zoom.min(ZOOM.len() - 1);
        self.columns = session.columns.clamped();
        self.panes = crate::panes::Panes::restore(&session.tree, &self.jobs);
        for pane in self.panes.every_mut() {
            pane.show_hidden = hidden;
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
            aside: self.sidebar.width,
            history: self.history_open,
            hidden: self.panes.focused().show_hidden,
            layout: self.views.defaults.layout,
            confirm_trash: self.confirm_trash,
            touched: self.marks,
            zoom: self.zoom,
            window: (self.logical.width as u32, self.logical.height as u32),
            columns: self.columns,
            expanded: self.sidebar.open_places().to_vec(),
            pages: self.pages_left_at.clone(),
            tree: self.panes.describe(),
        }
    }

    /// What all six settings are set to right now.
    ///
    /// Assembled from where each one lives rather than held twice — see
    /// [`Self::marks`] for why.
    pub(crate) fn values(&self) -> crate::settings::Values {
        crate::settings::Values {
            dark: self.theme.dark,
            hidden: self.panes.focused().show_hidden,
            sidebar: self.sidebar.open,
            aside: self.sidebar.width,
            layout: self.views.defaults.layout,
            confirm_trash: self.confirm_trash,
        }
    }

    /// Puts every setting where `values` says, marking none of them touched.
    ///
    /// **The config file's half of the precedence rule.** This runs before the
    /// session is restored, so what it sets is what a window with no session —
    /// and every setting this window has never changed — is left at.
    fn take_values(&mut self, values: &crate::settings::Values) {
        if values.dark != self.theme.dark {
            self.theme = if values.dark { Theme::dark() } else { Theme::light() };
        }
        self.sidebar.open = values.sidebar;
        self.sidebar.width = values.aside.clamp(crate::sidebar::NARROWEST, crate::sidebar::WIDEST);
        self.views.defaults.layout = values.layout;
        self.confirm_trash = values.confirm_trash;
        for pane in self.panes.every_mut() {
            pane.show_hidden = values.hidden;
        }
    }

    /// Changes one setting and records that this window did it.
    ///
    /// **Every path that changes a setting comes through here**, the panel and
    /// the chord alike, because the touched mark is what lets the session
    /// override the file and a path that skipped it would set a value the next
    /// start-up quietly discarded.
    pub(crate) fn change(&mut self, setting: crate::settings::Setting, yes: bool) {
        use crate::settings::Setting;
        self.marks.touch(setting);
        match setting {
            Setting::Theme if yes != self.theme.dark => self.toggle_theme(),
            Setting::Hidden if yes != self.panes.focused().show_hidden => self.toggle_hidden(),
            Setting::Sidebar if yes != self.sidebar.open => self.toggle_sidebar(),
            Setting::View => {
                self.views.defaults.layout =
                    if yes { crate::views::Layout::Grid } else { crate::views::Layout::Details };
                self.relayout();
            }
            Setting::ConfirmTrash => self.confirm_trash = yes,
            Setting::Theme | Setting::Hidden | Setting::Sidebar | Setting::SidebarWidth => {}
        }
        self.dirty = true;
    }

    /// Moves the sidebar's width by `delta` logical pixels, from the panel.
    pub(crate) fn resize_aside(&mut self, delta: f32) {
        let width =
            (self.sidebar.width + delta).clamp(crate::sidebar::NARROWEST, crate::sidebar::WIDEST);
        self.marks.touch(crate::settings::Setting::SidebarWidth);
        if (self.sidebar.width - width).abs() < f32::EPSILON {
            self.dirty = true;
            return;
        }
        self.sidebar.width = width;
        self.relayout();
        self.dirty = true;
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
        // **The floor throttles the write, not the comparison.** It used to
        // throttle both, and the comparison is the half that decides whether
        // anything is owed at all — so a change that landed inside the floor was
        // dropped rather than deferred, and a window that then went idle lost it
        // to `kill -9`. That is rule 6's own stated failure, produced by the
        // mechanism written to keep the write off the frames that matter.
        //
        // Rendering it every frame costs **0.8–1.5 µs** — a release build, four
        // tabs in a split, 100,000 renders, three runs — against a 0.342 ms p99,
        // because the file holds a line per tab and never touches the listing.
        // Writing it is the expensive half and still happens at most once every
        // `SESSION_FLOOR`.
        if !self.session_writer.ready(now) {
            self.session_owed = false;
            return;
        }
        let text = crate::session::render(&self.session());
        if text == self.session_writer.acknowledged() {
            self.session_owed = false;
            return;
        }
        if !now && self.session_at.elapsed() < SESSION_FLOOR {
            // **A deferred save is a wake-up with an owner**, the same rule an
            // animation follows: this loop sleeps on descriptors, so nothing
            // else will ever ask for it. And it is owed only when there is
            // something to write, which is what keeps an idle window at zero
            // CPU — the gate one line above this one in the invariant table.
            self.session_owed = true;
            return;
        }
        self.session_owed = false;
        self.session_at = Instant::now();
        self.saves = self.saves.saturating_add(1);
        if now {
            // **Written here rather than on a job**, and only here: this is the
            // last thing the process does, and a job queued now races the pool's
            // shutdown. There is no frame left to block.
            self.session_writer.finish(text);
            return;
        }
        self.session_writer.start(text, &self.jobs.pool, &self.jobs.wake);
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
            // No history yet, by construction: nothing has been changed.
            layout: Layout::compute_with(
                logical,
                &theme.metrics,
                frame_of(&window),
                crate::sidebar::DEFAULT_WIDTH,
                0.0,
            ),
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
            watch_failure: None,
            capabilities,
            zoom: ZOOM_DEFAULT,
            session_writer: crate::session::WriteBack::default(),
            session_lock: None,
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
            hovered_look: None,
            hovered_mode: None,
            scrolls: (0, 0, 0),
            scroll_time: 0,
            flings: 0,
            hits: crate::view::Hits::default(),
            cursor: sk_plat::Cursor::Arrow,
            hint: String::new(),
            typing_path: None,
            palette: None,
            search: None,
            indexes: crate::indexing::Indexes::default(),
            views: crate::views::load(),
            views_text: String::new(),
            properties: None,
            openers: None,
            tab_press: None,
            tab_dragging: false,
            erasing: None,
            cutting: false,
            menu: None,
            menu_wanted: None,
            submenu_wanted: None,
            menu_job: None,
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
            quick_look: None,
            previews: sk_preview::Previews::new(),
            thumbs: crate::previews::Thumbs::default(),
            history_open: false,
            hovered_edit: None,
            history_scroll: sk_ui::Scroll::default(),
            columns: crate::layout::Columns::default(),
            column_drag: None,
            aside_drag: false,
            place_drag: None,
            place_target: None,
            band: None,
            files_drag: None,
            dropped: false,
            pages_left_at: Vec::new(),
            raises: (0, 0),
            a11y: None,
            drops_started: 0,
            drops_delivered: std::sync::Arc::default(),
            drops_drained: 0,
            drops_asked: None,
            drops_took: std::sync::Arc::default(),
            keymap: crate::config::Config::of(""),
            saves: 0,
            session_owed: false,
            marks: crate::settings::Marks::default(),
            panel: None,
            confirm_trash: false,
            look_pan: None,
            page_drag: false,
            properties_owed: false,
            on_the_bus: String::new(),
            thumb_grab: None,
            text_drag: None,
            title_press: None,
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
        let commands = self.commands();
        self.hint = crate::commands::hint(&commands);
        if !commands.problems().is_empty() {
            // A conflicting binding is a bug and a test fails on it, but a
            // release build that meets one should say so rather than silently
            // giving a key to whichever command registered first.
            for problem in commands.problems() {
                eprintln!("skerry: keymap: {problem}");
            }
        }
        // **Beside them and in file order**, because both come from the same
        // file and a person reads them against it — ADR 0027. A value this build
        // cannot read is a problem said out loud rather than a setting quietly
        // left at its default.
        for refused in &self.keymap.refused {
            eprintln!("skerry: config: {refused}");
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
            self.drain_handler();
            self.drain_mounted();
            self.drain_found();
            self.drain_index();
            self.drain_places();
            self.drain_opened();
            self.drain_the_bus();
            self.settle_the_owed_panel();
            if self.edits.drain_progress() {
                self.dirty = true;
            }
            self.drain_edits();
            self.drain_previews();
            self.drain_the_look_text();
            if let Some(gone) = self.sidebar.drain() {
                self.dirty = true;
                self.leave_the_drives_that_went(&gone);
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
                // **After the frame, from what the frame recorded.** The tree is
                // `Hits` and the cell geometry with names on it, so it can only
                // be built once both exist — and only where an assistive
                // technology is listening, which is what `Accessibility::start`
                // answering `None` means. ADR 0026.
                if let Some(a11y) = self.a11y.as_ref() {
                    a11y.publish(crate::a11y::tree_of(self));
                }
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
        // **An animation owes the next frame and nothing else will ask for it.**
        // This loop requests no `wl_surface.frame` callbacks, so after a frame is
        // submitted the only things that wake it are input, a job and the
        // deadlines below. A scroll that eased part of the way and then slept
        // simply stopped there — and since `advance` is handed the time since the
        // *last* frame, which after an idle window is seconds, a wheel notch
        // covered its whole distance in one frame and landed as a jump. Which is
        // the thing the easing exists to prevent.
        //
        // `Scroll::animating` was written for this and had no caller. Zero rather
        // than a frame interval because presentation is `PresentMode::Fifo`: the
        // submit blocks on the vertical blank, so the loop is paced by the
        // display and asking for a timer would only pick a worse rate than the
        // one the monitor already has.
        let owes = |pane: &crate::pane::Pane| {
            pane.scroll.animating()
                || pane.across.animating()
                || crate::trail::animating(&pane.trail)
        };
        if self.panes.visible().any(owes) {
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
            // A save the floor deferred, for the reason above it.
            self.session_owed.then(|| SESSION_FLOOR.saturating_sub(self.session_at.elapsed())),
            // **A playing animation owes its next frame**, and nothing else will
            // ask for it: a GIF is the one thing on screen that changes while the
            // hands are still. `None` the moment it is paused or the file has one
            // frame, which is what keeps an idle window at zero CPU.
            self.quick_look.as_ref().and_then(crate::quicklook::QuickLook::due_in),
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
                //
                // A trail's columns take theirs from the same counter, so this is
                // where they are offered it — and a reply handed back is one no
                // column wanted, which then goes to the panes below.
                let Some(message) = panes.take_reply(message) else {
                    *dirty = true;
                    landed = true;
                    continue;
                };
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
            Pasted::KdeIntent(moving, generation, into) => {
                if self.paste_offer_is_current(generation) {
                    self.read_file_paste("text/uri-list", moving, generation, into);
                }
            }
            Pasted::Files(paths, moving, generation, into) => {
                if !self.paste_offer_is_current(generation) {
                    return;
                }
                // Cleared here rather than where the cut was made: a paste is
                // what consumes it, and a second paste of the same clipboard has
                // to copy — the originals are not where the clipboard says any
                // more.
                self.cutting = false;
                let took = self.edits.paste_all(paths, into, moving, &self.jobs);
                self.said_it_is_busy(took);
            }
            Pasted::Dropped(paths, into) => {
                self.drops_drained = self.drops_drained.saturating_add(1);
                // The bytes are in hand, so the drag is over: `wl_data_offer`
                // wants `finish` *after* the read, and a source told earlier
                // stops serving before it has served.
                self.window.finish_drop();
                if !paths.is_empty() {
                    let took = self.edits.paste_all(paths, into, false, &self.jobs);
                    self.said_it_is_busy(took);
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

    /// Says that an operation was refused because one is already running.
    ///
    /// **The one thing a refusal may not be is silent.** One operation at a time
    /// is M3's choice and it stands; a batch that arrives while one runs used to
    /// be dropped with nothing said, which looks exactly like a drop that never
    /// happened — and for a drag from another application, the files are somebody
    /// else's and the window is the only thing that knows they were offered.
    fn said_it_is_busy(&mut self, took: bool) {
        if took {
            return;
        }
        self.notice =
            Some((sk_model::i18n::lookup("error.one_at_a_time").to_owned(), Instant::now()));
    }

    /// Takes delivery of the freedesktop databases, once.
    ///
    /// **A pane grouped by kind is re-sorted here**, and nowhere else. Its bands
    /// are `sk_desktop::Category`, which needs `globs2`; a pane that sorted
    /// before this landed filed every file under "Other", and nothing else would
    /// ever ask again — the banding it wants has not changed, so
    /// [`Self::rebandif_needed`] sees nothing to do.
    fn drain_desktop(&mut self) {
        let Ok(loaded) = self.desktop_rx.try_recv() else { return };
        self.jobs.mime = Some(loaded.mime.clone());
        self.desktop = Some(loaded);
        let Self { panes, jobs, .. } = self;
        for pane in panes.every_mut() {
            if pane.grouping == crate::groups::Grouping::Kind {
                pane.reorder(jobs);
            }
        }
        if let Some(path) = self.open_owed.take() {
            self.launch(path);
        }
        self.dirty = true;
    }

    fn advance(&mut self) {
        self.spring_open();
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        // **Every visible pane, not only the focused one.** A pane scrolls when
        // the pointer is over it whether or not it holds the keyboard, so a fling
        // in one half of a split would otherwise freeze mid-flight the moment the
        // other half took focus, and an unfocused pane's wheel easing would never
        // arrive at all.
        let mut moved = false;
        for pane in self.panes.visible_mut() {
            moved |= pane.scroll.advance(elapsed);
            // The other axis and every column, because each of them is a `Scroll`
            // with an easing and a fling of its own.
            moved |= pane.across.advance(elapsed);
            moved |= crate::trail::advance(&mut pane.trail, elapsed);
        }
        self.dirty |= moved;
        // The animation's own step, beside the scrolls: it is a wake-up with an
        // owner and this is the owner.
        if self.quick_look.as_mut().is_some_and(crate::quicklook::QuickLook::advance) {
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
        if self
            .watched
            .iter()
            .any(|path| !self.watcher.armed(path) && self.watcher.failure(path).is_none())
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
        let failure = self.watcher.failure(&self.watch_target);
        if failure != self.watch_failure {
            self.watch_failure = failure;
            self.dirty = true;
        }
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
            Event::KeyDown(key) => {
                self.drag_modifier(key.mods.ctrl);
                self.key(key, commands);
            }
            Event::KeyUp(key) => self.drag_modifier(key.mods.ctrl),
            Event::PointerMoved { position, mods } => {
                self.drag_modifier(mods.ctrl);
                self.moved(*position);
            }
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
                // shaded until it came back. The chevron was missing from this
                // list and stayed lit for the same reason.
                let gone = [
                    self.panes.focused_mut().hover.take().is_some(),
                    self.hovered_button.take().is_some(),
                    self.hovered_crumb.take().is_some(),
                    self.hovered_separator.take().is_some(),
                    self.hovered_tool.take().is_some(),
                    self.hovered_mode.take().is_some(),
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
            Event::Scroll { delta, precise, time, mods } => {
                self.scrolls.0 += 1;
                self.scrolls.1 += u64::from(*precise);
                self.scroll_time = *time;
                self.scrolled(*delta, *precise, *time, *mods);
            }
            Event::ScrollEnded { time } => {
                self.scrolls.2 += 1;
                self.scroll_released(*time);
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
                // **The picture first, and the list only when there is none.** A
                // pinch is about the thing being looked at, and with a panel open
                // over the window that is the panel — zooming the rows behind it
                // would be zooming something nobody can see.
                let stepped = self.zoom_the_look(if *out { PINCH_STEP } else { 1.0 / PINCH_STEP });
                if !stepped && self.quick_look.is_none() {
                    self.zoom_by(if *out { 1 } else { -1 });
                }
            }
            Event::Swiped { .. } | Event::Woken => {}
        }
        self.settle_menu(commands);
    }

    /// Builds the menu somebody asked for.
    ///
    /// Every event ends here, which is what makes this the only construction
    /// site: the pointer and the keyboard both leave a request rather than a
    /// menu, so the two paths cannot drift into two different menus.
    fn settle_menu(&mut self, commands: &Registry<Self>) {
        // **Read rather than taken**, because a row's `enabled` may ask what the
        // menu is *about* — `App::place_in_question` does — and taking the
        // request first would answer that question as though no menu were being
        // opened at all.
        if let Some((at, groups)) = self.menu_wanted {
            self.menu = crate::menu::build(groups, commands, at, |id| self.runnable(commands, id));
            self.menu_wanted = None;
            // A hover's request was against the menu this one replaces.
            self.submenu_wanted = None;
            self.dirty = true;
            // The Open with row learns its file now; its job waits for the row
            // to open — `expand_menu_row`.
            if let Some(file) = self.selected_paths().into_iter().next()
                && let Some(menu) = self.menu.as_mut()
            {
                menu.asks_about(file);
            }
        } else if let Some((row, from)) = self.submenu_wanted.take() {
            self.expand_menu_row(row, Some(from), commands);
        }
    }

    /// Whether `id` names a command that can run right now — the question every
    /// menu row asks when it is built, and again in every frame it is drawn.
    fn runnable(&self, commands: &Registry<Self>, id: &str) -> bool {
        commands.find(id).is_some_and(|command| commands.enabled(command, self))
    }

    fn key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) {
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
        if self.panel.is_some() && self.settings_key(event, commands) {
            return;
        }
        // The Properties panel is a statement, not a field: any key dismisses
        // it, and dismisses only. The diagnostic report passes through for the
        // reason it passes through an open menu — it is the only way the harness
        // can see the panel at all.
        if self.properties.is_some() && named(commands, event) != Some("help.dump") {
            self.properties = None;
            self.dirty = true;
            return;
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
        // **The look's own two chords, before the listing's.** `Ctrl+A` and
        // `Ctrl+C` mean the text in the panel rather than the files behind it —
        // the same shape the field scope has, and settled in the registry rather
        // than by a branch, so a rebindable keymap has somewhere to put it.
        // **Unless a field has the keyboard.** `F2` starts a rename with the look
        // still open, and `Ctrl+A` then means the name being typed — the field is
        // where the caret is, and the caret decides.
        if self.field_scope() == Scope::Global
            && (self.looking_at_text()
                || self.looking_at_an_animation()
                || self.looking_at_a_page_with_words())
            && let Some(command) = commands.resolve(event, Scope::Look)
            && commands.scope_of(command) == Scope::Look
        {
            commands.run(command, self);
            self.dirty = true;
            return;
        }
        // **A quick look takes the four arrows and nothing else.** It is a viewer
        // rather than a question, so it does not swallow the keyboard: `Space`
        // still closes it through the registry, `Escape` through `clear`, and
        // `Ctrl+W` still closes the tab. What it does take is what would
        // otherwise move a cursor nobody can see behind the panel.
        if self.quick_look.is_some() && self.quick_look_key(event) {
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
    /// **The rectangle the last frame drew**, recorded in
    /// [`crate::view::Hits::name_field`] — which is how the palette's field and
    /// the search's already answer this question.
    ///
    /// It used to be recomputed here as `body.y + row * row_height`, and that is
    /// the drawn box only in an ungrouped details list at scroll zero. A heading
    /// above the row, a grid cell or a scroll moved the field and the press went
    /// on testing the arithmetic: the press missed, the rename committed, and it
    /// fell through to the row — where a second click is Open. The doc comment
    /// this replaces said the alternative was "two rectangles that agree until
    /// one of them is edited", and then one of them was.
    fn naming_field(&self) -> Area {
        self.hits.name_field.filter(|_| self.edits.naming.is_some()).unwrap_or_default()
    }

    /// Puts the caret where a click inside the name field landed.
    ///
    /// Measured by shaping, not by dividing the width by the character count: a
    /// click halfway along `étude` is not halfway along its bytes. The text
    /// starts six logical pixels inside the box, which is where
    /// `view::name_field_at` draws it — the one number the two share.
    fn place_caret(&mut self, x: f32) -> Option<usize> {
        let area = self.hits.name_field.filter(|_| self.edits.naming.is_some())?;
        let size = self.theme.metrics.text_size;
        self.place_caret_in(Claim::Naming, area, size, crate::view::NAME_INSET, x)
    }

    /// Keeps every visible pane's trail equal to the ancestors of its path.
    ///
    /// **Once a frame, and the only caller of [`crate::trail::sync`].** A trail is
    /// a function of the path, so what has to be noticed is a path that changed —
    /// and this is the one place that sees that whatever caused it: a click in a
    /// column, a breadcrumb, `Alt ←`, a sidebar row, a restored session, a
    /// directory that vanished underneath. The same shape `watch_target` and
    /// `view_owed` already have, and for the same reason: the fourteen call sites
    /// that open a directory cannot each be trusted to remember.
    ///
    /// Cheap when nothing moved — a zip and a `take_while` over a path's depth —
    /// and it drops the whole trail in every other mode, which cancels those reads
    /// and hands the listings back.
    fn settle_trails(&mut self) {
        let Self { panes, jobs, .. } = self;
        for pane in panes.visible_mut() {
            if pane.mode == crate::cells::Mode::Columns {
                let path = pane.path.clone();
                crate::trail::sync(&mut pane.trail, &path, jobs);
            } else if !pane.trail.is_empty() {
                pane.trail.clear();
            }
        }
    }

    /// The scroll extent, and the folder totals for what is on screen.
    ///
    /// Once per frame, and bounded by the viewport rather than by the directory:
    /// keyed by path rather than by row, so a new listing shows what is already
    /// known instead of starting again.
    fn settle_rows(&mut self) {
        self.settle_views();
        self.settle_trails();
        self.rebandif_needed();
        // **Every leaf, because every leaf can be scrolled.** A pane whose extent
        // is never set clamps every scroll against a `max` of zero, so the wheel
        // over the unfocused half of a split moved nothing whatsoever.
        let row_height = self.theme.metrics.row_height;
        let leaves = std::mem::take(&mut self.placement.leaves);
        for layout in &leaves {
            let Some(tabs) = self.panes.leaf(layout.id) else { continue };
            let content = self.cells_of(layout, tabs.showing()).content_height();
            let columns = tabs.showing().mode == crate::cells::Mode::Columns;
            if let Some(tabs) = self.panes.leaf_mut(layout.id) {
                let pane = tabs.showing_mut();
                pane.scroll.set_extent(content, layout.body.height);
                // The other axis, and only in the one mode that has one. Set
                // every frame like the vertical extent, because the run grows by
                // a column every time somebody opens a folder.
                if columns {
                    let depth = pane.trail.len();
                    let width = layout.body.width;
                    crate::trail::settle(&mut pane.trail, layout.body, row_height);
                    // **The pane's own column is brought into view when there is a
                    // new one, and not on every frame.** Descending past the right
                    // edge is the normal way to use this mode; being dragged back
                    // there after scrolling left to look at an ancestor is not.
                    let Pane { across, revealed, .. } = pane;
                    crate::trail::follow(across, revealed, depth, width);
                } else {
                    pane.across.set_extent(0.0, layout.body.width);
                    pane.revealed = (0, 0.0);
                }
            }
        }
        self.placement.leaves = leaves;
        let cells = self.cells();
        let visible = cells.visible(self.panes.focused().scroll.offset());
        let Self { measurer, panes, jobs, .. } = self;
        measurer.update(panes.focused_mut(), visible, jobs);
        self.settle_colours();
        self.settle_thumbnails();
        self.settle_quick_look();
        // **Last, because both of the above put things in the queue.** The pump
        // used to be the last line of `settle_thumbnails`, which ran first — so a
        // quick look's request was queued *after* the only pump of the frame, no
        // child was spawned, and nothing was left to wake the loop: the panel
        // stayed blank until an unrelated event drew another frame. A frame is
        // only drawn when something is dirty, so "the next frame" can be minutes
        // away, or never.
        self.previews.pump(&self.jobs.pool, &self.waker.handle());
    }

    /// Puts the caret where a click in the path field landed.
    fn place_path_caret(&mut self, x: f32) -> Option<usize> {
        let area = crate::view::path_field(&self.placement.here, &self.theme.metrics);
        let size = self.theme.metrics.path_size;
        self.place_caret_in(Claim::Path, area, size, crate::view::PATH_INSET, x)
    }

    /// A press on the strip the window draws for itself. Whether it was one.
    ///
    /// **The press waits.** The compositor takes the pointer the moment
    /// `start_move` is asked, so asking here would make every click a drag; what
    /// starts the move is the first movement past the threshold. Nothing is
    /// drawn in this strip where the compositor draws the frame, and it has no
    /// height there either.
    fn press_on_the_title_bar(&mut self, position: Point, clicks: u8) -> bool {
        if !self.layout.drag.contains(position.x, position.y) {
            return false;
        }
        // A double click on a title bar toggles maximize everywhere else, so it
        // does here.
        if clicks >= 2 {
            self.toggle_maximized();
        } else {
            self.title_press = Some(position);
        }
        true
    }

    /// A press in a pane's own path bar. Whether it was one.
    ///
    /// A chevron, a component, one of the four buttons, or the space beside them
    /// — which opens the path for typing, the way the address bar in every
    /// browser does. **Every route moves the keyboard into that pane first**,
    /// because all four act on a pane and the one they act on is the one that
    /// was clicked, not whichever pane happened to be focused before.
    fn press_on_a_pane_bar(&mut self, position: Point, commands: &Registry<Self>) -> bool {
        if let Some((id, depth)) = self.separator_at(position) {
            self.focus_pane(id);
            self.open_crumb_menu(depth);
            return true;
        }
        if let Some((id, depth)) = self.crumb_at(position) {
            self.focus_pane(id);
            self.go_to_crumb(depth);
            return true;
        }
        // Through the registry, because a dimmed button must do nothing when it
        // is clicked and `enabled` is the only place that knows it is dimmed.
        if let Some((id, tool)) = self.tool_at(position) {
            self.focus_pane(id);
            if let Some(command) = commands.find(tool.command()) {
                commands.run(command, self);
            }
            self.dirty = true;
            return true;
        }
        // The switcher runs the same command the palette and `Ctrl+1` do, so a
        // mode that is already on costs a no-op rather than a second code path.
        if let Some((id, mode)) = self.mode_at(position) {
            self.focus_pane(id);
            if let Some(command) = commands.find(mode.command()) {
                commands.run(command, self);
            }
            self.dirty = true;
            return true;
        }
        let Some(id) = self.bar_at(position) else { return false };
        self.focus_pane(id);
        self.toggle_path_entry();
        true
    }

    /// Puts the keyboard in one pane, if it is not there already.
    fn focus_pane(&mut self, id: crate::panes::PaneId) {
        if self.panes.focus() != id {
            self.panes.focus_on(id);
            self.settle_panes();
        }
    }

    /// The navigation button under a point, and whose bar it is in.
    fn tool_at(&self, position: Point) -> Option<(crate::panes::PaneId, crate::layout::Tool)> {
        self.placement
            .leaves
            .iter()
            .find_map(|leaf| leaf.tool_at(position.x, position.y).map(|tool| (leaf.id, tool)))
    }

    /// The switcher button under a point, and the pane whose bar it is in.
    fn mode_at(&self, position: Point) -> Option<(crate::panes::PaneId, crate::cells::Mode)> {
        self.placement
            .leaves
            .iter()
            .find_map(|leaf| leaf.mode_at(position.x, position.y).map(|mode| (leaf.id, mode)))
    }

    /// Which pane's bar a point is in, if any.
    fn bar_at(&self, position: Point) -> Option<crate::panes::PaneId> {
        self.placement
            .leaves
            .iter()
            .find_map(|leaf| leaf.bar.contains(position.x, position.y).then_some(leaf.id))
    }

    /// A press inside an open text field: the caret goes there and a drag begins.
    ///
    /// Returns whether the press belonged to a field. **A press outside the name
    /// field commits it**, which is what every other text field on the desktop
    /// does — and the press then carries on to whatever it landed on.
    fn press_in_a_field(&mut self, position: Point, clicks: u8) -> bool {
        if self.edits.naming.is_some() {
            if self.naming_field().contains(position.x, position.y) {
                let at = self.place_caret(position.x);
                // **Cleared rather than left alone when a run was taken.** The
                // first press of a double click armed the drag, and a pointer
                // that moved one pixel before the button came up would extend
                // from the caret — collapsing the word that was just chosen.
                let took = self.chose_a_run(Claim::Naming, clicks, at);
                self.text_drag = (!took).then_some(Claim::Naming);
                return true;
            }
            self.commit_name();
        }
        if self.typing_path.is_some()
            && crate::view::path_field(&self.placement.here, &self.theme.metrics)
                .contains(position.x, position.y)
        {
            let at = self.place_path_caret(position.x);
            let took = self.chose_a_run(Claim::Path, clicks, at);
            self.text_drag = (!took).then_some(Claim::Path);
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
                let Some(area) = self.hits.name_field.filter(|_| self.edits.naming.is_some())
                else {
                    return;
                };
                (area, self.theme.metrics.text_size, crate::view::NAME_INSET)
            }
            Claim::Path => (
                crate::view::path_field(&self.placement.here, &self.theme.metrics),
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
    ///
    /// **Where it landed**, so the press that follows a second and a third click
    /// can take the run there without measuring the same pixel twice.
    fn place_caret_in(
        &mut self,
        which: Claim,
        area: Area,
        size: f32,
        inset: f32,
        x: f32,
    ) -> Option<usize> {
        // Written out rather than `self.field_for(which)?`, which borrows the
        // whole of `self` and would take the fonts and the cache below with it:
        // the arms name four different fields, so what is borrowed here is those
        // four and not the window.
        let field = (match which {
            Claim::Path => self.typing_path.as_mut(),
            Claim::Naming => self.edits.naming.as_mut().map(|naming| &mut naming.field),
            Claim::Palette => self.palette.as_mut().map(|palette| &mut palette.query),
            Claim::Search => self.search.as_mut().map(|search| &mut search.query),
            Claim::Filter => None,
        })?;
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
        let offset = field.caret;
        self.dirty = true;
        Some(offset)
    }

    /// A second and a third click in a field: the word under it, or all of it.
    ///
    /// Whether it took one, which is also whether the press should arm a drag —
    /// a click that has already chosen a run is not the start of one, and a
    /// pointer that wandered a pixel afterwards would collapse the word it just
    /// took back to a caret.
    fn chose_a_run(&mut self, which: Claim, clicks: u8, at: Option<usize>) -> bool {
        if clicks < 2 {
            return false;
        }
        let Some(offset) = at else { return false };
        let Some(field) = self.field_for(which) else { return false };
        field.select_run(offset, clicks > 2);
        self.dirty = true;
        true
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
            path: self.panes.focused().path_of(id),
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
        let took = self.edits.duplicate_all(paths, &self.jobs);
        self.said_it_is_busy(took);
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
        if self.erasing.as_ref().is_some_and(|asking| asking.paths == paths && asking.forever) {
            self.answer_the_question();
            return;
        }
        self.erasing = Some(Erasing { paths, on: Confirm::Cancel, forever: true });
        self.dirty = true;
    }

    /// Does what the panel is asking about, and closes it.
    ///
    /// **Whichever question it was.** A permanent deletion and a Move to Trash
    /// that was asked about are the same panel with the same buttons, and this
    /// is the one place the difference is spent.
    pub(crate) fn answer_the_question(&mut self) {
        let Some(asking) = self.erasing.take() else { return };
        self.step_off_the_selection(&asking.paths);
        let took = if asking.forever {
            self.edits.erase_all(asking.paths, &self.jobs)
        } else {
            self.edits.trash_all(asking.paths, &self.jobs)
        };
        self.said_it_is_busy(took);
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
            Key::Named(Named::Delete) => self.answer_the_question(),
            Key::Named(Named::Left | Named::Right | Named::Tab) if bare => {
                if let Some(asking) = self.erasing.as_mut() {
                    asking.on = match asking.on {
                        Confirm::Cancel => Confirm::Delete,
                        Confirm::Delete => Confirm::Cancel,
                    };
                }
            }
            Key::Named(Named::Enter) => match self.erasing.as_ref().map(|asking| asking.on) {
                Some(Confirm::Delete) => self.answer_the_question(),
                _ => self.keep_them(),
            },
            _ => {
                // The diagnostic report passes through, for the reason it passes
                // through an open menu: it is the only way the harness sees this.
                let diagnostic = named(commands, event) == Some("help.dump");
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
            || self.hits.look.iter().any(|(_, area)| on(area))
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
        if self.hits.pick_up.is_some_and(|a| a.contains(position.x, position.y)) {
            self.resume_operation();
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
            self.answer_the_question();
        }
        true
    }

    /// A press while a quick look is open. Whether it was taken.
    ///
    /// **Every press inside the panel, and none outside it.** A control does what
    /// its arrow says; anywhere else in the panel does nothing, because a picture
    /// somebody is looking at is not a button. A press *outside* falls through, so
    /// clicking the list behind the look closes it by choosing a different file —
    /// which is what a person means by clicking a list they can see.
    fn press_in_the_quick_look(&mut self, position: Point, shift: bool, clicks: u8) -> bool {
        if self.quick_look.is_none() {
            return false;
        }
        match self.look_control_at(position) {
            Some(crate::view::LookControl::PreviousFile) => self.look_at_sibling(-1),
            Some(crate::view::LookControl::NextFile) => self.look_at_sibling(1),
            Some(crate::view::LookControl::PreviousPage) => self.look_by(-1, false),
            Some(crate::view::LookControl::NextPage) => self.look_by(1, false),
            Some(crate::view::LookControl::Edit) => self.toggle_editing_the_look(),
            Some(crate::view::LookControl::Discard) => self.discard_the_look(),
            Some(crate::view::LookControl::Play) => self.toggle_playing(),
            Some(crate::view::LookControl::Source) => self.toggle_the_rendering(),
            Some(crate::view::LookControl::Extract) => self.extract_selection(),
            None => {
                let inside = crate::view::quick_look_area(&self.layout);
                let strip = crate::view::quick_look_strip(&self.layout);
                if self.press_in_the_archive(position, clicks) {
                    self.dirty = true;
                    return true;
                }
                if inside.contains(position.x, position.y)
                    && (self.start_choosing_rendered(position, shift, clicks)
                        || self.start_choosing_text(position, shift, clicks)
                        || self.start_choosing_a_page(position, shift, clicks)
                        || self.start_panning(position))
                {
                    self.dirty = true;
                    return true;
                }
                if !inside.contains(position.x, position.y)
                    && !strip.contains(position.x, position.y)
                {
                    // **Click-away closes it, and the press still lands.** A
                    // person clicking a row they can see means to select that row,
                    // and a look that stayed open over a different file than the
                    // cursor would be two answers to "which file is this".
                    self.close_the_look();
                    self.dirty = true;
                    return false;
                }
            }
        }
        self.dirty = true;
        true
    }

    /// A press on a word of a page: the start of a selection. Whether it took.
    ///
    /// **Only on a word.** A press on the paper falls through to the pan, which
    /// is what makes the two gestures unambiguous without a modifier or a tool
    /// palette: the margin moves the page and the text selects out of it. A page
    /// with no text layer — a scan — is all margin, so it pans everywhere it
    /// used to.
    fn start_choosing_a_page(&mut self, position: Point, shift: bool, clicks: u8) -> bool {
        let inner = crate::view::quick_look_area(&self.layout);
        let at = (position.x, position.y);
        let Some(look) = self.quick_look.as_ref() else { return false };
        if !crate::view::page_word_at(look, inner, at) {
            return false;
        }
        let Some(place) = crate::view::page_place(look, inner, at) else { return false };
        let Some(page) = self.quick_look.as_mut().and_then(|look| look.page_text.as_mut()) else {
            return false;
        };
        // **Two clicks take the word and three take the line**, which is what a
        // double and a triple click mean in every text there is — and a drag that
        // begins on the second click extends from the word rather than from the
        // character, which is also what they do.
        match clicks {
            0 | 1 => page.choose(place, shift),
            2 => page.select_run(place, false),
            _ => page.select_run(place, true),
        }
        self.page_drag = clicks <= 1;
        true
    }

    /// Extends a page's selection while the button is down.
    fn extend_the_page_choice(&mut self, position: Point) -> bool {
        if !self.page_drag {
            return false;
        }
        let inner = crate::view::quick_look_area(&self.layout);
        let at = (position.x, position.y);
        let Some(look) = self.quick_look.as_ref() else { return false };
        // **The nearest place, not the word under the pointer.** A drag that
        // leaves the text keeps extending, which is what dragging past an edge
        // means in every text view — `Page::place_at` is the half of the hit test
        // that is never nothing, and this is its one caller.
        let Some(place) = crate::view::page_place(look, inner, at) else { return false };
        self.quick_look
            .as_mut()
            .and_then(|look| look.page_text.as_mut())
            .is_some_and(|page| page.drag_to(place))
    }

    /// A press inside a zoomed picture: the start of a pan. Whether it took.
    ///
    /// **Only while it is zoomed.** A press on a picture that fits is not the
    /// start of anything — there is nowhere to drag it — and a gesture that
    /// quietly armed itself would make the panel feel like it had a mode.
    fn start_panning(&mut self, position: Point) -> bool {
        // **The page's selection goes away first.** A press on the paper is a
        // press outside the text, and everywhere else in this program that puts
        // a selection away rather than leaving one nothing is acting on.
        if let Some(page) = self.quick_look.as_mut().and_then(|look| look.page_text.as_mut()) {
            page.clear();
            self.dirty = true;
        }
        let taken = self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::zoomed);
        if taken {
            self.look_pan = Some(position);
        }
        taken
    }

    /// Drags a zoomed picture with the pointer.
    fn pan_the_look(&mut self, position: Point) -> bool {
        let Some(from) = self.look_pan else { return false };
        self.look_pan = Some(position);
        let by = (position.x - from.x, position.y - from.y);
        if !self.quick_look.as_mut().is_some_and(|look| look.pan_by(by)) {
            return false;
        }
        self.settle_the_zoom();
        true
    }

    /// A press inside a text preview: the start of a selection. Whether it took.
    ///
    /// **Only for text.** A press on a picture or a page is not the start of
    /// anything — there is nothing there to select — and a click that quietly
    /// armed a drag would make the panel feel like it had a mode.
    fn start_choosing_rendered(&mut self, position: Point, shift: bool, clicks: u8) -> bool {
        let Some(spot) = self.rendered_spot_at(position) else { return false };
        let Some(look) = self.quick_look.as_mut() else { return false };
        if clicks >= 2 {
            let Some(laid) = look.laid.as_ref() else { return false };
            let Some(line) = laid.lines.get(spot.line) else { return false };
            let (from, to) = if clicks > 2 {
                (0, line.text.len())
            } else {
                crate::word::around(&line.text, spot.byte)
            };
            look.picked = Some(crate::quicklook::Chosen {
                anchor: crate::quicklook::Spot { line: spot.line, byte: from },
                caret: crate::quicklook::Spot { line: spot.line, byte: to },
            });
            look.picking = false;
            return true;
        }
        let anchor = match look.picked.filter(|_| shift) {
            Some(picked) => picked.anchor,
            None => spot,
        };
        look.picked = Some(crate::quicklook::Chosen { anchor, caret: spot });
        look.picking = true;
        true
    }

    /// Extends a rendered selection while the button is down.
    fn extend_the_rendered_choice(&mut self, position: Point) -> bool {
        if !self.quick_look.as_ref().is_some_and(|look| look.picking) {
            return false;
        }
        let Some(spot) = self.rendered_spot_at(position) else { return false };
        let Some(look) = self.quick_look.as_mut() else { return false };
        let Some(picked) = look.picked.as_mut() else { return false };
        if picked.caret == spot {
            return false;
        }
        picked.caret = spot;
        true
    }

    /// Which line and byte of the rendered view a point is over.
    ///
    /// **The nearest place, never nothing.** A drag that leaves the text keeps
    /// extending, which is what dragging past an edge means everywhere.
    fn rendered_spot_at(&self, position: Point) -> Option<crate::quicklook::Spot> {
        let inner = crate::view::quick_look_area(&self.layout);
        let look = self.quick_look.as_ref()?;
        let laid = look.laid.as_ref().filter(|_| look.rendered)?;
        if laid.lines.is_empty() {
            return None;
        }
        let y = position.y - inner.y + look.scroll.offset();
        let line = laid.line_at(y).min(laid.lines.len() - 1);
        let x = position.x - inner.x - crate::view::MARKDOWN_PAD;
        let byte = crate::mdlayout::byte_at(&laid.lines[line], x);
        Some(crate::quicklook::Spot { line, byte })
    }

    fn start_choosing_text(&mut self, position: Point, shift: bool, clicks: u8) -> bool {
        let Some(spot) = self.text_spot_at(position) else { return false };
        let Some(look) = self.quick_look.as_mut() else { return false };
        // **Two clicks take the word and three the line**, the same two sentences
        // a page's words and a field answer to — and neither arms the drag, so
        // the run survives a pointer that moved a pixel before the button came
        // up.
        if clicks >= 2 {
            look.select_run(spot, clicks > 2);
            look.dragging = false;
            return true;
        }
        // **`Shift` keeps the anchor**, which is what a shift-click means in
        // every text there is: select from where the last click left off to here.
        // Without it the only way to select a paragraph was to hold the button
        // down for the whole of it, and a drag that has to cross a scroll is a
        // gesture nobody completes.
        let anchor = match look.chosen.filter(|_| shift) {
            Some(chosen) => chosen.anchor,
            None => spot,
        };
        look.chosen = Some(crate::quicklook::Chosen { anchor, caret: spot });
        look.dragging = true;
        true
    }

    /// Extends a selection that is being dragged.
    fn extend_the_text_choice(&mut self, position: Point) -> bool {
        if !self.quick_look.as_ref().is_some_and(|look| look.dragging) {
            return false;
        }
        let Some(spot) = self.text_spot_at(position) else { return false };
        let Some(look) = self.quick_look.as_mut() else { return false };
        let Some(chosen) = look.chosen.as_mut() else { return false };
        if chosen.caret == spot {
            return false;
        }
        chosen.caret = spot;
        true
    }

    /// Which line and byte of the text preview a point is over.
    fn text_spot_at(&mut self, position: Point) -> Option<crate::quicklook::Spot> {
        let inner = crate::view::quick_look_area(&self.layout);
        let metrics = self.theme.metrics;
        let scale = self.window.scale();
        let Self { quick_look, cache, fonts, .. } = self;
        let look = quick_look.as_ref()?;
        let text = look.text.as_ref()?;
        Some(crate::view::text_spot(
            look,
            text,
            (inner, &metrics),
            (position.x, position.y),
            (cache, fonts, scale),
        ))
    }

    /// What was asked for, what came back, and whether it is a draft.
    ///
    /// The three numbers behind "the picture changed size when the real one
    /// arrived": a draft is drawn scaled up to fill the panel and a full render
    /// is drawn at one to one, so the two agreeing is the whole of what makes a
    /// page turn look still.
    fn look_shot(&self) -> String {
        self.quick_look.as_ref().map_or_else(
            || "none".to_owned(),
            |look| {
                let asked = look.asked.map_or_else(
                    || "none".to_owned(),
                    |it| format!("{}x{}", it.within.0, it.within.1),
                );
                let got = look.image.map_or_else(
                    || "none".to_owned(),
                    |(_, size)| format!("{}x{}", size.0, size.1),
                );
                format!("asked={asked},got={got},coarse={},drafted={}", look.coarse, look.drafted)
            },
        )
    }

    /// Which of the three states a markdown preview is in, for a scenario.
    ///
    /// `none` where the file is not markdown, so a test can tell "this file does
    /// not render" from "it is rendering and showing the source".
    fn look_mode(&self) -> &'static str {
        let Some(look) = self.quick_look.as_ref().filter(|it| it.has_markdown()) else {
            return "none";
        };
        if look.editing {
            "editing"
        } else if look.rendered {
            "rendered"
        } else {
            "source"
        }
    }

    /// Whether the panel is showing a markdown file, rendered or not.
    pub(crate) fn looking_at_markdown(&self) -> bool {
        self.quick_look.as_ref().is_some_and(|look| !look.blocks.is_empty())
    }

    /// Swaps the rendered view for the source and back.
    ///
    /// **Not remembered** — ADR 0029. Every open of a markdown file starts
    /// rendered; this lasts as long as the panel does.
    pub(crate) fn toggle_the_rendering(&mut self) {
        let Some(look) = self.quick_look.as_mut() else { return };
        if look.blocks.is_empty() {
            return;
        }
        look.rendered = !look.rendered;
        // The two views scroll different distances, and a position measured in
        // one is meaningless in the other.
        look.scroll.to(0.0);
        look.picked = None;
        look.chosen = None;
        self.dirty = true;
    }

    /// Whether a quick look is showing text, so its two chords are live.
    pub(crate) fn looking_at_text(&self) -> bool {
        self.quick_look.as_ref().is_some_and(|look| look.text.is_some())
    }

    /// Whether the panel is showing a page whose words the child could find.
    ///
    /// **The condition that turns three commands on over a PDF** — ADR 0024 — and
    /// it is about the *layer* rather than about the file being a document: a
    /// scanned page is a document with no words, and there is nothing on it to
    /// select.
    #[must_use]
    pub(crate) fn looking_at_a_page_with_words(&self) -> bool {
        self.quick_look.as_ref().is_some_and(|look| look.page_text.is_some())
    }

    /// Whether there is text of either kind in the panel.
    #[must_use]
    pub(crate) fn looking_at_any_text(&self) -> bool {
        self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::has_text)
    }

    /// Whether that text has something selected in it.
    pub(crate) fn look_has_a_selection(&self) -> bool {
        self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::has_selection)
    }

    /// Selects everything the preview read — `Ctrl+A` inside a text preview.
    pub(crate) fn select_all_of_the_look(&mut self) {
        if let Some(look) = self.quick_look.as_mut() {
            look.select_all();
            self.dirty = true;
        }
    }

    /// Puts the selected text on the clipboard.
    ///
    /// **Both selections, the same as [`Self::copy_path`].** X11's primary
    /// selection is what a middle click pastes, and a person who has just dragged
    /// across text expects a middle click elsewhere to produce it.
    pub(crate) fn copy_the_look_selection(&mut self) {
        let Some(text) =
            self.quick_look.as_ref().and_then(crate::quicklook::QuickLook::chosen_text)
        else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.window.set_selection(Selection::Clipboard, Content::text(&text));
        self.window.set_selection(Selection::Primary, Content::text(&text));
        let lines = text.lines().count().max(1);
        self.notice = Some((
            sk_model::i18n::plural("notice.copied_lines", lines as u64, &[&lines]),
            Instant::now(),
        ));
        self.dirty = true;
    }

    /// Moves what is selected to the trash, asking first if the setting says to.
    ///
    /// **The question is off by default**, because rule 3 makes every trash one
    /// `Ctrl+Z` and a confirmation for something reversible is friction rather
    /// than protection. It is a setting because a person who wants it is not
    /// wrong — ADR 0027.
    pub(crate) fn restore_selection(&mut self) {
        let paths = self.selected_paths();
        let took = self.edits.restore_all(paths, &self.jobs);
        self.said_it_is_busy(took);
    }

    /// Whether every selected item is a direct child of a trash listing.
    pub(crate) fn can_restore(&self) -> bool {
        let paths = self.selected_paths();
        !self.busy()
            && !paths.is_empty()
            && paths.iter().all(|path| {
                sk_fs::trash::contains(path)
                    && path.parent().and_then(std::path::Path::file_name)
                        == Some(std::ffi::OsStr::new("files"))
            })
    }

    /// Prevent ordinary mutations from separating trash data from its metadata.
    pub(crate) fn trash_context(&self) -> bool {
        sk_fs::trash::contains(&self.panes.focused().path)
            || self.selected_paths().iter().any(|p| sk_fs::trash::contains(p))
    }

    /// Moves selected items into trash.
    pub(crate) fn trash_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let asked = self.erasing.as_ref().is_some_and(|it| it.paths == paths && !it.forever);
        if self.confirm_trash && !asked {
            self.erasing = Some(Erasing { paths, on: Confirm::Cancel, forever: false });
            self.dirty = true;
            return;
        }
        self.erasing = None;
        // The cursor leaves the doomed rows now rather than when the listing
        // comes back: a person deleting four files presses Delete four times, and
        // a cursor that lands nowhere makes the second press do nothing at all —
        // `enabled` reads `focused`.
        self.step_off_the_selection(&paths);
        let took = self.edits.trash_all(paths, &self.jobs);
        self.said_it_is_busy(took);
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

    /// Whether there is a zoom to put back — the list's, or a picture's.
    ///
    /// **Both, because one chord resets whichever is in front of the person.**
    /// `Ctrl 0` with a photograph magnified in the panel was dimmed on the
    /// grounds that the *list* was at its default size, which is a row of the
    /// palette saying no about the wrong zoom.
    pub(crate) fn zoomed(&self) -> bool {
        self.zoom != ZOOM_DEFAULT
            || self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::zoomed)
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

    /// **The picture when there is one, and the list otherwise.** All three of
    /// these are one command each in the registry — rule 2 — and what they act on
    /// is what is in front of the person: `Ctrl +` with a photograph filling the
    /// window means the photograph, and it is the same argument the wheel and the
    /// pinch make one layer down.
    pub(crate) fn zoom_in(&mut self) {
        if !self.zoom_the_look(PINCH_STEP) {
            self.zoom_by(1);
        }
    }

    pub(crate) fn zoom_out(&mut self) {
        if !self.zoom_the_look(1.0 / PINCH_STEP) {
            self.zoom_by(-1);
        }
    }

    pub(crate) fn zoom_reset(&mut self) {
        if self.unzoom_the_look() {
            return;
        }
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
            return crate::view::path_field(&self.placement.here, &self.theme.metrics);
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
        // **The trail as well as the pane.** A column showing a directory that
        // changes has to notice, and M0 already moved this from one directory to
        // every visible pane; a trail is more visible directories. Bounded by path
        // depth, so the per-user inotify budget in docs/09 is nowhere near it.
        let wanted: Vec<PathBuf> = self
            .panes
            .visible()
            .flat_map(|pane| {
                std::iter::once(pane.path.clone())
                    .chain(pane.trail.iter().map(|level| level.dir.path.clone()))
            })
            .collect();
        if wanted != self.watched {
            self.watched.clone_from(&wanted);
            self.watcher.watch(wanted);
        }
    }

    /// A press on the sidebar or a window button.
    ///
    /// A window button goes through the registry rather than through a method: a
    /// compositor offering no minimise must not be minimised by clicking where
    /// the button would have been, and `enabled` is the only place that knows.
    /// The four navigation buttons are a pane's now —
    /// [`Self::press_on_a_pane_bar`].
    fn press_on_the_chrome(&mut self, position: Point, commands: &Registry<Self>) -> bool {
        // **Before the rows**, because the band is on the sidebar's own edge and
        // the row under it reaches to within a pixel of the same line: without
        // this, every press that took hold of the divider also navigated.
        if self.on_the_aside_divider(position) {
            self.aside_drag = true;
            return true;
        }
        // **Before the row**, because the twisty is inside it: a press that
        // opened a place must not also navigate to it, which is what every tree
        // control on the desktop does and the difference between looking inside
        // a folder and going there.
        if let Some(path) = self.twisty_at(position) {
            self.place_target = Some(path);
            if let Some(command) = commands.find("place.expand") {
                commands.run(command, self);
            }
            // The same rule the menu's target follows: it outlives the gesture by
            // exactly the one command the gesture was for.
            self.place_target = None;
            return true;
        }
        if let Some(index) = self.place_at(position) {
            // A pinned row is also the start of a possible reorder. The
            // navigation still happens on the press — dragging a place opens it,
            // the way dragging a tab shows it, so there is nothing to defer to
            // the release and nothing that behaves differently from the rows
            // above it.
            if let Some(from) = self.pinned_at(index) {
                self.place_drag =
                    Some(PlaceDrag { from, at: from, started: position, active: false });
            }
            if let Some(entry) = self.sidebar.at(index) {
                // **A volume nothing has mounted is mounted by the press** — ADR
                // 0030. It has no path to go to yet, and the job that mounts it
                // navigates when udisks2 says where it landed.
                if entry.unmounted {
                    let device = entry.device.clone();
                    let name = entry.label.clone();
                    if let Some(device) = device {
                        self.mount_the_volume(device, name, Mounting::AndOpen);
                    }
                } else {
                    let path = entry.path.clone();
                    if path != self.panes.focused().path {
                        self.panes.focused_mut().open(path, &self.jobs);
                    }
                }
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
        self.press_in_the_history(position)
    }

    /// A press on the history strip: its header collapses, its rows jump. Whether
    /// it was one.
    ///
    /// **Aimed at the rectangles the frame recorded**, which is what makes a
    /// scrolled and clipped list clickable at all: only the drawn rows are in
    /// `hits.history_rows`, and each carries its index in the timeline rather than
    /// its position in the vector.
    fn press_in_the_history(&mut self, position: Point) -> bool {
        if self.hits.history_header.is_some_and(|at| at.contains(position.x, position.y)) {
            self.toggle_history();
            return true;
        }
        let found = self
            .hits
            .history_rows
            .iter()
            .find(|(_, at)| at.contains(position.x, position.y))
            .map(|(index, _)| *index);
        if let Some(index) = found {
            // **To the point *after* that edit**, which is what a person means by
            // pressing a row in a history: the state the world was in once that
            // change had been made. So the row at index `i` is `i + 1` applied.
            self.edits.walk_to(index + 1, &self.jobs);
            self.dirty = true;
            return true;
        }
        // A press anywhere else in the strip is spent rather than falling through
        // to a pane that is no longer under it.
        self.layout.history.height > 0.0 && self.layout.history.contains(position.x, position.y)
    }

    /// Whether `position` is in the grab band of the sidebar's own divider.
    ///
    /// The same four logical pixels a column boundary uses, either side of the
    /// line the sidebar draws at its right edge — and nothing at all when the
    /// sidebar is closed, where the band would be a strip of the list that
    /// swallowed presses.
    fn on_the_aside_divider(&self, position: Point) -> bool {
        let aside = self.layout.sidebar;
        aside.width > 0.0
            && (position.x - aside.right()).abs() <= COLUMN_GRAB
            && position.y >= aside.y
            && position.y <= aside.bottom()
    }

    /// Moves the sidebar's divider to where the pointer is.
    fn drag_the_aside(&mut self, position: Point) {
        let width = crate::sidebar::Sidebar::dragged_to(position.x);
        if (self.sidebar.width - width).abs() < f32::EPSILON {
            return;
        }
        self.marks.touch(crate::settings::Setting::SidebarWidth);
        self.sidebar.width = width;
        self.relayout();
        self.dirty = true;
    }

    /// The path of the row whose twisty covers `position`.
    fn twisty_at(&self, position: Point) -> Option<PathBuf> {
        let index =
            self.hits.twisties.iter().position(|area| area.contains(position.x, position.y))?;
        // **Nothing for a volume that is not mounted**, which draws no twisty:
        // its path is empty until something mounts it, and a read of `""` is a
        // press that fails invisibly.
        self.sidebar.at(index).filter(|row| !row.unmounted).map(|row| row.path.clone())
    }

    /// Whether the place in question is a row the sidebar can open.
    #[must_use]
    pub(crate) fn can_expand_place(&self) -> bool {
        let path = self.place_in_question();
        self.sidebar.rows().iter().any(|row| row.path == path)
    }

    /// Opens or closes the place in question.
    pub(crate) fn expand_place(&mut self) {
        let path = self.place_in_question();
        if self.sidebar.toggle(&path) {
            self.read_a_place(path);
        }
        self.dirty = true;
    }

    /// Reads what is under one place, on a job.
    ///
    /// **Directories only, and never on the frame loop** — one `getdents64` over
    /// a directory that may be on a mount that has stopped answering, which is
    /// rule 1's canonical case. The rows appear when the answer lands; until then
    /// the twisty is open and the row has nothing under it.
    fn read_a_place(&mut self, path: PathBuf) {
        let sender = self.replies.opened_tx.clone();
        let wake = self.waker.handle();
        let show_hidden = self.panes.focused().show_hidden;
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |cancel| {
            let names = crate::crumbs::children(&path, show_hidden, cancel).unwrap_or_default();
            if sender.send((path, names)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Takes delivery of what somebody on the bus asked for.
    ///
    /// **The window's half of `FileManager1`.** The thread answered the call
    /// already — a caller must not wait for a directory read — so what arrives
    /// here is the navigation, and it is the same navigation a person makes:
    /// through the pane, so the history, the watcher and the per-folder record
    /// all behave as they do for any other way of getting there.
    fn drain_the_bus(&mut self) {
        while let Ok(asked) = self.replies.asked_rx.try_recv() {
            let crate::service::Asked { ask, token } = asked;
            let paths = ask.paths().to_vec();
            // **Forward, as well as there.** `FileManager1` says *show me this*,
            // and a window that navigated behind three others has half-answered.
            // The token is the caller's: a Wayland compositor grants a raise on
            // one and refuses a client that asks for itself, which is the whole
            // reason the argument travels this far.
            self.raises.0 += 1;
            self.raises.1 += u32::from(self.window.raise(token.as_deref()));
            match ask {
                crate::service::Ask::Folders(_) => {
                    // **Not checked for being directories**, which would be a
                    // `stat` on the frame loop — rule 1 — for a question the
                    // caller has already answered by choosing this method. A
                    // path that is not one fails its read on a job and the pane
                    // says so, which is what a directory deleted a moment ago
                    // does too.
                    let mut folders = paths.into_iter();
                    let Some(first) = folders.next() else { continue };
                    self.panes.focused_mut().open(first, &self.jobs);
                    // The rest behind it, which is what a caller naming four
                    // folders means and what every tab in this window is for.
                    let rest: Vec<PathBuf> = folders.collect();
                    if !rest.is_empty() {
                        self.panes.open_tabs_behind(rest, &self.jobs);
                    }
                    self.settle_panes();
                }
                crate::service::Ask::Items(_) => {
                    let Some(first) = paths.first() else { continue };
                    let Some(folder) = first.parent().map(std::path::Path::to_path_buf) else {
                        continue;
                    };
                    // **The folder, then the name.** The row does not exist until
                    // the read lands, so the name is remembered and the cursor
                    // goes to it when it does — the same path an operation's
                    // subject takes.
                    self.panes.focused_mut().open(folder, &self.jobs);
                    if let Some(name) = first.file_name() {
                        let name = name.to_string_lossy().into_owned();
                        self.panes.focused_mut().select_after(name);
                    }
                    self.settle_panes();
                }
                crate::service::Ask::Properties(_) => {
                    let Some(first) = paths.first() else { continue };
                    let Some(folder) = first.parent().map(std::path::Path::to_path_buf) else {
                        continue;
                    };
                    self.panes.focused_mut().open(folder, &self.jobs);
                    if let Some(name) = first.file_name() {
                        let name = name.to_string_lossy().into_owned();
                        self.panes.focused_mut().select_after(name);
                    }
                    self.settle_panes();
                    // The panel reads the selection, which the listing has not
                    // landed for yet — so it is opened when the read does.
                    self.properties_owed = true;
                }
            }
            self.dirty = true;
        }
    }

    /// Opens the Properties panel a caller asked for, once there is a row for it.
    ///
    /// The panel is about the selection, and the selection is about a row the
    /// listing does not contain until it lands — so the request outlives the
    /// navigation by exactly one read, and this is where the two meet.
    fn settle_the_owed_panel(&mut self) {
        if !self.properties_owed || self.panes.focused().loading {
            return;
        }
        self.properties_owed = false;
        if self.properties.is_none() {
            self.toggle_properties();
        }
    }

    /// Takes delivery of one place's contents.
    fn drain_opened(&mut self) {
        let mut any = false;
        while let Ok((path, names)) = self.replies.opened_rx.try_recv() {
            self.sidebar.took(path, names);
            any = true;
        }
        if any {
            self.dirty = true;
        }
    }

    /// Where the pinned rows are in the flattened list.
    ///
    /// One contiguous run, because they are one section. Nothing where nothing is
    /// pinned, which is also when there is no section and no heading.
    fn pinned_run(&self) -> Option<std::ops::Range<usize>> {
        let rows = self.sidebar.rows();
        let first = rows.iter().position(|row| row.pinned)?;
        let last = rows.iter().rposition(|row| row.pinned)?;
        Some(first..last + 1)
    }

    /// A flattened row index as an index among the pinned places.
    fn pinned_at(&self, row: usize) -> Option<usize> {
        let run = self.pinned_run()?;
        run.contains(&row).then(|| row - run.start)
    }

    /// The gap a reorder would drop into, from the pointer's height.
    ///
    /// A gap rather than a row: `0` is above the first pinned row and `n` is
    /// below the last, which is the only way to express "put it at the end". Each
    /// row's own midline is the boundary, so a row is displaced once the pointer
    /// is past half of it.
    fn place_gap_at(&self, y: f32) -> usize {
        let Some(run) = self.pinned_run() else { return 0 };
        let mut gap = 0;
        for row in run.clone() {
            let Some(area) = self.hits.places.get(row) else { continue };
            if y > area.y + area.height / 2.0 {
                gap = row - run.start + 1;
            }
        }
        gap
    }

    /// Follows a reorder with the pointer.
    fn drag_the_place(&mut self, position: Point) {
        let Some(drag) = self.place_drag else { return };
        let moved = (position.x - drag.started.x).abs().max((position.y - drag.started.y).abs());
        if !drag.active && moved <= DRAG_THRESHOLD {
            return;
        }
        let at = self.place_gap_at(position.y);
        let Some(drag) = self.place_drag.as_mut() else { return };
        if drag.active && drag.at == at {
            return;
        }
        drag.active = true;
        drag.at = at;
        self.dirty = true;
    }

    /// Ends a reorder, applying it if the row actually moved. Whether it was one.
    fn drop_the_place(&mut self) -> bool {
        let Some(drag) = self.place_drag.take() else { return false };
        if !drag.active {
            return false;
        }
        let order = self.sidebar.pinned();
        if let Some(moved) = crate::sidebar::moved(&order, drag.from, drag.at) {
            self.reorder_places(moved);
        }
        true
    }

    /// The place a place command is about.
    ///
    /// The row a menu was opened over, and otherwise the folder the focused pane
    /// is showing — so the same command means "this row" from the sidebar's menu
    /// and "where I am" from the palette, a chord or the background menu, and
    /// neither reading needs a second command.
    fn place_in_question(&self) -> PathBuf {
        self.place_target.clone().unwrap_or_else(|| self.panes.focused().path.clone())
    }

    /// The folder a colour would be given to, if there is one.
    ///
    /// **No fallback to the folder on screen**, which is where this differs from
    /// [`App::folder_to_pin`]: "pin this folder" means the one you are in when
    /// the row is a file, and "make this red" cannot — a person who right-clicks
    /// a PDF and picks Red means the PDF, and the honest answer to that is a
    /// dimmed row rather than a folder somewhere else turning red. Finder and
    /// Drive both colour the thing that was clicked, and neither offers it for
    /// nothing.
    fn folders_to_colour(&self) -> Vec<PathBuf> {
        // A place's own menu names one place and nothing else; over a listing the
        // answer is the selection, which is [rule
        // 8](../../../docs/03-principles.md): one selection, and every command
        // that acts on entries acts on all of it. Colouring six folders one at a
        // time is six menus for one decision.
        if let Some(place) = self.place_target.clone() {
            return vec![place];
        }
        self.selected_directories()
    }

    /// Whether there is a folder a colour can be given to.
    #[must_use]
    pub(crate) fn can_colour(&self) -> bool {
        !self.folders_to_colour().is_empty()
    }

    /// Whether any folder in question has a colour to take away.
    #[must_use]
    pub(crate) fn can_uncolour(&self) -> bool {
        self.folders_to_colour().iter().any(|path| self.views.colour_of(path).is_some())
    }

    /// Gives every folder in question a colour, or takes their colours away.
    pub(crate) fn colour_folder(&mut self, colour: Option<crate::colours::Colour>) {
        let mut changed = false;
        for path in self.folders_to_colour() {
            changed |= self.views.colour(&path, colour);
        }
        // **One write for the whole selection**, not one per folder: the record is
        // a file per folder's *parent* and six writes of it would be six passes
        // over the same lines.
        if changed {
            self.keep_the_views();
        }
        self.dirty = true;
    }

    /// The folder a pin would add to the list of places.
    ///
    /// **What is being pointed at, and the folder on screen when that is not a
    /// folder.** Three surfaces offer Pin and each points at something
    /// different: a place's own menu names a place, a row's menu opens over the
    /// row it selects, and the background menu points at nothing — so the order
    /// here is the order of specificity. A file's row falls through to the
    /// directory on screen, which is the only thing "this folder" can mean when
    /// the row is not one.
    ///
    /// **Not [`App::place_in_question`]**, which the other four place commands
    /// use: unpinning, reordering and opening act on a *place*, and a place is
    /// never a row of the listing. Only pinning can be asked about a folder that
    /// is not on the list yet, which is the whole point of it.
    fn folder_to_pin(&self) -> PathBuf {
        self.place_target
            .clone()
            .or_else(|| self.focused_directory())
            .unwrap_or_else(|| self.panes.focused().path.clone())
    }

    /// Whether the folder in question could be added to the list.
    ///
    /// Dimmed for something already on it: a well-known directory is there from
    /// `user-dirs.dirs` and a volume from the mount table, and `sidebar::build`
    /// drops a bookmark that duplicates a well-known directory — so the line
    /// would be written and change nothing on screen.
    #[must_use]
    pub(crate) fn can_pin(&self) -> bool {
        !self.sidebar.holds(&self.folder_to_pin())
    }

    /// Whether the place in question is one that can be taken off the list.
    #[must_use]
    pub(crate) fn can_unpin(&self) -> bool {
        self.sidebar.is_pinned(&self.place_in_question())
    }

    /// Adds the folder in question to the list of places.
    pub(crate) fn pin_place(&mut self) {
        let path = self.folder_to_pin();
        self.write_the_places(crate::sidebar::Edit::Pinned, path.clone(), move || {
            sk_desktop::pin(&path)
        });
    }

    /// Takes it off the list.
    pub(crate) fn unpin_place(&mut self) {
        let path = self.place_in_question();
        self.write_the_places(crate::sidebar::Edit::Unpinned, path.clone(), move || {
            sk_desktop::unpin(&path)
        });
    }

    /// Moves the place in question one row up the pinned list, or one row down.
    ///
    /// **The keyboard's half of the drag**, which is [rule
    /// 5](../../docs/03-principles.md): a reorder that only a pointer can do is a
    /// feature half the people using it cannot reach.
    pub(crate) fn move_place(&mut self, down: bool) {
        let path = self.place_in_question();
        let mut order = self.sidebar.pinned();
        let Some(from) = order.iter().position(|it| *it == path) else { return };
        let to = if down { from + 1 } else { from.wrapping_sub(1) };
        if to >= order.len() {
            return;
        }
        order.swap(from, to);
        self.reorder_places(order);
    }

    /// Whether the place in question has a pinned row above it.
    #[must_use]
    pub(crate) fn can_raise_place(&self) -> bool {
        self.sidebar.pinned().first().is_some_and(|first| *first != self.place_in_question())
            && self.can_unpin()
    }

    /// Whether it has one below it.
    #[must_use]
    pub(crate) fn can_lower_place(&self) -> bool {
        self.sidebar.pinned().last().is_some_and(|last| *last != self.place_in_question())
            && self.can_unpin()
    }

    /// Writes a new order for the pinned places.
    fn reorder_places(&mut self, order: Vec<PathBuf>) {
        let subject = order.first().cloned().unwrap_or_default();
        self.write_the_places(crate::sidebar::Edit::Reordered, subject, move || {
            sk_desktop::reorder(&order)
        });
    }

    /// Runs one write of the list of places on a job, and reads the list again
    /// afterwards.
    ///
    /// **On a job, and the reload is the job's own last act** — rule 1, and the
    /// reason the sidebar cannot simply be told what it now holds: the file is
    /// shared with every other file manager on the machine, so what it holds
    /// after a write is what the file says rather than what we asked for.
    fn write_the_places(
        &mut self,
        edit: crate::sidebar::Edit,
        subject: PathBuf,
        write: impl FnOnce() -> bool + Send + 'static,
    ) {
        let sender = self.replies.places_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let done = write();
            if sender.send((edit, subject, done)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Takes delivery of a write to the list of places.
    fn drain_places(&mut self) {
        let mut any = false;
        while let Ok((edit, subject, done)) = self.replies.places_rx.try_recv() {
            any = true;
            let name = crate::panes::title_of(&subject).to_owned();
            self.notice = Some((
                match (edit, done) {
                    (_, false) => sk_model::i18n::lookup("notice.places_refused").to_owned(),
                    (crate::sidebar::Edit::Pinned, true) => {
                        sk_model::i18n::format("notice.pinned", &[&name])
                    }
                    (crate::sidebar::Edit::Unpinned, true) => {
                        sk_model::i18n::format("notice.unpinned", &[&name])
                    }
                    (crate::sidebar::Edit::Reordered, true) => {
                        sk_model::i18n::lookup("notice.places_ordered").to_owned()
                    }
                },
                Instant::now(),
            ));
        }
        if any {
            self.sidebar.reload(&self.jobs);
            self.dirty = true;
        }
    }

    /// Follows the pointer across the sidebar. Whether the row under it changed.
    fn hover_the_sidebar(&mut self, position: Point) -> bool {
        let place = self.place_at(position);
        if self.sidebar.hover == place {
            return false;
        }
        self.sidebar.hover = place;
        true
    }

    /// Follows the pointer across the history list.
    ///
    /// From the rectangles the frame recorded rather than from arithmetic, for the
    /// reason [`crate::view::HistoryParts`] gives: only the drawn rows exist, and
    /// the first of them is not row zero.
    fn hover_the_history(&mut self, position: Point) {
        let edit = self
            .hits
            .history_rows
            .iter()
            .find(|(_, at)| at.contains(position.x, position.y))
            .map(|(index, _)| *index);
        if self.hovered_edit != edit {
            self.hovered_edit = edit;
            self.dirty = true;
        }
    }

    /// Whether there is any history at all — anything done or anything undone.
    ///
    /// What decides whether the strip exists, and therefore whether the command
    /// that collapses it can run.
    #[must_use]
    pub(crate) fn has_history(&self) -> bool {
        !self.edits.history.is_empty()
    }

    /// Opens the history list, or collapses it to its header.
    pub(crate) fn toggle_history(&mut self) {
        self.history_open = !self.history_open;
        self.relayout();
        self.settle_history();
        self.dirty = true;
    }

    /// Keeps the history list's scroll extent current.
    ///
    /// Called wherever the list's length or its box changes, which is a toggle, a
    /// recorded edit and a window resize — the same three things the pane's own
    /// extent is settled for.
    fn settle_history(&mut self) {
        let parts = crate::view::HistoryParts::of(self.layout.history, &self.theme.metrics);
        let content = self.edits.history.len() as f32 * self.theme.metrics.row_height;
        self.history_scroll.set_extent(content, parts.rows.height);
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
        self.focus_pane(id);
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

    /// A wheel notch or a touchpad delta, wherever it belongs.
    fn scrolled(&mut self, delta: Point, precise: bool, time: u32, mods: sk_plat::Modifiers) {
        // **Before anything decides what the scroll is for**, so that the list,
        // the palette and a tall menu all move at one sensitivity. `Ctrl` and a
        // wheel read the sign only, and a sign does not scale.
        let gain = sk_ui::SCROLL_GAIN;
        let delta = Point::new(delta.x * gain, delta.y * gain);
        // The palette takes the wheel while it is open. Without this the twelfth
        // result of fifty is unreachable by pointer alone, and the listing
        // scrolls behind a panel covering it.
        if mods.ctrl && self.palette.is_none() && self.menu.is_none() && self.quick_look.is_none() {
            // What `Ctrl` and a wheel do in every list and every browser, and
            // the pointer's half of what a pinch does.
            //
            // **Unless a panel is open**, where the same gesture means the
            // picture in it — the branch below — or nothing at all: zooming the
            // rows behind a panel covering them is zooming something nobody can
            // see, which is the rule every other overlay here follows.
            self.zoom_by(if delta.y < 0.0 { 1 } else { -1 });
        } else if let Some(palette) = self.palette.as_mut() {
            palette.wheel(delta.y);
        } else if self.menu.is_some() {
            // A menu taller than the window scrolls; one that fits does not
            // move, and neither does the list behind it.
            //
            // **The wheel belongs to the panel under the pointer.** A submenu of
            // fifteen applications scrolls on its own, and the parent behind it
            // stays put.
            let room = self.layout.window.height - crate::menu::MARGIN * 2.0;
            let over_the_submenu = self
                .hits
                .submenu_panel
                .is_some_and(|panel| panel.contains(self.pointer.x, self.pointer.y));
            if let Some(menu) = self.menu.as_mut() {
                if over_the_submenu && let Some(sub) = menu.sub.as_mut() {
                    sub.menu.wheel(delta.y, room);
                } else {
                    menu.wheel(delta.y, room);
                }
            }
        } else if mods.ctrl
            && self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::zoomable)
        {
            // **`Ctrl` and the wheel, which is the gesture every image viewer
            // has.** It is also the only one that works on X11, where there are
            // no touchpad gestures at all: `zwp_pointer_gestures_v1` is Wayland's
            // and the X11 half reports the capability absent
            // ([06-portability.md](../../../docs/06-portability.md)).
            let steps = if delta.y > 0.0 { 1.0 / PINCH_STEP } else { PINCH_STEP };
            self.zoom_the_look(steps);
        } else if let Some(look) = self.quick_look.as_mut() {
            // The panel is over everything, so the wheel is its own — the same
            // rule the palette and a tall menu follow.
            let inner = crate::view::quick_look_area(&self.layout);
            let step = self.theme.metrics.row_height;
            // **An archive scrolls its own list**, which is a different one from
            // the text preview's: the panel holds one or the other, and a wheel
            // that moved `look.scroll` over a listing would move a scroll nothing
            // was drawing from.
            let scroll = if let Some(archive) = look.inside.as_mut() {
                let rows = archive.rows.len();
                archive.scroll.set_extent(rows as f32 * step, inner.height);
                &mut archive.scroll
            } else {
                let rows = look.text.as_ref().map_or(0, |it| it.rows().len());
                look.scroll.set_extent(rows as f32 * step, inner.height);
                &mut look.scroll
            };
            scroll.by(delta.y, precise, time);
            self.dirty = true;
        } else if self.history_open && self.layout.history.contains(self.pointer.x, self.pointer.y)
        {
            // **The panel under the pointer takes the wheel**, which is the rule
            // the panes already follow: scrolling belongs to the thing you are
            // pointing at. Without it the list behind the strip moved while the
            // rows the hand was over did not.
            self.settle_history();
            self.history_scroll.by(delta.y, precise, time);
            self.dirty = true;
        } else {
            let (id, column) = self.scrolling_column();
            let sideways = mods.shift;
            if let Some(leaf) = self.panes.leaf_mut(id) {
                let pane = leaf.showing_mut();
                let columns = pane.mode == crate::cells::Mode::Columns;
                // **`Shift` and the wheel, which is how every other program moves
                // sideways.** The x delta below only exists on a touchpad, so
                // without this a run of columns deeper than the pane is wide could
                // not be reached with a mouse at all.
                if columns && sideways {
                    pane.across.by(delta.y, precise, time);
                } else if let Some(depth) = column.filter(|_| columns) {
                    // The column under the pointer, not the pane's own. An
                    // ancestor is a list like any other and the wheel over it is
                    // about it.
                    if let Some(level) = pane.trail.get_mut(depth) {
                        level.scroll.by(delta.y, precise, time);
                    }
                } else {
                    pane.scroll.by(delta.y, precise, time);
                }
                // **The x delta was dropped everywhere until there was a mode that
                // could use it.** A touchpad reports a hand's sideways drift on
                // every scroll, so this only reaches a run of columns.
                if columns && !sideways {
                    pane.across.by(delta.x, precise, time);
                }
            }
        }
        self.dirty = true;
    }

    /// **The fingers left the pad**, and the list is thrown if they were moving.
    ///
    /// Only ever sent for a touchpad, and only on Wayland: a wheel has no end and
    /// X11 reports no source. Does nothing at all when the fingers had come to
    /// rest, and nothing when a panel took the gesture — the pane saw no sample,
    /// so there is no velocity to throw and no guard needed for one.
    fn scroll_released(&mut self, time: u32) {
        let (id, column) = self.scrolling_column();
        let thrown = self.panes.leaf_mut(id).is_some_and(|leaf| {
            let pane = leaf.showing_mut();
            // The lift belongs to whatever the gesture was scrolling, which is the
            // column it was over if there was one.
            let columns = pane.mode == crate::cells::Mode::Columns;
            let scroll = match column.filter(|_| columns) {
                Some(depth) => match pane.trail.get_mut(depth) {
                    Some(level) => &mut level.scroll,
                    None => &mut pane.scroll,
                },
                None => &mut pane.scroll,
            };
            scroll.released(time);
            scroll.animating()
        });
        // A lift that threw nothing is not a frame: the list is already drawn
        // where the finger stopped it.
        self.flings += u64::from(thrown);
        self.dirty |= thrown;
    }

    /// The column boundary under `position`, and the leaf whose header it is in.
    ///
    /// **Any leaf's header, not the focused one's.** The widths are the window's,
    /// so a boundary in a split's other half sets the same three numbers — and a
    /// hit test that only looked at the focused pane would give one of two
    /// identical headers a grab band and the other none.
    ///
    /// Details only. A grid's header is three labels in a strip at the left, not
    /// three columns, and there is no boundary there to take hold of.
    fn column_edge_at(&self, position: Point) -> Option<(crate::panes::PaneId, ColumnEdge)> {
        let leaf = self
            .placement
            .leaves
            .iter()
            .find(|leaf| leaf.header.contains(position.x, position.y))?;
        if self.panes.leaf(leaf.id)?.showing().mode != crate::cells::Mode::Details {
            return None;
        }
        let edges = [
            (ColumnEdge::Name, leaf.name),
            (ColumnEdge::Size, leaf.size),
            (ColumnEdge::Modified, leaf.modified),
        ];
        edges.into_iter().find_map(|(edge, area)| {
            (area.width > 0.0 && (position.x - area.right()).abs() <= COLUMN_GRAB)
                .then_some((leaf.id, edge))
        })
    }

    /// Moves the column boundary being dragged to where the pointer is.
    ///
    /// Measured from the column's own left edge in the leaf the drag started in,
    /// so a drag that wanders into the other half of a split keeps setting the
    /// width it took hold of rather than jumping to whatever the pointer is over.
    fn drag_the_column(&mut self, id: crate::panes::PaneId, edge: ColumnEdge, position: Point) {
        let Some(leaf) = self.placement.leaves.iter().find(|leaf| leaf.id == id) else { return };
        let from = match edge {
            ColumnEdge::Name => leaf.name.x,
            ColumnEdge::Size => leaf.size.x,
            ColumnEdge::Modified => leaf.modified.x,
        };
        let width = position.x - from;
        let widths = &mut self.columns;
        match edge {
            ColumnEdge::Name => widths.name = width,
            ColumnEdge::Size => widths.size = width,
            ColumnEdge::Modified => widths.modified = width,
        }
        self.columns = self.columns.clamped();
        self.relayout();
        self.dirty = true;
    }

    /// Puts the column widths back to what they ship as.
    ///
    /// **The way out of a drag nobody meant.** A boundary dragged to its floor
    /// leaves a column of ellipses, and the grab band for widening it again is
    /// four pixels wide — so the reset is a command, in the registry with
    /// everything else, and therefore in the palette and the menu.
    pub(crate) fn reset_columns(&mut self) {
        self.columns = crate::layout::Columns::default();
        self.relayout();
        self.dirty = true;
    }

    /// The pane a scroll is about, and which of its ancestor columns if any.
    ///
    /// One answer for both halves of a gesture: the wheel and the lift that ends
    /// it have to reach the same list, and asking twice is two answers that agree
    /// until the pointer moves between them.
    fn scrolling_column(&self) -> (crate::panes::PaneId, Option<usize>) {
        let id = self.scrolling_pane();
        let Some(leaf) = self.placement.leaves.iter().find(|leaf| leaf.id == id) else {
            return (id, None);
        };
        let Some(pane) = self.panes.leaf(id).map(crate::panes::Tabs::showing) else {
            return (id, None);
        };
        let depth = crate::trail::column_at(
            pane.trail.len(),
            leaf.body,
            pane.across.offset(),
            self.pointer,
        );
        (id, depth)
    }

    /// The pane a scroll is about.
    ///
    /// **The one under the pointer, whichever pane holds the keyboard.** That is
    /// what every list in every application does, and a split makes the
    /// difference obvious: hovering one half and turning the wheel used to move
    /// the other. The whole leaf rather than its body, because a tab strip and a
    /// column header are part of the same list to look at, and a wheel that dies
    /// over the header is a worse surprise than one that scrolls.
    ///
    /// Falls back to the focused pane when the pointer is over no pane at all —
    /// the sidebar, a window edge, or a position never reported because nothing
    /// has moved the pointer yet.
    fn scrolling_pane(&self) -> crate::panes::PaneId {
        self.leaf_at(self.pointer).unwrap_or_else(|| self.panes.focus())
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
        self.marks.touch(crate::settings::Setting::Sidebar);
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

    /// Whether a stopped queue is waiting to be picked up.
    #[must_use]
    pub(crate) fn can_resume(&self) -> bool {
        self.edits.resumable()
    }

    pub(crate) fn can_skip_failed_operation(&self) -> bool {
        self.edits.can_skip_failed()
    }

    pub(crate) fn has_operation_issue(&self) -> bool {
        !self.busy() && (self.edits.issue.is_some() || self.can_resume())
    }

    pub(crate) fn skip_failed_operation(&mut self) {
        self.edits.skip_failed(&self.jobs);
        self.dirty = true;
    }

    pub(crate) fn dismiss_operation_issue(&mut self) {
        self.edits.dismiss_issue();
        self.notice = None;
        self.dirty = true;
    }

    /// Picks up what a stopped queue left.
    pub(crate) fn resume_operation(&mut self) {
        self.edits.resume(&self.jobs);
        self.dirty = true;
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
        let generation = self.window.clipboard().generation();
        let into = self.panes.focused().path.clone();
        if mime != "x-special/gnome-copied-files"
            && offered.iter().any(|mime| mime == "application/x-kde-cutselection")
        {
            let Some(fd) = self
                .window
                .request_selection(Selection::Clipboard, "application/x-kde-cutselection")
            else {
                self.clipboard_failed();
                return;
            };
            let sender = self.replies.paste_tx.clone();
            let wake = self.waker.handle();
            self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
                let reply = match sk_plat::receive_selection(&fd, 2, Duration::from_secs(5)) {
                    Ok(bytes) => Pasted::KdeIntent(bytes == b"1", generation, into),
                    Err(_) => Pasted::NotFiles,
                };
                let _ = sender.send(reply);
                wake.wake();
            });
        } else {
            self.read_file_paste(mime, false, generation, into);
        }
    }

    fn paste_offer_is_current(&mut self, generation: u64) -> bool {
        if generation == self.window.clipboard().generation() {
            return true;
        }
        self.notice =
            Some((sk_model::i18n::lookup("error.clipboard_changed").to_owned(), Instant::now()));
        self.dirty = true;
        false
    }

    fn clipboard_failed(&mut self) {
        self.notice =
            Some((sk_model::i18n::lookup("error.clipboard_no_files").to_owned(), Instant::now()));
        self.dirty = true;
    }

    fn read_file_paste(&mut self, mime: &str, moving: bool, generation: u64, into: PathBuf) {
        let Some(fd) = self.window.request_selection(Selection::Clipboard, mime) else {
            self.clipboard_failed();
            return;
        };
        let carries_verb = mime == "x-special/gnome-copied-files";
        let sender = self.replies.paste_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let Ok(bytes) =
                sk_plat::receive_selection(&fd, 16 * 1024 * 1024, Duration::from_secs(5))
            else {
                let _ = sender.send(Pasted::NotFiles);
                wake.wake();
                return;
            };

            // **The peer's verb wins where there is one.** A cut in Nautilus
            // arrives as `x-special/gnome-copied-files` opening with `cut`, and
            // pasting it as a copy leaves the original behind — which is a paste
            // that silently did half of what was asked.
            if carries_verb && let Some((intent, paths)) = sk_plat::decode_copied_files(&bytes) {
                let paths: Vec<PathBuf> =
                    paths.into_iter().filter(|path| path.symlink_metadata().is_ok()).collect();
                let moving = matches!(intent, sk_plat::Intent::Cut);
                let reply = if paths.is_empty() {
                    Pasted::NotFiles
                } else {
                    Pasted::Files(paths, moving, generation, into)
                };
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
            let reply = if paths.is_empty() {
                Pasted::NotFiles
            } else {
                Pasted::Files(paths, moving, generation, into)
            };
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
        let path = self.panes.focused().path_of(id);
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
        self.ask_which_applications(path, crate::openers::Asker::Panel);
    }

    /// Starts the job that finds which applications open `path`, for whichever
    /// of the two places shows the answer: the panel, or a menu's Open with row.
    fn ask_which_applications(&mut self, path: PathBuf, asker: crate::openers::Asker) {
        let Some(desktop) = self.desktop.clone() else { return };
        let sender = self.replies.openers_tx.clone();
        let wake = self.waker.handle();
        // **A menu's job is the menu's to stop.** On a mount that has stopped
        // answering it would otherwise sit on the pane's lane and then open a
        // file nobody is waiting to hear about — so the token is kept, and
        // `close_menu` pulls it.
        let cancel = sk_util::Cancel::new();
        if asker == crate::openers::Asker::Menu
            && let Some(previous) = self.menu_job.replace(cancel.clone())
        {
            previous.cancel();
        }
        self.jobs.pool.spawn(self.panes.focused().lane(), cancel, move |cancel| {
            if cancel.is_cancelled() {
                return;
            }
            let mime = desktop.mime.of(&path, false);
            let rows = desktop.apps.handlers_for(&mime);
            let found = crate::openers::Handlers { path, mime, rows };
            if sender.send((asker, found)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Closes every tab standing on a disk that has just gone.
    ///
    /// **What a USB stick being unplugged does to the window.** A tab whose
    /// directory has been removed from under it has nothing to show, and a
    /// listing that reports the folder missing is a worse answer than not
    /// offering it — so it goes, and the status line says how many did and which
    /// disk it was.
    ///
    /// Also closes a quick look at a file on it, for the same reason: a preview
    /// of a file that is not there is a panel nothing can refresh.
    fn leave_the_drives_that_went(&mut self, gone: &[PathBuf]) {
        if gone.is_empty() {
            return;
        }
        // `/` where there is no `$HOME`: it is always mounted, so the last tab of
        // a leaf lands somewhere rather than on an empty listing.
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        if self
            .quick_look
            .as_ref()
            .is_some_and(|look| gone.iter().any(|at| look.path.starts_with(at)))
        {
            self.quick_look = None;
        }
        let went = self.panes.close_under(gone, &home, &self.jobs);
        self.settle_panes();
        self.dirty = true;
        if went == 0 {
            return;
        }
        // The disk's own name, which is the mount point's last component — the
        // label somebody gave it, because that is where udisks2 mounts it.
        let name = gone.first().map(|at| crate::panes::title_of(at).to_owned()).unwrap_or_default();
        self.notice = Some((
            sk_model::i18n::plural("notice.drive_gone", went as u64, &[&went, &name]),
            Instant::now(),
        ));
    }

    /// The volume the row in question is about, for the three commands.
    ///
    /// **The row a menu was opened over, else the row the pane is standing in.**
    /// The same rule Pin follows, and for the same reason: a command reached from
    /// a menu acts on what the menu was about, and one reached from the palette
    /// has to act on something.
    fn volume_in_question(&self) -> Option<crate::sidebar::Row> {
        let target = self.place_target.clone();
        let volumes =
            || self.sidebar.rows().iter().filter(|row| row.mark == crate::sidebar::Mark::Volume);
        let over = target.and_then(|path| {
            volumes()
                .find(|row| row.path == path || row.device.as_deref() == Some(path.as_path()))
                .cloned()
        });
        over.or_else(|| {
            let here = self.panes.focused().path.clone();
            volumes()
                .filter(|row| !row.unmounted)
                .filter(|row| here.starts_with(&row.path))
                // The deepest match, so a volume inside another filesystem wins
                // over the root it happens to be under.
                .max_by_key(|row| row.path.as_os_str().len())
                .cloned()
        })
    }

    /// Whether there is a volume to mount.
    #[must_use]
    pub(crate) fn can_mount(&self) -> bool {
        self.volume_in_question().is_some_and(|row| row.unmounted && row.device.is_some())
    }

    /// Whether there is a mounted volume to unmount, which the root is not.
    #[must_use]
    pub(crate) fn can_unmount(&self) -> bool {
        self.volume_in_question().is_some_and(|row| {
            !row.unmounted && row.device.is_some() && row.path != std::path::Path::new("/")
        })
    }

    /// Mounts the volume in question and says where it went.
    pub(crate) fn mount_volume(&mut self) {
        let Some(row) = self.volume_in_question().filter(|row| row.unmounted) else { return };
        let Some(device) = row.device else { return };
        self.mount_the_volume(device, row.label, Mounting::AndSaySo);
    }

    /// Unmounts it.
    pub(crate) fn unmount_volume(&mut self) {
        let Some(row) = self.volume_in_question().filter(|row| !row.unmounted) else { return };
        let Some(device) = row.device else { return };
        self.act_on_the_volume(device, row.label, Act::Unmount);
    }

    /// Unmounts it and ejects the drive it is on.
    pub(crate) fn eject_volume(&mut self) {
        let Some(row) = self.volume_in_question() else { return };
        let Some(device) = row.device else { return };
        self.act_on_the_volume(device, row.label, Act::Eject);
    }

    /// Mounts `device` on a job, and navigates there if that is what was meant.
    ///
    /// **On a job, because it is a connection, an authentication and two round
    /// trips** — and the mount itself happens in another process and takes as
    /// long as the filesystem does. Rule 1 has no exemption for a call that is
    /// usually fast.
    fn mount_the_volume(&mut self, device: PathBuf, name: String, then: Mounting) {
        let sender = self.replies.mounted_tx.clone();
        let wake = self.waker.handle();
        self.said_it_is_working(&name);
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let reply = match sk_desktop::udisks::mount(&device) {
                Ok(at) => Mounted {
                    notice: sk_model::i18n::format("notice.mounted", &[&name, &at.display()]),
                    open: (then == Mounting::AndOpen).then_some(at),
                    detail: String::new(),
                },
                Err(why) => Mounted {
                    notice: sk_model::i18n::format(why.label_key(), &[&name]),
                    open: None,
                    detail: why.detail().to_owned(),
                },
            };
            if sender.send(reply).is_ok() {
                wake.wake();
            }
        });
    }

    /// Unmounts or ejects `device` on a job.
    fn act_on_the_volume(&mut self, device: PathBuf, name: String, act: Act) {
        let sender = self.replies.mounted_tx.clone();
        let wake = self.waker.handle();
        self.said_it_is_working(&name);
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let (done, said, refused) = match act {
                Act::Unmount => (
                    sk_desktop::udisks::unmount(&device),
                    "notice.unmounted",
                    "error.unmount_failed",
                ),
                Act::Eject => {
                    (sk_desktop::udisks::eject(&device), "notice.ejected", "error.eject_failed")
                }
            };
            let reply = match done {
                Ok(()) => Mounted {
                    notice: sk_model::i18n::format(said, &[&name]),
                    open: None,
                    detail: String::new(),
                },
                Err(why) => Mounted {
                    // **The refusal's own sentence where it has one that is more
                    // specific than the verb's.** polkit saying no and a busy
                    // filesystem are two different things to do about it.
                    notice: match why {
                        sk_desktop::udisks::Refused::Failed(_) => {
                            sk_model::i18n::format(refused, &[&name])
                        }
                        ref other => sk_model::i18n::format(other.label_key(), &[&name]),
                    },
                    open: None,
                    detail: why.detail().to_owned(),
                },
            };
            if sender.send(reply).is_ok() {
                wake.wake();
            }
        });
    }

    /// Says a disk is being worked on, because a mount can take a second.
    fn said_it_is_working(&mut self, name: &str) {
        self.notice = Some((sk_model::i18n::format("notice.mounting", &[&name]), Instant::now()));
        self.dirty = true;
    }

    /// Takes delivery of what a mount, unmount or eject did.
    fn drain_mounted(&mut self) {
        let Ok(reply) = self.replies.mounted_rx.try_recv() else { return };
        if !reply.detail.is_empty() {
            // Rule 8: the sentence is on screen and the chain is in the log.
            eprintln!("skerry: udisks2: {}", reply.detail);
        }
        self.notice = Some((reply.notice, Instant::now()));
        if let Some(at) = reply.open {
            self.panes.focused_mut().open(at, &self.jobs);
        }
        // The mount table changed, so the sidebar has a new row to draw — and
        // the uevent that says so may not arrive for a mount we asked for
        // ourselves.
        self.sidebar.reload(&self.jobs);
        self.dirty = true;
    }

    /// Opens the settings panel, or closes it if it is already open.
    ///
    /// **[ADR 0027](../../../docs/adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)**
    /// makes this the pointer's way to every setting and the answer to rule 4:
    /// a chord nobody knows is not discoverability, and neither is a config file
    /// nobody has been told about.
    pub(crate) fn toggle_settings(&mut self) {
        self.dirty = true;
        if self.panel.take().is_some() {
            return;
        }
        self.panel = Some(crate::settings::Panel::new());
        self.ask_about_the_handler();
    }

    /// Closes the panel.
    pub(crate) fn close_settings(&mut self) {
        self.panel = None;
        self.dirty = true;
    }

    /// Reads what makes this the desktop's file manager, so the row can say yes
    /// or no.
    ///
    /// **On a job because it is four `openat`s and a `readlink`**, which rule 1
    /// does not allow the frame loop — so the row starts saying nothing rather
    /// than saying *No* and being wrong for a frame.
    fn ask_about_the_handler(&mut self) {
        let sender = self.replies.handler_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
            let is = sk_desktop::mimeapps::is_default(FOLDERS, crate::service::DESKTOP_ID)
                && sk_desktop::activation::is_ours();
            if sender.send((is, None)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Writes both files on a job, and says what it did.
    fn become_the_handler(&mut self, yes: bool) {
        let sender = self.replies.handler_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(self.panes.focused().lane(), sk_util::Cancel::new(), move |_| {
            let (done, said) = handle_folders(yes);
            if sender.send((done && yes, Some(said))).is_ok() {
                wake.wake();
            }
        });
    }

    /// Takes delivery of what the handler job found or did.
    fn drain_handler(&mut self) {
        let Ok((is, said)) = self.replies.handler_rx.try_recv() else { return };
        if let Some(panel) = self.panel.as_mut() {
            panel.handler = Some(is);
            if said.is_some() {
                panel.said = said;
            }
        }
        self.dirty = true;
    }

    /// One key to the settings panel. Whether it was consumed.
    ///
    /// **`←`/`→` and `Space` all change the row**, because a two-word control is
    /// answered by either idiom and a person will try both. `Enter` is the same
    /// as `Space` so the panel needs no separate confirm.
    ///
    /// **The panel keeps the keyboard rather than closing on any key.** Unlike
    /// the Open With list and the Properties panel it is a surface a person
    /// works in — eight rows, each answered by an arrow — so a stray letter must
    /// not throw away what they were in the middle of. `Escape` is the way out,
    /// and so is `Ctrl ,`, which is why that one chord is handed back.
    fn settings_key(&mut self, event: &sk_plat::KeyEvent, commands: &Registry<Self>) -> bool {
        use sk_plat::{Key, Named};
        if named(commands, event) == Some("settings.open") {
            return false;
        }
        let bare = event.mods.none();
        match event.key {
            Key::Named(Named::Escape) => self.close_settings(),
            Key::Named(Named::Down | Named::Tab) if bare => self.step_the_panel(1),
            Key::Named(Named::Up) if bare => self.step_the_panel(-1),
            Key::Named(Named::Left) if bare => self.answer_the_panel(false),
            Key::Named(Named::Right) if bare => self.answer_the_panel(true),
            Key::Named(Named::Enter) | Key::Character(' ') => self.turn_the_panel_row(),
            // The diagnostic report passes through, for the reason it passes
            // through an open menu: it is the only way the harness sees this.
            _ => return named(commands, event) != Some("help.dump"),
        }
        self.dirty = true;
        true
    }

    fn step_the_panel(&mut self, delta: isize) {
        if let Some(panel) = self.panel.as_mut() {
            panel.step(delta);
            panel.said = None;
        }
    }

    /// Puts the row the keyboard is on into a stated position.
    fn answer_the_panel(&mut self, yes: bool) {
        use crate::settings::{Kind, Row};
        let Some(row) = self.panel.as_ref().map(crate::settings::Panel::row) else { return };
        match (row, row.kind()) {
            (Row::Of(setting), Kind::Either { .. }) => self.change(setting, yes),
            (Row::Of(_), Kind::Number) => {
                self.resize_aside(if yes {
                    crate::settings::WIDTH_STEP
                } else {
                    -crate::settings::WIDTH_STEP
                });
            }
            (Row::Handler, _) => self.become_the_handler(yes),
            _ => {}
        }
    }

    /// Does whatever the row the keyboard is on does, which for a two-word
    /// control is the other position.
    fn turn_the_panel_row(&mut self) {
        use crate::settings::{Kind, Row};
        let Some(panel) = self.panel.as_ref() else { return };
        let row = panel.row();
        match row {
            Row::Of(setting) => match setting.kind() {
                Kind::Either { .. } => {
                    let yes = !self.values().yes(setting);
                    self.change(setting, yes);
                }
                _ => self.resize_aside(crate::settings::WIDTH_STEP),
            },
            Row::Handler => {
                let yes = !panel.handler.unwrap_or(false);
                self.become_the_handler(yes);
            }
            Row::Keymap => self.write_the_config(),
        }
    }

    /// A press inside the settings panel. Whether it was taken.
    ///
    /// **Always taken while the panel is up, and a press outside closes it.**
    /// The rule every overlay here follows.
    fn press_in_the_settings(&mut self, position: Point) -> bool {
        if self.panel.is_none() {
            return false;
        }
        let control = self
            .hits
            .settings
            .iter()
            .find(|(_, _, area)| area.contains(position.x, position.y))
            .map(|(row, which, _)| (*row, *which));
        let Some((row, which)) = control else {
            let inside =
                self.hits.panels.last().is_some_and(|a| a.contains(position.x, position.y));
            if !inside {
                self.close_settings();
            }
            return true;
        };
        if let Some(panel) = self.panel.as_mut() {
            panel.active = row;
            panel.said = None;
        }
        match which {
            crate::view::SettingAim::Yes => self.answer_the_panel(true),
            crate::view::SettingAim::No => self.answer_the_panel(false),
            crate::view::SettingAim::Do => self.turn_the_panel_row(),
        }
        true
    }

    /// What the settings panel is showing, for the diagnostic line.
    fn settings_report(&self) -> String {
        use std::fmt::Write as _;
        let Some(panel) = self.panel.as_ref() else { return String::new() };
        let values = self.values();
        let mut out = String::with_capacity(160);
        for (index, row) in crate::settings::rows().into_iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let name = match row {
                crate::settings::Row::Of(setting) => setting.name(),
                crate::settings::Row::Handler => "handler",
                crate::settings::Row::Keymap => "keymap",
            };
            let value = match row {
                crate::settings::Row::Of(setting) => values.word(setting),
                crate::settings::Row::Handler => {
                    panel.handler.map_or_else(|| "?".to_owned(), |it| it.to_string())
                }
                crate::settings::Row::Keymap => "-".to_owned(),
            };
            let from = match row {
                crate::settings::Row::Of(setting) => match self.marks.source(setting) {
                    crate::settings::Source::Default => "default",
                    crate::settings::Source::File => "file",
                    crate::settings::Source::Window => "window",
                },
                _ => "-",
            };
            let _ = write!(out, "{name}={value}/{from}");
        }
        let _ = write!(out, " on={}", panel.active);
        out
    }

    /// A point inside every control the panel drew, for the pointer half of
    /// rule 5.
    ///
    /// `row:which:x:y`, from the rectangles the *frame* recorded — never
    /// recomputed here, which is the rule
    /// [12-measurement.md](../../../docs/12-measurement.md#a-test-that-aims-at-the-hit-box-agrees-with-the-bug)
    /// exists for: a harness aimed at a box this function derived would agree
    /// with a panel drawn somewhere else entirely.
    fn settings_aim(&self) -> String {
        self.hits
            .settings
            .iter()
            .map(|(row, which, area)| {
                let word = match which {
                    crate::view::SettingAim::Yes => "yes",
                    crate::view::SettingAim::No => "no",
                    crate::view::SettingAim::Do => "do",
                };
                format!(
                    "{row}:{word}:{:.0}:{:.0}",
                    area.x + area.width / 2.0,
                    area.y + area.height / 2.0
                )
            })
            .collect::<Vec<_>>()
            .join(",")
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
                let diagnostic = named(commands, event) == Some("help.dump");
                if diagnostic {
                    return false;
                }
                self.openers = None;
            }
        }
        self.dirty = true;
        true
    }

    /// Every piece of the window's own furniture under the pointer.
    ///
    /// Five independent comparisons with one shape, split out of [`Self::moved`]
    /// because that function was two ideas: what the pointer is *over*, and what a
    /// drag in flight is doing about it.
    fn hover_the_chrome(&mut self, position: Point) {
        let button = self.layout.button_at(position.x, position.y);
        if self.hovered_button != button {
            self.hovered_button = button;
            self.dirty = true;
        }
        // **`crumb_at` before `hover_the_path_bar`**, which is the order this had
        // before it moved out of `moved` — the second one is a `&mut self` call
        // and reordering it around a read is a behaviour change, not a tidy-up.
        let crumb = self.crumb_at(position);
        self.hover_the_path_bar(position);
        if self.hovered_crumb != crumb {
            self.hovered_crumb = crumb;
            self.dirty = true;
        }
        let tool = self.tool_at(position);
        let mode = self.mode_at(position);
        if self.hovered_mode != mode {
            self.hovered_mode = mode;
            self.dirty = true;
        }
        if self.hovered_tool != tool {
            self.hovered_tool = tool;
            self.dirty = true;
        }
        let control = self.look_control_at(position);
        if self.hovered_look != control {
            self.hovered_look = control;
            self.dirty = true;
        }
    }

    /// The quick look's control under `position`, if the pointer is on one.
    ///
    /// **Empty where nothing was drawn**, because `Hits::look` is filled by the
    /// drawing: a page arrow over a picture is not merely disabled, it is not
    /// there, and a rectangle left behind from a document would be a button the
    /// pointer could find and the eye could not.
    fn look_control_at(&self, position: sk_plat::Point) -> Option<crate::view::LookControl> {
        self.hits
            .look
            .iter()
            .find(|(_, area)| area.contains(position.x, position.y))
            .map(|(control, _)| *control)
    }

    /// A key while a quick look is open. Whether it was taken.
    ///
    /// `←`/`→` walk to the previous and next file that has a preview; `↑`/`↓` and
    /// `PageUp`/`PageDown` go further through the one on screen — a document's
    /// pages, a text file's lines, and nowhere at all for a picture.
    ///
    /// **Bare chords only.** `Ctrl ←` is a word in a field and `Alt ←` is Back,
    /// and a viewer that took either would be taking a key that means something
    /// else everywhere in the program.
    fn quick_look_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        use sk_plat::{Key, Named};
        // **The find bar takes the keyboard while it is open**, on the same terms
        // the search over a listing does: every letter belongs to the query, and
        // the two chords a person expects — `Enter` for the next match and
        // `Escape` for never mind — are answered here rather than fought over.
        if self.quick_look.as_ref().is_some_and(|look| look.finding.is_some())
            && self.finding_key(event)
        {
            return true;
        }
        // **Editing takes the keyboard, and that is the whole of what edit mode
        // is.** The same panel, the same selection, the same scroll — what
        // changes is that `Space` is a space rather than the key that closes the
        // look, and that the arrows move a caret rather than walking files.
        if self.editing_the_look() && self.editor_key(event) {
            return true;
        }
        if !event.mods.none() {
            return false;
        }
        // **Inside an archive the sideways keys mean in and out.** Everywhere
        // else `←` and `→` walk to the next file with a preview, and they still
        // do at the top of an archive — where there is no level to leave, so the
        // key means what it means everywhere else rather than nothing.
        if self.showing_an_archive() {
            match event.key {
                Key::Named(Named::Right) if self.enter_in_the_archive() => {
                    self.dirty = true;
                    return true;
                }
                Key::Named(Named::Left | Named::Backspace) if self.leave_in_the_archive() => {
                    self.dirty = true;
                    return true;
                }
                // **`Enter` is spent here whether or not it went anywhere.** On a
                // folder it goes in; on a file it does nothing, and the thing it
                // must not do is fall through to Open — which would launch the
                // *archive* in another program while somebody was reading its
                // contents, on the key that means "open this row" everywhere
                // else in the window.
                Key::Named(Named::Enter) => {
                    self.dirty |= self.enter_in_the_archive();
                    return true;
                }
                _ => {}
            }
        }
        match event.key {
            Key::Named(Named::Left) => self.look_at_sibling(-1),
            Key::Named(Named::Right) => self.look_at_sibling(1),
            Key::Named(Named::Up) => self.look_by(-1, false),
            Key::Named(Named::Down) => self.look_by(1, false),
            Key::Named(Named::PageUp) => self.look_by(-1, true),
            Key::Named(Named::PageDown) => self.look_by(1, true),
            _ => return false,
        }
        self.dirty = true;
        true
    }

    /// One key to the find bar. Whether it was taken.
    fn finding_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        use sk_plat::{Key, Named};
        match event.key {
            Key::Named(Named::Escape) => {
                self.toggle_finding();
                return true;
            }
            Key::Named(Named::Enter) => {
                let by = if event.mods.shift { -1 } else { 1 };
                if let Some(look) = self.quick_look.as_mut() {
                    look.step_found(by);
                }
                self.dirty = true;
                return true;
            }
            Key::Named(Named::Down) if event.mods.none() => {
                if let Some(look) = self.quick_look.as_mut() {
                    look.step_found(1);
                }
                self.dirty = true;
                return true;
            }
            Key::Named(Named::Up) if event.mods.none() => {
                if let Some(look) = self.quick_look.as_mut() {
                    look.step_found(-1);
                }
                self.dirty = true;
                return true;
            }
            _ => {}
        }
        // Everything else is typing, which is what a field does with it — and a
        // query that changed starts again from the first match rather than from
        // wherever the last one had walked to.
        let Some(look) = self.quick_look.as_mut() else { return false };
        let Some(field) = look.finding.as_mut() else { return false };
        if !field.key(event) {
            return false;
        }
        look.find_again(0);
        self.dirty = true;
        true
    }

    /// Opens the find bar over a page, or closes it.
    pub(crate) fn toggle_finding(&mut self) {
        if self.quick_look.as_mut().is_some_and(crate::quicklook::QuickLook::toggle_finding) {
            self.dirty = true;
            return;
        }
        self.dirty = true;
    }

    /// Whether a text preview is open and being edited.
    #[must_use]
    pub(crate) fn editing_the_look(&self) -> bool {
        self.quick_look.as_ref().is_some_and(|look| look.editing && look.text.is_some())
    }

    /// One key to the editor. Whether it was taken.
    ///
    /// **Everything that is not a chord.** A character is typed, the two deletes
    /// delete, `Enter` splits the line and the arrows move the caret — with
    /// `Shift` dragging the selection along, which is what `Shift` does in every
    /// text there is. Chords fall through: `Ctrl+C`, `Ctrl+A` and `Ctrl+S` are
    /// commands in [`Scope::Look`] and the registry is where they belong.
    fn editor_key(&mut self, event: &sk_plat::KeyEvent) -> bool {
        use crate::quicklook::Step;
        use sk_plat::{Key, Named};
        if event.mods.alt || event.mods.logo {
            return false;
        }
        let shift = event.mods.shift;
        // **`Ctrl` moves by word, and nothing else.** `Ctrl+A`, `Ctrl+C`,
        // `Ctrl+E` and `Ctrl+S` are commands in [`Scope::Look`] and are resolved
        // before this is reached; what is left is the movement half of what every
        // text field in this program does with `Ctrl` — with `Shift` selecting as
        // it goes, which is the thing that could not be done here at all.
        if event.mods.ctrl {
            let step = match event.key {
                Key::Named(Named::Left) => Step::WordBack,
                Key::Named(Named::Right) => Step::WordOn,
                Key::Named(Named::Home) => Step::Start,
                Key::Named(Named::End) => Step::End,
                Key::Named(Named::Backspace) => return self.delete_a_word(false),
                Key::Named(Named::Delete) => return self.delete_a_word(true),
                _ => return false,
            };
            let Some(look) = self.quick_look.as_mut() else { return false };
            let taken = look.move_caret(step, shift);
            if taken {
                self.reveal_the_caret();
                self.dirty = true;
            }
            return taken;
        }
        let Some(look) = self.quick_look.as_mut() else { return false };
        let taken = match &event.key {
            Key::Character(text) => look.type_in(&text.to_string()),
            Key::Named(Named::Enter) => look.type_in("\n"),
            Key::Named(Named::Tab) => look.type_in("\t"),
            Key::Named(Named::Backspace) => look.delete_back(),
            Key::Named(Named::Delete) => look.delete_forward(),
            Key::Named(Named::Left) => look.move_caret(Step::Back, shift),
            Key::Named(Named::Right) => look.move_caret(Step::On, shift),
            Key::Named(Named::Up) => look.move_caret(Step::Up, shift),
            Key::Named(Named::Down) => look.move_caret(Step::Down, shift),
            Key::Named(Named::Home) => look.move_caret(Step::LineStart, shift),
            Key::Named(Named::End) => look.move_caret(Step::LineEnd, shift),
            // **`Escape` throws the edit away**, which is what `Escape` means
            // everywhere in this program: a menu closes without choosing, a
            // question stops, a rename field abandons the name. Every *other* way
            // out saves — the Done button, `Ctrl+E`, `Ctrl+S`, `Space`, a click on
            // the list — so this is the one answer to "actually, never mind", and
            // it says so in the status line because a discard is the only thing
            // here that `Ctrl+Z` cannot walk back: nothing was written.
            //
            // With nothing to discard it falls through, and `Escape` closes the
            // panel as it always has.
            Key::Named(Named::Escape) => {
                if !self.discard_the_edit() {
                    return false;
                }
                self.stop_editing_the_look();
                return true;
            }
            _ => false,
        };
        if taken {
            self.reveal_the_caret();
            self.dirty = true;
        }
        taken
    }

    /// Throws away an edit that has not been written. Whether there was one.
    ///
    /// **Not on the undo stack, because it never reached the filesystem.** Rule 3
    /// is about mutations, and a buffer that was never saved is not one — which is
    /// exactly why this says so out loud rather than doing it quietly.
    pub(crate) fn discard_the_edit(&mut self) -> bool {
        let Some(look) = self.quick_look.as_mut() else { return false };
        if !look.discard() {
            return false;
        }
        self.notice =
            Some((sk_model::i18n::lookup("notice.edit_discarded").to_owned(), Instant::now()));
        self.dirty = true;
        true
    }

    /// Throws the edit away and leaves edit mode — the command, and the button.
    pub(crate) fn discard_the_look(&mut self) {
        if self.discard_the_edit() {
            self.stop_editing_the_look();
        }
    }

    /// `Ctrl+Backspace` and `Ctrl+Delete`: the word beside the caret.
    fn delete_a_word(&mut self, forward: bool) -> bool {
        use crate::quicklook::Step;
        let Some(look) = self.quick_look.as_mut() else { return false };
        if !look.editing {
            return false;
        }
        // Selected first and then deleted, which is what makes this the same two
        // functions a `Shift+Ctrl+←` and a `Backspace` already are.
        if !look.has_selection() {
            look.move_caret(if forward { Step::WordOn } else { Step::WordBack }, true);
        }
        let taken = if forward { look.delete_forward() } else { look.delete_back() };
        if taken {
            self.reveal_the_caret();
            self.dirty = true;
        }
        taken
    }

    /// Turns edit mode on, or off and saves.
    pub(crate) fn toggle_editing_the_look(&mut self) {
        if self.editing_the_look() {
            self.stop_editing_the_look();
            return;
        }
        let Some(look) = self.quick_look.as_mut().filter(|look| look.text.is_some()) else {
            return;
        };
        look.start_editing();
        self.dirty = true;
    }

    /// Leaves edit mode, writing what was typed.
    ///
    /// **Saved rather than asked about.** A panel that asked "save?" would be a
    /// question whose every answer can lose work, and the undo stack is what
    /// makes the other choice safe: the write is one entry, so `Ctrl+Z` takes
    /// back a save nobody meant — which is what `Change::Edited` carries both
    /// sides for.
    pub(crate) fn stop_editing_the_look(&mut self) {
        self.save_the_look();
        if let Some(look) = self.quick_look.as_mut() {
            look.editing = false;
            look.rendered = std::mem::take(&mut look.was_rendered);
            // **Re-parsed, because the document is not the one that was parsed.**
            // Coming back to the rendering after an edit and seeing the text from
            // before it is the same defect as a listing that goes stale.
            if look.rendered {
                let body = look.text.as_ref().map(crate::quicklook::Text::body);
                look.blocks = body.map(|it| crate::markdown::parse(&it)).unwrap_or_default();
                look.laid = None;
            }
        }
        self.dirty = true;
    }

    /// Closes the panel, writing anything the editor is holding.
    ///
    /// **One closer, because there are three ways out** — `Space`, `Escape` and a
    /// click on the list behind it — and an edit that survived two of them and
    /// not the third would be work lost by whichever way somebody happened to
    /// use. Whether one was open.
    pub(crate) fn close_the_look(&mut self) -> bool {
        self.save_the_look();
        self.quick_look.take().is_some()
    }

    /// Whether the panel is holding an edit that is not on disk.
    #[must_use]
    pub(crate) fn look_is_unsaved(&self) -> bool {
        self.quick_look.as_ref().is_some_and(|look| look.unsaved)
    }

    /// Writes what the editor holds, if it differs from the file.
    pub(crate) fn save_the_look(&mut self) {
        let Some(look) = self.quick_look.as_mut().filter(|look| look.unsaved) else { return };
        let Some(text) = look.text.as_ref() else { return };
        let (path, body) = (look.path.clone(), text.body());
        // Remembered as the file's own text, so a Discard after two saves puts
        // back what was last written rather than what was first opened.
        look.saved_as(body.clone());
        self.edits.write_text(path, body, &self.jobs);
        self.dirty = true;
    }

    /// Cuts the open text preview to the panel's width.
    ///
    /// **Once a frame, and it does nothing when the width has not changed.** The
    /// rows *are* the layout — where a line breaks decides what a click means and
    /// what `Down` does — so they cannot be a per-frame pass over the file, and
    /// they cannot be stale either. This is the one place that knows both the
    /// panel and the font, which is why it is here rather than in the drawing.
    fn fit_the_look(&mut self) {
        if self.quick_look.is_none() {
            return;
        }
        let inner = crate::view::quick_look_area(&self.layout);
        let metrics = self.theme.metrics;
        let scale = self.window.scale();
        let Self { quick_look, cache, fonts, .. } = self;
        let Some(look) = quick_look.as_mut() else { return };
        let Some(text) = look.text.as_mut() else { return };
        let cells =
            crate::view::text_cells(text.lines.len(), (inner, &metrics), (cache, fonts, scale));
        if cells != text.cells() {
            text.wrap(cells);
            self.dirty = true;
        }
    }

    /// Keeps a zoomed picture inside the panel as the panel changes size.
    ///
    /// **Beside the wrap, and for the same reason.** A window resized while a
    /// picture is zoomed and panned to a corner leaves a pan that was clamped
    /// against the old panel — which draws the picture with a margin of window
    /// inside a panel it is bigger than.
    fn fit_the_zoom(&mut self) {
        if self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::zoomed) {
            self.settle_the_zoom();
        }
    }

    /// Scrolls the caret's line into view.
    fn reveal_the_caret(&mut self) {
        let inner = crate::view::quick_look_area(&self.layout);
        let step = self.theme.metrics.row_height;
        let Some(look) = self.quick_look.as_mut() else { return };
        // **The drawn row, not the line.** A caret six rows down a wrapped
        // paragraph is six rows down the panel, and revealing the line would
        // scroll to the paragraph's first row and leave the caret off screen.
        let at = look.caret();
        let row = look.text.as_ref().map_or(0, |text| text.row_of(at)) as f32;
        // The same `reveal` a cursor uses, for the same reason: the least scroll
        // that brings the line into view, rather than re-centring under it.
        look.scroll.reveal(row * step, (row + 1.0) * step, inner.height.max(step));
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
                let diagnostic = named(commands, event) == Some("help.dump");
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
        // Every answer that has landed, because two landing between two turns
        // are one wake: a menu opened over one file and then another leaves
        // the first's answer ahead of the second's in the channel.
        while let Ok((asker, found)) = self.replies.openers_rx.try_recv() {
            match asker {
                // Into the menu's Open with row, if the menu the job was
                // started for is still up and still about that file; otherwise
                // nobody is waiting for it.
                crate::openers::Asker::Menu => {
                    if self.menu.as_mut().is_some_and(|menu| menu.answer(found)) {
                        self.dirty = true;
                    }
                }
                crate::openers::Asker::Panel if found.rows.is_empty() => {
                    self.notice = Some((
                        sk_model::i18n::format("error.no_handler", &[&found.mime]),
                        Instant::now(),
                    ));
                    self.dirty = true;
                }
                crate::openers::Asker::Panel => {
                    self.openers = Some(crate::openers::Openers::new(found));
                    self.dirty = true;
                }
            }
        }
    }

    /// Launches the highlighted application and closes the list.
    fn follow_openers(&mut self) {
        let Some(list) = self.openers.take() else { return };
        self.dirty = true;
        let Some(entry) = list.chosen().cloned() else { return };
        self.start_application(entry, list.path.clone());
    }

    /// Hands `path` to `entry` on a job, and says so in the status line once it
    /// has. From the Open with panel and from the menu's Open with submenu alike.
    ///
    /// A job because `spawn` is `fork` plus `exec`, which with a `pre_exec` set
    /// blocks the parent on the child's exec-status pipe — `launch`'s reason,
    /// and the same rule.
    fn start_application(&mut self, entry: sk_desktop::DesktopEntry, path: PathBuf) {
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
search_active=\"\" search_scope=none search_roots=0 search_scanned=0 scope_aim=\"\""
                .to_owned();
        };
        let active = search
            .chosen()
            .map(|hit| crate::panes::title_of(&hit.path).to_owned())
            .unwrap_or_default();
        let scope = if search.scope.is_everywhere() { "everywhere" } else { "here" };
        // One point per switcher button, named rather than positional — the same
        // `id@x:y` the menu reports, and for the same reason: a harness that
        // indexed into a list would be encoding which button is drawn first.
        let scope_aim = self
            .hits
            .search_scope
            .iter()
            .map(|(everywhere, area)| {
                format!(
                    "{}@{:.0}:{:.0}",
                    if *everywhere { "everywhere" } else { "here" },
                    area.x + area.width / 2.0,
                    area.y + area.height / 2.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "searching=true search_query={:?} search_hits={} search_done={} \
search_active={active:?} search_scope={scope} search_roots={} search_scanned={} \
scope_aim={scope_aim:?}",
            search.query.text,
            search.found.hits.len(),
            search.found.complete,
            self.search_roots().len(),
            search.found.scanned,
        )
    }

    /// Opens the search over everything under the folder being shown.
    pub(crate) fn open_search(&mut self) {
        let here = crate::search::Scope::Here(self.panes.focused().path.clone());
        self.search_in(here);
    }

    /// Opens it over every local filesystem.
    pub(crate) fn search_everywhere(&mut self) {
        self.search_in(crate::search::Scope::Everywhere);
    }

    /// Opens the search in `scope`, switches an open one to it, or closes one
    /// that is already in it.
    ///
    /// **The chord for the scope you are in closes; the chord for the other one
    /// switches**, and switching keeps what has been typed — which is
    /// [ADR 0022](../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md):
    /// widening a search that has found nothing is the commonest thing a person
    /// does next, and retyping the query to do it is the cost of not having this.
    fn search_in(&mut self, scope: crate::search::Scope) {
        match self.search.as_mut() {
            Some(search) if search.scope == scope => {
                self.close_search();
                return;
            }
            Some(search) => {
                search.scope = scope;
                // The query stands and the answer does not: a stale list under a
                // new scope is the old answer with a new label.
                search.asked = String::new();
            }
            None => {
                self.claim_keyboard(Claim::Search);
                self.search = Some(crate::search::Search::new(scope));
            }
        }
        // Asked for here rather than on the first keystroke, so the read is over
        // by the time anybody has typed enough to search for — 413.6 ms for a
        // whole filesystem's index against the second or so it takes to type
        // three characters.
        for root in self.search_roots() {
            self.want_index(&root);
        }
        self.search_again();
        self.dirty = true;
    }

    /// The roots the open search covers.
    ///
    /// **From the sidebar rather than from `/proc/self/mountinfo`**, because the
    /// mount table is a file and rule 1 has no exemption for a small one — and
    /// the sidebar has already read it, on a job, with the same allow-list of
    /// filesystem types. A remote mount is excluded: reaching one costs its
    /// timeout, which [Spike B](../../spikes/b-enumeration/README.md) measured at
    /// 28,000 ms for one unresponsive mount. `/` alone where the sidebar has
    /// nothing yet, because a search with no roots finds nothing and says nothing
    /// about why.
    fn search_roots(&self) -> Vec<std::path::PathBuf> {
        let Some(search) = self.search.as_ref() else { return Vec::new() };
        match &search.scope {
            crate::search::Scope::Here(path) => vec![path.clone()],
            crate::search::Scope::Everywhere => {
                let roots = self.sidebar.volumes();
                if roots.is_empty() { vec![std::path::PathBuf::from("/")] } else { roots }
            }
        }
    }

    /// Closes it and stops the walk.
    pub(crate) fn close_search(&mut self) {
        if let Some(search) = self.search.take() {
            search.cancel.cancel();
        }
        // **The index's bytes go with the panel.** A whole filesystem's index is
        // 326 MB on this machine, 49.6 bytes an entry, and holding it after the
        // search that wanted it has closed would make that a floor rather than a
        // peak — ADR 0022, which is ADR 0020's rule applied to the other store.
        // What is kept is the knowledge that the file exists, which is what
        // *Forget the index* is enabled by and costs nothing.
        self.indexes.drop_the_bytes();
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
        let sender = self.replies.found_tx.clone();
        let wake = self.waker.handle();
        let lane = self.panes.focused().lane();
        // **The index where there is one, the walk where there is not, and one
        // list either way.** ADR 0008's third constraint is that a missing index
        // is a performance event and never a correctness one, so the two arms
        // answer the same question and the caller cannot tell which ran — and ADR
        // 0022 makes that per root, because a scope of several disks may have an
        // index for one of them.
        let roots = self.search_roots();
        let scans: Vec<std::sync::Arc<sk_index::Index>> =
            roots.iter().filter_map(|root| self.indexes.covering(root).cloned()).collect();
        let walks: Vec<std::path::PathBuf> =
            roots.iter().filter(|root| self.indexes.covering(root).is_none()).cloned().collect();
        let pool = std::sync::Arc::clone(&self.jobs.pool);
        // Recorded now rather than on the reply, because the summary has to say
        // which of the two answered and a `Found` cannot: the whole point of ADR
        // 0008's third constraint is that the two are indistinguishable. The
        // oldest build, and only where nothing had to be walked.
        let built = scans.iter().map(|index| index.built()).min();
        if let Some(search) = self.search.as_mut() {
            search.indexed = built.filter(|_| walks.is_empty());
        }
        self.jobs.pool.spawn(lane, cancel, move |cancel| {
            let mut found = sk_index::Found::default();
            let mut say = |found: &sk_index::Found| {
                if sender.send((query.clone(), found.clone())).is_ok() {
                    wake.wake();
                }
            };
            // The indexed roots first, because tens of milliseconds of answer
            // belongs on screen before seconds of walking start.
            for index in &scans {
                sk_index::store::search_into(index, &query, &pool, cancel, &mut found);
                say(&found);
            }
            if walks.is_empty() {
                found.complete = !cancel.is_cancelled();
                say(&found);
            } else {
                sk_index::find::across(&walks, &query, cancel, &mut found, &mut say);
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
        self.start_a_build(root);
    }

    /// Builds an index for every local filesystem, one after another.
    ///
    /// **The sidebar's own Volumes, which is what "everywhere" already means** —
    /// [ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)
    /// settled that for a search and this is the same set: what the sidebar calls
    /// a place is what a root is, and a remote mount is excluded because walking
    /// one costs its timeout.
    ///
    /// It was two steps — stand in Filesystem, then Build the index — and two
    /// steps is a feature nobody finds for the machine's *other* disks at all.
    pub(crate) fn build_every_index(&mut self) {
        let mut roots = self.sidebar.volumes();
        if roots.is_empty() {
            return;
        }
        let first = roots.remove(0);
        self.indexes.owe(roots);
        self.start_a_build(first);
    }

    /// Whether the sidebar found any local filesystem to index.
    #[must_use]
    pub(crate) fn has_volumes(&self) -> bool {
        !self.sidebar.volumes().is_empty()
    }

    /// How many roots a queued build has left to do, including the one running.
    #[must_use]
    pub(crate) fn indexes_owed(&self) -> usize {
        self.indexes.owed().len()
    }

    /// Starts one build, with its counting pass beside it.
    fn start_a_build(&mut self, root: PathBuf) {
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

    /// The registry, with whatever the config file said applied to it.
    ///
    /// **Built rather than held**, which is the shape it already had: a command's
    /// `run` is a plain function pointer, so there is nothing to keep warm, and a
    /// registry that is not part of `&mut self` is what lets a command take one.
    /// What is new is the keymap, which is applied here so that every registry
    /// the window builds — the loop's and the palette's — is the same keymap.
    fn commands(&self) -> Registry<Self> {
        let mut registry = crate::commands::registry();
        self.keymap.apply(&mut registry);
        registry
    }

    /// Writes the keymap out, and says where it went or why it did not.
    pub(crate) fn write_the_config(&mut self) {
        let said = match crate::config::write(&self.commands(), &self.values()) {
            Ok(path) => sk_model::i18n::format("notice.config_written", &[&path.display()]),
            Err(why) => why,
        };
        self.notice = Some((said, Instant::now()));
        self.dirty = true;
    }

    /// Whether an index build is running.
    #[must_use]
    pub(crate) fn indexing(&self) -> bool {
        self.indexes.building().is_some()
    }

    /// Whether there is an index for the folder being shown.
    ///
    /// **Known of, not loaded.** The bytes go when the search panel closes — ADR
    /// 0022 — and *Forget the index* has to stay reachable after that, or the only
    /// way to delete a 326 MB file would be to open a search first.
    #[must_use]
    pub(crate) fn indexed(&self) -> bool {
        self.indexes.knows_of(&self.panes.focused().path)
    }

    /// Takes delivery of index reports.
    fn drain_index(&mut self) {
        while let Ok(note) = self.replies.index_rx.try_recv() {
            if self.indexes.arrived(note) {
                self.dirty = true;
            }
        }
        // **The next root, when one is owed and nothing is running.** This is
        // where a queued build steps: the reply that ends one is the event that
        // starts the next, so there is no timer and no second scheduler — the
        // same shape the preview's page-ahead uses.
        if !self.indexing()
            && let Some(next) = self.indexes.next_owed()
        {
            self.start_a_build(next);
            self.dirty = true;
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
        let mime = self.desktop.clone();
        self.panes.focused_mut().show_results(query, listing, mime.as_deref().map(|it| &*it.mime));
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
            self.panes.focused_mut().select_after(name.clone());
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
                let diagnostic = named(commands, event) == Some("help.dump");
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
    fn press_in_the_search(&mut self, position: Point, clicks: u8) -> bool {
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
        // **Before the field**, because the switcher sits inside the field's row:
        // without this every press on it also placed a caret.
        let switched = self
            .hits
            .search_scope
            .iter()
            .find(|(_, area)| area.contains(position.x, position.y))
            .map(|(everywhere, _)| *everywhere);
        if let Some(everywhere) = switched {
            let scope = if everywhere {
                crate::search::Scope::Everywhere
            } else {
                crate::search::Scope::Here(self.panes.focused().path.clone())
            };
            // Through `search_in`, which is what the two chords run: a second way
            // of switching must not be a second piece of code to keep in step.
            if self.search.as_ref().is_some_and(|search| search.scope != scope) {
                self.search_in(scope);
            }
            return true;
        }
        if let Some(area) = self.hits.search_field.filter(|a| a.contains(position.x, position.y)) {
            // The same field the palette draws, so the same two numbers measure
            // it — a caret placed against a different inset than the one the text
            // was drawn at lands one character out at the right-hand end.
            let size = self.theme.metrics.path_size;
            let at =
                self.place_caret_in(Claim::Search, area, size, crate::palette::INSET, position.x);
            self.chose_a_run(Claim::Search, clicks, at);
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

    /// Opens a quick look at the cursor's file, or closes the one that is open.
    ///
    /// **The same key both ways**, which is what every quick look on every desktop
    /// does — and it is why this is one command rather than two: a person presses
    /// `Space` to see a file and presses it again when they have seen it.
    pub(crate) fn toggle_quick_look(&mut self) {
        if self.close_the_look() {
            self.dirty = true;
            return;
        }
        let Some(row) = self.panes.focused().cursor_row() else { return };
        self.look_at_row(row);
    }

    /// What a row can be shown as, if anything.
    fn shows_at(&self, row: usize) -> Option<crate::previews::Shows> {
        let desktop = self.desktop.as_ref()?;
        let pane = self.panes.focused();
        let id = pane.listing.at(row)?;
        let kind = sk_desktop::kind_of(&pane.listing, id, Some(&desktop.mime));
        crate::previews::shows(pane.listing.name(id), kind, Some(&desktop.mime))
    }

    /// Opens a look at one row. Whether there was anything to open.
    ///
    /// **Nothing for a file this program cannot draw**, and no message either: a
    /// quick look at a spreadsheet saying "no preview" is a panel whose only
    /// content is an apology, and the file is still there in the list behind it.
    fn look_at_row(&mut self, row: usize) -> bool {
        let Some(shows) = self.shows_at(row) else { return false };
        let pane = self.panes.focused();
        let Some(id) = pane.listing.at(row) else { return false };
        let (path, mtime) = (pane.path_of(id), pane.listing.mtime(id));
        let mut look = crate::quicklook::QuickLook::open(path.clone(), mtime, shows, &self.jobs);
        // **Where this document was left.** Reopening a four-hundred-page manual
        // at page one is losing somebody's place, and the page is as much a part
        // of how the window was left as which folder it is in.
        if let Some(page) = self.page_left_at(&path) {
            look.page = page;
        }
        // **Whatever the atlas already has, drawn while the real one is fetched.**
        // The row was on screen, so its thumbnail is on the GPU; the only thing
        // this costs is the lookup, and it is the difference between a panel that
        // opens and one that appears sixty milliseconds later.
        if shows == crate::previews::Shows::Picture {
            let (_, image) = self.thumbs.name(&path, mtime);
            if let Some(size) = self.renderer.image_size(image) {
                look.standby = Some((image, size));
            }
        }
        self.quick_look = Some(look);
        self.dirty = true;
        true
    }

    /// Lays the rendered markdown out, if it is showing and the width changed.
    ///
    /// **Once per width, never per frame** — [`crate::mdlayout`]. Measuring a
    /// thousand words costs more than a frame has, and a panel that is not being
    /// resized asks the same question every time.
    fn lay_the_markdown_out(&mut self) {
        let inner = crate::view::quick_look_area(&self.layout);
        let size = self.theme.metrics.text_size;
        let scale = self.window.scale();
        let room = (inner.width - crate::view::MARKDOWN_PAD * 2.0).max(size * 4.0);
        let Some(look) = self.quick_look.as_ref() else { return };
        if !look.rendered || look.blocks.is_empty() {
            return;
        }
        if look.laid.as_ref().is_some_and(|laid| laid.fits(room, size, scale)) {
            return;
        }
        let Self { quick_look, cache, fonts, .. } = self;
        let Some(look) = quick_look.as_mut() else { return };
        let mut shaper = Shaper { cache, fonts, scale };
        let laid = crate::mdlayout::lay_out(&look.blocks, room, size, scale, &mut shaper);
        look.scroll.set_extent(laid.height, inner.height);
        look.laid = Some(laid);
        self.dirty = true;
    }

    /// Which page a document was last left on, if this one was.
    fn page_left_at(&self, path: &std::path::Path) -> Option<u32> {
        self.pages_left_at.iter().find(|(kept, _)| kept == path).map(|(_, page)| *page)
    }

    /// Records the page a document is on, most recent first.
    ///
    /// **Called as the page turns rather than as the panel closes.** A panel can
    /// go away with the process — `kill -9`, which rule 6 is about — and a
    /// position written only on the way out is a position lost exactly then.
    fn remember_the_page(&mut self) {
        let Some(look) = self.quick_look.as_ref().filter(|look| look.paged()) else { return };
        let (path, page) = (look.path.clone(), look.page);
        if self.page_left_at(&path) == Some(page) {
            return;
        }
        self.pages_left_at.retain(|(kept, _)| *kept != path);
        self.pages_left_at.insert(0, (path, page));
        self.pages_left_at.truncate(crate::session::MOST_PAGES);
    }

    /// Moves the look to the next file in `delta`'s direction that has one.
    ///
    /// **It walks past what it cannot draw** rather than closing on it. `→` over
    /// a folder of photographs with a `.zip` in the middle would otherwise put the
    /// panel away halfway through, which is not what the key means.
    ///
    /// **And it takes the cursor with it**, so the list behind the panel is
    /// standing where the panel is looking — and closing the look leaves the
    /// selection on the file that was last on screen rather than where it started.
    pub(crate) fn look_at_sibling(&mut self, delta: isize) {
        if self.quick_look.is_none() {
            return;
        }
        // Walking to the next file replaces the buffer, so what is in this one
        // goes to disk first — the same rule closing the panel follows.
        self.save_the_look();
        let rows = self.panes.focused().listing.rows();
        let Some(from) = self.panes.focused().cursor_row() else { return };
        let mut row = from;
        loop {
            let Ok(next) = isize::try_from(row).map(|it| it + delta) else { return };
            let Ok(next) = usize::try_from(next) else { return };
            if next >= rows {
                return;
            }
            row = next;
            if self.shows_at(row).is_some() {
                break;
            }
        }
        self.panes.focused_mut().cursor_to(row, false);
        self.reveal_cursor();
        self.look_at_row(row);
    }

    /// `↑`/`↓` and `PageUp`/`PageDown` inside the look.
    ///
    /// **What they mean depends on what is being looked at**, and that is the
    /// point rather than an inconsistency: a document's pages and a text file's
    /// lines are both "further through this file", and a picture has no further to
    /// go — where the key is swallowed rather than passed on, because an arrow
    /// that moved a cursor nobody can see would be worse than one that does
    /// nothing.
    pub(crate) fn look_by(&mut self, delta: i32, whole: bool) {
        let room = crate::view::quick_look_area(&self.layout).height;
        let step = self.theme.metrics.row_height;
        let Some(look) = self.quick_look.as_mut() else { return };
        match look.shows {
            crate::previews::Shows::Page => {
                look.turn(delta);
            }
            crate::previews::Shows::Text => {
                let rows = look.text.as_ref().map_or(0, |it| it.rows().len());
                look.scroll.set_extent(rows as f32 * step, room);
                // `to` rather than `by`: a keystroke goes where it was sent and
                // does not carry on afterwards, which is what `Scroll::to` is for
                // and what `by` — whose job is a finger's velocity — is not.
                let by = if whole { room } else { step } * delta as f32;
                look.scroll.to(look.scroll.offset() + by);
            }
            crate::previews::Shows::Picture => {
                // **Stepping stops the playback.** Somebody pressing an arrow has
                // said which frame they want to look at, and an animation that
                // carried on would take it away from them a tenth of a second
                // later. The Play button is how it starts again.
                if look.animated() {
                    look.playing = false;
                    look.turn(delta);
                }
            }
            // **The rows, which is what "further through this" means here.** A
            // whole-page step is a panel's worth of rows, the same arithmetic the
            // text preview does with its own line height.
            crate::previews::Shows::Archive => {
                let rows = if whole { (room / step) as i32 } else { 1 };
                if let Some(inside) = look.inside.as_mut() {
                    inside.step((delta * rows.max(1)) as isize);
                    inside.reveal(room, step);
                }
            }
        }
    }

    /// A press on a row of an archive's listing: one click chooses, two go in.
    ///
    /// **The same rule the pane's own rows follow**, so the panel is a list a
    /// person already knows how to use rather than a viewer with a cursor in it.
    /// Aimed at the rectangles the frame recorded, because the list scrolls.
    fn press_in_the_archive(&mut self, position: Point, clicks: u8) -> bool {
        let found = self
            .hits
            .inside
            .iter()
            .find(|(_, at)| at.contains(position.x, position.y))
            .map(|(index, _)| *index);
        let Some(index) = found else { return false };
        let Some(archive) = self.quick_look.as_mut().and_then(|look| look.inside.as_mut()) else {
            return false;
        };
        archive.chosen = index;
        if clicks >= 2 {
            archive.enter();
        }
        true
    }

    /// Whether the panel is showing what is inside an archive.
    pub(crate) fn showing_an_archive(&self) -> bool {
        self.quick_look.as_ref().is_some_and(|look| look.inside.is_some())
    }

    /// Goes into the folder the archive's cursor is on. Whether it did.
    fn enter_in_the_archive(&mut self) -> bool {
        self.quick_look
            .as_mut()
            .and_then(|look| look.inside.as_mut())
            .is_some_and(crate::archive::Inside::enter)
    }

    /// Comes back out of one. Whether there was one to leave.
    fn leave_in_the_archive(&mut self) -> bool {
        self.quick_look
            .as_mut()
            .and_then(|look| look.inside.as_mut())
            .is_some_and(crate::archive::Inside::leave)
    }

    /// Unpacks everything selected, each into a folder of its own beside it.
    ///
    /// **Not into the folder itself** — [ADR
    /// 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md).
    /// An archive whose entries are not under one directory would otherwise
    /// scatter forty names across the folder somebody was looking at, and one
    /// gesture would be forty things to put back.
    pub(crate) fn extract_selection(&mut self) {
        // **One entry, where one is being looked at.** The panel is a list
        // somebody is reading, and Extract while a row is chosen means that row —
        // the same command, because it is the same action on a smaller target,
        // and a second registry entry for it would put "Extract this entry" in
        // the palette where it would mean nothing.
        if let Some(entry) = self.entry_being_looked_at() {
            let (archive, inside) = entry;
            let took = self.edits.unpack_one(archive, inside, &self.jobs);
            self.said_it_is_busy(took);
            return;
        }
        let paths = self.archives_selected();
        if paths.is_empty() {
            return;
        }
        let took = self.edits.unpack_all(paths, &self.jobs);
        self.said_it_is_busy(took);
    }

    /// The archive being looked at and the entry its cursor is on, if both.
    fn entry_being_looked_at(&self) -> Option<(std::path::PathBuf, String)> {
        let look = self.quick_look.as_ref()?;
        let row = look.inside.as_ref()?.row()?;
        Some((look.path.clone(), row.path.clone()))
    }

    /// The selected rows that are archives, in the order they are shown.
    ///
    /// **Filtered rather than refused.** A selection of nine files with two zips
    /// in it means the two zips, the way Copy means every row and not "these are
    /// not all the same kind".
    /// Whether anything selected is an archive.
    ///
    /// **Short-circuiting and allocation-free, because this is an `enabled`
    /// predicate**: `settle_menu_state` runs one for every row of an open menu on
    /// every frame, and the sibling below would be a `PathBuf` per selected entry
    /// per frame — over a `Ctrl+A` in a folder of two hundred thousand, that is
    /// the frame budget several times over. The name is enough to decide, and the
    /// listing already holds it.
    pub(crate) fn an_archive_is_selected(&self) -> bool {
        let Some(desktop) = self.desktop.as_ref() else { return false };
        let pane = self.panes.focused();
        let listing = &pane.listing;
        let archive = |id| {
            crate::previews::shows(
                listing.name(id),
                sk_desktop::Category::Unknown,
                Some(&desktop.mime),
            ) == Some(crate::previews::Shows::Archive)
        };
        if pane.selection.is_empty() {
            return pane.focused().is_some_and(archive);
        }
        pane.selection.in_order(listing.order()).any(archive)
    }

    pub(crate) fn archives_selected(&self) -> Vec<std::path::PathBuf> {
        let Some(desktop) = self.desktop.as_ref() else { return Vec::new() };
        self.selected_paths()
            .into_iter()
            .filter(|path| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                // `Unknown` because the category is only the fallback for a
                // name the database cannot place, and an archive it cannot place
                // is not one this could unpack either.
                let kind = sk_desktop::Category::Unknown;
                crate::previews::shows(&name, kind, Some(&desktop.mime))
                    == Some(crate::previews::Shows::Archive)
            })
            .collect()
    }

    /// Zooms the open picture about the pointer. Whether there was one.
    ///
    /// **The pointer where it is over the panel, and the centre otherwise.** A
    /// pinch has a centre of its own that the protocol reports and this does not
    /// carry, so the pointer is the best answer available — and on a touchpad the
    /// pointer is where the hand was last, which is what a person is looking at.
    pub(crate) fn zoom_the_look(&mut self, factor: f32) -> bool {
        let inner = crate::view::quick_look_area(&self.layout);
        let middle = (inner.x + inner.width / 2.0, inner.y + inner.height / 2.0);
        let holding = if inner.contains(self.pointer.x, self.pointer.y) {
            (self.pointer.x - middle.0, self.pointer.y - middle.1)
        } else {
            (0.0, 0.0)
        };
        let Some(look) = self.quick_look.as_mut() else { return false };
        if !look.zoomable() || !look.zoom_by(factor, holding) {
            return false;
        }
        self.settle_the_zoom();
        self.dirty = true;
        true
    }

    /// Puts the open picture back to the panel's fit. Whether there was one.
    pub(crate) fn unzoom_the_look(&mut self) -> bool {
        let taken = self.quick_look.as_mut().is_some_and(crate::quicklook::QuickLook::unzoom);
        self.dirty |= taken;
        taken
    }

    /// Keeps a zoomed picture from being dragged off the panel.
    ///
    /// **Clamped against what is drawn**, which is why this is here and not in
    /// the panel: the drawn rectangle is the panel, the image's own size and the
    /// zoom together, and only the window knows the first of those.
    fn settle_the_zoom(&mut self) {
        let inner = crate::view::quick_look_area(&self.layout);
        let Some(look) = self.quick_look.as_mut() else { return };
        let Some((_, size)) = look.image else { return };
        look.pan = crate::view::clamped_pan(inner, size, look.drafted, (look.zoom, look.pan));
    }

    /// Whether the panel is showing something with frames that play.
    #[must_use]
    pub(crate) fn looking_at_an_animation(&self) -> bool {
        self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::animated)
    }

    /// Plays the animation, or stops it where it is.
    pub(crate) fn toggle_playing(&mut self) {
        if let Some(look) = self.quick_look.as_mut().filter(|look| look.animated()) {
            look.playing = !look.playing;
            self.dirty = true;
        }
    }

    /// The box a quick look's picture is rendered into, in device pixels.
    ///
    /// **Device, because that is what a renderer is asked for.** Everything above
    /// the `Painter` is logical and this is the one number that crosses on purpose:
    /// a page rendered at logical pixels on a 200% screen is a page at half the
    /// resolution of the screen showing it.
    fn quick_look_box(&self) -> (u32, u32) {
        let inner = crate::view::quick_look_area(&self.layout);
        let scale = f32::from(u16::try_from(self.scale_120).unwrap_or(120)) / 120.0;
        ((inner.width * scale).max(1.0) as u32, (inner.height * scale).max(1.0) as u32)
    }

    /// Keeps the quick look fed: asks for its picture, takes delivery of its text.
    fn settle_quick_look(&mut self) {
        // **One place that sees the panel gone, whatever closed it.** `Escape`,
        // `Space`, a click away and a walk to another file all put it away, and a
        // renderer still holding twenty-four megabytes of pages for a document
        // nobody is looking at is the shape of leak that only shows up as an RSS
        // number nobody can explain.
        if self.quick_look.is_none() {
            self.renderer.forget_previews();
            return;
        }
        let within = self.quick_look_box();
        // **The one place that can see both stores.** The renderer's budget and
        // this map are bounded separately, and only the renderer knows what it
        // dropped — so what it has is asked here, before anything is shown from a
        // key it may no longer hold.
        let renderer = &self.renderer;
        let Some(look) = &mut self.quick_look else { return };
        look.retain_held(|key| renderer.preview_size(key).is_some());
        // **What the budget may not drop** — ADR 0028. A neighbour has never been
        // drawn, so every eviction rule that sorts on when a picture was last on
        // screen picks it first, and the page you are about to turn to is the one
        // that keeps disappearing.
        let pinned = look.pinned();
        self.renderer.keep_previews(&pinned);
        self.remember_the_page();
        self.lay_the_markdown_out();
        // **And the queue holds only the pages still wanted.** A held page key
        // queues a render per press, and the panel then shows the page from two
        // seconds ago while the queue drains — see `Host::only_pages`.
        if let Some(look) = self.quick_look.as_ref().filter(|look| look.paged()) {
            let mut wanted = look.neighbours();
            wanted.push(look.page);
            self.previews.only_pages(&look.path, &wanted);
        }
        let Some(look) = &mut self.quick_look else { return };
        // A page already on the GPU is a turn with no wait at all — ADR 0020.
        if look.show_held(within) {
            self.dirty = true;
        }
        let owed = look.wants(within).then(|| {
            let shot = look.shot(within);
            look.asked = Some(shot);
            shot
        });
        let guess = look.next_ahead();
        if let Some(page) = guess {
            look.ahead = Some(page);
        }
        let (kind, path, mtime) = (look.shows, look.path.clone(), look.mtime);
        let helper = match kind {
            crate::previews::Shows::Picture => sk_preview::Kind::Image,
            crate::previews::Shows::Page => sk_preview::Kind::Document,
            // Neither is a picture, and neither goes through the preview host:
            // text is read on a job here, and an archive's own child answers a
            // table rather than pixels — ADR 0032.
            crate::previews::Shows::Text | crate::previews::Shows::Archive => return,
        };
        // **`shot.within`, not `within`.** The shot is what a reply is matched
        // against, so asking for a different box than the one recorded is an
        // answer that can never match: the draft went out at full size, came back
        // keyed to full size, and was dropped against a coarse `asked` — a panel
        // that waited for ever, which is what the arrows scenario caught.
        if let Some(shot) = owed {
            self.previews.want(sk_preview::Want {
                kind: helper,
                ask: sk_preview::Ask::Preview,
                path: path.clone(),
                mtime,
                within: shot.within,
                page: shot.page,
                ahead: false,
            });
        }
        // The guess goes at the full size, because it is the one that will be
        // shown: a draft of a page nobody has turned to yet would be a render
        // spent on a picture that is replaced before it is seen.
        if let Some(page) = guess {
            self.previews.want(sk_preview::Want {
                kind: helper,
                ask: sk_preview::Ask::Preview,
                path,
                mtime,
                within,
                page,
                ahead: true,
            });
        }
    }

    /// Asks for the thumbnails of what is on screen, and forgets the rest.
    ///
    /// **Once a frame, over the visible rows only.** A folder of ten thousand
    /// photographs asks for the forty somebody can see; scrolling asks for the
    /// next forty. That is the same rule the folder-total walk next door follows
    /// and the reason neither is a listing-sized piece of work.
    ///
    /// Nothing here decodes, opens or stats anything — [rule
    /// 1](../../../docs/03-principles.md). The `mtime` is already in the listing,
    /// and what leaves this function is a want on a queue.
    /// Hands every visible pane the colours of the entries in it.
    ///
    /// **Once per settle, not once per frame and not once per row.** The records
    /// are keyed by path and a listing's rows all share a parent, so the question
    /// is asked once per pane against names — see
    /// [`crate::views::Views::coloured_in`]. Drawing then reads a `Vec` of a
    /// handful, which is what `Pane::colours` is.
    fn settle_colours(&mut self) {
        let leaves: Vec<crate::panes::PaneId> =
            self.placement.leaves.iter().map(|leaf| leaf.id).collect();
        for leaf in leaves {
            let Some(tabs) = self.panes.leaf_mut(leaf) else { continue };
            let pane = tabs.showing_mut();
            let coloured: Vec<(&std::ffi::OsStr, crate::colours::Colour)> =
                self.views.coloured_in(&pane.path).collect();
            pane.colours.clear();
            if coloured.is_empty() {
                continue;
            }
            for row in 0..pane.listing.rows() {
                let Some(id) = pane.listing.at(row) else { continue };
                let name = pane.listing.name(id);
                if let Some((_, colour)) =
                    coloured.iter().find(|(kept, _)| kept.as_encoded_bytes() == name.as_bytes())
                {
                    pane.colours.push((id, *colour));
                }
            }
        }
    }

    fn settle_thumbnails(&mut self) {
        // Nothing to ask about until the MIME databases land, which is the first
        // few milliseconds of a session — see `sk_desktop::kind_of`.
        let Some(desktop) = self.desktop.clone() else { return };
        let types = desktop.mime.as_ref();
        let mut wanted = Vec::new();
        for leaf in &self.placement.leaves {
            let Some(pane) = self.panes.leaf(leaf.id).map(crate::panes::Tabs::showing) else {
                continue;
            };
            let cells = self.cells_of(leaf, pane);
            for row in cells.visible(pane.scroll.offset()) {
                let Some(id) = pane.listing.at(row) else { continue };
                let kind = sk_desktop::kind_of(&pane.listing, id, Some(types));
                if crate::previews::shows(pane.listing.name(id), kind, Some(types))
                    != Some(crate::previews::Shows::Picture)
                {
                    continue;
                }
                wanted.push((leaf.id, id, pane.path_of(id), pane.listing.mtime(id)));
            }
        }
        for (leaf, id, path, mtime) in wanted {
            let (key, image) = self.thumbs.name(&path, mtime);
            // **`holds_image`, not `get`.** Asking whether to fetch must not stamp
            // the frame the atlas evicts by, or the least recently *drawn* entry
            // would be whatever nobody has asked about.
            let held = self.renderer.holds_image(image);
            if let Some(tabs) = self.panes.leaf_mut(leaf) {
                // **Only what the atlas actually has.** A row draws its kind mark
                // or its picture, never a picture-shaped hole: an entry whose
                // thumbnail has not arrived — or whose slot was evicted — is an
                // entry with no thumbnail, and the mark is what goes there.
                if held {
                    tabs.showing_mut().thumbs.insert(id, image);
                } else {
                    tabs.showing_mut().thumbs.remove(&id);
                }
            }
            if !held && !self.thumbs.was_refused(&key) {
                self.previews.want(crate::previews::thumbnail(path, mtime));
            }
        }
    }

    /// Takes delivery of a text preview's read.
    ///
    /// **In the loop, not in the frame**, which is the whole of the bug it fixes.
    /// It used to be the first line of `settle_quick_look`, and that runs from
    /// `before_frame`, and *that* runs only when something is already dirty — so
    /// the job finished, woke the loop, and found nothing that would draw: the
    /// text sat in the channel until an unrelated event marked the window dirty
    /// for its own reasons. The same shape as the preview pump one layer along,
    /// and the same rule: **a drain belongs beside the other drains**, where the
    /// wake-up lands, not inside the frame the drain is supposed to cause.
    fn drain_the_look_text(&mut self) {
        if self.quick_look.as_mut().is_some_and(crate::quicklook::QuickLook::drain) {
            self.dirty = true;
        }
        // An archive's listing lands the same way and for the same reason — the
        // job wakes the loop, and the drain has to be where the wake-up lands.
        if self.quick_look.as_mut().is_some_and(crate::quicklook::QuickLook::drain_listing) {
            self.dirty = true;
        }
    }

    /// Puts finished thumbnails into the atlas.
    fn drain_previews(&mut self) {
        let missing = self.previews.missing.len();
        let answers = self.previews.drain();
        if missing != self.previews.missing.len() {
            self.dirty = true;
        }
        if answers.is_empty() {
            return;
        }
        for answer in answers {
            // **A preview first, because it is the one somebody is waiting for.**
            // It is not in the slot atlas — ADR 0018 — so it gets a key of its own
            // and a texture the size of the panel.
            if let Some(look) = &mut self.quick_look
                && look.answers_to(&answer.key)
            {
                // The count travels with every document reply, so a page turn
                // learns the length even when the first reply could not say it.
                if answer.pages > 0 {
                    look.pages = answer.pages;
                }
                // A frame's own delay, which is a GIF's and zero for everything
                // else — so `due_in` answers nothing for a document.
                if answer.delay_ms > 0 {
                    look.delay_ms = answer.delay_ms;
                }
                // **The page's words, where the child could say** — ADR 0024.
                // They arrive with the pixels because they cost a fifth of what
                // the render does, and a page with none is a scan or a machine
                // without `pdftotext`: both are a page with no selection.
                // **Once per page, not once per render.** A zoom asks the child
                // for the same page again and the words come back with it — and
                // replacing the layer would throw away the selection somebody
                // zoomed in to read. The layer is dropped when the page turns,
                // which is the only time it stops being about what is on screen.
                if !answer.words.is_empty()
                    && let Some(words) = crate::pagetext::Page::of(&answer.words)
                {
                    if answer.key.page != look.page {
                        // **A prefetched page's words, kept rather than
                        // dropped.** They arrive while their page is not the one
                        // being read, and nothing asks for them a second time —
                        // see `QuickLook::kept_words`.
                        look.keep_words(answer.key.page, words);
                    } else if look.page_text.is_none() {
                        look.page_text = Some(words);
                    }
                }
                if answer.refusal == sk_preview::Refusal::NoSuchPage {
                    // A `↓` at the last page. The page on screen stays.
                    look.no_such_page(answer.pages);
                } else if answer.refusal == sk_preview::Refusal::None && !answer.pixels.is_empty() {
                    let key = self.thumbs.name_preview();
                    self.renderer.hold_preview(key, answer.size, &answer.pixels);
                    look.took(key, answer.size, answer.key.page);
                } else {
                    look.refusal = Some(answer.refusal);
                }
                self.dirty = true;
                continue;
            }
            if answer.refusal != sk_preview::Refusal::None || answer.pixels.is_empty() {
                // Remembered, or the next frame asks for it again — see
                // `Thumbs::refused` for what that cost.
                if self.thumbs.look_up(&answer.key).is_some() {
                    self.thumbs.refuse(answer.key);
                }
                continue;
            }
            // A name nobody holds any more is an answer to a question about a
            // folder somebody has left. Dropping it is the whole of the cleanup.
            let Some(image) = self.thumbs.look_up(&answer.key) else { continue };
            self.renderer.hold_image(image, answer.size.0, answer.size.1, &answer.pixels);
        }
        self.thumbs.forget_the_oldest();
        self.dirty = true;
    }

    /// Takes delivery of a finished mutation.
    fn drain_edits(&mut self) {
        let Some(landed) = self.edits.drain() else { return };
        // Before the notice is built: a walk of four steps should not put four
        // notices up, and the next step going out now is what makes it one.
        self.edits.continue_walking(&self.jobs);
        // **The strip may have just appeared, or grown a row.** Its height is a
        // function of the stack, so the window's regions move when the stack does
        // — which is why this is a relayout and not only a redraw.
        let wanted = self.history_height();
        if (self.layout.history.height - wanted).abs() > 0.01 {
            self.relayout();
        }
        self.settle_history();
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
                if !self.edits.busy() {
                    self.sidebar.reload(&self.jobs);
                }
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

    /// Open this folder in an independent process with its own session slot.
    pub(crate) fn new_window(&mut self) {
        let path = self.panes.focused().path.clone();
        let sender = self.replies.launched_tx.clone();
        let wake = self.waker.handle();
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            let child = std::env::current_exe().and_then(|exe| {
                std::process::Command::new(exe).arg("--new-window").arg(path).spawn()
            });
            let key = if child.is_ok() { "notice.new_window" } else { "error.new_window" };
            let _ = sender.send(Launched {
                child: child.ok(),
                notice: sk_model::i18n::lookup(key).to_owned(),
            });
            wake.wake();
        });
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
        self.watcher.watch(self.watched.clone());
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

    /// Applies each pane's folder view where it has just arrived somewhere.
    ///
    /// Every pane, not the focused one: a split shows two folders and both are
    /// entitled to their own layout, which is the whole reason the store is keyed
    /// by path rather than held on the tab.
    fn settle_views(&mut self) {
        let mut moved = false;
        for pane in self.panes.every_mut() {
            if !std::mem::take(&mut pane.view_owed) {
                continue;
            }
            // Every folder gets an answer, its own or an ancestor's or the
            // default — the one thing this must not do is leave the pane showing
            // the *previous* folder's layout, which is how the setting leaked.
            // Whether the pane takes it is `Pane::apply_view`'s to say.
            let view = self.views.for_path(&pane.path);
            moved |= pane.apply_view(view);
        }
        if moved {
            self.dirty = true;
        }
    }

    /// Records the focused pane's view against the folder it is showing.
    ///
    /// **A pane in columns records its grouping and no layout.** It is showing a
    /// path, so it has said nothing about the folder it happens to be standing in
    /// — and a record naming columns would be inherited by every directory beneath
    /// that one, which is how switching a single destination back to a details
    /// list put a whole subtree into columns. The grouping is not that: it is
    /// about the folder's contents, and somebody in a trail who groups by time
    /// means it.
    ///
    /// So the layout half is left at whatever the folder already answers — its own
    /// record or an ancestor's — and only the grouping moves. Writing
    /// `Layout::default()` instead would look identical on the folder that was
    /// chosen for and quietly un-grid every album under a `~/Pictures` that is one.
    fn remember_view(&mut self) {
        let pane = self.panes.focused();
        let (mode, grouping, path) = (pane.mode, pane.grouping, pane.path.clone());
        let view = self.views.chosen(&path, mode, grouping);
        if !self.views.remember(&path, view) {
            return;
        }
        self.keep_the_views();
    }

    /// Writes the per-folder records, if they have moved.
    ///
    /// **Compared rather than tracked**, the same bargain the session file makes:
    /// rendering is a few hundred bytes of string and comparing it is a `str`
    /// compare, and a `dirty` flag every caller has to remember to set is a flag
    /// one of them will not.
    fn keep_the_views(&mut self) {
        let text = self.views.render();
        if text == self.views_text {
            return;
        }
        self.views_text.clone_from(&text);
        self.jobs.pool.spawn(sk_util::Lane::Parallel, sk_util::Cancel::new(), move |_| {
            crate::views::write(&text);
        });
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
        self.remember_view();
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
        // **A quick look is the most specific thing `Escape` can mean**, the same
        // way a drag in flight is: what is on screen goes before what is selected
        // behind it.
        if self.close_the_look() {
            self.dirty = true;
            return;
        }
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
        self.marks.touch(crate::settings::Setting::Hidden);
        self.panes.focused_mut().toggle_hidden(&self.jobs);
    }

    /// Swaps the palette.
    pub(crate) fn toggle_theme(&mut self) {
        self.marks.touch(crate::settings::Setting::Theme);
        self.theme = if self.theme.dark { Theme::light() } else { Theme::dark() };
        self.relayout();
    }

    /// How tall the history strip wants to be, in logical pixels.
    ///
    /// **Nothing until there is something to show.** A window that has changed
    /// nothing has no history, and a permanent empty strip is a row of chrome
    /// earning its keep on no window at all. Once the stack has an entry it stays —
    /// `record` only grows within a session and an undo moves entries to the other
    /// branch rather than dropping them — so the strip appears once and does not
    /// flicker.
    ///
    /// Collapsed is the header alone. Expanded adds a row per entry up to
    /// [`crate::view::HISTORY_ROWS`], and [`crate::layout::Layout::compute_with`]
    /// clamps the whole thing to a third of the window besides.
    fn history_height(&self) -> f32 {
        if self.edits.history.is_empty() {
            return 0.0;
        }
        let metrics = &self.theme.metrics;
        if self.history_open {
            let rows = self.edits.history.len().min(crate::view::HISTORY_ROWS) as f32;
            metrics.header_height + rows * metrics.row_height
        } else {
            metrics.header_height
        }
    }

    /// How wide the sidebar is asking to be, and zero when it is closed.
    ///
    /// One answer for the layout and for the divider's grab band, because a band
    /// over a closed sidebar is a band over the list.
    fn aside(&self) -> f32 {
        if self.sidebar.open { self.sidebar.width } else { 0.0 }
    }

    /// Recomputes every region, including whose frame this window is drawing.
    fn relayout(&mut self) {
        self.layout = Layout::compute_with(
            self.logical,
            &self.theme.metrics,
            frame_of(&self.window),
            self.aside(),
            self.history_height(),
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
        let Self { panes, layout, theme, placement, columns, .. } = self;
        panes.layout(
            layout.content,
            &theme.metrics,
            *columns,
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

    /// What an archive's listing says, for the diagnostic line.
    ///
    /// **Its own method for the reason [`Self::page_state`] is one**: `look_state`
    /// is at the hundred-line lint and an archive is one subject — what the child
    /// answered, where in it the panel is standing, and which rows a pointer
    /// could aim at.
    fn inside_state(&self) -> String {
        /// How many of this level's names go on the line. A scenario asserts on
        /// the first few; a tar of a million entries would otherwise put a
        /// megabyte on one line, which the harness reads with an expression.
        const NAMED: usize = 12;
        let Some(archive) = self.quick_look.as_ref().and_then(|look| look.inside.as_ref()) else {
            return "inside=none inside_at=\"\" inside_rows=\"\" inside_chosen=none inside_aim=\"\""
                .to_owned();
        };
        let listing = &archive.listing;
        let format = listing.format.map_or("none", sk_preview::archive::Format::name);
        let inside = format!("{format}:{}:{}:{:?}", listing.count, listing.bytes, listing.refusal);
        let rows = archive
            .rows
            .iter()
            .take(NAMED)
            .map(|row| row.name.clone())
            .collect::<Vec<_>>()
            .join(",");
        let chosen = archive.row().map_or_else(|| "none".to_owned(), |row| row.path.clone());
        // Where each drawn row is, so a scenario clicks one by index rather than
        // by arithmetic over this panel's own constants.
        let aim = self
            .hits
            .inside
            .iter()
            .map(|(index, area)| {
                format!(
                    "{index}@{:.0}:{:.0}",
                    area.x + area.width / 2.0,
                    area.y + area.height / 2.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let at = archive.at.trim_end_matches('/');
        format!(
            "inside={inside} inside_at={at:?} inside_rows={rows:?} inside_chosen={chosen} inside_aim={aim:?}"
        )
    }

    /// What a page's own text layer says, for the diagnostic line.
    ///
    /// **Its own method for the reason `look_state` is one**: that is at the
    /// hundred-line limit, and a page's words are one subject — how many there
    /// are, what is selected out of them, and where the page was drawn so a
    /// scenario can aim at one.
    fn page_state(&self) -> String {
        // How many words the page's layer holds, and what is selected out of it:
        // a scenario about selecting text on a page cannot see either any other
        // way, and the clipboard is a separate claim from the selection.
        let look_words = self
            .quick_look
            .as_ref()
            .and_then(|look| look.page_text.as_ref())
            .map_or(0, crate::pagetext::Page::count);
        // **Where the page was actually drawn**, so a scenario can aim at a word
        // by mapping the tool's own box through it. The alternative is the
        // harness keeping a copy of `fitted_box` — which is the arithmetic under
        // test, and a second copy of it would agree with itself and with nothing
        // else. The same argument `column_aim` and `history_aim` already make.
        let look_page_box = self
            .quick_look
            .as_ref()
            .filter(|look| look.page_text.is_some())
            .and_then(|look| {
                let (_, size) = look.image?;
                let inner = crate::view::quick_look_area(&self.layout);
                let box_ =
                    crate::view::fitted_box(inner, size, look.drafted, (look.zoom, look.pan));
                Some(format!("{:.1}:{:.1}:{:.1}:{:.1}", box_.x, box_.y, box_.width, box_.height))
            })
            // **Quoted on the line, because an empty one is a key that
            // vanishes.** The harness reads this line with a regular expression
            // that needs either quotes or a character, so a bare
            // `look_page_box=` is not a field with no value — it is no field,
            // and every scenario that reads it fails with a `KeyError` naming
            // something that looks removed. The same rule `name_field` states.
            .unwrap_or_default();
        // What is typed in the find bar, so a scenario can tell a query that
        // found nothing from a query that never arrived.
        let look_query = self
            .quick_look
            .as_ref()
            .and_then(|look| look.finding.as_ref())
            .map(|field| field.text.clone())
            .unwrap_or_default();
        // What the find bar found, as `which/count`, or `none` when it is shut.
        let look_found =
            self.quick_look.as_ref().filter(|look| look.finding.is_some()).map_or_else(
                || "none".to_owned(),
                |look| {
                    let (count, at) = look.found;
                    let which = if count == 0 { 0 } else { at + 1 };
                    format!("{which}/{count}")
                },
            );
        let look_picked = self
            .quick_look
            .as_ref()
            .and_then(|look| look.page_text.as_ref())
            .and_then(crate::pagetext::Page::text)
            .unwrap_or_default();
        format!(
            "look_words={look_words} look_picked={look_picked:?} \
look_page_box={look_page_box:?} look_found={look_found} look_query={look_query:?}"
        )
    }

    /// What the quick look is showing, for the diagnostic line.
    ///
    /// **Its own method rather than forty lines inside `shape_state`**, which is
    /// at the hundred-line limit: the panel is one subject — which file, which
    /// frame of how many, where its controls are, and what edit mode is doing —
    /// and `queue_state` is the same trade made for the same reason.
    fn look_state(&self) -> String {
        // What the quick look is showing, for a scenario: which file, and whether
        // a picture, some text, or a sentence saying why neither.
        let looking = self.quick_look.as_ref().map_or_else(
            || "none".to_owned(),
            |look| {
                let what = if look.image.is_some() {
                    // A document's page is an image too, and saying `picture`
                    // about a PDF made a dump that had to be read beside
                    // `look_page` to be understood at all.
                    if look.shows == crate::previews::Shows::Page { "page" } else { "picture" }
                } else if look.text.is_some() {
                    "text"
                } else if look.inside.is_some() {
                    // **Its own word, and it had to be.** An archive has no
                    // image and no text, so it read as `waiting` — a panel full
                    // of rows reported as one that had not answered yet, which
                    // is the state a scenario waits *out of*.
                    "archive"
                } else if look.refusal.is_some() {
                    "refused"
                } else {
                    "waiting"
                };
                format!("{}:{what}", look.title())
            },
        );
        // The page and the count, separately, so the assertions above keep working
        // and a page turn is visible on its own: `3/14`, `3/?` where the count is
        // unknown, or `none` for anything that is not paged.
        // Where each of the look's controls is, so a scenario clicks one by name
        // rather than by arithmetic over the strip's own constants — the same
        // trade `menu_aim` and `erase_aim` make.
        let look_aim = self
            .hits
            .look
            .iter()
            .map(|(control, area)| {
                let name = match control {
                    crate::view::LookControl::PreviousFile => "previous_file",
                    crate::view::LookControl::NextFile => "next_file",
                    crate::view::LookControl::PreviousPage => "previous_page",
                    crate::view::LookControl::NextPage => "next_page",
                    crate::view::LookControl::Edit => "edit",
                    crate::view::LookControl::Discard => "discard",
                    crate::view::LookControl::Play => "play",
                    crate::view::LookControl::Source => "source",
                    crate::view::LookControl::Extract => "extract",
                };
                format!("{name}@{:.0}:{:.0}", area.x + area.width / 2.0, area.y + area.height / 2.0)
            })
            .collect::<Vec<_>>()
            .join(",");
        // What is selected in a text preview, as `line:byte-line:byte` in reading
        // order — so a scenario can assert a drag landed where it aimed without
        // reading the clipboard, which is a separate claim.
        let look_chosen = self
            .quick_look
            .as_ref()
            .and_then(|look| look.chosen.filter(|it| !it.is_empty()))
            .map_or_else(
                || "none".to_owned(),
                |chosen| {
                    let (from, to) = chosen.ordered();
                    format!("{}:{}-{}:{}", from.line, from.byte, to.line, to.byte)
                },
            );
        // How many pages are rendered and on the GPU, which is what says the
        // prefetch ran: one is the page on screen alone — ADR 0020.
        let look_held = self.quick_look.as_ref().map_or(0, |look| look.held.len());
        // Whether the panel is being typed into, and where the caret is — the two
        // things edit mode adds that nothing else on this line reports.
        let look_editing = self.editing_the_look();
        let look_caret = self.quick_look.as_ref().filter(|look| look.editing).map_or_else(
            || "none".to_owned(),
            |look| {
                let at = look.caret();
                format!("{}:{}", at.line, at.byte)
            },
        );
        // **How many rows the text took**, which is the only way a scenario can
        // see that a long line wrapped rather than being cut off at the panel's
        // edge: the rows *are* the wrap.
        let look_rows = self
            .quick_look
            .as_ref()
            .and_then(|look| look.text.as_ref())
            .map_or(0, |text| text.rows().len());
        let look_unsaved = self.look_is_unsaved();
        let look_shot = self.look_shot();
        let page = self.page_state();
        let inside = self.inside_state();
        // The magnification and where the picture has been dragged to, which is
        // the whole of what a pinch and a pan change: a scenario asserting on
        // pixels would be asserting on the machine's filtering.
        let (look_zoom, look_pan) = self.quick_look.as_ref().map_or_else(
            || ("1.00".to_owned(), "0:0".to_owned()),
            |look| (format!("{:.2}", look.zoom), format!("{:.0}:{:.0}", look.pan.0, look.pan.1)),
        );
        // Whether an animation is running, which is the one thing on screen that
        // changes while the hands are still.
        let look_mode = self.look_mode();
        let look_playing = self.looking_at_an_animation()
            && self.quick_look.as_ref().is_some_and(|look| look.playing);
        let look_page = self.quick_look.as_ref().map_or_else(
            || "none".to_owned(),
            |look| {
                // **Anything paged, which is a document or an animation.** A
                // GIF's frames are its pages and a scenario about one asks the
                // same question: which of how many is on screen.
                if look.paged() {
                    let total =
                        if look.pages > 0 { look.pages.to_string() } else { "?".to_owned() };
                    format!("{}/{total}", look.page)
                } else {
                    "none".to_owned()
                }
            },
        );
        format!(
            "looking={looking} look_page={look_page} look_aim={look_aim:?} \
look_held={look_held} look_editing={look_editing} look_caret={look_caret} \
look_chosen={look_chosen} look_rows={look_rows} look_unsaved={look_unsaved} \
look_playing={look_playing} look_mode={look_mode} look_zoom={look_zoom} look_pan={look_pan} \
look_shot={look_shot} \
{page} {inside}"
        )
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
        // How many roots a queued build still owes, so a scenario can see that
        // "every filesystem" is a queue rather than one walk.
        let owed_roots = self.indexes_owed();
        let searched = self.search.as_ref().is_some_and(|it| it.indexed.is_some());
        let collapsed = self.panes.focused().hidden_count();
        let headings = self.headings_state();
        let history = format!("{}/{}", self.edits.history.applied(), self.edits.history.len());
        let history_open = self.history_open;
        let history_aim = self.history_aim();
        let look = self.look_state();
        format!(
            "mode={} grouping={} bands={} remembered={} columns={} cell={:.1}x{:.1} \
collapsed={collapsed} headings={headings} \
history={history} history_open={history_open} history_aim={history_aim} \
{look} \
index={index} indexing={indexing} owed_roots={owed_roots} search_indexed={searched}",
            cells.mode().token(),
            self.panes.focused().grouping.token(),
            cells.count(),
            self.views.len(),
            cells.columns(),
            cells.cell().0,
            cells.cell().1,
        )
    }

    /// Every drawn history row as `index:x:y`, or `none`.
    ///
    /// **From the rectangles the frame recorded**, for the reason `column_aim` and
    /// `check_aim` are: the list is scrolled and clipped, so which rows exist and
    /// where they are is something only the frame knows — and a harness that worked
    /// it out from `row_h` would be a second copy of the panel's layout.
    fn history_aim(&self) -> String {
        let middle =
            |at: &crate::layout::Area| format!("{:.1}:{:.1}", at.x + 40.0, at.y + at.height / 2.0);
        // The header first, because it is a control too — the pointer surface
        // `commands.rs` claims for `edit.history` — and a scenario that could not
        // aim at it would be testing the chord twice.
        let mut aims: Vec<String> =
            self.hits.history_header.iter().map(|at| format!("header:{}", middle(at))).collect();
        aims.extend(
            self.hits.history_rows.iter().map(|(index, at)| format!("{index}:{}", middle(at))),
        );
        if aims.is_empty() { "none".to_owned() } else { aims.join(",") }
    }

    /// Every on-screen heading as `band:x:y:state`, or `none`.
    ///
    /// **The aim comes from the program**, for the reason `column_aim` and
    /// `check_aim` do: a harness that worked a heading's middle out from
    /// `body_top` and `row_h` would be a second copy of the band layout, and this
    /// file's own lesson is that a coordinate derived from another region is a
    /// coordinate that moves. The state is `shut` or `open`, so a scenario can
    /// assert what it pressed did what it meant rather than that something moved.
    fn headings_state(&self) -> String {
        let pane = self.panes.focused();
        let found = self.cells().headings(pane.scroll.offset());
        if found.is_empty() {
            return "none".to_owned();
        }
        found
            .iter()
            .map(|it| {
                let (x, y) = (it.at.x + it.at.width / 2.0, it.at.y + it.at.height / 2.0);
                let state = if it.closed { "shut" } else { "open" };
                format!("{}:{x:.1}:{y:.1}:{state}", it.kind)
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// What the operation queue is doing, for the diagnostic line.
    ///
    /// Its own method rather than twenty lines inside `listing_state`, which is
    /// at the hundred-line limit: what is queued, what a stopped one owes, where
    /// its Pick up button is and which rows carry a colour are all things a
    /// scenario asks about the work rather than about the listing.
    fn queue_state(&self) -> String {
        let (queue_left, queue_total) = self.edits.queue_state();
        // What a stopped queue still owes, so a scenario can assert the remainder
        // rather than infer it from a sentence.
        let (owed_left, owed_total) = self.edits.owed().unwrap_or((0, 0));
        // Which of the rows carry a colour somebody chose, by name, so a scenario
        // asserts what is drawn rather than what a file says.
        let coloured = {
            let pane = self.panes.focused();
            pane.colours
                .iter()
                .map(|(id, colour)| format!("{}:{}", pane.listing.name(*id), colour.token()))
                .collect::<Vec<_>>()
                .join(",")
        };
        // Where the strip's Pick up button is, so a pointer can reach it without
        // the harness owning a copy of the strip's arithmetic.
        let pick_up_aim = self.hits.pick_up.map_or_else(String::new, |area| {
            format!("{:.0}:{:.0}", area.x + area.width / 2.0, area.y + area.height / 2.0)
        });
        format!(
            "queue={queue_left}/{queue_total} owed={owed_left}/{owed_total} \
pick_up_aim={pick_up_aim} coloured={coloured:?}"
        )
    }

    /// What the pane is showing.
    #[expect(clippy::unnecessary_debug_formatting, reason = "the quoting is the point")]
    fn listing_state(&mut self) -> String {
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
        let queues = self.queue_state();
        let transfers = self.transfer_state();
        let a11y = self.a11y_report();
        let raises = format!("{}/{}", self.raises.0, self.raises.1);
        format!(
            "{shape} \
path={:?} results={results:?} erasing={erasing} erasing_on={erasing_on} erase_aim={erase_aim:?} panel_area={panel_area:?} rows={} hidden={} sort={:?} descending={} \
cursor={cursor:?} cursor_row={cursor_row} \
selected={} scroll={:.1} loading={} watching={} watch_reason={:?} thumbs_missing={} notice={:?} \
hover={hover} hover_name={hover_name:?} \
naming={naming:?} naming_row={naming_row} naming_selected={naming_selected:?} \
path_entry={typing_path:?} path_caret={path_caret} path_selected={path_selected:?} \
undo={} redo={} busy={} {queues} \
progress={progress:?} \
asking={asking} clash={clash:?} clash_rest={clash_rest} answer={answer:?} \
for_the_rest={for_the_rest} paused={paused} total={total} total_done={total_done} \
theme={theme} zoom={zoom} sidebar_open={sidebar_open} {search} \
filtering={} filter={:?} {drag} \
{transfers} a11y={a11y} raises={raises} \
offered={offered:?} typed={typed:?} \
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
            // The two degraded-runtime explanations the status line puts in place
            // of the row count — ADR 0037. Reported because "the window says why"
            // is the claim, and a scenario cannot read the status line's pixels.
            self.watch_failure.map(sk_fs::watch::Failure::message).unwrap_or_default(),
            self.previews.missing.contains(&sk_preview::Kind::Image),
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
        let dimmed = menu.map_or_else(String::new, |menu| Self::dimmed_rows(&menu.items));
        let aim =
            menu.map_or_else(String::new, |menu| Self::row_aims(&menu.items, &self.hits.menu));
        // What each row that opens holds, as `owner:child|child`, so a scenario
        // that names a colour can find the row to open first.
        let holds = menu.map_or_else(String::new, |menu| {
            menu.items
                .iter()
                .filter(|item| item.opens())
                .map(|item| format!("{}:{}", item.id, item.holds().join("|")))
                .collect::<Vec<_>>()
                .join(",")
        });
        let scroll = menu.map_or(0, |menu| menu.scroll);
        let drawn = self.hits.menu.iter().position(|area| area.width > 0.0).unwrap_or(0);
        // The open submenu, reported the way its parent is.
        let sub = menu.and_then(|menu| menu.sub.as_deref());
        let sub_of =
            sub.and_then(|sub| menu?.items.get(sub.owner)).map_or("", |owner| owner.id.as_str());
        let sub_rows = sub.map_or(0, |sub| sub.menu.items.len());
        let sub_active = sub
            .and_then(|sub| sub.menu.active)
            .map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let sub_id = sub.and_then(|sub| sub.menu.active_id()).unwrap_or_default();
        let sub_dimmed = sub.map_or_else(String::new, |sub| Self::dimmed_rows(&sub.menu.items));
        let sub_aim =
            sub.map_or_else(String::new, |sub| Self::row_aims(&sub.menu.items, &self.hits.submenu));
        let sub_scroll = sub.map_or(0, |sub| sub.menu.scroll);
        let sub_drawn = self.hits.submenu.iter().position(|area| area.width > 0.0).unwrap_or(0);
        format!(
            "menu={rows} menu_active={active} menu_id={id:?} menu_aim={aim:?} menu_dim={dimmed:?} menu_scroll={scroll} menu_drawn={drawn} menu_holds={holds:?} \
submenu={sub_rows} submenu_of={sub_of:?} submenu_active={sub_active} submenu_id={sub_id:?} submenu_aim={sub_aim:?} submenu_dim={sub_dimmed:?} \
submenu_scroll={sub_scroll} submenu_drawn={sub_drawn}"
        )
    }

    /// The rows of a menu that cannot run, by identifier.
    ///
    /// "Dimmed rather than absent" is a claim about the interface and not an
    /// implementation detail, so it is reported — and every row is, drawn or
    /// not, so a scenario can ask about a row below the fold.
    fn dimmed_rows(items: &[crate::menu::Item]) -> String {
        items
            .iter()
            .filter(|item| !item.enabled)
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Where each drawn row of a menu can be clicked: the middle of the row, by
    /// identifier, so a scenario names a command instead of copying the menu's
    /// own constants — which would then be a second copy to keep right.
    fn row_aims(items: &[crate::menu::Item], areas: &[Area]) -> String {
        items
            .iter()
            .zip(areas)
            // A row scrolled out of a menu taller than the window has an empty
            // rectangle, and an aim at it would be an aim at nothing.
            .filter(|(_, area)| area.width > 0.0)
            .map(|(item, area)| {
                format!("{}@{:.0}:{:.0}", item.id, area.x + 20.0, area.y + area.height / 2.0)
            })
            .collect::<Vec<_>>()
            .join(",")
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
        let focus = self.panes.focus();
        let mine =
            self.hits.separators.iter().filter(|hit| hit.leaf == focus && hit.area.width > 0.0);
        let drawn = mine.clone().count();
        let hovered =
            self.hovered_separator.map_or(-1, |(_, depth)| i64::try_from(depth).unwrap_or(-1));
        let aim = mine
            .map(|hit| {
                format!(
                    "{}@{:.1}:{:.1}",
                    hit.depth,
                    hit.area.x + hit.area.width / 2.0,
                    hit.area.y + hit.area.height / 2.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        (drawn, hovered, aim)
    }

    /// The focused pane's own bar: its buttons, its crumbs, and where they are.
    ///
    /// **The focused pane's, out of however many bars there are.** A scenario
    /// asks where "the" path bar is, and the one it means is the one the keyboard
    /// is in — the same choice `body_top` and `name_x` already make.
    fn bar_state(&self) -> String {
        let here = self.placement.here;
        let focus = self.panes.focus();
        let mine: Vec<_> = self.hits.crumbs.iter().filter(|hit| hit.leaf == focus).collect();
        let crumbs = mine.len();
        let crumb_hover =
            self.hovered_crumb.map_or(-1, |(_, depth)| i64::try_from(depth).unwrap_or(-1));
        // The component one level up, which is the one a test clicks.
        let parent =
            mine.len().checked_sub(2).and_then(|index| mine.get(index)).map(|hit| hit.area);
        let (parent_x, parent_y) = parent
            .map_or((0.0, 0.0), |area| (area.x + area.width / 2.0, area.y + area.height / 2.0));
        let tool_hover = match self.hovered_tool.map(|(_, tool)| tool) {
            Some(crate::layout::Tool::Back) => "back",
            Some(crate::layout::Tool::Forward) => "forward",
            Some(crate::layout::Tool::Parent) => "parent",
            Some(crate::layout::Tool::Reload) => "reload",
            None => "none",
        };
        format!(
            "crumbs={crumbs} crumb_hover={crumb_hover} parent_crumb_x={parent_x:.1} \
parent_crumb_y={parent_y:.1} bar_y={:.1} bar_h={:.1} tools_x={:.1} tool_w={:.1} \
tool_hover={tool_hover}",
            here.bar.y, here.bar.height, here.tools[0].x, here.tools[0].width,
        )
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
        // Each leaf's list and each leaf's own bar, so a scenario can put the
        // pointer inside a pane that does not have the keyboard — which
        // `body_top`, `name_x` and `bar_y` cannot, because all three answer about
        // the focused one.
        let bodies = self.boxes_of(|leaf| leaf.body);
        let bars = self.boxes_of(|leaf| leaf.bar);
        format!(
            "panes={} pane_focus={order} pane_sizes={widths:?} pane_paths={paths:?} \
tabs={} tab={} tab_aim={tab_aim:?} tab_cross={cross_aim:?} plus_aim={plus_aim:?} \
tab_drop={tab_drop} strip_aim={strip_aim:?} divider_aim={divider_aim:?} \
pane_body={bodies:?} pane_bar={bars:?}",
            self.placement.leaves.len(),
            leaf.panes.len(),
            leaf.active,
        )
    }

    /// One rectangle per leaf, in layout order, as `x:y:w:h`.
    fn boxes_of(&self, pick: impl Fn(&crate::panes::PaneLayout) -> Area) -> String {
        self.placement
            .leaves
            .iter()
            .map(|leaf| {
                let area = pick(leaf);
                format!("{:.0}:{:.0}:{:.0}:{:.0}", area.x, area.y, area.width, area.height)
            })
            .collect::<Vec<_>>()
            .join(",")
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

    /// What the list of places is showing, for the diagnostic line.
    ///
    /// Its own method rather than twenty lines inside `chrome_state`, which is at
    /// the hundred-line limit: the sidebar is one subject and this is everything
    /// a scenario can ask about it.
    fn places_state(&self) -> String {
        let hover = self.sidebar.hover.map_or(-1, |index| i64::try_from(index).unwrap_or(-1));
        let name = self
            .sidebar
            .hover
            .and_then(|index| self.sidebar.at(index))
            .map(|entry| entry.label.clone())
            .unwrap_or_default();
        // The pinned places by name, in the order they are drawn, so a scenario
        // can assert an order rather than a count — and the row a place menu is
        // about, which is the target a command with no argument acts on.
        let pinned = self
            .sidebar
            .pinned()
            .iter()
            .map(|path| crate::panes::title_of(path).to_owned())
            .collect::<Vec<_>>()
            .join(",");
        // Every volume row and whether anything has mounted it, so a scenario can
        // see the row a disk nothing mounted produces — ADR 0030. Machine-
        // dependent by nature, which is why what is asserted about it is a shape.
        let volumes = self
            .sidebar
            .rows()
            .iter()
            .filter(|row| row.mark == crate::sidebar::Mark::Volume)
            .map(|row| format!("{}:{}", row.label, if row.unmounted { "no" } else { "yes" }))
            .collect::<Vec<_>>()
            .join(",");
        let at = self.pinned_run().map_or(-1, |run| i64::try_from(run.start).unwrap_or(-1));
        let menu = self
            .place_target
            .as_deref()
            .map(|path| crate::panes::title_of(path).to_owned())
            .unwrap_or_default();
        let drop = self
            .place_drag
            .filter(|drag| drag.active)
            .map_or(-1, |drag| i64::try_from(drag.at).unwrap_or(-1));
        // One point inside every row, in the flattened order, so a press can aim
        // at a place without the harness owning a copy of the row height and the
        // heading's — and the index the pinned run starts at, so "the second pin"
        // is arithmetic rather than a guess about which sections exist.
        //
        // **Six tenths of the way across, not thirty pixels in.** A row at depth
        // one has its twisty where a place's label is, so an aim by a fixed
        // offset would have opened the tree row it meant to click.
        let aim = self
            .hits
            .places
            .iter()
            .map(|area| {
                format!("{:.0}:{:.0}", area.x + area.width * 0.6, area.y + area.height / 2.0)
            })
            .collect::<Vec<_>>()
            .join(",");
        // And one per twisty, which is a different target on the same row.
        let twisties = self
            .hits
            .twisties
            .iter()
            .map(|area| {
                format!("{:.0}:{:.0}", area.x + area.width / 2.0, area.y + area.height / 2.0)
            })
            .collect::<Vec<_>>()
            .join(",");
        // Which rows are open, and how deep each is drawn, so a scenario can
        // assert a shape rather than a count.
        let depths = self
            .sidebar
            .rows()
            .iter()
            .map(|row| format!("{}{}", row.depth, if row.open { "+" } else { "" }))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "sidebar_w={:.1} sidebar_top={:.1} places={} places_loaded={} \
place_hover={hover} place_name={name:?} pinned={pinned:?} pinned_at={at} place_menu={menu:?} \
place_drop={drop} place_aim={aim:?} twisty_aim={twisties:?} place_depths={depths:?} \
volumes={volumes:?} aside_grabbed={}",
            self.layout.sidebar.width,
            self.layout.sidebar.y,
            self.sidebar.len(),
            self.sidebar.loads,
            self.aside_drag || self.on_the_aside_divider(self.pointer),
        )
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
        let places_report = self.places_state();
        // Where the path field's box is, so a scenario can drag inside it by
        // arithmetic over what the frame drew rather than over its own copy of the
        // metrics.
        let box_of = crate::view::path_field(&self.placement.here, &self.theme.metrics);
        let path_field =
            format!("{:.1},{:.1},{:.1},{:.1}", box_of.x, box_of.y, box_of.width, box_of.height);
        // And the name field's, when one is open — the box the *frame drew*, which
        // is now also the box a press is tested against. Reporting the hit test's
        // own arithmetic instead would give a scenario a rectangle that agrees
        // with the bug: it clicked where the window believed the field was, which
        // is exactly what was wrong.
        // `""` and not the empty string, because the harness reads this line with
        // a regular expression that needs either quotes or a character: a bare
        // `name_field=` is a key that vanishes rather than a key with no value.
        let name_field = self.hits.name_field.map_or_else(
            || "\"\"".to_owned(),
            |it| format!("{:.1},{:.1},{:.1},{:.1}", it.x, it.y, it.width, it.height),
        );
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
        let bar_report = self.bar_state();
        let (column_aim, mode_hover, column_scroll, column_over) = self.column_state();
        let check_aim = self.check_aim();
        let settings = self.settings_report();
        let settings_aim = self.settings_aim();
        format!(
            "body_top={:.1} body_height={:.1} row_h={:.1} name_x={:.1} size_x={:.1} size_w={:.1} \
header_y={:.1} header_h={:.1} modified_x={:.1} modified_w={:.1} \
rows_right={:.1} body_right={:.1} modes_x={:.1} mode_w={:.1} mode_hover={mode_hover} \
trail={} across={:.1} column_aim={column_aim} column_scroll={column_scroll} \
column_over={column_over} check_aim={check_aim} pointer_at={:.1}:{:.1} \
scrolls={} precise={} lifts={} scroll_time={} flings={} \
frame={frame} frame_hover={frame_hover} maximized={} close_x={close_x:.1} title_h={:.1} \
{bar_report} \
separators={separators} separator_hover={separator_hover} separator_aim={separator_aim:?} \
crumb_menu={crumb_menu} crumb_menu_depth={crumb_menu_depth} crumb_menu_active={crumb_menu_active:?} \
crumb_menu_aim={crumb_menu_aim:?} \
{panes_report} {menu_report} {palette_report} \
properties={properties:?} facts={facts:?} openers={openers} opener={opener:?} \
composing={composing:?} pointer={cursor_shape} settings={settings:?} settings_aim={settings_aim:?} \
{places_report} \
path_field={path_field} name_field={name_field} \
logical={:.0}x{:.0} scale_120={} first_frame_scale_120={} \
frame_p50_us={} frame_p99_us={} frames={} over={} saves={} keys={} stats={}",
            self.placement.here.body.y,
            self.placement.here.body.height,
            self.theme.metrics.row_height,
            self.placement.here.name.x,
            self.placement.here.size.x,
            self.placement.here.size.width,
            self.placement.here.header.y,
            self.placement.here.header.height,
            self.placement.here.modified.x,
            self.placement.here.modified.width,
            crate::view::table_of(&self.placement.here, self.panes.focused()).right(),
            self.placement.here.body.right(),
            self.placement.here.modes.first().map_or(0.0, |area| area.x),
            self.placement.here.modes.first().map_or(0.0, |area| area.width),
            self.panes.focused().trail.len(),
            self.panes.focused().across.offset(),
            self.pointer.x,
            self.pointer.y,
            self.scrolls.0,
            self.scrolls.1,
            self.scrolls.2,
            self.scroll_time,
            self.flings,
            self.window_state.maximized,
            self.layout.title.height,
            self.logical.width,
            self.logical.height,
            self.scale_120,
            self.first_frame_scale_120.unwrap_or(0),
            self.frames.percentile(0.5),
            self.frames.percentile(0.99),
            self.frames.budget().0,
            self.frames.budget().1,
            self.saves,
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
            crate::view::table_of(layout, pane),
            self.theme.metrics.row_height,
            &pane.bands,
            pane.listing.rows(),
            pane.closed,
        )
    }

    /// Collapses the band `band`, or opens it.
    pub(crate) fn toggle_band(&mut self, band: u8) {
        if self.panes.focused_mut().toggle_band(band) {
            self.after_bands_moved();
        }
    }

    /// Opens every collapsed band in the focused pane.
    pub(crate) fn show_every_band(&mut self) {
        if self.panes.focused_mut().show_every_band() {
            self.after_bands_moved();
        }
    }

    /// **The list got shorter or longer without the order changing.** Which is
    /// three things and not one: the scrollbar's extent is now wrong, the cursor
    /// may be off screen, and the frame is stale. The same three `set_mode` does,
    /// for the same reason — a listing whose height moved under a fixed offset.
    fn after_bands_moved(&mut self) {
        let content = self.cells().content_height();
        let body = self.placement.here.body.height;
        self.panes.focused_mut().scroll.set_extent(content, body);
        self.reveal_cursor();
        self.dirty = true;
    }

    /// Cuts the focused pane into headings, or stops.
    pub(crate) fn set_grouping(&mut self, grouping: crate::groups::Grouping) {
        if self.panes.focused().grouping == grouping {
            return;
        }
        self.panes.focused_mut().grouping = grouping;
        // **A band number means something else now.** Band six is "Older" under
        // time and "Tiny" under size, so keeping the set would collapse a group
        // nobody chose — and there is no mapping between the two taxonomies to
        // carry it across.
        self.panes.focused_mut().show_every_band();
        self.remember_view();
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
        // **Every pane, not the focused one.** A split shows two folders and each
        // has its own grouping; banding only the focused half left the other one
        // grouped with no bands, which draws no headings at all — the setting
        // looked like it had been forgotten by walking into the other pane.
        let now = clock_seconds();
        let Self { panes, jobs, .. } = self;
        for pane in panes.every_mut() {
            let wanted = pane.grouping.banding(now);
            if pane.sort.bands == wanted {
                continue;
            }
            pane.sort.bands = wanted;
            pane.reorder(jobs);
        }
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
        if self.press_in_an_overlay(position, (shift, clicks), commands) {
            return;
        }
        if self.press_in_a_field(position, clicks) {
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
        // The open path field before anything in the bar it is drawn across: a
        // press inside it belongs to the caret, and a press outside it commits
        // nothing and closes it, the way every other field on the desktop does.
        if self.typing_path.is_some() {
            let field = crate::view::path_field(&self.placement.here, &self.theme.metrics);
            if field.contains(position.x, position.y) {
                self.place_path_caret(position.x);
            } else {
                self.typing_path = None;
                self.dirty = true;
            }
            return;
        }
        // A pane's own bar before the tree, because the bar's four buttons, its
        // chevrons and its components are all more specific than "a press
        // somewhere in this pane" — and each of them focuses that pane itself.
        if self.press_on_a_pane_bar(position, commands) {
            return;
        }
        if self.press_on_the_title_bar(position, clicks) {
            return;
        }
        // The tree before the header and the list, because both of those are
        // questions about *which* pane — and the answer can change here.
        if self.press_in_the_tree(position, clicks, commands) {
            return;
        }
        // **Before the sort**, because a boundary sits inside the header of the
        // column left of it: without this, every press that took hold of a line
        // also sorted by whatever label the line was at the end of.
        if let Some((id, edge)) = self.column_edge_at(position) {
            self.focus_pane(id);
            self.column_drag = Some((id, edge));
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
        // **A press in a column left of the pane's own is a navigation.** Which is
        // what a column browser is: the trail is a function of the path, so
        // choosing something three columns back moves the pane there and every
        // level above it is kept rather than re-read.
        if let Some((depth, row)) = self.column_press(position) {
            self.open_from_column(depth, row);
            return;
        }

        if self.press_on_a_heading(position) {
            return;
        }
        let Some(row) = self.row_under(position) else {
            self.press_on_empty_space(position);
            return;
        };
        let Some(id) = self.panes.focused().listing.at(row) else { return };
        // **The check box is a toggle and nothing else.** Not `ctrl || ticked`
        // through the row press: that path also arms a drag, and a second press
        // inside the box would then open what it had just un-ticked. A person
        // ticking four rows is not dragging them and has not asked to open one.
        if self.check_under(row, position) {
            self.panes.focused_mut().selection.toggle(id);
            self.dirty = true;
            return;
        }
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
                copying: true,
                directory: false,
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
        let action =
            if self.files_drag.as_ref().is_some_and(|drag| !drag.paths.is_empty() && !drag.copying)
            {
                sk_plat::DragAction::Move
            } else {
                sk_plat::DragAction::Copy
            };
        self.window.accept_drag(wanted, action);
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
            let copying = self.files_drag.as_ref().is_some_and(|drag| drag.copying);
            self.drop_files(copying);
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
        self.drops_started = self.drops_started.saturating_add(1);
        self.drops_asked = Some(Instant::now());
        let asked = Instant::now();
        let took = std::sync::Arc::clone(&self.drops_took);
        let delivered = std::sync::Arc::clone(&self.drops_delivered);
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
            let spent = u32::try_from(asked.elapsed().as_millis()).unwrap_or(u32::MAX);
            took.store(spent, std::sync::atomic::Ordering::Relaxed);
            delivered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if sender.send(reply).is_ok() {
                wake.wake();
            }
        });
    }

    // ---- What the accessibility tree reads. ----
    //
    // **Accessors and not public fields**, because `crate::a11y` is a sibling
    // module: everything it needs is named here, so the tree cannot reach into
    // state nobody meant it to have — and the list of names below is the whole
    // of what a screen reader is told, which is a thing to be able to read in
    // one place. ADR 0026.

    /// What is in flight between this window and another application.
    ///
    /// **Three numbers and a stopwatch, because a drop that leaves nothing
    /// behind has three possible causes.** `selections` is what the display
    /// server's machinery holds — a conversion asked for and unanswered, a write
    /// parked for room, an answer that matched nothing. `drops` is the read this
    /// program started, the bytes that came back, and the message it acted on,
    /// with how long the read took and how long ago it began. A `1/1/0` is a
    /// message delivered and not drained; a `1/0/0` is a read that never came
    /// back; and the milliseconds tell a slow transfer from a stuck one.
    fn transfer_state(&self) -> String {
        let held = self.window.selections();
        format!(
            "drag_in={} drag_out={} selections={}/{}/{} drops={}/{}/{}@{}ms-of-{}ms",
            self.window.dragging().is_some(),
            self.window.dragging_out(),
            held.awaiting,
            held.sending,
            held.unmatched,
            self.drops_started,
            self.drops_delivered.load(std::sync::atomic::Ordering::Relaxed),
            self.drops_drained,
            self.drops_took.load(std::sync::atomic::Ordering::Relaxed),
            self.drops_asked.map_or(0, |at| at.elapsed().as_millis())
        )
    }

    /// How many nodes the accessibility tree last published, or `off` where this
    /// desktop has no accessibility bus.
    fn a11y_report(&self) -> String {
        self.a11y.as_ref().map_or_else(|| "off".to_owned(), |it| it.nodes().to_string())
    }

    /// How many device pixels a logical one is.
    pub(crate) fn a11y_scale(&self) -> f32 {
        self.window.scale()
    }

    /// The window, as a rectangle.
    pub(crate) fn a11y_window(&self) -> Area {
        Area { x: 0.0, y: 0.0, width: self.layout.window.width, height: self.layout.window.height }
    }

    /// The status line's rectangle.
    pub(crate) fn a11y_status_area(&self) -> Area {
        self.layout.status
    }

    /// The focused pane's list body.
    pub(crate) fn a11y_body(&self) -> Area {
        self.placement.here.body
    }

    /// The quick look's panel.
    pub(crate) fn a11y_look_area(&self) -> Area {
        crate::view::quick_look_area(&self.layout)
    }

    /// What the window is called: the folder being shown.
    pub(crate) fn a11y_title(&self) -> String {
        crate::panes::title_of(&self.panes.focused().path).to_owned()
    }

    /// Whether this window has the keyboard.
    pub(crate) fn a11y_active(&self) -> bool {
        self.window.state().activated
    }

    /// The sentence the status line is showing, which is what it counts.
    pub(crate) fn a11y_status(&self) -> String {
        let pane = self.panes.focused();
        let rows = pane.listing.rows();
        let selected = pane.selection.count();
        if selected > 0 {
            sk_model::i18n::plural(
                "status.counts",
                rows as u64,
                &[&sk_ui::format::grouped(rows), &sk_ui::format::grouped(selected)],
            )
        } else {
            sk_model::i18n::plural(
                "status.counts.none",
                rows as u64,
                &[&sk_ui::format::grouped(rows)],
            )
        }
    }

    /// What the listing is called, which is the folder it is of.
    pub(crate) fn a11y_listing_name(&self) -> String {
        self.a11y_title()
    }

    /// Whether the rows are what the keyboard is acting on.
    ///
    /// A field, a menu or a palette takes it away, and a row that claimed focus
    /// while a rename was open would have a screen reader announcing the row a
    /// person is typing over rather than the name they are typing.
    pub(crate) fn a11y_list_focused(&self) -> bool {
        self.menu.is_none()
            && self.palette.is_none()
            && self.search.is_none()
            && self.edits.naming.is_none()
            && self.typing_path.is_none()
            && self.quick_look.is_none()
    }

    /// What a row is, beyond its name: a folder, or a file with a size.
    pub(crate) fn a11y_row_description(&self, id: sk_model::EntryId) -> String {
        let pane = self.panes.focused();
        let flags = pane.listing.flags(id);
        if flags.has(sk_model::Flags::DIR) {
            return sk_model::i18n::lookup("kind.folder").to_owned();
        }
        sk_ui::format::size(pane.listing.size(id))
    }

    /// The sidebar's rectangles, which the frame recorded.
    pub(crate) fn a11y_places(&self) -> &[Area] {
        &self.hits.places
    }

    /// What the sidebar's row at `at` is called.
    pub(crate) fn a11y_place_name(&self, at: usize) -> String {
        self.sidebar.rows().get(at).map(|row| row.label.clone()).unwrap_or_default()
    }

    /// The one field being typed into: what it is, its text, and where it is.
    pub(crate) fn a11y_field(&self) -> Option<(String, &crate::field::Field, Area)> {
        if let Some(naming) = self.edits.naming.as_ref() {
            let area = self.hits.name_field?;
            return Some((sk_model::i18n::lookup("field.name").to_owned(), &naming.field, area));
        }
        if let Some(path) = self.typing_path.as_ref() {
            let area = crate::view::path_field(&self.placement.here, &self.theme.metrics);
            return Some((sk_model::i18n::lookup("field.path").to_owned(), path, area));
        }
        if let Some(palette) = self.palette.as_ref() {
            let area = self.hits.palette_field?;
            return Some((
                sk_model::i18n::lookup("field.palette").to_owned(),
                &palette.query,
                area,
            ));
        }
        if let Some(search) = self.search.as_ref() {
            let area = self.hits.search_field?;
            return Some((sk_model::i18n::lookup("field.search").to_owned(), &search.query, area));
        }
        None
    }

    /// The open submenu's rows, the same way, and which row of the menu they
    /// hang off.
    pub(crate) fn a11y_submenu(&self) -> Option<(usize, Vec<A11yRow>)> {
        let sub = self.menu.as_ref()?.sub.as_deref()?;
        let rows = sub
            .menu
            .items
            .iter()
            .zip(&self.hits.submenu)
            .enumerate()
            .map(|(at, (item, area))| (item.name.clone(), *area, sub.menu.active == Some(at)))
            .collect();
        Some((sub.owner, rows))
    }

    /// The open menu's rows: what each says, where it is, and which is on.
    pub(crate) fn a11y_menu(&self) -> Vec<A11yRow> {
        if let Some(menu) = self.menu.as_ref() {
            // While the open submenu has a highlight of its own, the owner is
            // not the focus — one focus, or a reader reads two.
            let sub_lit = menu.sub.as_deref().is_some_and(|sub| sub.menu.active.is_some());
            return menu
                .items
                .iter()
                .zip(&self.hits.menu)
                .enumerate()
                .map(|(at, (item, area))| {
                    (item.name.clone(), *area, menu.active == Some(at) && !sub_lit)
                })
                .collect();
        }
        if let Some(palette) = self.palette.as_ref() {
            // The palette draws a window of its results, so the rectangles start
            // at `scroll` and the names have to start there too — zipping from
            // zero would name the first result over the rectangle of whichever
            // one is drawn first.
            return self
                .hits
                .palette
                .iter()
                .enumerate()
                .filter_map(|(offset, area)| {
                    let at = palette.scroll + offset;
                    let row = palette.row(at)?;
                    Some((row.name.clone(), *area, palette.active == at))
                })
                .collect();
        }
        Vec::new()
    }

    /// The quick look, for the text it is showing.
    pub(crate) fn a11y_look(&self) -> Option<&crate::quicklook::QuickLook> {
        self.quick_look.as_ref()
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
    fn drag_modifier(&mut self, copying: bool) {
        if let Some(drag) = self.files_drag.as_mut().filter(|drag| !drag.paths.is_empty())
            && drag.copying != copying
        {
            drag.copying = copying;
            self.dirty = true;
        }
    }

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
        let took = self.edits.paste_all(drag.paths, into, !copying, &self.jobs);
        self.said_it_is_busy(took);
        self.dirty = true;
    }

    /// Chooses what a press on a row selects, and whether it may become a drag.
    ///
    /// **A plain press on an already-selected row changes nothing yet.** It
    /// means either "just this one" or "pick all of these up", and which it was
    /// is not known until the pointer moves or does not — so the selection is
    /// deferred to the release. Deciding here is what made dragging three files
    /// drag one of them.
    /// A press taken by something drawn over the list: a panel, a menu, a field.
    ///
    /// **First, and in this order, because each is more specific than what is
    /// under it.** Split out of [`Self::press`] to keep that function inside the
    /// hundred-line limit; it is one idea — a surface over the list gets the press
    /// before the list does — and the chain below it is the other, which is the
    /// window's own furniture and then the rows.
    fn press_in_an_overlay(
        &mut self,
        position: Point,
        held: (bool, u8),
        commands: &Registry<Self>,
    ) -> bool {
        let (shift, clicks) = held;
        // First, and it takes every press: the question has no dismissal, so a
        // press that fell through to the list behind it would move a selection
        // the queue is about to act on.
        if self.press_in_the_clash(position) {
            return true;
        }
        if self.press_in_the_erasing(position) {
            return true;
        }
        if self.press_in_the_status(position) {
            return true;
        }
        if self.palette.is_some() {
            self.palette_press(position, clicks, commands);
            return true;
        }
        if self.press_in_the_search(position, clicks) {
            return true;
        }
        if self.properties.take().is_some() {
            self.dirty = true;
            return true;
        }
        if self.press_in_the_openers(position) {
            return true;
        }
        if self.press_in_the_settings(position) {
            return true;
        }
        if self.press_in_the_crumb_menu(position) {
            return true;
        }
        if self.menu.is_some() {
            self.menu_press(position, commands);
            return true;
        }
        // **After the menu, because a menu opens over the panel.** The preview's
        // own context menu is drawn on top of the text it acts on, so a handler
        // that took every press inside the panel first swallowed the click on the
        // menu row — and started an empty selection where the row was, which is
        // how `Copy` came to copy nothing.
        if self.press_in_the_quick_look(position, shift, clicks) {
            return true;
        }
        false
    }

    /// A press on a band's heading collapses it, or opens it. Whether it was one.
    ///
    /// **Asked before the row hit test.** A heading is not an entry, so
    /// [`crate::groups::Bands::index_at`] answers `None` on one — and the caller
    /// reads `None` as empty space and clears the selection. This is the other half
    /// of that sentence: a heading is not an entry, and it is a control.
    fn press_on_a_heading(&mut self, position: Point) -> bool {
        let offset = self.panes.focused().scroll.offset();
        let Some(band) = self.cells().heading_under(position.x, position.y, offset) else {
            return false;
        };
        self.toggle_band(band);
        true
    }

    /// A press below the last row: nothing is chosen, and a rubber band is armed.
    fn press_on_empty_space(&mut self, position: Point) {
        self.panes.focused_mut().selection.clear();
        self.panes.focused_mut().selection.set_cursor(None);
        self.band = Some((position, position));
        self.dirty = true;
    }

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
                copying: false,
                directory: self.panes.focused().listing.flags(id).has(sk_model::Flags::DIR),
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
        let chose = self.quick_look.as_mut().is_some_and(|look| {
            // Both, for the reason above: two selection models, one release.
            let text = std::mem::take(&mut look.dragging);
            let rendered = std::mem::take(&mut look.picking);
            text || rendered
        });
        let panned = self.look_pan.take().is_some();
        let read = std::mem::take(&mut self.page_drag);
        let divided = self.placement.dragging.take().is_some();
        // A width is part of the layout, so letting go of one is a thing to save.
        // The session is compared rather than tracked, so this only has to make
        // the frame happen.
        let sized = self.column_drag.take().is_some();
        let widened = std::mem::take(&mut self.aside_drag);
        let ordered = self.drop_the_place();
        // A press on the title bar that never moved is a click on a title bar,
        // and a title bar holds nothing to click: the path moved into the panes,
        // so there is nothing here for a press to be about.
        self.title_press = None;
        if ended
            || released
            || dragged
            || divided
            || sized
            || chose
            || widened
            || ordered
            || panned
            || read
        {
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
        // lands on that used to promise a click and do nothing. Asked of the pane
        // the button belongs to, which is where its dimming comes from.
        let over_tool = self
            .tool_at(position)
            .map(|(id, tool)| self.panes.leaf(id).is_some_and(|leaf| leaf.showing().allows(tool)));
        // The status strip's Pause and Stop are here rather than with the panel
        // buttons above: the strip is not a panel, and they are drawn only while
        // there is an operation to stop.
        let on_the_strip = [self.hits.pause, self.hits.cancel]
            .iter()
            .flatten()
            .any(|area| area.contains(position.x, position.y));
        if over_tool == Some(true)
            || on_the_strip
            || self.crumb_at(position).is_some()
            || self.layout.button_at(position.x, position.y).is_some()
            || self.place_at(position).is_some()
        {
            return sk_plat::Cursor::Hand;
        }
        // Over a button that is drawn and dimmed, and nothing else is under it.
        if over_tool == Some(false) {
            return sk_plat::Cursor::Arrow;
        }
        if self.mode_at(position).is_some() {
            return sk_plat::Cursor::Hand;
        }
        // Before the header's hand, for the same reason the press is: the
        // boundary is inside the header and is the more specific thing there.
        if self.column_drag.is_some() || self.column_edge_at(position).is_some() {
            return sk_plat::Cursor::Divider { horizontal: true };
        }
        if self.aside_drag || self.on_the_aside_divider(position) {
            return sk_plat::Cursor::Divider { horizontal: true };
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
            && crate::view::path_field(&self.placement.here, &self.theme.metrics)
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
    fn crumb_at(&self, position: Point) -> Option<(crate::panes::PaneId, usize)> {
        let hit = self
            .hits
            .crumbs
            .iter()
            .find(|hit| hit.area.width > 0.0 && hit.area.contains(position.x, position.y))?;
        // The deepest component *of that pane's own bar*: every leaf pushes its
        // own run of crumbs, so "the last one" is a question per pane.
        let deepest = self
            .hits
            .crumbs
            .iter()
            .filter(|other| other.leaf == hit.leaf)
            .map(|other| other.depth)
            .max()?;
        (hit.depth != deepest).then_some((hit.leaf, hit.depth))
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

    /// Which breadcrumb separator is under a point, and whose bar it is in.
    fn separator_at(&self, position: Point) -> Option<(crate::panes::PaneId, usize)> {
        self.hits
            .separators
            .iter()
            .find(|hit| hit.area.width > 0.0 && hit.area.contains(position.x, position.y))
            .map(|hit| (hit.leaf, hit.depth))
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
        self.close_menu();
        self.palette = None;
        self.crumb_menu = None;
        self.crumb_pending = Some(depth);
        self.dirty = true;
        let parent = self.path_to_crumb(depth);
        let focus = self.panes.focus();
        let at = self
            .hits
            .separators
            .iter()
            .find(|hit| hit.leaf == focus && hit.depth == depth)
            .map_or(0.0, |hit| hit.area.x);
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
                let diagnostic = named(commands, event) == Some("help.dump");
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
        if self.close_menu() {
            self.dirty = true;
        }
        // A secondary click inside a text field belongs to the field. Without
        // this the path bar answered one with the compositor's window menu — it
        // was the title bar then — over a field with a caret in it.
        if self.typing_path.is_some()
            && crate::view::path_field(&self.placement.here, &self.theme.metrics)
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
        // **A place's own menu, not the list's.** Every action on the background
        // menu is about the directory the list is showing, and a sidebar row is a
        // different directory — a menu offering New folder over `Downloads` would
        // make it somewhere else. What a place offers instead is the two things
        // that are about *it*, and the target a command needs is
        // [`App::place_target`].
        if self.layout.sidebar.contains(position.x, position.y) {
            let over = self.place_at(position).and_then(|index| self.sidebar.at(index));
            let Some(entry) = over else { return };
            // **A volume nothing has mounted is named by its device.** Its path
            // is empty on purpose — ADR 0030 — and every unmounted row's is, so
            // a target of `""` would name all of them at once and Unmount would
            // act on whichever came first.
            self.place_target = match (entry.unmounted, entry.device.clone()) {
                (true, Some(device)) => Some(device),
                _ => Some(entry.path.clone()),
            };
            self.menu_wanted = Some((position, crate::menu::ON_A_PLACE));
            return;
        }
        if self.edits.naming.is_some() {
            self.commit_name();
        }
        // **The preview's own menu first, while one is over the list.** A
        // secondary click inside a text preview is about the text, and a menu of
        // file operations over a panel covering the file would act on something
        // the person cannot see.
        if (self.looking_at_text()
            || self.looking_at_an_animation()
            || self.looking_at_a_page_with_words())
            && crate::view::quick_look_area(&self.layout).contains(position.x, position.y)
        {
            self.menu_wanted = Some((position, crate::menu::IN_A_LOOK));
            return;
        }
        let row = self.row_under(position);
        if let Some(id) = row.and_then(|row| self.panes.focused().listing.at(row))
            && !self.panes.focused().selection.contains(id)
        {
            self.panes.focused_mut().selection.replace_with(id);
        }
        let groups = if self.trash_context() && row.is_some() {
            crate::menu::IN_TRASH
        } else if row.is_some() {
            crate::menu::ON_ROW
        } else {
            crate::menu::ON_BACKGROUND
        };
        self.menu_wanted = Some((position, groups));
    }

    /// Closes the menu that is up, and forgets what it was about. Whether one
    /// was open.
    ///
    /// **One function rather than eight assignments**, because the target and the
    /// menu have to disappear together: a `place_target` that outlived its menu
    /// would make the next `place.unpin` — from the palette, from a chord — act on
    /// a row somebody clicked minutes ago.
    fn close_menu(&mut self) -> bool {
        self.place_target = None;
        self.submenu_wanted = None;
        self.stop_menu_job();
        self.menu.take().is_some()
    }

    /// Stops the job finding the menu's applications, if one is still out.
    fn stop_menu_job(&mut self) {
        if let Some(job) = self.menu_job.take() {
            job.cancel();
        }
    }

    /// Opens the context menu from the keyboard, or closes an open one.
    ///
    /// At the cursor row rather than at the pointer, because a keyboard user's
    /// attention is on the cursor and the pointer may be on another monitor.
    pub(crate) fn toggle_menu(&mut self) {
        if self.close_menu() {
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
            if self.trash_context() { crate::menu::IN_TRASH } else { crate::menu::ON_ROW }
        } else {
            crate::menu::ON_BACKGROUND
        };
        self.menu_wanted = Some((at, groups));
    }

    /// What the diagnostic line says about the column view.
    ///
    /// The aim as `x:y` or `none` — the shape every other aim on that line uses,
    /// because a harness made to parse `Some((768.0, 123.0))` would be parsing
    /// Rust's `Debug` rather than a report — and which switcher button the pointer
    /// is on.
    fn column_state(&self) -> (String, &'static str, String, String) {
        let aim =
            self.column_aim().map_or_else(|| "none".to_owned(), |(x, y)| format!("{x:.1}:{y:.1}"));
        let hover = match self.hovered_mode.map(|(_, mode)| mode) {
            Some(mode) => mode.token(),
            None => "none",
        };
        // Which column the pointer is over, which is where a scroll would go. The
        // offsets below say what moved; this says what *should* have.
        let over =
            self.scrolling_column().1.map_or_else(|| "none".to_owned(), |depth| depth.to_string());
        // Every ancestor column's own offset, in order, so a scenario can say
        // *which* column the wheel reached rather than only that something moved.
        let offsets = self
            .panes
            .focused()
            .trail
            .iter()
            .map(|level| format!("{:.0}/{:.0}", level.scroll.offset(), level.scroll.target()))
            .collect::<Vec<_>>()
            .join(",");
        (aim, hover, offsets, over)
    }

    /// The middle of the chosen row in the deepest ancestor column, for a test to
    /// aim at.
    ///
    /// **From the rectangle the frame drew it in**, which is docs/12's rule after
    /// a chrome aim taken from a neighbour moved without it. `None` in every other
    /// mode and while the column's listing is still arriving, so a scenario that
    /// waits for it is waiting for something real.
    fn column_aim(&self) -> Option<(f32, f32)> {
        let pane = self.panes.focused();
        if pane.mode != crate::cells::Mode::Columns {
            return None;
        }
        let depth = pane.trail.len().checked_sub(1)?;
        let level = pane.trail.get(depth)?;
        let chosen = level.chosen?;
        let row =
            (0..level.dir.listing.rows()).find(|row| level.dir.listing.at(*row) == Some(chosen))?;
        let area =
            crate::trail::areas(self.placement.here.body, pane.trail.len(), pane.across.offset())
                .nth(depth)?;
        let cells = crate::groups::Bands::new(
            crate::cells::Mode::Columns,
            area,
            self.theme.metrics.row_height,
            &[],
            level.dir.listing.rows(),
            crate::groups::Closed::default(),
        );
        let at = cells.at(row, level.scroll.offset());
        Some((at.x + at.width / 2.0, at.y + at.height / 2.0))
    }

    /// The ancestor column and row under a point, in the pane the point is in.
    fn column_press(&self, position: Point) -> Option<(usize, usize)> {
        let leaf =
            self.placement.leaves.iter().find(|leaf| leaf.body.contains(position.x, position.y))?;
        let pane = self.panes.leaf(leaf.id)?.showing();
        if pane.mode != crate::cells::Mode::Columns {
            return None;
        }
        crate::trail::hit(
            &pane.trail,
            leaf.body,
            pane.across.offset(),
            self.theme.metrics.row_height,
            position,
        )
    }

    /// Opens what was pressed in the ancestor column at `depth`.
    ///
    /// A folder becomes the new deepest column; anything else makes its own
    /// column the deepest and is selected there. Both are navigations, so
    /// [`Self::settle_trails`] rebuilds the trail on the next frame — keeping
    /// every level that is still an ancestor — and nothing here touches the
    /// vector.
    fn open_from_column(&mut self, depth: usize, row: usize) {
        let pane = self.panes.focused();
        let Some((directory, name, is_folder)) = crate::trail::entry(&pane.trail, depth, row)
        else {
            return;
        };
        let pane = self.panes.focused_mut();
        if is_folder {
            pane.open(directory.join(&name), &self.jobs);
        } else {
            pane.open_at(directory, name, &self.jobs);
        }
        self.dirty = true;
    }

    /// Which listing row is under a point, or `None` outside the list.
    fn row_under(&self, position: Point) -> Option<usize> {
        self.cells().index_at(position.x, position.y, self.panes.focused().scroll.offset())
    }

    /// The middle of the check box a press would land on, or nothing.
    ///
    /// **From the program, not from a harness's own arithmetic.** A scenario that
    /// worked the box out from `row_h` and the icon column would be a second
    /// implementation of [`crate::view::check_of`] — and this file has already
    /// learned what happens then: a coordinate derived from another region is a
    /// coordinate that moves. It is `none` until something is hovered, which is
    /// also the condition for the box existing at all.
    fn check_aim(&self) -> String {
        let pane = self.panes.focused();
        let Some(row) = pane.hover else { return "none".to_owned() };
        let cell = self.cells().at(row, pane.scroll.offset());
        let box_ =
            crate::view::check_of(pane.mode, &self.placement.here, cell, &self.theme.metrics);
        // The shape every other aim on that line uses, because a harness made to
        // parse `Some((768.0, 123.0))` would be parsing Rust's `Debug`.
        format!("{:.1}:{:.1}", box_.x + box_.width / 2.0, box_.y + box_.height / 2.0)
    }

    /// Whether a press at `position` landed on `row`'s check box.
    ///
    /// **Hover is part of the question, not context for it.** The box is drawn only
    /// under the pointer, so asking only about the rectangle makes a target out of
    /// a square that is not on screen — and the pointer is not always where the
    /// window last saw it: a window that maps underneath a pointer which has not
    /// moved gets no `enter` from this compositor, so `hover` is `None` while the
    /// pointer is genuinely over a row. A press there has to be an ordinary press
    /// on the row rather than a toggle nobody could see coming.
    fn check_under(&self, row: usize, position: Point) -> bool {
        let pane = self.panes.focused();
        if pane.hover != Some(row) {
            return false;
        }
        let cell = self.cells().at(row, pane.scroll.offset());
        crate::view::check_of(pane.mode, &self.placement.here, cell, &self.theme.metrics)
            .contains(position.x, position.y)
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
    fn palette_press(&mut self, position: Point, clicks: u8, commands: &Registry<Self>) {
        if let Some(offset) = self.palette_at(position) {
            self.run_palette_row(offset, commands);
            return;
        }
        if let Some(area) = self.hits.palette_field.filter(|a| a.contains(position.x, position.y)) {
            let size = self.theme.metrics.path_size;
            let at =
                self.place_caret_in(Claim::Palette, area, size, crate::palette::INSET, position.x);
            self.chose_a_run(Claim::Palette, clicks, at);
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
        // Taken out and put back, so the registry can be asked about `self`
        // while the menu's rows are being written.
        let Some(mut menu) = self.menu.take() else { return };
        if menu.settle(&|id| self.runnable(commands, id)) {
            self.dirty = true;
        }
        self.menu = Some(menu);
    }

    /// A press while a menu is open: it runs a row, or it dismisses.
    ///
    /// **A dismissing press does nothing else.** The click that closes a menu
    /// must not also land on the row underneath it — which is what every menu on
    /// the desktop does, and the difference between closing a menu and trashing
    /// the file the menu was over.
    fn menu_press(&mut self, position: Point, commands: &Registry<Self>) {
        self.dirty = true;
        // Inside the submenu: one of its rows, or its padding.
        if self.hits.submenu_panel.is_some_and(|panel| panel.contains(position.x, position.y)) {
            let chosen = self
                .submenu_at(position)
                .and_then(|row| self.menu.as_ref()?.sub.as_ref()?.menu.items.get(row))
                .and_then(crate::menu::Item::choice);
            self.choose(chosen, commands);
            return;
        }
        let row = self.menu_at(position);
        // **A press on a row that opens keeps the menu.** Its submenu opens, or
        // stays open if it was, and nothing closes.
        if let Some(row) = row
            && self.menu.as_ref().is_some_and(|menu| {
                menu.items.get(row).is_some_and(|item| item.opens() && item.enabled)
            })
        {
            self.expand_menu_row(row, Some(position), commands);
            return;
        }
        let chosen = row
            .and_then(|row| self.menu.as_ref()?.items.get(row))
            .and_then(crate::menu::Item::choice);
        self.choose(chosen, commands);
    }

    /// Closes the menu and does what the chosen row does — nothing, for a press
    /// or a key that dismissed.
    ///
    /// **A dismissing press does nothing else.** The click that closes a menu
    /// must not also land on the row underneath it — which is what every menu on
    /// the desktop does, and the difference between closing a menu and trashing
    /// the file the menu was over. The target outlives the menu by exactly one
    /// command, which is the command the menu was opened for.
    fn choose(&mut self, chosen: Option<crate::menu::Choice>, commands: &Registry<Self>) {
        self.menu = None;
        self.stop_menu_job();
        self.dirty = true;
        match chosen {
            Some(crate::menu::Choice::Run(id)) => {
                if let Some(command) = commands.find(&id) {
                    commands.run(command, self);
                }
            }
            Some(crate::menu::Choice::Launch { entry, path }) => {
                self.start_application(entry, path);
            }
            None => {}
        }
        self.place_target = None;
    }

    /// Opens the submenu under a row of the open menu, in place of whichever was
    /// open, and puts the highlight on the row.
    ///
    /// `from` is the pointer, when a pointer asked. The same row asked for again
    /// keeps its submenu and moves the apex of the triangle
    /// [`crate::menu::Sub::heading_in`] draws, so the triangle follows the hand
    /// along the row.
    fn expand_menu_row(&mut self, row: usize, from: Option<Point>, commands: &Registry<Self>) {
        let Some(mut menu) = self.menu.take() else { return };
        if menu.sub.as_ref().is_some_and(|sub| sub.owner == row) {
            if let Some(sub) = menu.sub.as_mut()
                && from.is_some()
            {
                sub.from = from;
            }
        } else {
            menu.collapse();
            if !menu.expand(row, commands, |id| self.runnable(commands, id), from) {
                menu.hover(Some(row));
            }
            self.dirty = true;
        }
        // **The Open with row's job starts when the row opens, not with the
        // menu.** Started with the menu it ran on the pane's device lane ahead
        // of whatever the same menu was opened to do — measured, a Move to Trash
        // chosen a moment later waited seconds behind it for its listing to come
        // back. The submenu says `Reading…` for the frames the job takes, which
        // is what `Ctrl+Shift+O` has always cost.
        let asks = match menu.items.get(row).map(|item| &item.kind) {
            Some(crate::menu::Kind::OpenWith { path, found: None }) if self.menu_job.is_none() => {
                Some(path.clone())
            }
            _ => None,
        };
        self.menu = Some(menu);
        if let Some(path) = asks {
            self.ask_which_applications(path, crate::openers::Asker::Menu);
        }
    }

    /// The pointer moved with a menu open. Whether it is on a row a click would
    /// do something on — `menu_at` answers where the pointer is and not whether
    /// anything is there to run, and a dimmed row reported as a hand is the
    /// interface promising a click that `menu_press` then drops.
    fn hover_menu(&mut self, position: Point) -> bool {
        if self.hits.submenu_panel.is_some_and(|panel| panel.contains(position.x, position.y)) {
            return self.hover_submenu_row(position);
        }
        let row = self.menu_at(position);
        if self.holds_the_submenu_open(position, row) {
            return row.is_some();
        }
        let Some(menu) = self.menu.as_mut() else { return false };
        let item = row.and_then(|row| menu.items.get(row));
        let clickable = item.is_some_and(|item| item.enabled);
        if let Some(row) = row
            && item.is_some_and(|item| item.enabled && item.opens())
        {
            // A request rather than the submenu itself, because building its
            // rows needs the registry and a pointer's motion does not carry it.
            self.submenu_wanted = Some((row, position));
            return true;
        }
        let mut changed = menu.collapse();
        changed |= menu.hover(row);
        if changed {
            self.dirty = true;
        }
        clickable
    }

    /// The pointer is inside the submenu: its rows take the highlight, and the
    /// owner keeps its own.
    fn hover_submenu_row(&mut self, position: Point) -> bool {
        let row = self.submenu_at(position);
        let Some(sub) = self.menu.as_mut().and_then(|menu| menu.sub.as_mut()) else {
            return false;
        };
        if sub.menu.hover(row) {
            self.dirty = true;
        }
        row.and_then(|row| sub.menu.items.get(row)).is_some_and(|item| item.enabled)
    }

    /// Whether a pointer over the parent leaves the open submenu where it is:
    /// on the owner's row, off every row, or inside the triangle towards the
    /// submenu — [`crate::menu::Sub::heading_in`] — which is a way there rather
    /// than a row.
    fn holds_the_submenu_open(&mut self, position: Point, row: Option<usize>) -> bool {
        let Some(sub) = self.menu.as_mut().and_then(|menu| menu.sub.as_mut()) else {
            return false;
        };
        if row == Some(sub.owner) {
            sub.from = Some(position);
            return true;
        }
        let owner = self.hits.menu.get(sub.owner).copied().unwrap_or_default();
        let panel = self.hits.submenu_panel.unwrap_or_default();
        row.is_none() || sub.heading_in(position, panel, owner)
    }

    /// Which row of the open submenu is under a point, if one is open.
    fn submenu_at(&self, position: Point) -> Option<usize> {
        self.menu.as_ref()?.sub.as_ref()?;
        self.hits.submenu.iter().position(|area| area.contains(position.x, position.y))
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
        let in_a_submenu = menu.sub.is_some();
        match event.key {
            // **One level at a time**, and Right on a row that opens nothing
            // does nothing rather than dismissing: a person walking a menu with
            // the arrows has not asked to leave it.
            Key::Named(Named::Escape) if in_a_submenu => {
                menu.collapse();
            }
            Key::Named(Named::Escape) => {
                self.close_menu();
            }
            Key::Named(Named::Left) if bare => {
                menu.collapse();
            }
            Key::Named(Named::Down) if bare => {
                let inner = menu.innermost_mut();
                inner.step(true);
                inner.reveal(room);
            }
            Key::Named(Named::Up) if bare => {
                let inner = menu.innermost_mut();
                inner.step(false);
                inner.reveal(room);
            }
            Key::Named(Named::Right) if bare => {
                let opener = (!in_a_submenu).then_some(menu.active).flatten().filter(|row| {
                    menu.items.get(*row).is_some_and(|item| item.opens() && item.enabled)
                });
                if let Some(row) = opener {
                    self.expand_menu_row_by_key(row, commands);
                }
            }
            Key::Named(Named::Enter) => {
                let inner = menu.innermost_mut();
                let opener = inner.active.filter(|row| {
                    inner.items.get(*row).is_some_and(|item| item.opens() && item.enabled)
                });
                if let Some(row) = opener {
                    self.expand_menu_row_by_key(row, commands);
                } else {
                    let chosen = inner.active_item().and_then(crate::menu::Item::choice);
                    self.choose(chosen, commands);
                }
            }
            _ => {
                // **The diagnostic report passes through every mode, this one
                // included.** A report that cannot be taken while a menu is open
                // cannot describe the menu, and it is the only way the GUI
                // harness can see one at all. Asked of the registry rather than
                // matched on the key, so rebinding the report does not quietly
                // make the menu opaque again.
                let diagnostic = named(commands, event) == Some("help.dump");
                if diagnostic {
                    return false;
                }
                // Anything else dismisses, and dismisses only: a key that
                // closes a menu must not also act behind it, which is the rule
                // the pointer already follows.
                self.close_menu();
                self.dirty = true;
            }
        }
        self.dirty = true;
        true
    }

    /// Right or Enter on a row that opens: the submenu, with its first row lit,
    /// because a key has nowhere to point.
    fn expand_menu_row_by_key(&mut self, row: usize, commands: &Registry<Self>) {
        self.expand_menu_row(row, None, commands);
        if let Some(sub) = self.menu.as_mut().and_then(|menu| menu.sub.as_mut()) {
            sub.menu.step(true);
        }
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
        let commands = self.commands();
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
            let clickable = self.hover_menu(position);
            self.point(if clickable { sk_plat::Cursor::Hand } else { sk_plat::Cursor::Arrow });
            return true;
        }
        // **A quick look is over the whole window, so nothing under it hovers.**
        // The panel already keeps the *rows* beneath it from lighting up —
        // `hits.panels` — but the chrome, the sidebar, the tab strips and the
        // dividers are not rows, and every one of them went on tracking a
        // pointer that was reading a file, offering clicks that only close the
        // panel. Its own controls and its own text are what is left.
        if self.quick_look.is_some() {
            let control = self.look_control_at(position);
            if self.hovered_look != control {
                self.hovered_look = control;
                self.dirty = true;
            }
            self.nothing_hovers_under_the_look();
            // **Before the return.** A drag that leaves the panel still extends
            // the selection, which is what dragging past an edge means in every
            // text view — and this branch is now the only place a motion event
            // reaches while a look is open.
            if self.extend_the_rendered_choice(position)
                || self.extend_the_text_choice(position)
                || self.extend_the_page_choice(position)
                || self.pan_the_look(position)
            {
                self.dirty = true;
            }
            let inside =
                crate::view::quick_look_area(&self.layout).contains(position.x, position.y);
            // The I-beam over a page's words as well as over a text file's, which
            // is the only thing that says a page can be selected out of.
            let over_text = inside
                && (self.looking_at_text()
                    || self.quick_look.as_ref().is_some_and(|look| {
                        crate::view::page_word_at(
                            look,
                            crate::view::quick_look_area(&self.layout),
                            (position.x, position.y),
                        )
                    }));
            // A zoomed picture can be dragged, and a pointer that says so is the
            // only thing that tells somebody it can.
            let grabbable =
                inside && self.quick_look.as_ref().is_some_and(crate::quicklook::QuickLook::zoomed);
            self.point(match (control.is_some(), over_text, grabbable) {
                (true, _, _) => sk_plat::Cursor::Hand,
                (false, true, _) => sk_plat::Cursor::Text,
                (false, false, true) => sk_plat::Cursor::Grabbing,
                (false, false, false) => sk_plat::Cursor::Arrow,
            });
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

    /// Puts out every highlight that belongs to something the panel covers.
    ///
    /// **Cleared rather than left alone**, because a hover is a fact about where
    /// the pointer is and the pointer is over a panel: a button still lit from
    /// before the panel opened is a button that looks pressable and is not.
    fn nothing_hovers_under_the_look(&mut self) {
        let lit = self.hovered_button.is_some()
            || self.hovered_crumb.is_some()
            || self.hovered_tool.is_some()
            || self.hovered_mode.is_some()
            || self.sidebar.hover.is_some()
            || self.placement.hovered_tab.is_some()
            || self.placement.hovered_plus
            || self.placement.hovered_divider.is_some();
        if !lit {
            return;
        }
        self.hovered_button = None;
        self.hovered_crumb = None;
        self.hovered_tool = None;
        self.hovered_mode = None;
        self.sidebar.hover = None;
        self.placement.hovered_tab = None;
        self.placement.hovered_plus = false;
        self.placement.hovered_divider = None;
        self.dirty = true;
    }

    fn moved(&mut self, position: Point) {
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
        // A column boundary that has been taken hold of keeps the pointer until
        // the button comes up, wherever it wanders — including out of the header
        // and into the list, which is where a hand ends up when it is dragging a
        // line rather than aiming at one.
        if let Some((id, edge)) = self.column_drag {
            self.drag_the_column(id, edge, position);
            return;
        }
        if self.aside_drag {
            self.drag_the_aside(position);
            return;
        }
        if self.place_drag.is_some() {
            self.drag_the_place(position);
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
        self.hover_the_chrome(position);
        // **Before the hover work below it takes an early return.** A drag that
        // leaves the panel still extends the selection, which is what dragging
        // past an edge means in every text view.
        if self.extend_the_rendered_choice(position) || self.extend_the_text_choice(position) {
            self.dirty = true;
        }
        self.hover_the_history(position);
        if self.hover_the_sidebar(position) {
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
        let cursor = self.cursor_at(position);
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
        // The strip the frame drew the thumb in, widened to the left: the thumb is
        // eight logical pixels and a pointer is not. To the pane's edge on the
        // right, because that edge is the strip's — but no further left than the
        // strip itself, since the space between the table and the strip is nothing
        // and a press in nothing clears the selection.
        let track = self.placement.here.track();
        let inside_x = position.x >= track.x - 8.0 && position.x < self.placement.here.body.right();
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
    fn before_frame(&mut self, commands: &Registry<Self>) -> ((u32, u32), f32) {
        let physical = self.window.physical();
        if physical != self.renderer.size() {
            self.renderer.resize(physical);
            self.physical = physical;
        }
        let scale = self.window.scale();
        self.settle_rows();
        self.settle_menu_state(commands);
        (physical, scale)
    }

    // The same shape `assemble` is, and settled the same way: almost all of it
    // is one struct literal with one call site, over fields the destructure has
    // already split the borrow of. Two halves would each hold part of an `App`
    // and neither could be read without the other.
    #[expect(clippy::too_many_lines, reason = "a struct literal with one call site")]
    fn render(&mut self, commands: &Registry<Self>, turn: Instant) {
        // **Before the frame, because the rows are the layout.** A width that has
        // not changed costs a comparison; one that has costs a pass over four
        // hundred lines, which is what a resize is.
        self.fit_the_look();
        self.fit_the_zoom();
        let (physical, scale) = self.before_frame(commands);
        self.list.begin(physical);
        // Two answers taken before the borrow is split, because both ask `self`
        // a question about the pointer and the destructure below hands the
        // pieces out.
        let aside_grabbed = self.aside_drag || self.on_the_aside_divider(self.pointer);
        let place_drop = self.place_drag.filter(|drag| drag.active).map(|drag| drag.at);
        // The same, for the settings panel: `values` assembles from six places
        // and `marks` is a copy, so both are taken while `self` is still whole.
        let settings_values = self.values();
        let settings_marks = self.marks;

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
            previews,
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
            watch_failure,
            notice,
            capabilities,
            hovered_button,
            hovered_crumb,
            hovered_separator,
            crumb_menu,
            hovered_tool,
            hovered_look,
            hovered_mode,
            sidebar,
            edits,
            quick_look,
            history_open,
            history_scroll,
            hovered_edit,
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
            panel,
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
            let said = indexes.counted().map_or_else(
                || sk_model::i18n::plural("index.building", count as u64, &[&done]),
                |total| {
                    let all = sk_ui::format::grouped(total);
                    sk_model::i18n::plural("index.building_of", total as u64, &[&done, &all])
                },
            );
            // **And which root of how many**, where a queue is running: three
            // filesystems is three walks, and a line that said the same thing for
            // eleven minutes would be a line nobody could tell from a stuck one.
            let owed = indexes.owed().len();
            if owed == 0 {
                said
            } else {
                let at = 1;
                let all = owed + 1;
                format!("{said} · {}", sk_model::i18n::format("index.of_roots", &[&at, &all]))
            }
        });
        let progress = edits.progress_line().or(indexing);
        let doomed: Option<(Vec<String>, usize, bool, bool)> = erasing.as_ref().map(|asking| {
            let names = asking
                .paths
                .iter()
                .take(crate::view::ERASE_NAMES)
                .map(|path| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .collect();
            (names, asking.paths.len(), asking.on == Confirm::Delete, asking.forever)
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
                directory: drag.directory,
                copying: drag.copying,
            }
        });
        let chrome = Chrome {
            keys: rows.as_deref(),
            stats: *show_stats,
            quick_look: quick_look.as_ref(),
            edits: Some(edits),
            history: (*history_open, history_scroll.offset()),
            hovered_edit: *hovered_edit,
            typed: (!typed.is_empty()).then_some(typed.as_str()),
            band: *band,
            watching: *watching,
            preview_missing: previews.missing.contains(&sk_preview::Kind::Image),
            watch_failure: watch_failure.map(sk_fs::watch::Failure::message),
            // Progress wins over a notice while an operation is running: what a
            // person wants to see during a copy is the copy, and the notice that
            // preceded it has already been read.
            notice: progress.as_deref().or(notice.as_ref().map(|(message, _)| message.as_str())),
            hovered_button: *hovered_button,
            hovered_crumb: *hovered_crumb,
            hovered_separator: *hovered_separator,
            crumb_menu: crumb_menu.as_ref(),
            hovered_tool: *hovered_tool,
            hovered_look: *hovered_look,
            hovered_mode: *hovered_mode,
            places: sidebar.rows(),
            hovered_place: sidebar.hover,
            aside_grabbed,
            place_drop,
            drop_row: dropping.and_then(|drag| drag.landing.row),
            drop_tab: dropping.and_then(|drag| drag.landing.tab),
            drop_pane: dropping.and_then(|drag| drag.landing.into_pane),
            carrying: carrying.as_ref(),
            search: search.as_ref(),
            clash: edits.clash.as_ref(),
            erasing: doomed
                .as_ref()
                .map(|(names, total, on, forever)| (names.as_slice(), *total, *on, *forever)),
            answering: edits.answering,
            for_the_rest: edits.for_the_rest,
            // The copy's bar if one is running, the index build's otherwise. Both
            // are a real fraction of a counted total or nothing at all.
            progress: edits.fraction().or_else(|| indexes.fraction()),
            // Only where there is something a latch can hold. See `can_pause`.
            pausable: edits.busy(),
            resumable: edits.resumable(),
            paused: edits.held(),
            naming: edits.naming.as_ref(),
            filtering: *filtering,
            types: desktop.as_deref().map(|it| it.mime.as_ref()),
            window: *window_state,
            menu: menu.as_ref(),
            path_entry: typing_path.as_ref(),
            palette: palette.as_ref(),
            properties: properties.as_ref(),
            openers: openers.as_ref(),
            settings: panel.as_ref().map(|it| (it, settings_values, settings_marks)),
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
             frame {:.1}; {} on {}, surface {}×{} at {}%; platform: {}; bus: {}",
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
            if self.on_the_bus.is_empty() { "none" } else { &self.on_the_bus },
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
