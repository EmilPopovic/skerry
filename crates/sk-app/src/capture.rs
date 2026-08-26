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
//! carries the cursor and one is hovered, so a capture exercises the selection
//! shape, the focus ring and the hover tint rather than just the text.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sk_gpu::{DrawList, FrameStats, Renderer, capture as png};
use sk_model::{Problem, ProblemKind};
use sk_plat::Size;
use sk_text::{Fonts, LayoutCache};
use sk_ui::{Painter, Theme};
use sk_util::{Pool, Waker};

use crate::frames::Frames;
use crate::layout::Layout;
use crate::pane::{Jobs, Pane};
use crate::view::{self, Chrome, Diagnostics};

/// How long to wait for the directory read before drawing anyway.
///
/// A capture of a half-read directory is a valid frame and a useful one, so the
/// deadline is a deadline and not a failure.
const READ_DEADLINE: Duration = Duration::from_secs(5);

/// Rows selected in a capture, and which of them carries the cursor.
const SELECTED: std::ops::Range<usize> = 2..5;
const CURSOR: usize = 3;
const HOVERED: usize = 7;

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
}

/// Reads `path` to completion, returning the pane and the wall time in
/// milliseconds.
///
/// The only place in the program that blocks on a directory read. A window never
/// does this — it draws what has arrived and asks for another frame when more
/// does — but a capture and a benchmark are both a deliberate stop, and a
/// benchmark that measured a half-read directory would measure the wrong thing.
pub(crate) fn read_fully(
    path: &Path,
    jobs: &Jobs,
    updates: &mpsc::Receiver<crate::pane::Message>,
) -> (Pane, f64) {
    let started = Instant::now();
    let mut pane = Pane::new(path.to_path_buf());
    pane.reload(jobs);

    let deadline = started + READ_DEADLINE;
    while pane.loading && Instant::now() < deadline {
        match updates.recv_timeout(Duration::from_millis(20)) {
            Ok(message) => {
                pane.apply(message, jobs);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Anything that arrived while the last slice was being applied, including a
    // reorder that was dispatched by it.
    let settle = Instant::now() + Duration::from_millis(50);
    while Instant::now() < settle {
        match updates.recv_timeout(Duration::from_millis(5)) {
            Ok(message) => {
                pane.apply(message, jobs);
            }
            Err(_) => break,
        }
    }
    (pane, started.elapsed().as_secs_f64() * 1000.0)
}

/// Renders one frame and writes it.
pub fn run(request: &Capture) -> Result<(), Problem> {
    let physical = (
        (request.logical.0 * request.scale_120).div_ceil(120).max(1),
        (request.logical.1 * request.scale_120).div_ceil(120).max(1),
    );
    let scale = request.scale_120 as f32 / 120.0;

    let mut renderer = Renderer::offscreen(physical)?;
    let mut fonts = Fonts::discover()?;
    let waker = Waker::new()
        .map_err(|error| Problem::plain(ProblemKind::NoDisplay, format!("eventfd: {error}")))?;
    let (sender, updates) = mpsc::channel();
    let jobs = Jobs { pool: Pool::new(), wake: waker.handle(), sender };

    let (mut pane, _) = read_fully(&request.path, &jobs, &updates);

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

    let theme = if request.dark { Theme::dark() } else { Theme::light() };
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    let layout = Layout::compute(logical, &theme.metrics);
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    // Frames until the atlas owes nothing. The first frame rasterizes; the
    // second draws from a warm atlas. A frame is repeated only if texels are
    // still queued, so a capture is never an image with holes in the text — and
    // if that ever takes more than a couple of passes, the loop's own bound says
    // so rather than the image quietly being wrong.
    let mut stats = FrameStats::default();
    for pass in 0..MAX_PASSES {
        if pass >= 2 && renderer.pending_uploads() == 0 {
            break;
        }
        list.begin(physical);
        let (finished, panics) = jobs.pool.stats();
        let mut diagnostics = Diagnostics {
            frames: &mut frames,
            last: stats,
            atlas: renderer.atlas_stats(),
            shaping: (cache.len(), 0, 0),
            pool: (jobs.pool.width(), finished, panics),
            adapter: renderer.adapter(),
            backend: renderer.backend(),
            physical,
            scale_120: request.scale_120,
        };
        {
            let mut painter = Painter {
                list: &mut list,
                fonts: &mut fonts,
                cache: &mut cache,
                theme: &theme,
                scale,
            };
            let chrome =
                Chrome { keys: request.keys, stats: request.stats, typed: None, band: None };
            view::draw(&mut painter, &pane, &layout, now, &chrome, &mut diagnostics);
        }
        stats = renderer.draw(&mut list, &mut fonts, theme.palette.background);
    }

    let pixels = renderer.capture().ok_or_else(|| {
        Problem::plain(ProblemKind::NoGpu, "the offscreen target could not be read back")
    })?;
    write(&request.file, physical, &pixels)?;

    eprintln!(
        "skerry: wrote {} — {}×{} at {}%, {} entries, {} quads, {} draws, {} glyphs",
        request.file.display(),
        physical.0,
        physical.1,
        request.scale_120 * 100 / 120,
        pane.listing.rows(),
        stats.quads,
        stats.draws,
        renderer.atlas_stats().0,
    );
    Ok(())
}

fn write(file: &Path, physical: (u32, u32), pixels: &[u8]) -> Result<(), Problem> {
    png::write_png(file, physical.0, physical.1, pixels).map_err(|error| {
        Problem::new(ProblemKind::Unreadable, file.display().to_string(), error.to_string())
    })
}
