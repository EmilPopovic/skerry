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
cold-cache arm. Warm cache, 1,000,000 files on ext4, measured on four machines
including a **Raspberry Pi 5** — four Cortex-A76 cores, aarch64, NVMe — which is
the closest thing available to the machine this document describes, and the
repository's first aarch64 measurement.
[06-portability.md](06-portability.md) requires aarch64 to be first-class.

| Row | Target | Measured on the Pi 5 | On a 24-thread laptop |
| --- | --- | --- | --- |
| Open a 10k-entry directory | < 30 ms | **0.365 ms** to the first 200 rows | 0.072 ms |
| Full 1M-file walk with size and mtime | < 1 s (roadmap) | **0.91–1.07 s** median depending on pool width, every width with runs over a second; 1.626 s at `ncpu - 2` | 0.130 s |
| Frame during a hung mount | < 8 ms | not run — no `/dev/fuse` in the container | **4.001 ms**, against 28,000 ms for the same call on the frame loop |

**The full-walk row is marginal on reference-class hardware: the Pi sits on the
bar rather than inside it.** For scale on the same machine, `read_dir` plus
`metadata()` takes 3.854 s and `getdents64` with no stat at all takes 0.974 s —
so this bar is not comfortably reachable by any design, is unreachable by the
obvious one, and is reachable by ours only on a good run.

**This row is not settled.** It wants either a bar revised against measured
hardware or another factor of two in the enumeration path, and that is a decision
rather than a measurement. Spike B reported it as 0.7–0.9 s, then 0.9–1.3 s, then
0.966 s with "3.4% of margin" — the last from a single median quoted as a result,
before the width curve was taken.

Four findings that change what the other documents say rather than confirming
them:

- **The `read_dir` tax is CPU time, not memory.** Peak RSS is flat across every
  arm at 82–85 MB. Its per-entry allocations are churn the allocator hands
  straight back, so the phrase "per-entry allocation and implicit stat" in
  [04-architecture.md](04-architecture.md#data-layout) names two costs of
  different kinds, and only one of them is a cost.
- **A parallel tree walk is worth more than either syscall choice.** Making
  directories jobs, taking `statx` through the directory's own file descriptor,
  and giving each job its own store chunk instead of a shared locked arena is
  3.4× on the fast machine and 1.6× on the Pi. Deferring `statx` is the next
  largest effect; replacing `read_dir` with `getdents64` is the smallest.
- **It holds the bar cold, and it is the only arm that does.** With the page,
  dentry and inode caches dropped, the full walk is **0.80–0.85 s** against
  3.3–3.9 s for `getdents64+statx` and 5.6 s for `read_dir+meta`. The advantage
  *widens* cold, from 3.4× to 4.3×, because cold work is latency-bound and
  parallelism hides latency. Spike B predicted the arms would converge cold, and
  predicted it twice; they diverged both times.
- **The parallel walk costs 137 MB against 84 MB**, and briefly cost 522 MB — a
  chunk over-reservation that cleared Spike A's 300 MB bar and was caught only
  because a cold run happened to print peak RSS. The `RSS at 1M entries` row
  above is enumeration *plus* a GPU stack; neither spike has measured them in
  one process.
- **`ncpu - 2` is the wrong pool width, and worst where it matters most.** On a
  four-core machine it is two workers and leaves half the machine idle: 1.592 s
  against 0.966 s on the Pi, 1.119 s against 0.716 s on a four-core VM. Every
  machine measured has its optimum at or above `ncpu` — 22 of 24, 12 of 12, 8 of
  4, 6 of 4. Beyond `ncpu` the curve is flat and the exact value is inside the
  noise, so the supportable rule is "at least `ncpu`" rather than any formula.
- **An earlier revision of this section claimed the opposite** — that width
  saturates at three workers because metadata work is "kernel-bound on inode
  locks". That was the spike measuring its own single-threaded traversal and its
  own store mutex. Retracted, and kept visible, because the claim reached two
  design documents before anything caught it.

The hung-mount row is measured **with a control**: the same listing on the frame
loop instead of the pool. Without it the row can only pass, because a frame loop
with no path to a filesystem syscall cannot be blocked by one. Any future version
of this gate keeps the control or it is not a gate.

## On measuring frame time honestly

p99 over a realistic input trace, not an average over an idle window. The interesting
frames are the ones where a directory finishes loading, a thumbnail batch arrives and the
user is mid-scroll — so the traces deliberately overlap those. A budget measured on a quiet
frame loop is a budget that will be met and still feel slow.
