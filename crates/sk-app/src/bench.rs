//! A scripted scroll, rendered offscreen, timed.
//!
//! This exists because of the rule in
//! [docs/12-measurement.md](../../../docs/12-measurement.md): a frame number is
//! a p99 over a realistic trace, never an average over an idle window. It runs
//! the **same** code path as the window — the same immediate-mode pass, the same
//! renderer, the same glyph atlas — with the surface replaced by a texture, so
//! it can run in CI with no display server and no GPU beyond llvmpipe.
//!
//! What it reports is the CPU cost of a turn of the frame loop: taking delivery
//! of whatever the read has finished, scheduling folder totals, the
//! immediate-mode pass, then instance packing, encoding and submission. The wait
//! for a presentation slot is excluded because there is no compositor here to
//! wait for, which is also why the number is comparable to the windowed one —
//! see [`sk_gpu::FrameStats`].
//!
//! **The read runs during the trace, not before it.** It used to complete first,
//! which meant the two most expensive things the loop ever does — moving a 10 MB
//! `Listing` into the pane and writing 2,048 metadata rows — happened where no
//! clock was running, and the row named "Keystroke to frame" was measured on a
//! directory that had nothing left to deliver. `docs/12-measurement.md` asks for
//! a trace that overlaps a directory finishing loading; this one is that trace,
//! and the frame the listing lands on is reported by itself.
//!
//! Measured on 200,000 entries: the scroll is 0.19 ms p50 and 0.34 ms p99, and
//! **the worst frame while the read is in flight is 2.5 to 5.0 ms** — an order of
//! magnitude worse, and the number the old harness could not see. Two thirds of
//! it is not this thread's own work: the pool is 24 wide and saturating every
//! core with `statx` while the loop tries to draw. The run that shaped a whole
//! fresh viewport, 27 runs, paid 0.7 ms more than one that shaped 8.

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
use crate::pane::{Message, Pane};
use crate::sizes::Measurer;
use crate::view::{self, Chrome, Diagnostics};

/// How long a turn may block waiting for the read, while the read is running.
///
/// One frame at 120 Hz. It bounds the wait, it does not pace the trace: a turn
/// starts the moment something arrives, which is what the window does.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_micros(8_333);

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

/// Everything the loop does between waking and drawing.
///
/// Applying what the read finished, recording folder totals, and scheduling the
/// walks for what is on screen — the three things the clock has to cover and did
/// not, before it was moved to the top of the turn.
fn take_delivery(
    pane: &mut Pane,
    measurer: &mut Measurer,
    session: &Session,
    pending: Option<Message>,
    layout: &Layout,
    theme: &Theme,
) {
    for message in pending.into_iter().chain(session.updates.try_iter()) {
        pane.apply(message, &session.jobs);
    }
    measurer.drain();
    let visible = sk_ui::visible_rows(
        pane.scroll.offset(),
        layout.body.height,
        theme.metrics.row_height,
        pane.listing.rows(),
    );
    measurer.update(pane, visible, &session.jobs);
}

/// What the trace accumulated, apart from the frame distribution.
#[derive(Default)]
struct Trace {
    /// The worst frame while the read was in flight, and how many runs it shaped.
    worst_while_loading: (u64, u64),
    /// The frame the listing landed on, and the wall time the read took.
    loaded_at: Option<(u32, f64)>,
    worst_quads: usize,
    rasterized: usize,
}

/// What a trace draws that does not change between frames.
struct Fixed {
    types: sk_desktop::MimeDatabase,
    places: Vec<crate::sidebar::Section>,
    theme: Theme,
    layout: Layout,
    hint: String,
}

fn fixed(request: &Bench) -> Fixed {
    let theme = Theme::dark();
    let logical = Size::new(request.logical.0 as f32, request.logical.1 as f32);
    Fixed {
        types: sk_desktop::MimeDatabase::load(),
        places: crate::sidebar::sample(&request.path),
        layout: Layout::compute_with(logical, &theme.metrics, crate::layout::Frame::None, true),
        theme,
        hint: crate::commands::hint(&crate::commands::registry()),
    }
}

/// Runs the trace and prints the result.
pub fn run(request: &Bench) -> Result<(), Problem> {
    let mut session = Session::open(request.logical, request.scale_120)?;
    let mut pane = Pane::new(request.path.clone());
    let mut measurer = Measurer::new(session.waker.handle());
    let read_started = Instant::now();
    pane.reload(&session.jobs);

    // Everything the trace draws that is not a row, built once. The windowed
    // loop asks the MIME database once per visible row per frame and draws a
    // sidebar that narrows the list, so a trace without either measures a frame
    // the product does not draw.
    let Fixed { types, places, theme, layout, hint } = fixed(request);
    let mut cache = LayoutCache::new(4096);
    let mut list = DrawList::default();
    let mut frames = Frames::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed());

    let mut stats = FrameStats::default();
    let mut trace = Trace::default();

    for frame in 0..request.frames {
        let content = pane.listing.rows() as f32 * theme.metrics.row_height;
        pane.scroll.set_extent(content, layout.body.height);
        // A triangle wave down the listing and back, at a speed that is not a
        // whole number of rows: every frame lands on a different subpixel
        // phase, so the trace exercises the four subpixel buckets and the
        // layout cache the way a real fling does rather than re-drawing one
        // aligned frame N times.
        let span = (content - layout.body.height).max(1.0);
        let phase = f32::from(u16::try_from(frame % 512).unwrap_or(0)) / 512.0;
        let position = if phase < 0.5 { phase * 2.0 } else { (1.0 - phase) * 2.0 };
        pane.scroll.to(position * span + f32::from(u8::try_from(frame % 7).unwrap_or(0)) * 0.37);

        // While the read is in flight, wait for it the way the window does —
        // blocked, off the clock — so a turn takes delivery of the batch that had
        // actually accumulated. Spinning instead would spread one 2,048-row
        // metadata slice over as many turns as the CPU could take, and report a
        // cheaper frame than the loop will ever have.
        let pending =
            if pane.loading { session.updates.recv_timeout(FRAME_INTERVAL).ok() } else { None };

        // The clock covers the whole turn, exactly as `App::run` does.
        let started = Instant::now();
        take_delivery(&mut pane, &mut measurer, &session, pending, &layout, &theme);
        let loading = pane.loading;
        let shaped_before = cache.stats().1;

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
                hovered_button: None,
                hovered_crumb: None,
                hovered_separator: None,
                crumb_menu: None,
                hovered_tool: None,
                places: &places,
                hovered_place: None,
                drop_row: None,
                naming: None,
                tools: [true, true, true, true],
                filtering: false,
                types: Some(&types),
                window: sk_plat::WindowState::default(),
                menu: None,
                path_entry: None,
                palette: None,
                hint: &hint,
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
        let turn_us = started.elapsed().as_micros() as u64;
        stats = session.renderer.draw(&mut list, &mut session.fonts, theme.palette.background);
        trace.worst_quads = trace.worst_quads.max(stats.quads);
        trace.rasterized += stats.rasterized;

        if loading {
            trace.worst_while_loading = trace
                .worst_while_loading
                .max((turn_us + stats.cpu_us, cache.stats().1 - shaped_before));
        } else if trace.loaded_at.is_none() {
            trace.loaded_at = Some((frame, read_started.elapsed().as_secs_f64() * 1000.0));
        }
        // The first frame pays for an empty atlas and an empty layout cache. It
        // is reported separately rather than folded into the distribution,
        // because a cold start is a different question from a scroll.
        if frame > 0 {
            frames.record(turn_us + stats.cpu_us);
        }
    }

    report(request, &session, &pane, &mut frames, &trace);
    Ok(())
}

fn report(request: &Bench, session: &Session, pane: &Pane, frames: &mut Frames, trace: &Trace) {
    let (total, over) = frames.budget();
    match trace.loaded_at {
        Some((frame, read_ms)) => println!(
            "{:>10}  {} entries · read {read_ms:.1} ms (names {:.1}, metadata {:.1}, sort {:.1}) \
             · landed on frame {frame}, worst frame while loading {:.3} ms shaping {} runs",
            "listing",
            pane.listing.rows(),
            pane.names_us as f64 / 1000.0,
            pane.meta_us as f64 / 1000.0,
            pane.sort_us as f64 / 1000.0,
            trace.worst_while_loading.0 as f64 / 1000.0,
            trace.worst_while_loading.1,
        ),
        // Not a failure and not a number to quote: the trace was too short to
        // contain the read, so the distribution below is a scroll over a
        // half-built listing and says nothing about a finished one.
        None => println!(
            "{:>10}  still loading after {} frames — ask for more",
            "listing", request.frames
        ),
    }
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
        "{:>10}  {} quads worst · {} glyphs cached · {} rasterized over the trace",
        "frame",
        trace.worst_quads,
        session.renderer.atlas_stats().0,
        trace.rasterized,
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
}
