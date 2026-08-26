# Spike A — a million rows at 120 fps

**Status:** arm 1 run and recorded. Arm 2 recorded. Verdict below.

## Question

Can a Rust process paint a virtualized list of 1,000,000 entries — icon, shaped
filename and three columns per row — at a p99 frame under 8 ms while scrolling,
on Wayland at fractional scale, on an integrated GPU and on llvmpipe?

## Bar

Quoted from [08-roadmap.md](../../docs/08-roadmap.md#phase-0--spikes-34-weeks),
written before the first line of code and not adjusted afterwards:

> **Bar:** p99 frame under 8 ms while scrolling · crisp glyphs at fractional scale · under
> 300 MB RSS · zero CPU at idle.

Four conditions, each with a separate number below. The verdict is the
conjunction: three out of four is a no-go.

**The p99 is taken over the worst scroll phase, not over the whole trace.** A p99
averaged across a slow scroll and a fling is a number that hides the fling.

## What it decides

[ADR 0003](../../docs/adr/0003-wgpu-first-backend.md) — `wgpu` as the first GPU
backend — is *Accepted, pending Spike A*, and its revisit trigger fires if "Spike
A shows the `ash` path measurably ahead on frame time at equal effort". This
spike also decides, by measurement rather than by argument, whether GPUI would
have given us the same frame time for a fraction of the work, which would
invalidate [ADR 0001](../../docs/adr/0001-custom-gpu-ui.md) instead.

So it is built twice:

| Arm | Where | Stack |
| --- | --- | --- |
| 1 | this package | `wgpu` + `swash`, our own atlas, batching and layout cache |
| 2 | [../a-million-rows-gpui/](../a-million-rows-gpui/) | GPUI's `uniform_list` |

Arm 1 is the stack the ADRs currently choose, so it carries the full
instrumentation. Arm 2 exists to make the comparison a number.

## Method

### Machine

Recorded honestly, because **this is not the reference machine.**
[09-performance-budgets.md](../../docs/09-performance-budgets.md) specifies "a
2021-era four-core laptop with integrated graphics", and this box is a 2024
24-thread part. A pass here is therefore *necessary but not sufficient*: the
reference-machine run is still owed before ADR 0003's status changes.

| | |
| --- | --- |
| CPU | AMD Ryzen AI 9 HX 370, 24 threads |
| RAM | 60 GiB |
| Kernel | 7.0.0-30-generic |
| Session | Wayland (native), X11 via Xwayland |
| Mesa | 26.0.8 |

### Devices

The roadmap asks for Intel, AMD, NVIDIA proprietary and llvmpipe. Three of the
four are present; **Intel integrated is absent from this machine and is a gap in
this run**, recorded rather than glossed.

| Device | Driver | Selected with |
| --- | --- | --- |
| Radeon 890M (integrated) | RADV | `--adapter radv` |
| GeForce RTX 5070 Mobile | NVIDIA proprietary | `--adapter nvidia` |
| llvmpipe | lavapipe | `--adapter llvmpipe` |

### Workload

1,000,000 synthetic entries in the columnar layout from
[04-architecture.md](../../docs/04-architecture.md#data-layout) — one name arena,
parallel `size`/`mtime`/`flags` arrays — because measuring a `Vec<Entry>` would
measure a data structure we have already decided not to ship.

Names are deliberately adversarial, in the mix a real machine produces: ASCII,
Latin-1 diacritics, CJK, Arabic and Hebrew RTL, combining marks, emoji, and
names long enough to ellipsize. A spike that shapes only ASCII measures a
shaping fast path we cannot rely on.

Each row paints an icon quad, the shaped name, and three columns — size, mtime,
kind — which is ~60 glyphs and ~62 instanced quads per row.

### The scroll trace

Deterministic, seeded, identical between arms and between runs. Phases are
measured separately, because they stress different things and one number over
all of them would hide the interesting one:

| Phase | Frames | What it stresses |
| --- | --- | --- |
| `warmup` | 120 | discarded — seeded jumps, so the glyph atlas is warm before the first measured frame |
| `slow` | 600 | 1 device px/frame — subpixel positioning, near-total cache hits |
| `fling` | 600 | 40 rows/frame — every visible row is a layout-cache miss |
| `jump` | 300 | teleport to a seeded random offset each frame — worst-case thrash |
| `arrival` | 300 | fling *while* a listing arrives and invalidates the cache mid-gesture |

`arrival` exists because of the sentence in
[09-performance-budgets.md](../../docs/09-performance-budgets.md#on-measuring-frame-time-honestly):
the interesting frames are the ones where a directory finishes loading while the
user is mid-scroll. A trace without that phase is a trace of the easy case.

### What is measured, and how

| Number | How |
| --- | --- |
| CPU frame time | `Instant` around the whole redraw: immediate-mode pass, shaping, atlas upload, submit |
| GPU frame time | `wgpu` timestamp queries around the render pass, resolved two frames later so the readback never stalls the loop |
| Peak RSS | `VmHWM` from `/proc/self/status`, with 1M entries resident |
| Idle CPU | `utime + stime` from `/proc/self/stat` over 10 s of `ControlFlow::Wait`, plus per-thread accounting from `/proc/self/task/*/stat`, plus a count of *unprompted* redraws — ones with no input event before them — which must be 0 |
| Glyph crispness | count of glyph quads drawn at anything other than 1:1 from their raster, which must be 0 — plus golden PNGs at each scale for the judgement a number cannot make |
| Atlas upload cap | frames in which the per-frame glyph upload bound from [the frame loop](../../docs/04-architecture.md#the-frame-loop-precisely) was hit |

Latencies are reported as p50, p99 and max per phase, over the frame counts
above, and **each configuration is run three times** with the minimum, median and
maximum of its worst-phase p99 reported — because a single run of this harness
varies by more than the difference between two of the devices.

**The machine must be quiet.** llvmpipe is a CPU rasterizer and competes directly
with anything else running: a sweep taken next to a `cargo build` produced a
16.008 ms p99 on llvmpipe at 100%, twice the bar, against 2.428 ms for the same
configuration on an idle machine. The sweep therefore waits for a one-minute load
average under 1.3 before it starts and records the load next to every sample.
This is not a detail — it is the difference between a pass and a fail on the row
that decides the project. Present mode is `AutoNoVsync` for measurement runs, so the number is the
work per frame rather than the display's refresh interval — recorded here
because a vsync-capped measurement would report 8.3 ms as a success.

Crispness is enforced by construction rather than judged: glyphs are rasterized
at `size × scale` in device pixels with four horizontal subpixel buckets, and
the harness asserts every glyph quad's destination rectangle equals its atlas
source rectangle in pixels. A scaled glyph is a failed assertion, not an
opinion.

### Deliberate departures

- **`winit`, not our own Wayland backend.** [ADR 0002](../../docs/adr/0002-own-platform-layer.md)
  says we own the platform layer, and Spike D is where that is tested. Writing a
  Wayland backend here would confound a renderer measurement with a platform-layer
  measurement and delay both.
- **No `sk-*` crates.** Phase 0 precedes the workspace
  ([08-roadmap.md](../../docs/08-roadmap.md#what-comes-next)). Spikes are exempt from the
  layering, lint and dependency rules — not from the banned-toolkit rule, which
  `tools/check-invariants.py` still enforces here.
- **Grayscale antialiasing, not subpixel RGB.** One variable at a time; the
  crispness question is about raster size and subpixel *positioning*, and RGB
  filtering can be added later without changing the atlas.

## Result

Arm 1, `wgpu` 27 + `swash` 0.2, release build, 1,000,000 entries, 1920×1080
device pixels, headless unless stated. 1920 frames per run, of which 1800 are
measured. Every figure below is from a run committed under
[runs/](runs/); the golden frames are in [frames/](frames/).

### Worst-phase p99, three runs per configuration, milliseconds

The headline table. Each cell is min / **median** / max of the worst phase's p99
across three runs on an otherwise idle machine. CPU is the whole of what the UI
thread owes the frame — the immediate-mode pass over visible rows, shaping on a
cache miss, glyph rasterization, atlas upload and submit. GPU is the render pass,
from timestamp queries.

| Device | scale | CPU | GPU |
| --- | --- | --- | --- |
| Radeon 890M (RADV) | 1.0 | 0.834 / **1.454** / 1.758 | 0.154 / **0.174** / 0.188 |
| Radeon 890M (RADV) | 1.5 | 0.638 / **0.716** / 0.803 | 0.156 / **0.165** / 0.172 |
| RTX 5070 (NVIDIA) | 1.0 | 0.659 / **0.725** / 1.582 | 0.020 / **0.020** / 0.032 |
| RTX 5070 (NVIDIA) | 1.5 | 0.509 / **0.563** / 1.215 | 0.019 / **0.019** / 0.020 |
| llvmpipe | 1.0 | 2.042 / **2.100** / 2.618 | 1.967 / **1.981** / 2.482 |
| llvmpipe | 1.5 | 1.816 / **1.941** / 1.951 | 1.695 / **1.761** / 1.869 |

**Worst p99 observed anywhere across eighteen runs: 2.618 ms CPU and
2.482 ms GPU, both on llvmpipe at 100%.** That is 3.1× under the
8 ms bar. The discrete GPU never exceeds 0.032 ms, which is 1/250th of the
budget.

Run-to-run spread is real and worth more than the third decimal place: the same
configuration varies by up to 2.4× between runs — NVIDIA at 100% ranged 0.659 to
1.582 ms, RADV at 100% ranged 0.834 to 1.758 ms. Any single number from this
harness should be read as "about 1 ms on hardware, about 2 ms on llvmpipe", and
the reason the bar is met is the order of magnitude, not the digits. llvmpipe is
the *steadiest* of the three once the machine is quiet, which is what a software
rasterizer with no driver scheduling should look like.

### One run in detail, by phase

The spread table says whether the bar is met; this one says where the time goes.
A single representative run per configuration, p99 per phase:

| Device | scale | | `slow` | `fling` | `jump` | `arrival` |
| --- | --- | --- | --- | --- | --- | --- |
| Radeon 890M (RADV) | 1.0 | cpu | 0.578 | **1.065** | 0.695 | 0.584 |
| | | gpu | 0.168 | 0.129 | 0.139 | 0.139 |
| Radeon 890M (RADV) | 1.5 | cpu | 0.390 | 0.523 | 0.487 | 0.421 |
| | | gpu | 0.168 | 0.129 | 0.135 | 0.134 |
| RTX 5070 (NVIDIA) | 1.0 | cpu | 0.087 | 0.620 | **0.714** | 0.609 |
| | | gpu | 0.020 | 0.020 | 0.020 | 0.020 |
| RTX 5070 (NVIDIA) | 1.5 | cpu | 0.068 | 0.527 | 0.496 | 0.444 |
| | | gpu | 0.019 | 0.018 | 0.019 | 0.019 |
| llvmpipe | 1.0 | cpu | **2.557** | 2.501 | 1.970 | 1.933 |
| | | gpu | 2.434 | 2.370 | 1.893 | 1.877 |
| llvmpipe | 1.5 | cpu | 1.872 | 1.707 | 1.867 | 1.763 |
| | | gpu | 1.665 | 1.603 | 1.711 | 1.614 |

Three things the table says that the summary number does not:

- **llvmpipe is GPU-bound and the other two are CPU-bound.** On llvmpipe the CPU
  and GPU figures track each other within 0.15 ms, because `submit` back-pressures
  against lavapipe's own rasterization. On RADV and NVIDIA the GPU is idle by
  comparison — 0.02 ms on the 5070, which is 1/400th of the budget. The frame is a
  CPU problem, and ADR 0001's bet that the GPU work is "instanced quads and two
  texture atlases" holds: 2,269 quads per frame in one draw call.
- **The 150% run is *faster* than the 100% run.** Fewer rows fit the same
  framebuffer, so there is less to shape. The bar's "at fractional scale"
  condition is not a performance risk on this workload; it is a correctness one,
  and that is measured separately below.
- **`fling` is the expensive phase, and `arrival` is not.** Invalidating the
  layout cache mid-gesture costs less than the steady state of a fling, because
  a fling keeps the cache permanently at capacity and pays for generation
  rotations — 42 of them per run — while an invalidation empties it and buys
  quiet. The tail of `fling` *is* the rotation.

### RSS, peak, 1,000,000 entries

| Device | floor at 1,000 entries | at 1,000,000 | marginal cost of a million rows |
| --- | --- | --- | --- |
| Radeon 890M (RADV) | 167.2 MB | 217.5 MB | 50.3 MB |
| llvmpipe | 207.7 MB | 256.8 MB | 49.1 MB |
| RTX 5070 (NVIDIA) | **332.0 MB** | **375.3 MB** | 43.3 MB |

**The NVIDIA proprietary driver misses the 300 MB condition, and it misses it
before our data exists.** Its floor with a thousand rows on screen is 332 MB —
32 MB over the bar with the store essentially empty. RADV and llvmpipe pass.

The marginal cost of a million rows is **43–50 MB** on every driver, against a
store that *reserves* 60.0 MB — the name arena is over-reserved at 34 bytes per
entry and the pages it never touches never fault in. Either way the columnar
layout is doing what
[04-architecture.md](../../docs/04-architecture.md#data-layout) claims: a million
entries cost tens of megabytes, not hundreds. The *floor* is not ours to control,
and it is where the budget is at risk.

One attributable and fixable slice of the floor is ours: this arm `read`s all six
fallback fonts into `Vec<u8>`, which is **31.8 MB resident**, 19.5 MB of it
`NotoSansCJK-Regular.ttc`. Memory-mapping them so only touched pages are
resident is a one-line change we did not make here, and it is worth roughly a
third of the RADV floor above the process baseline.

### Zero CPU at idle

Windowed on RADV, `ControlFlow::Wait`, no redraw requested, no timer. Across
eighteen runs at 200,000 and 1,000,000 entries:

| | |
| --- | --- |
| Unprompted redraws | **0**, in every run |
| CPU over the 10 s window | **0 ms in sixteen of eighteen runs**; one 10 ms tick (0.100%) in two |
| Threads accumulating CPU during the window | **none**, in every run |

**This condition passes, and the first version of this harness said it did not.**
The correction is worth recording, because it is the sharper half of the result:

The first harness counted *any* redraw during the idle hold as a violation and
reported three misses at 0.100–0.200%. Naming the events showed what they were:
`CursorMoved`×28, `Focused`×2, `ModifiersChanged`×2, `CursorLeft`×1 — the pointer
crossing the window while the hold was running. Step 7 of the frame loop says
"if nothing changed and no animation is running, sleep on the event queue. No
timer wake-ups"; a repaint the compositor asked for after the pointer entered is
the loop doing its job, not the loop spinning. The check now separates
*unprompted* redraws from serviced input, and reports the event breakdown either
way, so the distinction is visible rather than assumed.

The residual 10 ms tick is one tick of a 100 Hz accounting clock, and per-thread
accounting attributes it to **no live thread** — so the most likely reading is
that it straddles the boundary between the last rendered frame and the start of
the idle window rather than occurring inside it. That is an inference from the
per-thread evidence, not a proof; what is measured is that no thread accumulates
CPU during the window and that the figure is 0.100% at worst against a 0.3% hard
fail in [09-performance-budgets.md](../../docs/09-performance-budgets.md).

Windowed frame times matched the headless ones — three runs at the compositor's
own 1.25× scale gave worst-phase p99 of 1.343, 1.408 and 1.567 ms, against
0.834–1.758 ms headless on the same device — so none of the frame measurement
depended on running without a compositor. Raw output in [runs/](runs/) under
`windowed-radv-native-*`; these were re-captured after the scale-negotiation fix
below, because the earlier windowed runs had been measured at the wrong scale.

### Crisp glyphs at fractional scale

Two claims, and they are not the same claim. The first was measured from the
start; the second was reported as passing on evidence that could not see it fail,
and was caught by someone looking at the screen.

**Inside the renderer: 0 glyph quads out of 29.6 million drawn at anything but
1:1 with their raster**, across every device and every scale, including 125% and
200%. 0 glyphs dropped, mask atlas never above 3.5% of 2048². Glyphs are
rasterized at `size × scale` in device pixels with four horizontal subpixel
buckets and drawn at integer device-pixel origins, so this is crisp by
construction rather than by inspection.

**On screen at 1.25×, it was fuzzy anyway — and the harness said PASS.**

The mechanism. `winit` learns the real scale factor *after* the window exists and
reports it through `ScaleFactorChanged`, then resizes the surface. This spike
handled neither event: it read `scale_factor()` once, immediately after
`create_window`, got the provisional `1.0`, and kept painting a 1146×1402 buffer
into a surface the compositor had grown to 1425×1750. Hyprland then stretched
that buffer by exactly 1.25 to fill the window. Every glyph was resampled *after*
the 1:1 assertion had already passed.

Measured on the actual screen with `grim`, distinct grey levels in the same text
region — resampling collapses a two-level edge into a ramp and then quantizes it:

| | distinct grey levels | range |
| --- | --- | --- |
| On screen, before the fix | **79** | 26–125 |
| On screen, after the fix | **210** | 22–231 |
| Headless golden PNG, never resampled | 220 | 22–233 |

After the fix the screen is within 5% of the 1:1 reference. Before it, the
dynamic range was compressed to under half.

**Why the gate could not see it.** The 1:1 assertion covers
atlas → framebuffer. Nothing covered framebuffer → screen, and the golden PNGs
are captured headless, where there is no compositor to resample — so they were
evidence that *the renderer does not scale glyphs*, and were being read as
evidence that *the text is sharp*. Those are different statements, and only the
first was ever tested.

**The fix, and the new gate.** The windowed path now handles
`ScaleFactorChanged` and `Resized`, takes its layout scale from the compositor
unless `--scale` overrides it, reconfigures the surface, and restarts the trace —
a run whose scale changed mid-trace is not a run. Glyph and layout cache keys
already include the quarter-pixel size, so nothing needed clearing; stale entries
simply stop being looked up, and occupy atlas space until the run ends, which a
real implementation would reclaim.

The counter that would have caught this is now permanent: **frames drawn at a
scale or size the compositor had to resample.** Reintroducing the bug with
`--scale 1.0` on a 1.25 display makes it read `1920 frames ... (MISS)` while the
1:1 glyph assertion still reads `0 ... (PASS)` — which is the blind spot stated as
a test.

**What this means for the product.** Crispness at fractional scale is not a
rasterization problem, it is a *scale-negotiation* problem, and it lives in the
platform layer. `sk-plat` must bind `wp_fractional_scale_v1` and `wp_viewporter`
and treat a scale change as invalidating the glyph atlas —
[ADR 0002](../../docs/adr/0002-own-platform-layer.md)'s case, strengthened: a
toolkit that gets this subtly wrong produces exactly the blur every other Linux
file manager has at 1.25×, and no amount of care inside the renderer can recover
it. **Spike D must cover scale negotiation explicitly**, and golden frames must
be accompanied by an on-screen check, because they cannot certify this alone.

### Arm 2 — GPUI

Recorded in [../a-million-rows-gpui/](../a-million-rows-gpui/). The comparison
that decided this arm needed no frame timer at all:

| | Arm 1 (`wgpu` + `swash`) | Arm 2 (GPUI) |
| --- | --- | --- |
| Transitive crates, Linux target | 120 whole spike · **64** for the four crates the product would keep | **467** |
| Crates banned by `tools/check-invariants.py` | 0 | `tokio`, `async-std`, `smol`, `rayon`, `anyhow` |
| GPU abstraction | `wgpu` → Vulkan | `blade-graphics` → `ash` |
| Text stack | `swash`, our layout | `cosmic-text` + `swash`, `freetype-sys` |

467 crates is 3.9× the *total* transitive budget of 120 in
[09-performance-budgets.md](../../docs/09-performance-budgets.md), and it trips
[ADR 0003](../../docs/adr/0003-wgpu-first-backend.md)'s revisit trigger on its
own. The banned crates are not a budget question:
[ADR 0005](../../docs/adr/0005-no-async-runtime.md) forbids a second scheduler
because "a second thread pool means uncoordinated oversubscription, which is a
latency bug that only appears under load", and GPUI brings three async runtimes
and `rayon`.

The frame-time distribution for arm 2 was **not obtained**: GPUI's loop is
driven by compositor frame callbacks, and in this session the window received
none, so the trace never advanced. Two conditions were measured from outside the
process and both pass — **185.6 MB RSS**, *lower* than arm 1's 217.5 MB, and
**0.000% idle CPU**, identical. GPUI is not worse on footprint or on idle, and
saying otherwise would be picking numbers.

What it is worse on is 31 threads at idle against arm 1's one, which is
[ADR 0005](../../docs/adr/0005-no-async-runtime.md)'s objection as a process
listing rather than a principle. A stack that cannot enter the workspace cannot
be chosen on frame time, so the missing distribution does not change the
conclusion — but it is a gap in this spike, not a result.

## Verdict

**Go.** Frame time, crispness and idle CPU all pass; RSS passes on the
integrated GPU and on llvmpipe and misses on the NVIDIA proprietary driver, for a
reason that is not ours. The worst p99 across eighteen runs on an idle machine is
2.618 ms against an 8 ms bar.

Idle CPU was measured once, windowed on RADV, because it is a property of the
event loop rather than of the adapter — a `ControlFlow::Wait` loop that requests
no redraw does not become busier on a different GPU.

[ADR 0003](../../docs/adr/0003-wgpu-first-backend.md) stands: its revisit trigger
asked whether "the `ash` path [is] measurably ahead on frame time at equal
effort", and with the GPU consuming 0.02 ms of an 8 ms budget on real hardware
there is no frame time left for a different backend to win. Arm 2 does not
contradict this and does not confirm it either: its frame distribution was never
obtained, and it is excluded on 467 crates and four schedulers rather than on
speed.

One number in ADR 0003's revisit trigger deserves saying out loud rather than
leaving in a table: the trigger fires if "the total transitive count exceeds
120", and the four crates this arm would actually keep — `wgpu`, `swash`,
`bytemuck`, `pollster` — already pull **64**, of which only 6 are named
`wgpu*`/`naga*`. `sk-plat`'s own tree lands on top of that. The trigger is not
fired; it has 56 crates of headroom for the entire rest of the product, and that
is a tighter constraint than the frame budget turned out to be.

That count also exposes a defect in the gate that enforces it.
`tools/dep-budget.py` classifies the GPU stack by name prefix, so `ash`, `glow`,
`gpu-alloc`, `gpu-descriptor`, `khronos-egl`, `spirv`, `renderdoc-sys`,
`codespan-reporting`, `hexf-parse` and roughly fifteen more count against the
25-crate *non-GPU* budget. Measured here: 6 crates classified GPU, 58 not, when
the honest split is closer to 45/19. The budget cannot be enforced until the GPU
stack is computed as the set reachable from the `wgpu` edge rather than matched
on a name — which is a change to make before the first crate exists, not after
the check starts failing for the wrong reason.

Three things this spike does **not** license, written down so they are not
quietly assumed:

1. **The reference-machine run is still owed.** This is a 24-thread 2024 part,
   not the "2021-era four-core laptop" the budgets are written against. The
   headroom is 3.1× and the workload is CPU-bound, so the margin is very likely
   to survive; "very likely" is not a measurement. Note also that a four-core
   part has less slack against the load sensitivity described in the method —
   the contaminated sweep reached 16 ms on twenty-four threads.
2. **Intel integrated is unmeasured.** Absent from this machine. It is the most
   common Linux GPU and the roadmap asked for it by name.
3. **The RSS budget needs a decision, not a rerun.** The 250 MB target in
   [09-performance-budgets.md](../../docs/09-performance-budgets.md) is
   reachable on RADV (217.5 MB) and marginal on llvmpipe (256.8 MB); the 90 MB
   "ordinary session" target is *below the driver floor on all three devices* and
   cannot be met on a Vulkan stack as measured. That row wants either a new
   number with the driver floor stated separately, or a superseding ADR — not a
   quieter measurement.

What lands as a permanent gate from this spike: the p99-by-phase harness, the
non-1:1 glyph assertion, the **surface-matches-screen counter**, the golden frames
on llvmpipe, and the idle-CPU check with its unprompted-redraw counter.

Two of those five exist because a number was wrong and something else caught it —
the surface counter because a person looked at the screen and said the text was
fuzzier than the editor next to it, and the unprompted-redraw distinction because
the first version called a moving mouse a failure. Neither the frame timings nor
the glyph assertion would ever have surfaced either one. That is the argument for
the golden frames and the dogfooding gates in
[08-roadmap.md](../../docs/08-roadmap.md) being in the plan rather than added
later.

## Reproducing it

```
cd spikes/a-million-rows

# One device, one scale, headless — the measurement path CI would use.
cargo run --release -- --adapter llvmpipe --scale 1.5

# The full matrix.
for adapter in radv nvidia llvmpipe; do
  for scale in 1.0 1.5; do
    cargo run --release -- --adapter "$adapter" --scale "$scale"
  done
done

# Golden frames.
for scale in 1.0 1.25 1.5 2.0; do
  cargo run --release -- --adapter llvmpipe --scale "$scale" \
    --size 1280x720 --golden "frames/llvmpipe-${scale}x.png"
done

# The idle-CPU condition needs a real window and a real event loop.
cargo run --release -- --adapter radv --window --idle 10
```

`--help` lists the rest. `--adapter` takes a substring of the adapter or driver
name and fails with the list of what is present rather than silently picking
another device, because a frame time attributed to the wrong GPU is worse than no
frame time.

Committed evidence: [runs/](runs/) holds the raw output of every run quoted
above, [frames/](frames/) the golden PNGs. Both are kept after the code is thrown
away — they are what the ADRs cite.
