// SPDX-License-Identifier: Apache-2.0
//! Spike A, arm 2 — GPUI. The question, the bar and the method are in
//! [../a-million-rows/README.md](../a-million-rows/README.md), which also
//! records both arms' results.
//!
//! This arm exists so that "we should write our own renderer" is a measurement
//! rather than a preference. It runs the same synthetic store and the same
//! seeded scroll trace as arm 1, over GPUI's `uniform_list`.
//!
//! `data.rs`, `trace.rs` and `stats.rs` are copied from arm 1 rather than shared
//! through a crate. Deliberate: each arm has to build on its own, and a shared
//! crate would let one arm's dependency resolution move the other's.

// `trace::Step::invalidate` is unread here and `stats` is partly unused: both
// modules are byte-identical copies of arm 1's, and keeping them that way is
// worth more than trimming them. The unread field is itself a finding — arm 1's
// `arrival` phase invalidates its layout cache mid-gesture, and GPUI exposes no
// cache to invalidate, so that phase is indistinguishable from `fling` here.
#![allow(dead_code)]

mod data;
mod stats;
mod trace;

use std::time::Instant;

use gpui::{
    div, point, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context,
    SharedString, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};

use data::Store;
use stats::Samples;
use trace::{Phase, Trace};

const ROWS: usize = 1_000_000;
const ROW_HEIGHT: f32 = 24.0;
const IDLE_SECONDS: u64 = 10;

struct ListView {
    store: Store,
    scroll: UniformListScrollHandle,
    trace: Trace,
    /// Time to construct the element tree for the visible range. The closest
    /// analogue to arm 1's immediate-mode pass — and strictly smaller than a
    /// frame, because GPUI's layout and paint happen after `render` returns.
    construct: Vec<Samples>,
    /// Start-of-render to start-of-render. This is the honest total, and it is
    /// bounded below by the display's refresh interval.
    interval: Vec<Samples>,
    last_frame: Option<Instant>,
    phases: Vec<Phase>,
    stage: Stage,
    idle_from: Option<(Instant, f64)>,
    rows_last_frame: usize,
    /// Progress, so a run that is not advancing is distinguishable from a run
    /// that is merely slow. The first attempts at this arm produced no output
    /// for minutes and the difference mattered.
    rendered: u32,
    started: Instant,
}

#[derive(PartialEq, Eq)]
enum Stage {
    Scrolling,
    Idling,
    Done,
}

impl ListView {
    fn new(store: Store, viewport_height: f32) -> Self {
        let phases: Vec<Phase> = trace::PLAN
            .iter()
            .map(|(phase, _)| *phase)
            .filter(|phase| phase.measured())
            .collect();
        Self {
            trace: Trace::new(store.len() as u64, ROW_HEIGHT.into(), viewport_height.into()),
            store,
            scroll: UniformListScrollHandle::new(),
            construct: phases.iter().map(|p| Samples::new(p.label())).collect(),
            interval: phases.iter().map(|p| Samples::new(p.label())).collect(),
            last_frame: None,
            phases,
            stage: Stage::Scrolling,
            idle_from: None,
            rows_last_frame: 0,
            rendered: 0,
            started: Instant::now(),
        }
    }

    fn slot(&self, phase: Phase) -> Option<usize> {
        self.phases.iter().position(|p| *p == phase)
    }

    fn report(&self) {
        println!("frame construction, milliseconds — GPUI element tree for the visible range");
        for samples in &self.construct {
            println!("  {}", samples.row());
        }
        println!("\nframe interval, milliseconds — start of render to start of render, vsync-paced");
        for samples in &self.interval {
            println!("  {}", samples.row());
        }

        let worst = self
            .interval
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| (s.label, s.percentile(99.0)))
            .fold(("none", 0.0f32), |acc, next| if next.1 > acc.1 { next } else { acc });
        println!(
            "\n  worst phase `{}`: interval p99 {:.3} ms, construction p99 {:.3} ms",
            worst.0,
            worst.1,
            self.construct
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| s.percentile(99.0))
                .fold(0.0f32, f32::max)
        );
        match stats::peak_rss() {
            Some(bytes) => println!(
                "  RSS peak {:.1} MB ({})",
                bytes as f32 / 1.0e6,
                if bytes < 300_000_000 { "PASS" } else { "MISS" }
            ),
            None => println!("  RSS UNMEASURED"),
        }
        println!("  rows in the last visible range: {}", self.rows_last_frame);
    }
}

impl Render for ListView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();

        if self.stage == Stage::Idling {
            // Reached only if something asked for a frame during the idle
            // window, which is itself the failure being watched for.
            if let Some((started, cpu_at_start)) = self.idle_from {
                let elapsed = started.elapsed().as_secs_f64();
                if elapsed >= IDLE_SECONDS as f64 {
                    let cpu = stats::cpu_seconds().unwrap_or(cpu_at_start) - cpu_at_start;
                    println!(
                        "  zero CPU at idle          {:.0} ms of CPU over {:.0} s = {:.3}%",
                        cpu * 1000.0,
                        elapsed,
                        100.0 * cpu / elapsed
                    );
                    self.stage = Stage::Done;
                    self.report();
                    cx.quit();
                }
            }
        }

        let step = if self.stage == Stage::Scrolling {
            let step = self.trace.advance();
            self.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.0), px(-(self.trace.scroll as f32))));
            Some(step)
        } else {
            None
        };

        let list = uniform_list(
            "entries",
            self.store.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _window, _cx| {
                this.rows_last_frame = range.len();
                let mut size_text = String::with_capacity(16);
                let mut mtime_text = String::with_capacity(24);
                let mut rows = Vec::with_capacity(range.len());
                for index in range {
                    let id = index as u32;
                    let flags = this.store.flags(id);
                    let is_dir = flags & data::FLAG_DIR != 0;
                    data::format_size(this.store.size(id), flags, &mut size_text);
                    data::format_mtime(this.store.mtime(id), &mut mtime_text);

                    rows.push(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(ROW_HEIGHT))
                            .px(px(8.0))
                            .gap(px(8.0))
                            .bg(if index % 2 == 1 { rgb(0x202026) } else { rgb(0x0e0e12) })
                            .text_color(rgb(0xe8e8ea))
                            .text_size(px(13.0))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(16.0))
                                    .h(px(16.0))
                                    .bg(if is_dir { rgb(0x6e9ed8) } else { rgb(0x7a7a86) }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(SharedString::from(this.store.name(id).to_string())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(92.0))
                                    .text_color(rgb(0x9a9aa4))
                                    .child(SharedString::from(size_text.clone())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(132.0))
                                    .text_color(rgb(0x9a9aa4))
                                    .child(SharedString::from(mtime_text.clone())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(104.0))
                                    .text_color(rgb(0x9a9aa4))
                                    .child(SharedString::from(
                                        data::kind_of(this.store.name(id), flags).to_string(),
                                    )),
                            ),
                    );
                }
                rows
            }),
        )
        .track_scroll(self.scroll.clone())
        .h_full();

        let element = div()
            .size_full()
            .bg(rgb(0x0e0e12))
            .font_family("DejaVu Sans")
            .child(list);

        if let Some(step) = step {
            self.rendered += 1;
            if self.rendered % 240 == 0 {
                println!(
                    "  progress     {} of {} frames in {:.1} s",
                    self.rendered,
                    trace::total_frames(),
                    self.started.elapsed().as_secs_f32()
                );
            }
            let construct_ms = now.elapsed().as_secs_f32() * 1000.0;
            let interval_ms = self
                .last_frame
                .map(|previous| (now - previous).as_secs_f32() * 1000.0);
            self.last_frame = Some(now);

            if step.phase.measured() {
                if let Some(slot) = self.slot(step.phase) {
                    self.construct[slot].push(construct_ms);
                    if let Some(interval) = interval_ms {
                        self.interval[slot].push(interval);
                    }
                }
            }

            if step.finished {
                self.stage = Stage::Idling;
                self.idle_from = Some((Instant::now(), stats::cpu_seconds().unwrap_or(0.0)));
                println!("  idle         holding {IDLE_SECONDS} s with no frame requested\n");
                // One timer, outside the frame loop, purely to end the run.
                cx.spawn(async move |view, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(IDLE_SECONDS))
                        .await;
                    let _ = view.update(cx, |view, cx| {
                        if let Some((started, cpu_at_start)) = view.idle_from {
                            let elapsed = started.elapsed().as_secs_f64();
                            let cpu =
                                stats::cpu_seconds().unwrap_or(cpu_at_start) - cpu_at_start;
                            println!(
                                "  zero CPU at idle          {:.0} ms of CPU over {:.0} s = {:.3}%",
                                cpu * 1000.0,
                                elapsed,
                                100.0 * cpu / elapsed
                            );
                        }
                        view.stage = Stage::Done;
                        view.report();
                        cx.quit();
                    });
                })
                .detach();
            } else {
                // Keep the loop running. Two earlier attempts did not: a bare
                // `request_animation_frame` left the process at 1.1% CPU with no
                // further frames, and a `cx.notify()` from inside `render` is
                // dropped, because GPUI clears the dirty flag after the render
                // it is already performing. Notifying from `on_next_frame` —
                // after the frame is presented — is what actually continues the
                // loop. GPUI paces the result to the display, which is why the
                // interval and not the construction time is the total.
                let entity = cx.entity();
                window.on_next_frame(move |_window, cx| {
                    entity.update(cx, |_, cx| cx.notify());
                });
            }
        }

        element
    }
}

fn main() {
    let started = Instant::now();
    let store = Store::synthetic(ROWS, 0xA11_C0DE);
    println!("Spike A arm 2 — GPUI 0.2");
    println!(
        "  store        {} entries, {:.1} MB, built in {:.0} ms",
        store.len(),
        store.footprint() as f32 / 1.0e6,
        started.elapsed().as_secs_f32() * 1000.0
    );
    println!("  trace        {} frames across {} phases", trace::total_frames(), trace::PLAN.len());

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1920.0), px(1080.0)), cx);
        let height = f32::from(bounds.size.height);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ListView::new(store, height)),
        )
        .expect("open_window");
    });
}
