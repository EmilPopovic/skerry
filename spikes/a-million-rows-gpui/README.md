# Spike A, arm 2 — GPUI

The question, the bar, the method and the verdict for Spike A live in
[../a-million-rows/README.md](../a-million-rows/README.md). This arm exists so
that "we should write our own renderer" is a measurement rather than a
preference: GPUI already has a virtualized list, a GPU renderer and a text stack,
and if it meets the bar then [ADR 0001](../../docs/adr/0001-custom-gpu-ui.md) is
buying months of work for nothing.

It runs the same synthetic store and the same seeded scroll trace as arm 1, over
GPUI's `uniform_list`, at 1,000,000 entries.

`data.rs`, `trace.rs` and `stats.rs` are copied from arm 1 rather than shared
through a crate. Deliberate: each arm has to build on its own, and a shared crate
would let one arm's dependency resolution move the other's.

## What decided this arm

Not the frame time. The dependency graph, measured with
`cargo tree --edges normal --target x86_64-unknown-linux-gnu`:

| | Arm 1 | Arm 2 |
| --- | --- | --- |
| Transitive crates | 120 whole spike, 64 for the four crates the product keeps | **467** |
| Crates `tools/check-invariants.py` bans | none | `tokio`, `async-std`, `smol`, `rayon`, `anyhow` |
| GPU abstraction | `wgpu` → Vulkan | `blade-graphics` → `ash` |
| Text | `swash`, our own layout | `cosmic-text` + `swash` + `freetype-sys` |

Two independent disqualifications, neither of which a frame timer can overturn:

- **467 crates is 3.9× the total transitive budget of 120** in
  [09-performance-budgets.md](../../docs/09-performance-budgets.md), which is a
  hard fail, and it fires
  [ADR 0003](../../docs/adr/0003-wgpu-first-backend.md)'s revisit trigger on its
  own.
- **Three async runtimes and `rayon`.**
  [ADR 0005](../../docs/adr/0005-no-async-runtime.md) forbids a second scheduler
  because "a second thread pool means uncoordinated oversubscription, which is a
  latency bug that only appears under load". This is four.

`freetype-sys` is worth naming separately: it is a C dependency, which is what
[06-portability.md](../../docs/06-portability.md)'s glibc-and-musl requirement
has to survive, and arm 1's stack has none.

## Frame time

Recorded honestly, including how it was obtained and what it cost to obtain.

GPUI's frame loop is driven entirely by the compositor. `Window::on_next_frame`
appends to `next_frame_callbacks`, and those callbacks are drained from
`platform_window.on_request_frame` — a Wayland frame callback. Three ways of
asking for a continuous loop were tried:

| Attempt | Result |
| --- | --- |
| `window.request_animation_frame()` from `render` | process sat at 1.4% CPU, no further frames |
| `cx.notify()` from inside `render` | dropped — GPUI clears the dirty flag after the render it is already performing |
| notify from `window.on_next_frame` | correct in principle, and equivalent to what `request_animation_frame` does internally |

The third is the right call and is what the code does. It still advances only as
fast as the compositor delivers frame callbacks, which means **this arm cannot be
driven headless, and cannot be driven at all in a session where its window is not
being presented.** Arm 1 has no such dependency: it runs headless against any of
three adapters, which is also what lets its numbers come from CI rather than from
a desk.

That difference is not a footnote. A renderer that can only be measured when
someone is looking at it cannot have the performance regression CI that
[08-roadmap.md](../../docs/08-roadmap.md) puts in M8, and cannot produce the
golden frames on llvmpipe that
[04-architecture.md](../../docs/04-architecture.md#testing-strategy-by-layer) makes
mandatory for every commit.

### Numbers

The frame-time distribution was **not obtained**, for the reason above: in this
session the compositor did not deliver frame callbacks to the window, so `render`
ran a handful of times and the 1920-frame trace never advanced. The harness
prints progress every 240 frames and printed none in 150 seconds. That is
recorded as a gap, not filled with an estimate.

Two conditions of the bar *were* measured, from outside the process, and both
pass — stated plainly because they do not favour the conclusion this arm reaches:

| Condition | Arm 2 (GPUI) | Arm 1 (`wgpu` + `swash`) |
| --- | --- | --- |
| RSS, 1M entries | **185.6 MB** | 217.5 MB (RADV) |
| Idle CPU | **0 ticks over 10 s = 0.000%** | 0 ms over 10 s = 0.000% |
| Threads at idle | **31** | 1 |

GPUI's footprint is *lower* than arm 1's, and its idle behaviour is identical.
Arm 1's figure includes 31.8 MB of fallback fonts read eagerly into memory where
GPUI loads lazily through fontconfig, which accounts for most of the difference —
so this is close to a tie rather than a loss, and either way it is not the
argument against GPUI.

The 31 threads are, though. GPUI brings its own executor with a full worker pool
before a single row is drawn, and that pool is not ours to coordinate. That is
[ADR 0005](../../docs/adr/0005-no-async-runtime.md)'s objection stated as a
process listing rather than as a principle: "a second thread pool means
uncoordinated oversubscription, which is a latency bug that only appears under
load."

### What a like-for-like frame comparison would take

Not a longer timeout. GPUI paces to the compositor and exposes no way to turn it
off, so its interval is floored at the refresh period, while arm 1's number is
work per frame with `AutoNoVsync`. Making the two comparable means patching GPUI
to present unthrottled and to expose layout and paint timing. Given that the
dependency graph disqualifies it twice over — 467 crates against a budget of 120,
and four schedulers against ADR 0005's one — that was not worth doing, and saying
so is more honest than reporting two numbers that do not measure the same thing.
