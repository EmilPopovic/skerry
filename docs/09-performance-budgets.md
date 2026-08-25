# Performance budgets

These are CI gates expressed as failing tests, not aspirations in a wiki. A regression in
any row breaks the build. Measured on the reference machine (a 2021-era four-core laptop
with integrated graphics) and, for the frame rows, also on llvmpipe.

## Latency and throughput

| Measure | Target | Hard fail | Method |
| --- | --- | --- | --- |
| Cold start to first frame | < 80 ms | 150 ms | Wall clock from `exec` to first present, warm page cache |
| Keystroke to frame | < 8 ms | 16 ms | p99 over a scripted 10k-event input replay |
| Open a 10k-entry directory | < 30 ms | 60 ms | Time to first painted row set |
| Scroll 1M entries | < 8 ms/frame | 16 ms | p99 frame time via GPU timestamp queries |
| Sort 1M entries | < 120 ms | 300 ms | Off-thread; the UI stays interactive throughout, which is asserted separately |
| First search result | < 50 ms | 120 ms | From the first keystroke on a warm 1M-file tree |
| Thumbnail appears | < 200 ms | 500 ms | For a visible JPEG not yet in the shared cache |
| Frame during a hung mount | < 8 ms | 16 ms | sshfs to a suspended host, unplugged mid-listing — rule 1's canonical test |

## Footprint

| Measure | Target | Hard fail | Method |
| --- | --- | --- | --- |
| RSS at 1M entries | < 250 MB | 450 MB | Peak, one pane, thumbnails off |
| RSS, ordinary session | < 90 MB | 150 MB | Three tabs, a few thousand entries, thumbnails on |
| Idle CPU | 0.0% | 0.3% | No redraw without input or animation; no timer wake-ups |
| Stripped binary | < 20 MB | 40 MB | Release, LTO, no debug symbols |
| Transitive crates (non-GPU) | ≤ 25 | 25 | `tools/dep-budget` |
| Transitive crates (total) | ≤ 120 | 120 | Exceeding this fires [ADR 0003](adr/0003-wgpu-first-backend.md)'s revisit trigger |

## Correctness gates that are not negotiable

Not performance, but they run in the same CI stage and fail the build the same way:

| Gate | Method |
| --- | --- |
| Zero data loss across the filesystem matrix | Spike E's matrix, nightly on the full set, per-commit on tmpfs and ext4 |
| Every mutation is undoable | A test enumerates the command registry and asserts every mutating command has an inverse |
| Every command is in the palette | Registry introspection — rule 2 as an assertion |
| Every fallback path works | The forced-degradation matrix from [06-portability.md](06-portability.md) |
| Adversarial filenames render and round-trip | A fixture directory: RTL, combining marks, emoji, 255-byte names, embedded newlines, invalid UTF-8 |
| Golden frames at 100/125/150/200% | Perceptual diff against references, on llvmpipe |

## What Spike A measured

[Spike A](../spikes/a-million-rows/README.md) is resolved, so four rows above are
no longer aspirations. **The frame rows require an idle machine to measure:** the
same llvmpipe configuration reported 2.4 ms idle and 16.0 ms alongside a
`cargo build`, so the regression CI in M8 has to control for load or it will
report noise as a regression. Measured on a 24-thread 2024 laptop — **not** the
reference machine, so these are necessary and not sufficient — at 1,000,000
entries, 1920×1080, over an 1800-frame seeded scroll trace:

| Row | Target | Measured, worst case | Where |
| --- | --- | --- | --- |
| Scroll 1M entries | < 8 ms/frame | **2.618 ms** p99 worst of 18 runs (llvmpipe at 100%); 0.032 ms GPU worst on discrete hardware | 3 runs × 6 device/scale configurations, idle machine |
| Idle CPU | 0.0% | **0 unprompted redraws in 18 runs; 0 ms of CPU in 16 of 18, one 100 Hz tick in the other two, charged to no live thread** | windowed, RADV |
| Golden frames at 100/125/150/200% | perceptual diff | **0 of 29.6 million glyph quads drawn at anything but 1:1** with their raster | llvmpipe, headless |
| RSS at 1M entries | < 250 MB | **217.5 MB** RADV · 256.8 MB llvmpipe · **375.3 MB** NVIDIA proprietary | see below |

**A golden frame cannot certify crispness on its own.** Spike A passed the 1:1
glyph assertion and still rendered visibly fuzzy text at 1.25× scale, because the
assertion covers atlas-to-framebuffer and the compositor was resampling
framebuffer-to-screen. Golden frames are captured headless, where no compositor
exists to resample, so they can only prove the renderer does not scale glyphs.
The gate list therefore needs one more row, which does not exist yet because no
windowed code does:

| Gate | Method |
| --- | --- |
| Surface matches screen | Every frame is drawn at the compositor's reported scale, into a surface of the window's physical size. Any frame that is not is one the compositor resampled. Asserted per frame, not sampled |

That row is `sk-plat`'s to satisfy, and Spike D must cover scale negotiation
explicitly — see [ADR 0002](adr/0002-own-platform-layer.md) and Spike A's
[crispness finding](../spikes/a-million-rows/README.md#crisp-glyphs-at-fractional-scale).

The RSS row is the one that did not come out clean, and the reason is not ours:
the marginal cost of a million rows is 43–50 MB on every driver, while the
*floor* with a thousand rows on screen is 167 MB on RADV, 208 MB on llvmpipe and
**332 MB on the NVIDIA proprietary driver** — over the 300 MB spike bar before
any data exists.

Two consequences, stated rather than absorbed into a revised target:

- The 250 MB target here is reachable on RADV, marginal on llvmpipe, and
  unreachable on NVIDIA proprietary. It is not adjusted on the strength of one
  machine's measurement; it needs the reference-machine run and then a decision.
- **The 90 MB "ordinary session" target is below the driver floor on all three
  devices** and cannot be met on a Vulkan stack as measured. That row is not a
  tight budget, it is an impossible one, and the honest resolution is a
  superseding ADR that either states the floor separately from our own
  allocation or changes the stack — not a quieter measurement.

31.8 MB of the floor *is* ours and is fixable: the spike reads all six fallback
fonts into memory rather than mapping them, 19.5 MB of it `NotoSansCJK`.

## What Spike B measured

[Spike B](../spikes/b-enumeration/README.md) is resolved except for its
cold-cache arm. Warm cache, 1,000,000 files on ext4, same machine and same
caveats as Spike A:

| Row | Target | Measured | Note |
| --- | --- | --- | --- |
| Open a 10k-entry directory | < 30 ms | **0.132 ms** to the first 200 rows | warm cache only — cold is the arm that makes this row real |
| — | — | **0.530 s** for a full 1M-file walk with size and mtime | `std::fs::read_dir` plus `metadata()` takes 1.195 s and misses the roadmap's 1 s bar |
| Frame during a hung mount | < 8 ms | **4.001 ms** with the walk on the job pool | the same listing issued from the frame loop blocked it for **28 s** |

Two findings that change what the other documents say rather than confirming
them:

- **The `read_dir` tax is CPU time, not memory.** Peak RSS is flat across all
  four arms at 82–85 MB. Its per-entry allocations are churn the allocator hands
  straight back, so the phrase "per-entry allocation and implicit stat" in
  [04-architecture.md](04-architecture.md#data-layout) names two costs of
  different kinds, and only one of them is a cost.
- **Batching `statx` is the win; `getdents64` is the trim.** Of the 0.665 s
  between the two comparable arms, deferring `statx` to the job pool accounts for
  0.614 s and replacing `read_dir` with `getdents64` accounts for 0.051 s. The
  thing to get right in `sk-fs` is the batching.

A third finding, about the pool rather than the filesystem: **pool width has an
optimum well below `ncpu - 2`, and `ncpu - 2` overshoots it.** Measured on two
machines — 0.436 s at three workers against 0.539 s at twenty-two on a 24-thread
part, and 0.637 s at five against 0.745 s at ten on a 12-thread part. The penalty
for the default is 17–23%.

The optimum is machine-dependent and cannot be a constant: three workers on one
machine, five on the other, and two workers is comfortable on the faster machine
while missing the bar by 34% on the slower one. Width wants a short probe at
first use, not a formula over `ncpu`.

Measured on a 6-core Ryzen 5600X, the full walk is **0.726 s** at the default
width and **0.637 s** at its best, so the bar holds there. Extrapolating to the
four-core reference machine gives 0.9–1.3 s, which straddles the bar — this is
the Phase 0 number least likely to survive that measurement.

The hung-mount row is measured **with a control**: the same listing on the frame
loop instead of the pool. Without it the row can only pass, because a frame loop
with no path to a filesystem syscall cannot be blocked by one. Any future version
of this gate keeps the control or it is not a gate.

## On measuring frame time honestly

p99 over a realistic input trace, not an average over an idle window. The interesting
frames are the ones where a directory finishes loading, a thumbnail batch arrives and the
user is mid-scroll — so the traces deliberately overlap those. A budget measured on a quiet
frame loop is a budget that will be met and still feel slow.
