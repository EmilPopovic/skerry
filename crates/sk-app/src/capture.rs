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
use crate::layout::Layout;
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
}

/// Renders one frame and writes it.
pub fn run(request: &Capture) -> Result<(), Problem> {
    let mut session = Session::open(request.logical, request.scale_120)?;
    let (mut pane, _) = session.read_fully(&request.path);

    // Folder totals are measured for what is on screen, and a capture is all of
    // it. Bounded by the same deadline as the read, because a capture of `/` is
    // not a reason to walk the whole disk.
    let mut measurer = crate::sizes::Measurer::new(session.waker.handle());
    let rows = 0..pane.listing.rows().min(MEASURED);
    let deadline = std::time::Instant::now() + READ_DEADLINE;
    loop {
        // `update` applies what is known and claims what is not, so a pass that
        // claims nothing and has nothing running is a pass with nothing left.
        measurer.update(&mut pane, rows.clone(), &session.jobs);
        measurer.drain();
        if measurer.stats().1 == 0 || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::yield_now();
    }
    measurer.stop();

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

    // The overlay reads the command registry, so a capture of it builds one.
    let keys_rows = request.keys.then(|| {
        let registry = crate::commands::registry();
        crate::commands::overlay_rows(&registry)
    });

    let theme = if request.dark { Theme::dark() } else { Theme::light() };
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    let layout = Layout::compute(logical, &theme.metrics);
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    // Frames until the atlas owes nothing. The first rasterizes; the second draws
    // from a warm atlas. A frame is repeated only while texels are still queued,
    // so a capture is never an image with holes in the text.
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
                hovered_button: None,
                hovered_crumb: None,
                filtering: false,
                window: sk_plat::WindowState::default(),
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
}

fn write(file: &Path, physical: (u32, u32), pixels: &[u8]) -> Result<(), Problem> {
    png::write_png(file, physical.0, physical.1, pixels).map_err(|error| {
        Problem::new(ProblemKind::Unreadable, file.display().to_string(), error.to_string())
    })
}
