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
| Golden frames at 100/125/150/200% | `tools/golden`, on lavapipe, against `tests/golden`. Per-channel tolerance **2**, which is the measured spread between llvmpipe, RADV and NVIDIA proprietary on this machine |

## What M0 measured

The first commit with crates in it. Measured on a 24-thread 2024 laptop with an
AMD Radeon 890M on RADV, under Hyprland at 125%, on a warm page cache and an idle
machine — **not** the reference machine, so these are necessary and not
sufficient, the same caveat Spike A carries. Reproduced with `skerry --bench`,
which renders a scripted scroll offscreen through the same immediate-mode pass,
the same renderer and the same glyph atlas the window uses.

| Row | Target | Measured | Verdict |
| --- | --- | --- | --- |
| Cold start to first frame | < 80 ms | **92.5 ms best, 122.6 ms median** of 12, plus one 2,654 ms outlier | **missed**, see below |
| Keystroke to frame | < 8 ms | frame p99 **0.342 ms** median of 5 runs at 200,000 entries; **worst 5.0 ms**, while the read is landing | met |
| Open a 10k-entry directory | < 30 ms | **23.9 ms** to a sorted, painted listing | met |
| Scroll a large listing | < 8 ms/frame | p99 **0.342–0.417 ms**; **0 of 899 frames over 8 ms** at 100%, 125% and 200% at 200,000. At **973,759**: 1 run in 5 breaches, worst frame **16.4 ms** | met at 200,000; **missed at 1M**, see below |
| Sort a large listing | < 120 ms at 1M | **166.0 ms** median of 5 at 973,759, on a job — down from 1,460 ms | **missed** by 1.4×, see below |
| First search result | < 50 ms | **0.7 ms** median of 7 over 817,288 entries, **2.9 ms** over 563,605 | met by 17×, and see below for what it does not say |
| Repeat search over an index | < 50 ms | **6.99 ms** median of 3 over 563,605 entries, against **639.9 ms** to walk the same tree for the same complete answer | met by 7×, and it is the *complete* answer rather than the first result |
| Load an index | < 250 ms at 1M | **31.7 ms** warm, median of 3 at 563,605 — 45 bytes an entry | met warm; **cold is unmeasured here**, and [Spike F](../spikes/f-index-and-search/README.md) measured 195 ms cold at 1M on a Pi 5 |
| Idle CPU | 0.0% | **0 scheduler ticks over 8 s** on a mapped, focused window, on **both** display servers. On X11 the frame loop was traced across a three-second idle window and made **zero syscalls** | met |
| RSS, ordinary session | < 90 MB | **117.6 MB** at 10,000 entries, of which 116.3 MB is the floor | **missed**, and the floor is the driver's |
| RSS at 1M entries | < 250 MB | **136.3 MB** at 200,000 — 20 MB of marginal cost, about 100 bytes an entry | on track, 1M unmeasured |
| Stripped binary | < 20 MB | **6.25 MB** | met |
| Transitive crates (non-GPU) | ≤ 27 | **27** | met, and see [07-dependencies.md](07-dependencies.md) for why the figure moved |

Four of those rows are worth more than a cell.

**The frame budget is not close, and the number above is the second one taken.**
The first was 0.445 ms and it was measured wrong. Two things were outside the
clock: the harness read the directory to completion *before* the trace started,
so the two most expensive things the loop ever does — moving a 10 MB `Listing`
into the pane and writing 2,048 metadata rows — happened where nothing was
timing; and the clock inside the window started after those same steps had run.
Both are fixed. The clock now starts when `pump` returns and stops at the
submission, and the trace starts the read and lets it land underneath the scroll.

**One click could break the budget on its own.** Placing the caret from a click
in a text field is shaped rather than counted — a cluster is not a character —
and the first version shaped every prefix, which is O(n) shapings of O(n) work
each, on the UI thread, over a string bounded by `PATH_MAX`. Release build, 500
clicks each: **133 µs at 26 characters, 460 µs at 49, 3.10 ms at 128 and 11.96 ms
at 252**. Binary-searching the prefixes answers the same click in 47, 87, 257 and
564 µs, and gives the same offset — asserted against the scan it replaced, over
every character boundary of five strings.

The scroll itself did not move: 0.19 ms p50, 0.34 ms p99, and it still does not
depend on the entry count, because the loop runs once per *visible* row — about
forty times — whatever the directory holds. What appeared is a second regime.
**The worst frame while the read is in flight is 2.5–5.0 ms**, against 0.9 ms
before, and it is a real cost the old harness was constructed not to see:

- The pool is 24 wide and saturating every core with `statx` while the loop tries
  to draw, which is most of it. A frame that shaped a whole fresh viewport — 27
  runs, the frame where a directory first appears — paid 0.7 ms more than one that
  shaped 8, so shaping is the smaller half.
- It is still inside 8 ms, and it is the single most expensive frame the product
  has. **Lowering the priority of the pool's threads does not help, and that was
  measured rather than assumed.** At nice +5, eight runs each: 2.6–6.2 ms before
  and 3.8–9.5 ms after, and the only frame ever recorded over the 8 ms budget on
  this machine was one of the niced ones. The loop is not only competing with the
  pool, it is *waiting* for it, so slowing the workers lengthens the window in
  which there is anything to compete over. The negative result is a comment on
  `sk_util::pool::worker` so it is not tried a third time.

`skerry --bench` reports it as its own row so it cannot be averaged away.

**And it moves with the machine's load, which is why it is written down twice.**
The 2.5–5.0 ms above was taken on an idle machine. Ten runs later, five of the
same binary and five of the commit before the operations landed, back to back at
a one-minute load average of **2.3**: **3.6–4.9 ms before and 3.5–6.1 ms after**,
with `p99` identical at 0.64–0.89 ms and no frame over 8 ms in any of the ten. An
earlier set of three at the same load produced one **9.6 ms** frame, which is the
first breach on this machine at nice 0 — one frame of 599, in the window where a
200,000-entry read lands, and it did not recur in the ten runs after it.

Two things follow, and neither is a widened budget. The row stays at 8 ms
because [12-measurement.md](12-measurement.md#the-bar-is-quoted-before-the-code-and-never-adjusted-after)
says a bar is not adjusted after the fact. And the one frame in the product that
is anywhere near the budget is the one that shares a machine with whatever else
is running, so **it is the frame to fix first** when M3's queue makes the read
concurrent with an operation.

**What the sidebar and the toolbar cost: below what this machine could resolve
at the time.** Seven runs with a load average of **4.4** — a browser, an editor
and this repository's own tooling — gave a best p50 of 0.210 ms against 0.191–0.205
ms before either. Eight rows of text, four drawn marks and a registry lookup per
frame is not nothing, but it is under the run-to-run spread on a machine in use,
and [12-measurement.md](12-measurement.md) says a number taken on a loaded
machine is worth 6× less than the same number taken on an idle one. Recorded with
the load rather than quoted as a change.

**Moving the path bar into each pane cost nothing measurable.** The four
navigation buttons and the breadcrumbs are per pane now, under that pane's tab
strip, so a window with one pane draws the same widgets in a different place and a
split draws them twice. Thirteen alternating runs of `--bench 900` at 200,000
entries, load average 2.6–3.1: p50 median **0.197 ms before, 0.196 ms after**;
p99 median **0.418 ms before, 0.361 ms after**. One run in thirteen breached 8 ms
on each binary — the frame the read lands on, which is the row above and not this
change. The spread is wider than the difference, so what this records is the
absence of a regression rather than a gain.

**What the row mark costs: 0.012 ms a frame.** Colouring a row by the kind of
file it names asks `MimeDatabase::by_name` once per visible row per frame — a
hash lookup and, for the four globs on this machine that are not `*.ext`, a short
scan. Measured at 200,000 entries: p50 **0.191 ms before, 0.205 ms after**, p99
unchanged.

The first version cost **0.05 ms** and put one frame over the 8 ms budget, from
two allocations nobody had looked for: the glob matcher collected the pattern and
the name into `Vec<char>` per call, and `by_name` lowercased the name whether or
not it had a capital in it. Both are gone — bytes where both sides are ASCII,
which is every glob and almost every name, and a borrow where the name is already
lowercase.

**Cold start on the NVIDIA proprietary driver is twice as slow, and it is all
the driver.** Measured under a nested GNOME on the same machine, six sequential
runs after a warm-up: **211, 220, 220, 241, 220 ms**, of which `wgpu` device
setup is **193–220 ms** and everything of ours is about 20 ms — a window in 1–3
ms, fonts in 8–20 ms, the first frame in 2.4–5.8 ms. Against 92.5 ms best on
RADV with the same code. The first run against a freshly started compositor was
999 ms with 783 ms of it inside `Window::open`, which is the compositor settling
rather than a driver and is recorded as an outlier with a cause.

That closes Spike A's NVIDIA arm, which was owed, and it says something the AMD
number could not: **the cold-start row is a driver number wherever it is taken.**
A persisted pipeline cache and a first frame from a software buffer are the two
remaining levers, and neither is ours to take inside `wgpu`.

**Cold start misses, and 67–113 ms of it is the Vulkan loader.** Attributed:
`wgpu::Instance::new` is 67–113 ms, `request_device` 19–25 ms, surface
configuration 2–3 ms, and shader plus pipeline creation **1.3–2.2 ms**. Our own
code is 8 ms of the total — the window is 2–4 ms and font discovery is 2.5 ms.
Asking for `VULKAN | GL` together instead of Vulkan-then-GL cost an extra 60 ms,
because it initialized an EGL display for an adapter that was never used; fixing
that took the total from 195 ms to 122 ms. What is left is the loader enumerating
ICDs and layers, which is not ours to make faster. The remaining options are a
persisted pipeline cache and presenting a first frame from a software buffer while
the device opens, and neither is M0's. The 2,654 ms outlier appeared once in
twelve runs and once more after a rebuild; it is recorded rather than explained.

**The 1M sort budget was missed by an order of magnitude and is now missed by
1.4×.** The comparison sort measured **1,460 ms** at 973,759 entries — the 150 ms
figure was at 200,000 and extrapolated badly. It is now **166.0 ms**, an **8.8×**
improvement, measured as five alternating pairs of this binary and the commit
before it at a one-minute load average of **2.4**, on the flat million-entry
fixture Spike F built. Median of five each: before 1,424–1,522, after
165.0–169.7. The row stays at 120 ms and stays marked missed.

What changed is *what is sorted*, which is what
[Spike F](../spikes/f-index-and-search/README.md) said it would have to be. The
spike proposed an eight-byte lexical prefix key with a radix pass; **that key
cannot work for this comparison**, and the counterexample is still a test in
`sk_model::collate` — natural order is not lexical order, so a lexical key decides
`img10.png` against `img2.png` before the tiebreak runs, and decides it wrongly.
So the key became a **collation key whose byte order is `natural_cmp` exactly**:
folded letters, digit runs as a length prefix and their significant digits, a
leading-zero signature, and the raw bytes to keep the order total. A byte-wise MSD
radix over that produces a natural order without ever comparing two names, and
`sk_model::listing`'s tests run both and require the same answer over every sort a
person can ask for.

Three things were tried, measured and kept or dropped:

- **A two-byte pass** for runs over 65,536. Worth 129 → 127 ms, which is nothing:
  the cost is the *number of levels*, and these names share long word prefixes —
  25,926 passes visited items 16.5 million times, 17 bytes of depth per name.
- **Comparing the packed key head as one `u64`** before touching the arena. Also
  worth nothing on its own, and kept because it is free and the arena is 63 MB.
- **The leaf cutoff**, which is what actually moved: 24 gives a 124 ms radix, 256
  gives 117, 1024 gives 101. Above 1024 the difference is inside the run-to-run
  spread on a machine in use.

Key construction was 105 of the first 237 ms and is now 53. Two causes, both
measured: pushing one lowercased byte at a time where `make_ascii_lowercase` over
a slice is vectorised, and `StrArena::get` revalidating 27 MB of UTF-8 that
`push` had already validated. The key builder now takes bytes.

**Getting inside 120 ms from here is parallelism**, and it is not available inside
`sk-model`: the job pool is `sk-util`, which is the same level, so the edge would
be sideways. Spike F measured threaded buckets at 367 → 216 ms on a Pi and 69 → 23
ms on a laptop, so the headroom is real and the work is an API that hands buckets
to a caller above. Two things reduce the harm meanwhile: the first sort runs
inside the directory-reading job, and a re-sort above 8,192 entries runs on a job
over a clone, so the UI thread never blocks on it.

**At a million entries the frame budget is breached, and it is the scale rather
than anything this session changed.** Measured as five alternating pairs of the
current binary and the commit before the radix sort, back to back at a one-minute
load average of **5**, over Spike F's flat million-entry fixture, 900 frames each:

| | worst frame | frames over 8 ms |
| --- | --- | --- |
| before | 2.4, 6.0, **9.2**, 4.1, 3.6 ms | 0, 0, **1**, 0, 0 of 899 |
| after | **16.4**, 2.1, 6.3, 3.5, 4.8 ms | **1**, 0, 0, 0, 0 of 899 |

One run in five breaches on each side. The worst single frame is larger on the
after side in this sample, and with five runs at a load of five that is not a
signal — [12-measurement.md](12-measurement.md#the-machine-must-be-idle-and-the-load-must-be-recorded)
prices a number taken on a loaded machine at 6× less. What the sample does
establish is that **the breach is not new**: the row said "1M unmeasured" and the
reason it said so is that nobody had run it.

Every breach is in the window where the read lands, which is the same regime the
200,000-entry worst frame sits in — the pool saturating every core with `statx`
and a sort while the loop tries to draw. At 1M that window is four times as long,
so there is four times as much of it to be unlucky in. Two things are owed before
this row can be called: **an idle re-measure**, and the fix
[08-roadmap.md](08-roadmap.md) already names — the frame that shares a machine
with whatever else is running is the frame to fix, and it is the same one.

**The RSS target is below the driver's floor.** 116.3 MB with 154 entries on
screen is what the process costs before any of our data exists, and 10,000
entries add 1.3 MB to it. This is the same shape as Spike A's finding, where the
NVIDIA proprietary driver's floor was 332 MB against a 300 MB bar. The marginal
cost is the number under our control and it is 100 bytes an entry.

### What "first search result" measures, and what it does not

**Method.** `sk_index::find` over two real trees on the laptop above, load
average 1.4–1.8 rather than idle, warm page cache, seven runs each, median
reported: `$HOME/repos` at 817,288 entries for `report`, and `/usr` at 563,605
for `config`. The clock runs from the call to the first report that carries a
match. The load is recorded because
[12-measurement.md](12-measurement.md#the-machine-must-be-idle-and-the-load-must-be-recorded)
prices contamination at 6×, and 6× of 0.7 ms is still 12× inside the bar — which
is the only reason a number taken on a busy machine is worth writing down here.

**It is a measure of the first match, not of the answer.** The walk is
depth-first from the folder the search started in, so a query whose matches are
spread through the tree produces one almost at once. A query whose *only* match
is the last file read takes the whole walk: **944 ms** over those 817,288
entries, **629 ms** over the 563,605. There is no shortcut without the index
[ADR 0008](adr/0008-persistent-index-is-required.md) requires, and this row is
about the scope that does not have one — one subtree, walked now. The panel
counts entries looked at while it goes, which is what stops the honest 944 ms
case from looking like a hang.

**The interval was the number before the work was.** Reports were throttled to
one every 64 ms, so the first result could not have arrived inside 50 ms however
fast the walk ran — the measurement would have been of the throttle. The first
match now goes out at once and the interval takes over after it.

### What the index measured

**Method.** `SKERRY_INDEX_TREE=/usr cargo test -p sk-index --release measure_an_index
-- --ignored --nocapture`, the measurement that lives beside the code so the
numbers below can be reproduced rather than believed. `/usr` on this machine is
**563,605 entries**. Three runs, warm page cache, on a 24-thread Ryzen AI 9
HX 370 with 60 GB — the *favourable* machine, and [Spike
F](../spikes/f-index-and-search/README.md) measured the laptop-to-Pi factor at
**2.6× to 12×**, so every figure here should be read with that range in mind
rather than a single multiplier.

| | median of 3 | spread |
| --- | --- | --- |
| Build, walking the tree | 652.1 ms | 651.8–659.5 |
| **The counting pass beside it** | **275.2 ms**, 42% of the build | 246.1–278.2 |
| Save | 31.5 ms | one run |
| Load, warm | 31.7 ms | 30.7–32.5 |
| **Scan for a query** | **6.99 ms** | 6.36–7.15 |
| **The walk it replaces, complete** | **639.9 ms** | 639.8–673.6 |
| On disk | 25.3 MB | 44.9 bytes an entry |

**The comparison that matters is the last two rows, and it is 92×.** Not first
result against first result — the walk already streams its first match in under a
millisecond, and [the section above](#what-first-search-result-measures-and-what-it-does-not)
is about how little that says. This is the *complete* answer, which is what an
as-you-type keystroke needs before the next one arrives: 639.9 ms of walking, or
6.99 ms of scanning an index that was loaded once for 31.7 ms.

44.9 bytes an entry against Spike F's predicted 50 is the same format measured on
a different tree, and it is the figure for the privacy conversation [ADR
0008](adr/0008-persistent-index-is-required.md) asked for: a million files is
about 45 MB on the user's disk.

**The counting pass is what pays for the progress bar.** A bar needs a
denominator and a walk does not have one until it has finished, so the build runs
a second walk beside it — the same bargain the copy engine already makes, and the
same rule: nothing is estimated from a partial count, so there is no bar at all
until that walk has finished. It is affordable because it reads names and does
**not** `statx` each entry, which [Spike
B](../spikes/b-enumeration/README.md) and [Spike
F](../spikes/f-index-and-search/README.md) both measure as the expensive half.
Measured at **42% of the build**, so the bar appears about four tenths of the way
in and is real for the rest. Sequentially measured; run concurrently the two
contend for I/O, and on a cold tree the counting pass also warms the cache for
the build behind it — neither effect is measured here.

**Three things this does not say.** It is warm, so it is not the row's cold
target. It is one tree on one machine. And the build is 652 ms for half a million
entries, which is the cost a person pays deliberately and once — [ADR
0012](adr/0012-a-flat-index-file-per-root-rebuilt-whole.md) rebuilds whole and
has no incremental path, so a tree that changes constantly is a tree this does
not help.

### The two numbers that were wrong before they were right

Both are recorded because each was a *measurement* that read as a success.

**Idle CPU measured 100% of one core**, against a row that reads 0.0%. The frame
loop blocks in `poll` on the display socket and the job pool's eventfd — and an
eventfd that is signalled and never read stays readable, so `poll` returned
immediately, forever. The fix is one `read`; the interesting part is that
`sk_plat::Window::pump` now takes the whole `Waker` rather than its descriptor,
so the drain cannot be the caller's job to remember.

**The `statx` pass ran on one thread**, at 186.3 ms for 200,000 entries. Spike B
had already measured the answer — spreading it across the pool is 3.4× on a
four-core Pi — and the first version of `sk-fs` simply did not do it. Fanned out
in 2,048-name slices: **20.4 ms**, which is **9.1×** on 24 workers. A spike's
finding is not implemented because it was written down.

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

**M0 satisfies that gate from the second frame and not the first.** `sk-plat`
derives the surface size from the integer numerator the compositor sends and
reconfigures on every change, so a mapped window is always drawn at its physical
size. But Hyprland does not send `wp_fractional_scale_v1.preferred_scale` until
the surface is *mapped*, and a surface is mapped by its first buffer — so frame 1
is drawn at 120/120 and frame 2 onward at the real 150/120. One frame, and it is
the frame nobody sees. The fix is to map a 1×1 transparent `wl_shm` buffer before
the GPU surface exists and wait briefly for the scale; it is written down here
rather than done, because it is a protocol trick and this milestone had not
measured whether it is needed.

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
- **Cold, the bar holds only on the 24-thread machine.** With the page, dentry
  and inode caches dropped, the full walk is **0.80–0.85 s** there, against
  3.3–3.9 s for `getdents64+statx` and 5.6 s for `read_dir+meta`. On four cores
  it is **2.3 s (x86) and 3.7 s (aarch64)** at a sensible width, and 4.9 s and
  6.4 s at `ncpu - 2`. The roadmap's bar says warm cache, so this is not a miss —
  but a first recursive walk after a boot takes seconds on reference hardware,
  and that should be designed for rather than discovered.
- **The interactive condition holds cold on every machine.** Worst first-200-rows
  cell anywhere, cold, on four cores, is **0.744 ms** against a 30 ms bar. Opening
  a directory stays instant while the recursive walk behind it takes seconds,
  which is the split rule 1 exists to create.
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

## What Spike F measured

[Spike F](../spikes/f-index-and-search/README.md) measured flattened search, the
persistent index and sorting, on a 24-thread laptop and on a **Raspberry Pi 5**.
The Pi is the row that counts.

| Row | Target | Hard fail | Pi 5 | Verdict |
| --- | --- | --- | --- | --- |
| First search result | < 50 ms | 120 ms | **13.3 ms** parallel, 47.4 ms serial | met, and only with a parallel scan |
| Sort 1M entries | < 120 ms | 300 ms | **191 ms** best of eight approaches | missed on target, comfortably inside the hard fail |
| RSS at 1M entries | < 250 MB | 450 MB | 152 MB with the flattened store resident | met |

### The row that fails

**Sorting a million filenames is not met on reference hardware, at 191 ms** — but
comfortably inside the 300 ms hard fail, having started at 948 ms. Eight
approaches were measured on the Pi, each verified to produce a correctly ordered
result:

| Approach | Pi 5 |
| --- | --- |
| comparison, through the arena | 948 ms |
| radix over keys, then repair ties by comparison | 775 ms |
| recursive radix, never compares | 367 ms |
| recursive radix, buckets across threads | 216 ms |
| **MSD split, split and buckets both parallel** | **191 ms** |
| *(one packed radix pass, ties left unsorted)* | *131 ms* |

The changes that mattered, in order: **never fall back to comparison** (775 → 367,
because 94% of filenames share their first eight bytes and repairing ties by
string comparison costs more than the radix before it); **sort the buckets in
parallel** (367 → 216, disjoint slices of the order array over an immutable
arena); **pack the key beside the index** so passes read sequentially instead of
probing 8 MB at random (232 → 131 for one pass on the Pi, and almost nothing on a
cache-rich laptop, which is why it went unnoticed); and splitting on the most
significant byte with that split parallel too (216 → 191).

**The remaining 1.6× is memory, not parallelism — on the Pi.** Four cores buy
1.9×, bucket skew is not the constraint (29 non-empty first-byte buckets, largest
13.4%), and the sort achieves 33% of the machine's measured random-read rate,
which is close enough for memory to be a real limit. On the laptop the same sort
uses **12%** of what is available, so there the limit is something else and an
earlier revision of this section was wrong to say otherwise.

Closing the gap means **moving less memory rather than adding threads**, and
sorting by a metadata column is 8 ms, so this remains specifically a *filename*
sorting problem.

### The row that needs a mechanism, not a number

`First search result` is met at 13.3 ms — but serial on the same machine it is
47.4 ms, inside a 50 ms bar by 5%, and as-you-type is 72.3 ms per keystroke,
outside it. **The parallel scan is the feature, not an optimisation**, and the
budget row is only meaningful alongside it. The speedup is 3.4–3.7× on four cores
and 8× on twenty-four — far below core count either way, because a scan over a
28 MB arena is memory-bandwidth bound.

### A row to add once `sk-index` exists — **added**

| Measure | Target | Hard fail | Method |
| --- | --- | --- | --- |
| Load the index for 1M entries | < 250 ms | 500 ms | Cold, page cache evicted for the index file; measured at 195 ms on a Pi 5, 48 ms on a laptop |

An earlier revision of this section proposed 100 ms, taken from the laptop's
48 ms. The Pi measures 195 ms. The number above is set from the slow machine,
which is the only way a budget row means anything.

The row is now in the table at the top, and the index it describes is built:
[`sk_index::store`](../crates/sk-index/src/store.rs), under [ADR
0012](adr/0012-a-flat-index-file-per-root-rebuilt-whole.md). The product's own
numbers are in [what the index measured](#what-the-index-measured); the cold
figure is still the spike's, because nothing in the product evicts a page cache
to take one.

### On extrapolating between machines

This section previously projected reference hardware from the laptop using a
single "3× slower per core" factor. Measured, the factor runs from **2.6× to 12×
on the same pair of machines**, and [Spike F](../spikes/f-index-and-search/README.md#measuring-what-the-machines-offer-and-retracting-bandwidth-bound)
then measured *why*, over the same 28.5 MB arena:

| | laptop | Pi 5 | ratio |
| --- | --- | --- | --- |
| sequential read | 16.88 GB/s | 5.43 GB/s | **3.1×** |
| independent random 8-byte reads, all threads | 793.9 M/s | 26.1 M/s | **30×** |
| dependent random reads, one thread | 220.6 M/s | 82.2 M/s | 2.7× |

**An operation's portability factor is the factor of whichever access pattern
dominates it.** A scan ports at 2.6× against a 3.1× primitive; a radix scatter
ports at 12× against a 30× primitive. Those two primitives differ by an order of
magnitude on the same pair of machines, so no single multiplier could have worked.

The cause is a cache boundary, isolated on a third machine. `homelab` has the
**same four cores as the Pi** and a **64 MiB L3 like the laptop**:

| Over the same 28.5 MB arena | laptop (24 cores) | homelab (4 cores) | Pi 5 (4 cores) |
| --- | --- | --- | --- |
| random reads, all threads | 793.9 M/s | **576.4 M/s** | **26.1 M/s** |
| sort by name, best | 23.6 ms | **65.2 ms** | 296.0 ms |

**Two four-core machines, 22× apart on random reads and 4.5× apart on the sort.**
The variable is neither core count nor instruction set: it is whether 28 MB of
names fits in the last-level cache. homelab **meets the 120 ms sort budget at
65 ms** with a quarter of the laptop's cores; the Pi with the same four cores does
not. On the Pi, four threads doing independent random reads achieve *less* than
one thread doing dependent ones — concurrent random access over a working set
that does not fit collapses rather than scales.

So `Sort 1M entries` is better read as **"the working set is 28 MB and a machine
whose cache cannot hold it pays 22× per random access"** than as a sorting
problem. **Keep the hot working set inside a modest cache, or make the access
sequential.** A million entries at 28 bytes of name apiece sits exactly on that
boundary, which is why the single packed radix pass — 8 MB of keys, no arena
access — is the fastest thing measured that does real work.

The design rule that follows, and it is worth more than any single number here:
**an operation that streams ports well, and an operation that scatters does not.**
Prefer sequential passes over a compact representation to clever indirection,
because the indirection is what the reference machine cannot afford.

Every projection in this document should be treated as a hypothesis until a slow
machine has run it. This project has now guessed wrong about that twice, and once
about the reason.

### One shape that is not in any row yet

A single directory holding a million entries takes **8.4× a realistic tree of the
same file count** — 1.013 s against 0.120 s on the laptop — because a directory is
the unit of parallel work and there is only one of them. `node_modules`, Maildir,
`/nix/store` and dataset dumps all produce that shape, and it is a directory a
user opens.

Its name stream **cannot** be split: `getdents64` cookies are discoverable only by
reading forward. Its `statx` calls can be, and deferring them to the pool is
**3.4× faster on a flat directory and 3× slower on a realistic tree** — and on a
Pi it takes the p99 from **11.9 s to 1.2 s**, ten times better in the tail against
two in the median. `sk-fs` therefore needs both strategies and a per-directory
choice between them, which is a shape decision rather than a tuning constant.

**And on 8 GB, a warm cache does not stay warm.** Five repeats of the same walk
on the Pi gave a p50 of 1,136 ms and a p99 of 6,332 ms, the signature of runs
going back to the device. Every "warm cache" number elsewhere in this document was
taken on a machine with 60 GB of RAM.

## What Spike C measured

[Spike C](../spikes/c-watching/README.md) measured inotify across the 21,338
directories of a million-file tree, on three machines.

| Measure | laptop | homelab | **Pi 5** |
| --- | --- | --- | --- |
| One file created, to event delivered | 0.039 ms | 0.043 ms | **0.063 ms** |
| `rm -rf` 2,000 files, to last event | 7.13 ms | 25.4 ms | **40.1 ms** |
| 500 renames, 1,000 paired events | 3.84 ms | 11.1 ms | **16.9 ms** |
| Establishing 21,338 watches | 0.112 s | 0.103 s | 0.171 s |

Notification costs 63 microseconds on the slowest machine. Three findings sit
behind that, and each one changes a row.

### The `rm -rf` row measures the deletion, not the notification

40 ms on a Pi for 2,000 files is **50 files per millisecond**, and the clock is
dominated by `unlink` rather than by inotify. So the row scales with the size of
the deletion: a 30,000-file `node_modules` is **~600 ms, six times the bar.**

**A row that passes or fails depending on how large the user's `node_modules` is
not measuring what it names.** It should be two rows: notification latency, which
is microseconds and deserves a tight bar, and time for a bulk external change to
be fully reflected, which is bounded by the kernel's unlink rate.

### The watch budget is per-user, shared, and not a constant

| | `max_user_watches` | obtained while holding 21,338 | sum vs limit |
| --- | --- | --- | --- |
| laptop | 65,536 | 41,539 | 96% |
| homelab | 60,793 | 39,446 | **99.99%** |
| Pi 5 | 62,164 | 40,815 | **99.98%** |

The limit is **exactly per-user**, to within 0.02%. One million-file tree costs
34–35% of it and three exhaust it — and a file manager only ever gets what is left
after the IDE, the sync client and the language server.

**It is also not 65,536.** It was 65,536, 62,164 and 60,793 on three machines,
because the kernel derives it from available memory. Any code containing the
constant is wrong on two of these three.

### Overflow recovery is 12× the bar on reference hardware

A burst of 52,000 creations across 4,000 directories delivered **exactly 16,384
events from 316 distinct watches on all three machines** — the queue depth is
hardware-independent — and dropped 35,616 in silence.

| Recovery | laptop | homelab | **Pi 5** |
| --- | --- | --- | --- |
| rescan the 4,000 directories touched | 132 ms | 148 ms | 701 ms |
| rescan all 21,338 | 331 ms | 569 ms | **1,234 ms** |
| **prune on directory mtime, then read what moved** | **96 ms** | **184 ms** | **411 ms** |

**Pruning is the recovery to build.** A directory's mtime moves whenever an entry
is created, deleted or renamed in it, so `statx` on every directory says which can
be skipped — and it found all 4,000 changed directories with **zero false
negatives on all three machines.** The stat pass alone is 62 ms on a Pi, so for a
*localised* burst — a `git checkout` in one subtree, which is the common case —
recovery approaches the bar rather than exceeding it by an order of magnitude.

It is sound for *structure* only: mtime does not move when a file already in the
directory is modified, so a lost `MODIFY` or `ATTRIB` event stays lost. Recovering
the columns needs a re-`statx` of entries in the directories actually on screen,
which is a tiny set. Neither half is a correct recovery alone; together they are.

**The surviving-event set is misleading, not merely insufficient.** 16,384 events
arrived from 316 watches, so "rescan what we heard from" covers 316 of the 4,000
directories that changed and reports itself finished. `IN_Q_OVERFLOW` is one bit
and the dropped events are gone.

### The rows these three findings imply

| Measure | Target | Hard fail | Method |
| --- | --- | --- | --- |
| Notification latency | < 1 ms | 10 ms | inotify event delivered after a single create; measured at 0.063 ms on a Pi 5 |
| Bulk external change fully reflected | < 100 ms per 5,000 entries | 250 ms | Wall clock from the start of an external `rm -rf`; bounded by the kernel's unlink rate, ~50 files/ms on a Pi 5 |
| Overflow recovery *begins* | < 100 ms | 250 ms | First rescanned directory repainted, visible pane first |
| Overflow recovery completes | < 2 s per 1M entries | 5 s | mtime-pruned rescan of watched directories, progressive and cancellable; measured at 411 ms on a Pi 5, against 1,234 ms unpruned |
| Watch budget consumed | ≤ 40% of `max_user_watches` for one tree | 80% | Read the limit at runtime; 21,338 directories per million files is 34% of 62,164 |

## What Spike D measured

[Spike D](../spikes/d-platform-and-dnd/README.md) is a capability spike, so most
of what it produced is a matrix rather than a table of milliseconds. Four numbers
belong here because they became gates.

| Measure | Target | Hard fail | Measured | How |
| --- | --- | --- | --- | --- |
| Dispatch interval with a clipboard transfer outstanding from a hung peer | < 8 ms | 8 ms | **0.394 ms** worst, p99 0.275 ms, n=4,784 | A second process owns the selection, is asked for the bytes, and never writes. The read stays open for 1,200 ms while the loop is timed |
| Fractional-scale negotiation | exact | any resample | **150/120**, a 480×320 logical window as a 600×400 buffer | `wp_fractional_scale_v1` plus `wp_viewporter`, asserted against the integer the compositor sent |
| One buffer pixel to one output pixel | 64 px | < 62 px | **64 px** | The longest run of one-physical-pixel alternation between two greys, found by scanning the compositor's own 2880×1800 capture |
| CJK preedit to field, and commit | < 16 ms | 50 ms | **2.19 ms** and **2.18 ms** | `zwp_text_input_v3`, driven by the spike acting as its own input method over `zwp_input_method_manager_v2` |

### The crispness row is now a framebuffer test, not an assertion

Spike A's crispness check compared each glyph's quad to the size it was
rasterized at and reported 0 of 29.6 M quads scaled — while the compositor was
stretching the entire surface. The number was true and the screen was blurry.

Spike D's replacement asks the framebuffer instead, and it locates itself: it
scans the whole output for the longest horizontal run in which consecutive pixels
alternate between two known greys. A resample turns that run into a gradient, and
nothing else on a desktop produces one. The arithmetic that would convert a
logical window position into a physical one is deliberately left out, because that
arithmetic can be wrong in the same direction as the bug.

**This is the row that should be in CI**, not the per-glyph assertion — or rather,
both, because the per-glyph assertion catches a different failure and neither
subsumes the other.

### Three rows that cannot exist, because the protocol reports nothing

Spike D's other findings are not numbers, and pretending otherwise would produce
exactly the decoration [12-measurement.md](12-measurement.md) warns about. Each of
these is a *capability* that is absent with no error attached:

- **A negotiated drag action.** `wl_data_source.action` and
  `wl_data_offer.action` were delivered zero times across every pair tested,
  including pairs where a file demonstrably landed on disk. There is no number to
  bound here; there is a design that must be correct when the answer never comes.
- **`dnd_finished`.** Delivered by Nautilus, delivered by Konsole without
  `dnd_drop_performed`, and delivered by neither in a two-process pair of ours
  where the peer read the data. A move that deletes its originals on this event
  would silently never delete.
- **The primary selection.** Hyprland discards
  `zwp_primary_selection_device_v1.set_selection` while
  `misc:middle_click_paste` is off, with no protocol error and no `cancelled`.

The gate for all three is the same and it is a test, not a threshold: `sk-plat`'s
own suite must pass with each of these absent, because on at least one shipping
compositor each of them is.

## What Spike E measured

[Spike E](../spikes/e-operations/README.md) is a correctness spike, so almost all
of its output is a pass or a limitation rather than a number. Three numbers belong
here, and one row in [the correctness gates](#correctness-gates-that-are-not-negotiable)
now has a test behind it.

| Measure | Target | Measured | How |
| --- | --- | --- | --- |
| A 1 GiB sparse file with 12 KiB of data, copied | no growth | **12 KiB → 12 KiB** | the extent map from `SEEK_HOLE`/`SEEK_DATA` and `st_blocks`, not a byte comparison — a byte comparison passes while the file grows 87,000× |
| `ENOSPC` mid-copy | no partial file | **fails at byte 307,200 of 1,048,576; the destination name does not exist** | a FUSE filesystem that refuses the 307,201st byte, every time |
| Move within a filesystem | a rename | **0.011 ms** | `renameat2`, with `EXDEV` as the only reliable test of whether two paths share a filesystem |
| Reflink instant on btrfs | measurable | **0 KiB of free space for a 192 MiB clone, in 2.0 ms against 123.4 ms** | free space before and after, with both copies flushed. `copy_file_range` is excluded from the control because on btrfs and xfs it *is* a clone |

### The row that now has a test

"Zero data loss across the filesystem matrix" was a gate with nothing behind it.
It now has 46 checks, and the number that matters is not 37 passes — it is that
**four defects were found by a check failing**, listed in
[the spike](../spikes/e-operations/README.md#four-things-this-changed-three-of-them-in-our-own-code).
Two of them pass a byte-for-byte comparison. One of them copies nothing at all to
an external drive.

### Why the reflink measurement is space and not time

Time at this size measures page cache. Free space measures extent sharing, which
is what a reflink *is* — and it is the number a user cares about, because the
question a file manager answers with it is "will this fit".

The check requires the **control** to consume at least half the file before it will
call anything a pass. Without that it passed on btrfs with `0 KiB` against `0 KiB`,
because btrfs delays allocation and the comparison was `0 * 8 < 1`.

### Why the ENOSPC number is exact

A real full disk is a bad instrument: filling one is slow, and the byte at which
it gives up depends on the allocator, so a regression would not be visible.
Spike E ships a filesystem that fails at an offset it is told, which turns the
gate from "we handle ENOSPC" into a number that a CI job can compare against.

The same instrument produces a read error mid-copy, a destination that never
accepts a whole buffer, and a destination that refuses extended attributes — three
more paths that have no other way to be reached on demand.

## On measuring frame time honestly

p99 over a realistic input trace, not an average over an idle window. The interesting
frames are the ones where a directory finishes loading, a thumbnail batch arrives and the
user is mid-scroll — so the traces deliberately overlap those. A budget measured on a quiet
frame loop is a budget that will be met and still feel slow.
