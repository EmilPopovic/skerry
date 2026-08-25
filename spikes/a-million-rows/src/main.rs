//! Spike A, arm 1 — `wgpu` + `swash`. The question, the bar and the method are
//! in README.md next to this file.
//!
//! Throwaway code, kept for its measurements. The one thing it is careful about
//! is the honesty of the numbers: every figure printed here has a method in the
//! README, and the harness refuses to report a figure it could not measure.

mod atlas;
mod data;
mod frame;
mod render;
mod stats;
mod text;
mod trace;

use std::time::Instant;

use atlas::Atlas;
use data::Store;
use frame::{Metrics, Pass};
use render::Gpu;
use stats::Samples;
use text::{Fonts, LayoutCache};
use trace::{Phase, Trace};

struct Options {
    rows: usize,
    scale: f32,
    width: u32,
    height: u32,
    adapter: Option<String>,
    /// Set only when `--scale` was passed. Otherwise the windowed path takes the
    /// scale from the compositor, which is the only source that can be right.
    scale_override: Option<f32>,
    windowed: bool,
    idle_seconds: u64,
    golden: Option<String>,
    cache_rows: usize,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            rows: 1_000_000,
            scale: 1.0,
            // Device pixels. Fixed across scale factors so the two runs differ
            // in glyph raster size rather than in framebuffer size.
            width: 1920,
            height: 1080,
            adapter: None,
            scale_override: None,
            windowed: false,
            idle_seconds: 10,
            golden: None,
            cache_rows: 4096,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--rows" => options.rows = value()?.parse().map_err(|_| "bad --rows")?,
                "--scale" => {
                    options.scale = value()?.parse().map_err(|_| "bad --scale")?;
                    options.scale_override = Some(options.scale);
                }
                "--size" => {
                    let text = value()?;
                    let (w, h) = text.split_once('x').ok_or("--size wants WxH")?;
                    options.width = w.parse().map_err(|_| "bad --size width")?;
                    options.height = h.parse().map_err(|_| "bad --size height")?;
                }
                "--adapter" => options.adapter = Some(value()?),
                "--window" => options.windowed = true,
                "--idle" => options.idle_seconds = value()?.parse().map_err(|_| "bad --idle")?,
                "--golden" => options.golden = Some(value()?),
                "--cache-rows" => {
                    options.cache_rows = value()?.parse().map_err(|_| "bad --cache-rows")?
                }
                "--help" | "-h" => return Err(USAGE.into()),
                other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
            }
        }
        Ok(options)
    }
}

const USAGE: &str = "\
spike-a-million-rows — Spike A arm 1, wgpu + swash

  --rows N          entries in the store (default 1000000)
  --scale F         display scale factor, e.g. 1.0 or 1.5 (default 1.0)
  --size WxH        framebuffer in device pixels (default 1920x1080)
  --adapter NAME    substring of the adapter name: radv, nvidia, llvmpipe
  --window          open a real window; also runs the idle-CPU measurement
  --idle S          idle-CPU window in seconds, windowed only (default 10)
  --golden PATH     write a PNG of the last frame (headless only)
  --cache-rows N    layout cache capacity in shaped runs (default 4096)";

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

/// Everything the harness accumulated, so reporting is one function with one
/// view of the truth rather than prints scattered through the loop.
struct Report {
    cpu: Vec<Samples>,
    gpu: Vec<Samples>,
    quads: u64,
    glyphs: u64,
    scaled_glyphs: u64,
    dropped_glyphs: u64,
    rows_per_frame: u32,
    frames: u32,
}

impl Report {
    fn new() -> Self {
        let phases: Vec<Phase> = trace::PLAN
            .iter()
            .map(|(phase, _)| *phase)
            .filter(|phase| phase.measured())
            .collect();
        Self {
            cpu: phases.iter().map(|p| Samples::new(p.label())).collect(),
            gpu: phases.iter().map(|p| Samples::new(p.label())).collect(),
            quads: 0,
            glyphs: 0,
            scaled_glyphs: 0,
            dropped_glyphs: 0,
            rows_per_frame: 0,
            frames: 0,
        }
    }

    fn slot(&self, phase: Phase) -> Option<usize> {
        self.cpu.iter().position(|s| s.label == phase.label())
    }
}

fn run() -> Result<(), String> {
    let options = Options::parse()?;
    let metrics = Metrics::new(options.scale);

    let build_started = Instant::now();
    let store = Store::synthetic(options.rows, 0xA11_C0DE);
    let build_ms = build_started.elapsed().as_secs_f32() * 1000.0;

    let mut fonts = Fonts::open()?;
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });

    println!("Spike A arm 1 — wgpu 27 + swash 0.2, release build");
    println!(
        "  store        {} entries, {:.1} MB, built in {:.0} ms",
        store.len(),
        store.footprint() as f32 / 1.0e6,
        build_ms
    );
    println!("  fonts        {} faces in the fallback chain", fonts.face_count());

    if options.windowed {
        windowed(options, metrics, store, fonts, instance)
    } else {
        headless(options, metrics, store, &mut fonts, instance)
    }
}

/// Shared per-frame body. Returns the CPU cost of the frame in milliseconds and
/// any GPU samples that finished resolving.
#[allow(clippy::too_many_arguments)]
fn one_frame(
    gpu: &mut Gpu,
    view: &wgpu::TextureView,
    pass: &mut Pass,
    store: &Store,
    metrics: &Metrics,
    viewport: (u32, u32),
    scroll: f64,
    fonts: &mut Fonts,
    cache: &mut LayoutCache,
    atlas: &mut Atlas,
) -> (f32, Vec<f32>) {
    let started = Instant::now();
    pass.build(store, metrics, viewport, scroll, fonts, cache, atlas);
    atlas.flush(&gpu.queue, gpu.mask_texture(), gpu.color_texture());
    let gpu_samples = gpu.draw(
        view,
        viewport,
        &pass.quads,
        wgpu::Color { r: 0.055, g: 0.055, b: 0.070, a: 1.0 },
    );
    // The CPU figure spans the immediate-mode pass, shaping, atlas upload and
    // submit — everything the UI thread owes the frame. Present is excluded and
    // measured separately, because a blocked present is a pacing question.
    (started.elapsed().as_secs_f32() * 1000.0, gpu_samples)
}

fn headless(
    options: Options,
    metrics: Metrics,
    store: Store,
    fonts: &mut Fonts,
    instance: wgpu::Instance,
) -> Result<(), String> {
    let adapter = render::pick_adapter(&instance, options.adapter.as_deref())?;
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut gpu = Gpu::new(&adapter, format)?;

    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: options.width,
            height: options.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let (mut pass, mut cache, mut atlas) = harness(&options);
    let viewport = (options.width, options.height);
    let mut report = Report::new();
    let mut trace = Trace::new(
        store.len() as u64,
        f64::from(metrics.row_height * metrics.scale),
        f64::from(options.height),
    );

    describe(&gpu, &options, &metrics, options.height);

    loop {
        let step = trace.advance();
        if step.invalidate {
            cache.invalidate();
        }
        let (cpu_ms, gpu_ms) = one_frame(
            &mut gpu, &view, &mut pass, &store, &metrics, viewport, trace.scroll, fonts,
            &mut cache, &mut atlas,
        );
        record(&mut report, step.phase, cpu_ms, gpu_ms, &pass);
        if step.finished {
            break;
        }
    }

    if let Some(path) = &options.golden {
        write_png(&gpu, &target, options.width, options.height, path)?;
        println!("  golden       {path}");
    }

    finish(&report, &gpu, &cache, &atlas, &store, None, None);
    Ok(())
}

fn harness(options: &Options) -> (Pass, LayoutCache, Atlas) {
    let mut pass = Pass::new();
    // An anchor deep in the listing, so the selection is somewhere the trace
    // actually scrolls through.
    pass.set_selection(400_000);
    (pass, LayoutCache::new(options.cache_rows), Atlas::new())
}

fn record(report: &mut Report, phase: Phase, cpu_ms: f32, gpu_ms: Vec<f32>, pass: &Pass) {
    report.frames += 1;
    report.rows_per_frame = pass.stats.rows;
    report.quads += u64::from(pass.stats.quads);
    report.glyphs += u64::from(pass.stats.glyphs);
    report.scaled_glyphs += u64::from(pass.stats.scaled_glyphs);
    report.dropped_glyphs += u64::from(pass.stats.dropped_glyphs);

    if !phase.measured() {
        return;
    }
    if let Some(slot) = report.slot(phase) {
        report.cpu[slot].push(cpu_ms);
        report.gpu[slot].extend(gpu_ms);
    }
}

fn describe(gpu: &Gpu, options: &Options, metrics: &Metrics, height: u32) {
    let row_px = metrics.row_height * metrics.scale;
    if !options.windowed {
        println!(
            "  adapter      {} · {} · {:?}",
            gpu.info.name, gpu.info.driver, gpu.info.backend
        );
    }
    println!(
        "  target       {}x{} device px @ {}x scale · {} rows visible · {}",
        options.width,
        options.height,
        metrics.scale,
        (height as f32 / row_px).ceil() as u32 + 1,
        if options.windowed { "windowed" } else { "headless" }
    );
    println!(
        "  timing       cpu: Instant · gpu: {}",
        if gpu.has_gpu_timing() {
            "timestamp queries"
        } else {
            "UNAVAILABLE — adapter lacks TIMESTAMP_QUERY"
        }
    );
    println!("  trace        {} frames across {} phases", trace::total_frames(), trace::PLAN.len());
    println!();
}

/// Prints the result table. The bar lives in the README; this prints the numbers
/// it is compared against and states which of the four conditions each one is.
fn finish(
    report: &Report,
    gpu: &Gpu,
    cache: &LayoutCache,
    atlas: &Atlas,
    store: &Store,
    idle: Option<IdleResult>,
    scale_mismatch: Option<u64>,
) {
    println!("frame time, milliseconds — CPU (immediate-mode pass, shaping, upload, submit)");
    for samples in &report.cpu {
        println!("  {}", samples.row());
    }
    println!("\nframe time, milliseconds — GPU (render pass, timestamp queries)");
    for samples in &report.gpu {
        println!("  {}", samples.row());
    }

    let worst_cpu = report
        .cpu
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| (s.label, s.percentile(99.0)))
        .fold(("none", 0.0f32), |acc, next| if next.1 > acc.1 { next } else { acc });
    let worst_gpu = report
        .gpu
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| (s.label, s.percentile(99.0)))
        .fold(("none", 0.0f32), |acc, next| if next.1 > acc.1 { next } else { acc });

    println!("\nagainst the bar");
    println!(
        "  p99 frame < 8 ms          worst phase `{}`: cpu {:.3} ms, gpu {:.3} ms ({})",
        worst_cpu.0,
        worst_cpu.1,
        worst_gpu.1,
        verdict(worst_cpu.1.max(worst_gpu.1) < 8.0)
    );

    let rss = stats::peak_rss();
    match rss {
        Some(bytes) => println!(
            "  RSS < 300 MB              peak {:.1} MB, current {:.1} MB, store {:.1} MB ({})",
            bytes as f32 / 1.0e6,
            stats::current_rss().unwrap_or(0) as f32 / 1.0e6,
            store.footprint() as f32 / 1.0e6,
            verdict(bytes < 300_000_000)
        ),
        None => println!("  RSS < 300 MB              UNMEASURED — /proc/self/status unreadable"),
    }

    println!(
        "  crisp at fractional scale {} glyph quads not 1:1 with their raster, {} dropped ({})",
        report.scaled_glyphs,
        report.dropped_glyphs,
        verdict(report.scaled_glyphs == 0 && report.dropped_glyphs == 0)
    );
    match scale_mismatch {
        None => println!(
            "    └ surface matches screen      NOT MEASURED — headless has no compositor to resample"
        ),
        Some(frames) => println!(
            "    └ surface matches screen      {frames} frames drawn at a scale or size the \
compositor had to resample ({})",
            verdict(frames == 0)
        ),
    }

    match idle {
        // "Zero" against a 100 Hz accounting clock means "no tick we can see":
        // 5 ms over the window is one sixtieth of the 0.3% hard fail in
        // docs/09-performance-budgets.md, and one scheduler tick above nothing.
        // The CPU threshold applies only to an undisturbed window; with input
        // arriving, CPU above zero is the loop doing its job.
        Some(idle) => {
            let clean = idle.inputs == 0;
            println!(
                "  zero CPU at idle          {:.0} ms of CPU over {} s = {:.3}%, \
{} redraws of which {} unprompted, {} input events ({})",
                idle.cpu_ms,
                idle.seconds,
                100.0 * idle.cpu_ms / (idle.seconds as f64 * 1000.0),
                idle.redraws,
                idle.spontaneous,
                idle.inputs,
                verdict(idle.spontaneous == 0 && (!clean || idle.cpu_ms <= 5.0))
            );
            if !idle.threads.is_empty() {
                let who: Vec<String> = idle
                    .threads
                    .iter()
                    .map(|(name, tid, ticks)| format!("{name}({tid}) +{}ms", ticks * 10))
                    .collect();
                println!("                            CPU charged to: {}", who.join(" "));
            }
            if !clean {
                println!(
                    "                            window was not undisturbed — the CPU figure \
includes servicing that input; the condition is the {} unprompted redraws",
                    idle.spontaneous
                );
            }
        }
        None => println!(
            "  zero CPU at idle          NOT MEASURED — needs --window, an event loop is the test"
        ),
    }

    println!("\nwhere the work went");
    println!(
        "  layout cache  {} runs resident, {:.4}% miss, {} generations rotated",
        cache.len(),
        100.0 * cache.misses as f64 / (cache.hits + cache.misses).max(1) as f64,
        cache.generations
    );
    let (mask_use, color_use) = atlas.utilization();
    println!(
        "  glyph atlas   {} entries, mask {:.1}% full, colour {:.1}% full{}",
        atlas.len(),
        mask_use * 100.0,
        color_use * 100.0,
        if atlas.full { ", EXHAUSTED" } else { "" }
    );
    println!(
        "  atlas upload  {} glyphs, {:.2} MB, {} frames hit the {}-glyph bound ({} deferred)",
        atlas.uploads,
        atlas.bytes_uploaded as f64 / 1.0e6,
        atlas.frames_at_budget,
        atlas::UPLOAD_BUDGET,
        atlas.deferred_glyphs
    );
    println!(
        "  per frame     {} rows, {} quads, {} glyphs",
        report.rows_per_frame,
        report.quads / u64::from(report.frames.max(1)),
        report.glyphs / u64::from(report.frames.max(1))
    );
    if gpu.has_gpu_timing() {
        println!(
            "  gpu sampling  {} frames without a free query slot, {} unusable deltas",
            gpu.timing_gaps,
            gpu.dropped_gpu_samples()
        );
    }
}

/// What the idle hold observed. Separated from a bare pair of counters because
/// "0 redraws" and "0 redraws that were not asked for" are different claims.
struct IdleResult {
    cpu_ms: f64,
    seconds: u64,
    redraws: u64,
    spontaneous: u64,
    inputs: u64,
    threads: Vec<(String, u32, u64)>,
}

fn event_name(event: &WindowEvent) -> &'static str {
    match event {
        WindowEvent::RedrawRequested => "RedrawRequested",
        WindowEvent::Resized(_) => "Resized",
        WindowEvent::Moved(_) => "Moved",
        WindowEvent::Focused(_) => "Focused",
        WindowEvent::Occluded(_) => "Occluded",
        WindowEvent::ScaleFactorChanged { .. } => "ScaleFactorChanged",
        WindowEvent::CursorEntered { .. } => "CursorEntered",
        WindowEvent::CursorLeft { .. } => "CursorLeft",
        WindowEvent::CursorMoved { .. } => "CursorMoved",
        WindowEvent::ModifiersChanged(_) => "ModifiersChanged",
        WindowEvent::KeyboardInput { .. } => "KeyboardInput",
        WindowEvent::MouseInput { .. } => "MouseInput",
        WindowEvent::CloseRequested => "CloseRequested",
        _ => "other",
    }
}

fn verdict(pass: bool) -> &'static str {
    if pass {
        "PASS"
    } else {
        "MISS"
    }
}

fn write_png(
    gpu: &Gpu,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    path: &str,
) -> Result<(), String> {
    // Texture-to-buffer copies need 256-byte row alignment, so the readback is
    // padded and the padding is stripped on the way to the encoder.
    let unpadded = width * 4;
    let padded = unpadded.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("golden readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("golden") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);

    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("poll: {e}"))?;

    let mapped = buffer.slice(..).get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    buffer.unmap();

    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&pixels))
        .map_err(|e| format!("{path}: {e}"))?;
    Ok(())
}

// ── windowed ───────────────────────────────────────────────────────────────────

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// The windowed run exists for two things the headless run cannot do: the
/// idle-CPU condition of the bar, which is a statement about an event loop, and
/// a human looking at the glyphs.
struct App {
    options: Options,
    metrics: Metrics,
    store: Store,
    fonts: Fonts,
    instance: wgpu::Instance,
    window: Option<std::sync::Arc<Window>>,
    surface: Option<wgpu::Surface<'static>>,
    surface_format: wgpu::TextureFormat,
    surface_alpha: wgpu::CompositeAlphaMode,
    gpu: Option<Gpu>,
    pass: Pass,
    cache: LayoutCache,
    atlas: Atlas,
    trace: Option<Trace>,
    report: Report,
    stage: Stage,
    idle_started: Option<(Instant, f64)>,
    idle_threads: Vec<(String, u32, u64)>,
    idle_redraws: u64,
    /// Redraws that arrived with no input event before them. These are the ones
    /// the rule forbids: rule 1's step 7 is "if nothing changed and no
    /// animation is running, sleep on the event queue. No timer wake-ups." A
    /// repaint caused by the pointer entering the window is the loop working,
    /// not the loop spinning, and counting the two together measured the wrong
    /// thing — the first version of this harness reported three MISSes that
    /// were all a cursor crossing the window.
    idle_spontaneous_redraws: u64,
    idle_inputs: u64,
    idle_events: Vec<&'static str>,
    /// Frames drawn while the layout scale disagreed with the compositor, or
    /// while the surface was a different size from the window. Either means the
    /// compositor is resampling what we drew, which no amount of care inside
    /// the renderer can make crisp. This counter exists because the 1:1 glyph
    /// assertion is blind to it: that assertion covers atlas-to-framebuffer,
    /// and this covers framebuffer-to-screen.
    scale_mismatch_frames: u64,
    /// The geometry line is printed from the first frame, not from `resumed`:
    /// the compositor reports the real scale *after* the window exists, so
    /// anything printed earlier is a number about to be replaced.
    described: bool,
}

#[derive(PartialEq, Eq)]
enum Stage {
    Scrolling,
    Idling,
    Done,
}

fn windowed(
    options: Options,
    metrics: Metrics,
    store: Store,
    fonts: Fonts,
    instance: wgpu::Instance,
) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("event loop: {e}"))?;
    let (pass, cache, atlas) = harness(&options);
    let mut app = App {
        options,
        metrics,
        store,
        fonts,
        instance,
        window: None,
        surface: None,
        surface_format: wgpu::TextureFormat::Bgra8Unorm,
        surface_alpha: wgpu::CompositeAlphaMode::Auto,
        gpu: None,
        pass,
        cache,
        atlas,
        trace: None,
        report: Report::new(),
        stage: Stage::Scrolling,
        idle_started: None,
        idle_threads: Vec::new(),
        idle_redraws: 0,
        idle_spontaneous_redraws: 0,
        idle_inputs: 0,
        idle_events: Vec::new(),
        scale_mismatch_frames: 0,
        described: false,
    };
    event_loop.run_app(&mut app).map_err(|e| format!("run: {e}"))
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Spike A — a million rows")
            .with_inner_size(winit::dpi::PhysicalSize::new(
                self.options.width,
                self.options.height,
            ));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => std::sync::Arc::new(window),
            Err(e) => {
                eprintln!("create_window: {e}");
                event_loop.exit();
                return;
            }
        };

        let surface = match self.instance.create_surface(window.clone()) {
            Ok(surface) => surface,
            Err(e) => {
                eprintln!("create_surface: {e}");
                event_loop.exit();
                return;
            }
        };
        let adapter = match render::pick_adapter(&self.instance, self.options.adapter.as_deref()) {
            Ok(adapter) => adapter,
            Err(e) => {
                eprintln!("{e}");
                event_loop.exit();
                return;
            }
        };
        let capabilities = surface.get_capabilities(&adapter);
        // Non-sRGB deliberately: coverage is blended in the encoded space, the
        // departure the README records. An sRGB surface would silently change
        // apparent glyph weight between this arm and the other.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or(capabilities.formats[0]);

        let gpu = match Gpu::new(&adapter, format) {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("{e}");
                event_loop.exit();
                return;
            }
        };

        let size = window.inner_size();
        self.surface_format = format;
        self.surface_alpha = capabilities.alpha_modes[0];
        self.options.width = size.width;
        self.options.height = size.height;
        surface.configure(&gpu.device, &self.surface_config(size.width, size.height));
        println!("  adapter      {} · {}", gpu.info.name, gpu.info.driver);

        self.trace = Some(Trace::new(
            self.store.len() as u64,
            f64::from(self.metrics.row_height * self.metrics.scale),
            f64::from(size.height),
        ));
        self.gpu = Some(gpu);
        self.surface = Some(surface);
        self.window = Some(window);
        event_loop.set_control_flow(ControlFlow::Poll);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // During the idle hold, every event is named. A redraw counter alone
        // says the condition was missed; it does not say whether the wake-up
        // came from a timer of ours — which rule 1 forbids — or from the
        // compositor asking for a repaint, which is not ours to refuse.
        if self.stage == Stage::Idling {
            self.idle_events.push(event_name(&event));
            if !matches!(event, WindowEvent::RedrawRequested) {
                self.idle_inputs += 1;
            }
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.rescale(scale_factor as f32)
            }
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.stage != Stage::Idling {
            return;
        }
        // Waiting on the event queue, not on a timer — the condition under test
        // is that no wake-up happens at all. The deadline is checked only when
        // some other event has already woken us.
        let Some((started, cpu_at_start)) = self.idle_started else {
            return;
        };
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed < self.options.idle_seconds as f64 {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                started + std::time::Duration::from_secs(self.options.idle_seconds),
            ));
            return;
        }
        let cpu = stats::cpu_seconds().unwrap_or(cpu_at_start) - cpu_at_start;
        self.stage = Stage::Done;
        if !self.idle_events.is_empty() {
            let mut counted: Vec<(&str, usize)> = Vec::new();
            for name in &self.idle_events {
                match counted.iter_mut().find(|(seen, _)| seen == name) {
                    Some((_, count)) => *count += 1,
                    None => counted.push((name, 1)),
                }
            }
            let summary: Vec<String> =
                counted.iter().map(|(name, n)| format!("{name}x{n}")).collect();
            println!("  idle events   {}", summary.join(" "));
        }
        finish(
            &self.report,
            self.gpu.as_ref().expect("gpu exists once resumed"),
            &self.cache,
            &self.atlas,
            &self.store,
            Some(IdleResult {
                cpu_ms: cpu * 1000.0,
                seconds: elapsed as u64,
                redraws: self.idle_redraws,
                spontaneous: self.idle_spontaneous_redraws,
                inputs: self.idle_inputs,
                threads: stats::thread_cpu_delta(&self.idle_threads, &stats::thread_cpu()),
            }),
            Some(self.scale_mismatch_frames),
        );
        event_loop.exit();
    }
}

impl App {
    /// The compositor learned us a new scale. Glyph raster size is part of both
    /// cache keys, so nothing has to be cleared — old entries simply stop being
    /// looked up. They do occupy atlas space until the run ends, which a real
    /// implementation would reclaim and a spike only has to admit.
    fn rescale(&mut self, scale: f32) {
        if self.options.scale_override.is_some() || (self.metrics.scale - scale).abs() < 0.001 {
            return;
        }
        println!("  scale        compositor says {scale}x — re-rasterizing glyphs for it");
        self.metrics = Metrics::new(scale);
        self.retrace();
    }

    fn surface_config(&self, width: u32, height: u32) -> wgpu::SurfaceConfiguration {
        wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            width,
            height,
            // Not Fifo: a vsync-capped measurement reports the refresh interval
            // as the frame time and calls 8.3 ms a success.
            present_mode: wgpu::PresentMode::AutoNoVsync,
            alpha_mode: self.surface_alpha,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width, height) == (self.options.width, self.options.height)
        {
            return;
        }
        self.options.width = width;
        self.options.height = height;
        if let (Some(gpu), Some(surface)) = (self.gpu.as_ref(), self.surface.as_ref()) {
            surface.configure(&gpu.device, &self.surface_config(width, height));
        }
        self.retrace();
    }

    /// The trace's scroll span depends on row height and viewport height, both of
    /// which just changed, so it restarts rather than continuing with stale
    /// geometry. Any frames already measured are discarded with it — a run whose
    /// scale changed mid-trace is not a run.
    fn retrace(&mut self) {
        self.trace = Some(Trace::new(
            self.store.len() as u64,
            f64::from(self.metrics.row_height * self.metrics.scale),
            f64::from(self.options.height),
        ));
        self.report = Report::new();
        self.cache.invalidate();
        self.described = false;
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(gpu), Some(surface), Some(window)) =
            (self.gpu.as_mut(), self.surface.as_ref(), self.window.as_ref())
        else {
            return;
        };
        if self.stage == Stage::Idling {
            self.idle_redraws += 1;
            if self.idle_inputs == 0 {
                self.idle_spontaneous_redraws += 1;
            }
            return;
        }
        if self.stage == Stage::Done {
            return;
        }

        if !self.described {
            describe(gpu, &self.options, &self.metrics, self.options.height);
            self.described = true;
        }

        let physical = window.inner_size();
        if (window.scale_factor() as f32 - self.metrics.scale).abs() > 0.001
            || (physical.width, physical.height) != (self.options.width, self.options.height)
        {
            self.scale_mismatch_frames += 1;
        }

        let texture = match surface.get_current_texture() {
            Ok(texture) => texture,
            Err(e) => {
                eprintln!("surface: {e}");
                event_loop.exit();
                return;
            }
        };
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let trace = self.trace.as_mut().expect("trace exists once resumed");
        let step = trace.advance();
        if step.invalidate {
            self.cache.invalidate();
        }
        let scroll = trace.scroll;
        let viewport = (self.options.width, self.options.height);

        let (cpu_ms, gpu_ms) = one_frame(
            gpu, &view, &mut self.pass, &self.store, &self.metrics, viewport, scroll,
            &mut self.fonts, &mut self.cache, &mut self.atlas,
        );
        record(&mut self.report, step.phase, cpu_ms, gpu_ms, &self.pass);
        texture.present();

        if step.finished {
            self.stage = Stage::Idling;
            self.idle_started = Some((Instant::now(), stats::cpu_seconds().unwrap_or(0.0)));
            self.idle_threads = stats::thread_cpu();
            println!(
                "  idle         holding {} s on the event queue with no redraw requested\n",
                self.options.idle_seconds
            );
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_secs(self.options.idle_seconds),
            ));
        } else {
            window.request_redraw();
        }
    }
}
