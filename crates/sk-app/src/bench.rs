//! A scripted scroll, rendered offscreen, timed.
//!
//! This exists because of the rule in
//! [docs/12-measurement.md](../../../docs/12-measurement.md): a frame number is
//! a p99 over a realistic trace, never an average over an idle window. It runs
//! the **same** code path as the window — the same immediate-mode pass, the same
//! renderer, the same glyph atlas — with the surface replaced by a texture, so
//! it can run in CI with no display server and no GPU beyond llvmpipe.
//!
//! What it reports is the CPU cost of a frame: the immediate-mode pass plus
//! instance packing, encoding and submission. The wait for a presentation slot
//! is excluded because there is no compositor here to wait for, which is also
//! why the number is comparable to the windowed one — see
//! [`sk_gpu::FrameStats`].

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sk_gpu::{DrawList, FrameStats};
use sk_model::Problem;
use sk_plat::Size;
use sk_text::LayoutCache;
use sk_ui::{Painter, Theme};

use crate::frames::{BUDGET_US, Frames};
use crate::headless::Session;
use crate::layout::Layout;
use crate::view::{self, Chrome, Diagnostics};

/// What to measure.
pub struct Bench {
    /// The directory to read.
    pub path: PathBuf,
    /// Logical size.
    pub logical: (u32, u32),
    /// The scale as a numerator over 120.
    pub scale_120: u32,
    /// How many frames the trace runs for.
    pub frames: u32,
}

/// Runs the trace and prints the result.
pub fn run(request: &Bench) -> Result<(), Problem> {
    let mut session = Session::open(request.logical, request.scale_120)?;
    let (mut pane, read_ms) = session.read_fully(&request.path);

    let theme = Theme::dark();
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    let layout = Layout::compute(logical, &theme.metrics);
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    let content = pane.listing.rows() as f32 * theme.metrics.row_height;
    pane.scroll.set_extent(content, layout.body.height);

    let mut stats = FrameStats::default();
    let mut worst_quads = 0usize;
    let mut rasterized = 0usize;

    for frame in 0..request.frames {
        // A triangle wave down the listing and back, at a speed that is not a
        // whole number of rows: every frame lands on a different subpixel
        // phase, so the trace exercises the four subpixel buckets and the
        // layout cache the way a real fling does rather than re-drawing one
        // aligned frame N times.
        let span = (content - layout.body.height).max(1.0);
        let phase = f32::from(u16::try_from(frame % 512).unwrap_or(0)) / 512.0;
        let position = if phase < 0.5 { phase * 2.0 } else { (1.0 - phase) * 2.0 };
        pane.scroll.to(position * span + f32::from(u8::try_from(frame % 7).unwrap_or(0)) * 0.37);

        let started = Instant::now();
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
                keys: None,
                stats: false,
                typed: None,
                band: None,
                watching: true,
                notice: None,
            };
            view::draw(&mut painter, &pane, &layout, now, &chrome, &mut diagnostics);
        }
        let build_us = started.elapsed().as_micros() as u64;
        stats = session.renderer.draw(&mut list, &mut session.fonts, theme.palette.background);
        worst_quads = worst_quads.max(stats.quads);
        rasterized += stats.rasterized;

        // The first frame pays for an empty atlas and an empty layout cache. It
        // is reported separately rather than folded into the distribution,
        // because a cold start is a different question from a scroll.
        if frame > 0 {
            frames.record(build_us + stats.cpu_us);
        }
    }

    let (total, over) = frames.budget();
    println!(
        "{:>10}  {} entries · read {read_ms:.1} ms (names {:.1}, metadata {:.1}, sort {:.1})",
        "listing",
        pane.listing.rows(),
        pane.names_us as f64 / 1000.0,
        pane.meta_us as f64 / 1000.0,
        pane.sort_us as f64 / 1000.0,
    );
    println!(
        "{:>10}  p50 {:.3} ms · p99 {:.3} ms · worst {:.3} ms · {} of {} over {} ms",
        "frame",
        f64::from(frames.percentile(0.5)) / 1000.0,
        f64::from(frames.percentile(0.99)) / 1000.0,
        f64::from(frames.worst()) / 1000.0,
        over,
        total,
        BUDGET_US / 1000,
    );
    println!(
        "{:>10}  {worst_quads} quads worst · {} glyphs cached · {rasterized} rasterized over the trace",
        "frame",
        session.renderer.atlas_stats().0,
    );
    println!(
        "{:>10}  {}×{} at {}% · {} on {}",
        "surface",
        session.physical.0,
        session.physical.1,
        request.scale_120 * 100 / 120,
        session.renderer.adapter(),
        session.renderer.backend(),
    );
    Ok(())
}
