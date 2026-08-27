//! One frame, rendered with no display server and written to a PNG.
//!
//! Two jobs, and the second is the reason it lives in the product rather than in
//! a test:
//!
//! * **The golden-frame harness.** `docs/04-architecture.md` wants offscreen
//!   frames diffed against references at 100/125/150/200% on llvmpipe, so CI
//!   needs no GPU. This renders them.
//! * **Looking at the thing.** A renderer that can only be judged by running it
//!   on a machine with a screen is a renderer nobody reviews. This makes a frame
//!   a file.
//!
//! The frame is deliberately not empty: a fixed set of rows is selected, one
//! carries the cursor and one is hovered, and every visible folder is measured —
//! so a capture exercises the selection shape, the focus ring, the hover tint and
//! the size column rather than just the text.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sk_gpu::{DrawList, FrameStats, capture as png};
use sk_model::{Problem, ProblemKind};
use sk_plat::Size;
use sk_text::LayoutCache;
use sk_ui::{Painter, Theme};

use crate::frames::Frames;
use crate::headless::{READ_DEADLINE, Session};
use crate::layout::{Frame, Layout};
use crate::pane::Pane;
use crate::view::{self, Chrome, Diagnostics};

/// Rows selected in a capture, and which of them carries the cursor.
const SELECTED: std::ops::Range<usize> = 2..5;
const CURSOR: usize = 3;
const HOVERED: usize = 7;

/// Rows a capture will measure folder totals for.
const MEASURED: usize = 120;

/// How many frames a capture may draw before giving up on a warm atlas.
const MAX_PASSES: usize = 8;

/// What to capture.
pub struct Capture {
    /// Where the PNG goes.
    pub file: PathBuf,
    /// The directory to read.
    pub path: PathBuf,
    /// Logical size.
    pub logical: (u32, u32),
    /// The scale as a numerator over 120, exactly as a compositor sends it.
    pub scale_120: u32,
    /// Whether to use the dark palette.
    pub dark: bool,
    /// Whether to draw the keyboard overlay.
    pub keys: bool,
    /// Whether to draw the statistics overlay.
    pub stats: bool,
    /// Whether to draw the context menu.
    pub menu: bool,
    /// Whether to draw the path bar as a field being typed into.
    pub path_entry: bool,
    /// Whether to draw the command palette, with a query typed into it.
    pub palette: bool,
    /// Whether to draw the find popup, with a query narrowing the list.
    pub filter: bool,
    /// Whether to draw the sidebar.
    pub sidebar: bool,
    /// Whether to draw the window frame the program draws for itself.
    ///
    /// A capture has no compositor to ask, so it is told. Without this the
    /// frame — the one GNOME requires, with its three buttons and its resize
    /// band — has no golden coverage at all.
    pub own_frame: bool,
}

/// The background menu, in the state a fresh window would have it in.
///
/// Nothing has been done, so Undo and Redo are dimmed, and nothing has been
/// copied, so Paste is. Stated rather than asked because a capture has no `App`
/// for the registry to ask — the same trade the navigation buttons' `tools`
/// makes, for the same reason. Highlighted, because that is the state a pointer
/// holds a menu in for as long as it is open.
fn sample_menu(layout: &Layout) -> Option<crate::menu::Menu> {
    let at = sk_plat::Point::new(layout.body.x + 140.0, layout.body.y + 96.0);
    let mut menu =
        crate::menu::build(crate::menu::ON_BACKGROUND, &crate::commands::registry(), at, |id| {
            !matches!(id, "edit.undo" | "edit.redo" | "file.paste" | "file.cut")
        })?;
    menu.step(true);
    menu.step(true);
    Some(menu)
}

/// The palette with a query in it, narrowed but not to one row.
///
/// `co` rather than nothing and rather than something exact: an empty query is a
/// frame of the whole registry, which is the keyboard overlay's job, and a query
/// matching one command would not show the ranking, the dimmed rows or the
/// highlight together. Dimmed the way a fresh window dims them — nothing done,
/// nothing copied — which is the same set `sample_menu` states and for the same
/// reason.
fn sample_palette() -> crate::palette::Palette {
    let commands = crate::commands::registry();
    let mut palette = crate::palette::open(&commands, |id| {
        !matches!(id, "edit.undo" | "edit.redo" | "file.paste_location")
    });
    palette.query = crate::field::Field::new("co".to_owned());
    palette.refilter();
    palette
}

/// The path bar as a field, with the caret five sixths of the way along it.
///
/// **Not at either end, and not in the middle.** The caret's position is the
/// shaped width of the text before it, so a caret at offset zero or at the end
/// would be in the right place even if that measurement were wrong; and far
/// enough along that the narrow frame has to scroll the text to show it, which
/// is the arithmetic that keeps a caret from being drawn outside its own box.
fn sample_path_entry(path: &std::path::Path) -> crate::field::Field {
    let mut field = crate::field::Field::new(path.to_string_lossy().into_owned());
    field.jump(false);
    for _ in 0..field.text.chars().count() * 5 / 6 {
        field.step(true);
    }
    field
}

/// Every overlay a capture was asked for, built once.
///
/// One struct rather than four locals because `run` is at the hundred-line limit
/// and the four of them are the same thing: a thing the window would be showing,
/// stated rather than reached.
struct Overlays {
    menu: Option<crate::menu::Menu>,
    path_entry: Option<crate::field::Field>,
    palette: Option<crate::palette::Palette>,
}

fn sample_overlays(
    request: &Capture,
    layout: &Layout,
    pane: &mut Pane,
    jobs: &crate::pane::Jobs,
) -> Overlays {
    // A query that matches some of the fixture and not all of it, so the frame
    // carries the count as well as the field.
    if request.filter {
        pane.set_filter("im".to_owned(), jobs);
    }
    Overlays {
        menu: request.menu.then(|| sample_menu(layout)).flatten(),
        path_entry: request.path_entry.then(|| sample_path_entry(&request.path)),
        palette: request.palette.then(sample_palette),
    }
}

/// Walks every visible folder to completion, so the frame is reproducible.
///
/// Bounded by the same deadline as the read, because a capture of `/` is not a
/// reason to walk the whole disk.
fn settle_folder_totals(pane: &mut Pane, session: &Session) {
    let mut measurer = crate::sizes::Measurer::new(session.waker.handle());
    let rows = 0..pane.listing.rows().min(MEASURED);
    let deadline = std::time::Instant::now() + READ_DEADLINE;
    loop {
        // `update` applies what is known and claims what is not, so a pass that
        // claims nothing and has nothing running is a pass with nothing left.
        measurer.update(pane, rows.clone(), &session.jobs);
        measurer.drain();
        if measurer.stats().1 == 0 || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::yield_now();
    }
    // **One more pass after the loop ends.** `drain` records a total into the
    // cache and `update` is what copies it into the listing, so the last walk to
    // finish was recorded and never applied — and the frame drew a dash for a
    // folder whose size was known. It depended on which side of the last `drain`
    // the walk landed, which made the golden frames flicker between two images
    // and is exactly the kind of thing they exist to catch.
    measurer.update(pane, rows, &session.jobs);
    measurer.stop();
}

/// Puts the list into the state a golden frame is worth taking of.
///
/// A selection, a cursor and a hover, at fixed rows: a frame with none of them
/// covers none of the three shapes a row can have.
fn pose(pane: &mut Pane) {
    for row in SELECTED {
        if let Some(id) = pane.listing.at(row) {
            pane.selection.toggle(id);
        }
    }
    if let Some(id) = pane.listing.at(CURSOR) {
        pane.selection.set_cursor(Some(id));
    }
    if pane.listing.rows() > HOVERED {
        pane.hover = Some(HOVERED);
    }
}

/// Renders one frame and writes it.
pub fn run(request: &Capture) -> Result<(), Problem> {
    let mut session = Session::open(request.logical, request.scale_120)?;
    let (mut pane, _) = session.read_fully(&request.path);

    settle_folder_totals(&mut pane, &session);
    pose(&mut pane);

    // The overlay reads the command registry, so a capture of it builds one.
    let keys_rows = request.keys.then(|| {
        let registry = crate::commands::registry();
        crate::commands::overlay_rows(&registry)
    });

    // Read here rather than left as `None`: the mark at the head of a row is
    // coloured from it, so a golden frame without it is a golden frame of grey.
    let types = sk_desktop::MimeDatabase::load();
    // A fixed set rather than this machine's: the sidebar is part of the frame,
    // and a reference image that embeds somebody's `user-dirs.dirs` and mount
    // table is one nobody else can reproduce.
    let places = if request.sidebar { crate::sidebar::sample(&request.path) } else { Vec::new() };
    let theme = if request.dark { Theme::dark() } else { Theme::light() };
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    let frame = if request.own_frame {
        Frame::Own { edges: true, minimize: true, maximize: true }
    } else {
        Frame::None
    };
    let layout = Layout::compute_with(logical, &theme.metrics, frame, request.sidebar);
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    let overlays = sample_overlays(request, &layout, &mut pane, &session.jobs);

    // Frames until the atlas owes nothing. The first rasterizes; the second draws
    // from a warm atlas. A frame is repeated only while texels are still queued,
    // so a capture is never an image with holes in the text.
    let keyboard_hint = crate::commands::hint(&crate::commands::registry());
    let mut stats = FrameStats::default();
    for pass in 0..MAX_PASSES {
        if pass >= 2 && session.renderer.pending_uploads() == 0 {
            break;
        }
        list.begin(session.physical);
        let mut diagnostics = Diagnostics {
            frames: &mut frames,
            last: stats,
            atlas: session.renderer.atlas_stats(),
            shaping: (cache.len(), 0, 0),
            pool: session.pool_stats(),
            adapter: session.renderer.adapter(),
            backend: session.renderer.backend(),
            physical: session.physical,
            scale_120: request.scale_120,
            capabilities: "offscreen",
        };
        {
            let mut painter = Painter {
                list: &mut list,
                fonts: &mut session.fonts,
                cache: &mut cache,
                theme: &theme,
                scale: session.scale,
            };
            let chrome = Chrome {
                keys: keys_rows.as_deref(),
                stats: request.stats,
                typed: None,
                band: None,
                watching: true,
                notice: None,
                // The pointer is on a row, so nothing in the frame is hovered
                // — the hover shapes are covered by the GUI scenarios, which can
                // actually move a pointer.
                hovered_button: None,
                hovered_crumb: None,
                hovered_tool: None,
                places: &places,
                hovered_place: None,
                naming: None,
                // What a freshly opened directory looks like: nowhere to go
                // back to, nowhere forward, a parent and a reload. Hardcoded
                // because a capture has no history — and it is the state worth
                // having a golden frame of, since it is the one with two of the
                // four dimmed.
                tools: [false, false, true, true],
                filtering: request.filter,
                types: Some(&types),
                window: sk_plat::WindowState {
                    activated: true,
                    can_maximize: true,
                    can_minimize: true,
                    ..sk_plat::WindowState::default()
                },
                menu: overlays.menu.as_ref(),
                path_entry: overlays.path_entry.as_ref(),
                palette: overlays.palette.as_ref(),
                hint: &keyboard_hint,
            };
            view::draw(
                &mut painter,
                &pane,
                &layout,
                now,
                &chrome,
                &mut diagnostics,
                &mut crate::view::Hits::default(),
            );
        }
        stats = session.renderer.draw(&mut list, &mut session.fonts, theme.palette.background);
    }

    let pixels = session.renderer.capture().ok_or_else(|| {
        Problem::plain(ProblemKind::NoGpu, "the offscreen target could not be read back")
    })?;
    write(&request.file, session.physical, &pixels)?;
    report(request, &session, &pane, stats);
    Ok(())
}

/// What the capture contained, so a golden frame is comparable to the run that
/// produced it without opening the file.
///
/// The face chain is on its own line because it is the input that decides most
/// of the pixels and the only one that comes from outside the repository: the
/// fixture holds Hebrew, Arabic and an emoji, so a machine with a different set
/// of fonts installed renders a different frame from the same commit. A harness
/// comparing against a stored reference has to be able to say so.
fn report(request: &Capture, session: &Session, pane: &Pane, stats: FrameStats) {
    eprintln!(
        "skerry: wrote {} — {}×{} at {}%, {} entries, {} quads, {} draws, {} glyphs",
        request.file.display(),
        session.physical.0,
        session.physical.1,
        request.scale_120 * 100 / 120,
        pane.listing.rows(),
        stats.quads,
        stats.draws,
        session.renderer.atlas_stats().0,
    );
    for path in session.fonts.paths() {
        eprintln!("skerry: face {}", path.display());
    }
}

fn write(file: &Path, physical: (u32, u32), pixels: &[u8]) -> Result<(), Problem> {
    png::write_png(file, physical.0, physical.1, pixels).map_err(|error| {
        Problem::new(ProblemKind::Unreadable, file.display().to_string(), error.to_string())
    })
}
