// SPDX-License-Identifier: MIT OR Apache-2.0
//! Everything a frame needs, with no display server.
//!
//! Shared by `--capture` and `--bench`, which want the same thing: the real
//! renderer, the real fonts, the real job pool and a real directory read, with
//! the surface replaced by a texture. Written once because the two of them
//! agreeing about how a frame is set up is the whole reason either number is
//! comparable to the window's.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sk_gpu::Renderer;
use sk_model::{Problem, ProblemKind};
use sk_text::Fonts;
use sk_util::{Pool, Waker};

use crate::pane::{Jobs, Message, Pane};

/// How long to wait for a directory read before drawing anyway.
///
/// A capture of a half-read directory is a valid frame and a useful one, so the
/// deadline is a deadline and not a failure.
pub(crate) const READ_DEADLINE: Duration = Duration::from_secs(5);

/// The diagnostics a headless frame carries.
///
/// Both the benchmark and the capture draw with no window, so both report the
/// same nine values about the same session — and a second copy of the literal is
/// a second place for `capabilities` to say something other than `offscreen`.
/// The renderer is taken rather than the whole `Session`, because the caller is
/// about to borrow the faces out of it mutably for the painter.
pub(crate) fn offscreen<'a>(
    renderer: &'a Renderer,
    pool: (usize, u64, u64),
    physical: (u32, u32),
    frames: &'a mut crate::frames::Frames,
    last: sk_gpu::FrameStats,
    runs: usize,
    scale_120: u32,
) -> crate::view::Diagnostics<'a> {
    crate::view::Diagnostics {
        frames,
        last,
        atlas: renderer.atlas_stats(),
        shaping: (runs, 0, 0),
        pool,
        adapter: renderer.adapter(),
        backend: renderer.backend(),
        physical,
        scale_120,
        capabilities: "offscreen",
    }
}

/// A renderer, fonts and a pool, with no window.
pub(crate) struct Session {
    pub renderer: Renderer,
    pub fonts: Fonts,
    pub jobs: Jobs,
    pub updates: mpsc::Receiver<Message>,
    /// The eventfd the jobs signal.
    ///
    /// Nothing here waits on it — a headless run has no frame loop to wake — but
    /// it has to outlive the handles the jobs hold, and a capture hands one to
    /// the folder-total walks.
    pub waker: Waker,
    /// The surface size in device pixels.
    pub physical: (u32, u32),
    /// Device pixels per logical pixel.
    pub scale: f32,
}

impl Session {
    /// Opens a device and a pool for a surface of `logical` at `scale_120`.
    pub fn open(logical: (u32, u32), scale_120: u32) -> Result<Self, Problem> {
        let physical = (
            (logical.0 * scale_120).div_ceil(120).max(1),
            (logical.1 * scale_120).div_ceil(120).max(1),
        );
        let renderer = Renderer::offscreen(physical)?;
        let fonts = Fonts::discover()?;
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
        Ok(Self {
            renderer,
            fonts,
            jobs,
            updates,
            waker,
            physical,
            scale: scale_120 as f32 / 120.0,
        })
    }

    /// Reads `path` to completion, returning the pane and the wall time in
    /// milliseconds.
    ///
    /// The only place in the program that blocks on a directory read. A window
    /// never does this — it draws what has arrived and asks for another frame
    /// when more does — but a capture and a benchmark are both a deliberate stop,
    /// and a benchmark that measured a half-read directory would measure the
    /// wrong thing.
    pub fn read_fully(&self, path: &Path) -> (Pane, f64) {
        let started = Instant::now();
        let mut pane = Pane::new(path.to_path_buf());
        pane.reload(&self.jobs);

        let deadline = started + READ_DEADLINE;
        while pane.loading && Instant::now() < deadline {
            match self.updates.recv_timeout(Duration::from_millis(20)) {
                Ok(message) => {
                    pane.apply(message, &self.jobs);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.settle(&mut pane, Duration::from_millis(50));
        (pane, started.elapsed().as_secs_f64() * 1000.0)
    }

    /// Applies whatever arrives for up to `window`, then stops.
    ///
    /// Used after a read, to take delivery of the last metadata slice and any
    /// reorder it dispatched, and after asking for folder totals.
    pub fn settle(&self, pane: &mut Pane, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            match self.updates.recv_timeout(Duration::from_millis(5)) {
                Ok(message) => {
                    pane.apply(message, &self.jobs);
                }
                Err(_) => break,
            }
        }
    }

    /// Pool width, jobs finished, jobs that panicked.
    pub fn pool_stats(&self) -> (usize, u64, u64) {
        let (finished, panics) = self.jobs.pool.stats();
        (self.jobs.pool.width(), finished, panics)
    }
}
