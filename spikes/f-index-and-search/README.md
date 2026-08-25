# Spike F — the index, flattened search, and the shapes that break a walk

**Status:** method fixed, not yet run.

## Question

Can a flattened, fuzzy-filtered view of a million files put its first result on
screen within 50 ms of a keystroke — and what does a persistent index cost to
build, store, load and query, compared to walking?

## Bar

From the table in [09-performance-budgets.md](../../docs/09-performance-budgets.md),
quoted exactly and not adjusted afterwards:

> | First search result | < 50 ms | 120 ms | From the first keystroke on a warm 1M-file tree |

Two supporting conditions from the same table, because a search that meets its
own row while breaking two others has not met anything:

> | Sort 1M entries | < 120 ms | 300 ms | Off-thread; the UI stays interactive throughout |
>
> | RSS at 1M entries | < 250 MB | 450 MB | Peak, one pane, thumbnails off |

## What it decides

[ADR 0008](../../docs/adr/0008-persistent-index-is-required.md) has already
decided that the index exists and is not optional. It explicitly did **not**
decide the on-disk format, the update mechanism or the freshness policy, on the
grounds that a persistence format is the most expensive thing in this area to get
wrong and none of the numbers existed. This spike produces those numbers, and a
later ADR spends them.

It also answers a question [Spike B](../b-enumeration/README.md) left open by
construction. Spike B's tree is realistic — mostly small directories, a few large
ones — and realistic trees are not where enumeration breaks.

## Method

### Machines

The same four as Spike B, and the same rule: the machine must be idle, because
this work is load-sensitive and a sweep taken next to a compile measures the
compile. The Raspberry Pi 5 is the one that matters, being the closest available
to the hardware the budgets describe.

### The workloads

| Shape | Why |
| --- | --- |
| Spike B's tree — 1M files, 21,338 directories | the realistic baseline, already characterised |
| **One directory, 1,000,000 entries** | the shape that turns a parallel walk back into a serial one, because a directory is the unit of work |
| One directory, 100,000 entries | where the previous shape starts to bite, for a curve rather than a point |

The flat case is not hypothetical. `node_modules` caches, Maildir stores, object
directories, `/nix/store`, dataset dumps and anything written by a naive
downloader all produce single directories with six or seven figures of entries,
and every one of them is a directory a user opens.

### Flattened search

Three latencies, measured separately because they fail differently:

| Measure | What it is |
| --- | --- |
| **Time to first result** | keystroke to the first matching row being available to paint. This is the bar |
| Time to a complete result set | keystroke to the last match, which bounds "sort by relevance" |
| Time per subsequent keystroke | typing `report` is six searches, and the sixth is over a smaller set than the first |

Queries are chosen to span the range the matcher actually sees, because a fuzzy
matcher's cost is dominated by how early it can reject:

- a query matching almost nothing (`zzqx`) — the rejection path
- a query matching a handful (`snapshot-render-70`) — the realistic case
- a query matching a large fraction (`e`) — the path where the result set, not
  the matching, is the cost
- a query matching everything (empty) — the floor

Matching is subsequence-with-scoring, in the spirit of `fzf`, over the name
arena rather than over allocated strings. No matcher crate: the point is to
measure the design in [04-architecture.md](../../docs/04-architecture.md#data-layout),
where filtering is a scan over a contiguous arena.

### The index

Built from a completed walk, then measured on four axes:

| Number | Why it matters |
| --- | --- |
| Build time | it is paid at least once, in the background, while the user does something else |
| On-disk size | it is a list of every filename a user has, and it lives on their disk |
| Load time, cold and warm | this is the number that replaces the walk, and the one the whole ADR turns on |
| Query time against a loaded index | whether an index changes the search bar or only the startup cost |

The comparison that decides the format ADR is **cold index load against cold
walk**: 2.3–3.7 s on four-core hardware from Spike B. An index that takes as long
to load as the walk it replaces has bought nothing.

Format is deliberately the dullest thing that can work — the name arena written
whole, plus fixed-width parallel arrays, plus a header with a version and a
checksum. No serialization crate, per rule 10 and the pressure ADR 0008 names on
the 25-crate budget. If the dull format is fast enough, the format ADR is easy.

### What is not measured here

Freshness. Keeping an index current is inotify's problem and belongs to
[Spike C](../../docs/08-roadmap.md#spike-c--watching-at-scale), whose bar already
covers overflow and rescan. This spike measures a cold-built index against a
cold walk; the cost of *maintaining* one is Spike C's to find.

## Result

Five repeats per measurement, machine idle at load 1.26. This is the 24-thread
laptop, which is the *most* favourable hardware measured in this project — Spike
B found a Raspberry Pi 5 runs the same walk 7–8× slower, so every number below
should be read with that multiplier in mind. Raw output in
[runs/sweep.txt](runs/sweep.txt).

### The flat directory is the shape that breaks a walk

| Shape | files | directories | walk |
| --- | --- | --- | --- |
| realistic | 1,000,000 | 21,338 | **0.120 s** |
| **one directory** | 1,000,000 | **1** | **1.013 s** |

**Same file count, 8.4× the time**, and the whole difference is that a directory
is the unit of parallel work. One directory is one job, so twenty-four workers
do what one worker does. Spike B's design is not slow here; it simply has nothing
to divide.

This matters more than the ratio suggests, because the flat case is the one a
user *opens*. A million-entry `node_modules`, a Maildir, `/nix/store`, a dataset
dump — the walk behind that directory is a second on this machine and would be
seven or eight on a Pi. The first-200-rows path is unaffected and still
sub-millisecond, so the window paints instantly; it is the recursive size, the
sort and the search that wait.

**The fix is not more workers, it is a smaller unit of work.** A directory has to
be splittable — `getdents64` returns a cookie that can be resumed from, so chunks
of a large directory can become separate jobs. That is a change to `sk-fs`'s
shape rather than a tuning parameter, and it is worth knowing before the code
exists rather than after.

### The index is decisive

| | 100k flat | 1M flat | 1M realistic |
| --- | --- | --- | --- |
| build | 4.2 ms | 43.7 ms | 43.2 ms |
| load, warm | 2.2 ms | 27.8 ms | 27.7 ms |
| **load, cold** | **4.8 ms** | **40.2 ms** | **40.5 ms** |
| on disk | 4.9 MB | 50.4 MB | 51.0 MB |
| bytes per entry | 49 | 50 | 50 |

Cold load is measured with `posix_fadvise(POSIX_FADV_DONTNEED)` on the index
file, which evicts it from the page cache without root. It is a *floor* on cold
cost — the kernel drops the clean pages it can and may keep others — so a real
cold read can only be slower than this.

**40 ms to load a million entries, against a cold walk of 2.3 s on four-core x86
and 3.7 s on a Pi 5.** That is 57× and 92× faster than the thing it replaces, and
it settles the question [ADR 0008](../../docs/adr/0008-persistent-index-is-required.md)
opened: the dull format is fast enough, and the format ADR can be a short
document. No mmap, no compression, no sorted ordering is needed to beat a walk by
two orders of magnitude.

50 bytes per entry is also the number for the privacy and disk-space
conversation: a million files costs 50 MB, and a user with ten million files on a
disk is looking at half a gigabyte of index.

### Flattened search meets its bar here, and will not on a Pi

| Query | first result | full scan | matches |
| --- | --- | --- | --- |
| `(empty)` | 0.000 ms | 1.4 ms | 1,021,338 |
| `zzqx` — matches nothing | **17.9 ms** | 17.8 ms | 0 |
| `snapshot-render-70` | 0.15 ms | 23.0 ms | 118 |
| `e` | 0.000 ms | 13.3 ms | 620,938 |
| `rpt` — scattered subsequence | 0.002 ms | 25.1 ms | 154,331 |

Against a 50 ms bar, the first-result column passes comfortably — but the
interesting cell is `zzqx` at **17.9 ms**, because a query that matches nothing
has to read every name before it can say so. "Time to first result" and "time to
know there is no result" are different measurements and the second is the one a
user meets while typing a query that has not matched yet.

The number that actually decides this feature is neither:

> **as-you-type, one full scan per keystroke: 13.9, 19.1, 27.7, 28.6, 28.7, 27.5 ms
> for the six keystrokes of `report`.**

Typing a six-character word costs **146 ms of scanning**, and the worst single
keystroke is **28.7 ms — 57% of the 50 ms budget on the fastest machine in this
project.** Spike B measured a Pi 5 at roughly 3× this machine's per-core speed on
comparable work. **That puts a single keystroke at 60–90 ms on reference
hardware: over the 50 ms target and approaching the 120 ms hard fail.**

A fuzzy match cannot narrow incrementally the way a prefix match can — `repo` can
match names that `rep` matched at a different offset — so every keystroke really
is a full scan. The scan itself is already close to memory bandwidth: 28 MB of
arena in 28 ms. Getting under the bar on a Pi therefore needs one of parallelism
across the arena, an early-exit that stops at the first screenful, or an
n-gram prefilter — and that is a design decision this spike surfaces rather than
settles.

### Sorting misses its target, and the architecture's stated reason is wrong

| Sort, 1,021,338 entries | median | against a 120 ms target |
| --- | --- | --- |
| by size (`u64` column) | **0.93 ms** | 129× inside |
| by mtime (`i64` column) | 1.82 ms | 66× inside |
| by name, through the arena | **152.5 ms** | **27% over** |
| by name, precomputed `u64` keys | **148.3 ms** | **24% over** |
| building those keys | 1.74 ms | — |

Two findings, and the second is the one that matters.

**Sorting by name misses the 120 ms target** — 152 ms, between the target and the
300 ms hard fail, on the fastest machine available. On a Pi it would be several
times the hard fail.

**And [04-architecture.md](../../docs/04-architecture.md#data-layout)'s stated
remedy does not work.** The document says:

> Sort keys are derived once at listing time so a re-sort never recomputes a
> natural-order collation.

Precomputing the key is worth **3%** — 152.5 ms against 148.3 ms. The sentence is
not wrong about what it avoids; it is wrong about what the cost *is*. Sorting a
permutation of a million `u32` indices is dominated by the random access those
indices make into an 8 MB key array and a 28 MB arena, not by the comparison
function. Making the comparison cheaper does not help when the comparison is not
the cost.

The two column sorts are the evidence: `by size` reads one contiguous `u64` array
with no indirection at all and finishes in **0.93 ms**, 160× faster than the same
sort over names. The difference between those rows is entirely memory access
pattern.

What that suggests — and this spike does not measure it, so it is a direction and
not a result — is a radix sort over the precomputed keys, which is linear, reads
sequentially, and never dereferences the arena. The keys already exist and cost
1.7 ms to build.

### Both misses are fixable, and one is fixed

Having named a radix sort and a parallel scan as directions, they were measured
rather than left as suggestions.

#### Sorting: radix brings it inside the budget

| 1,021,338 entries | median | vs the 120 ms target |
| --- | --- | --- |
| by name, through the arena | 164.0 ms | 37% over |
| by name, precomputed keys | 154.3 ms | 29% over |
| **radix over the keys, then repair ties** | **96.6 ms** | **inside** |
| radix over the keys alone | **17.8 ms** | 6.7× inside |
| building the keys | 1.8 ms | — |

**A radix pass is 8.7× faster than the comparison sort it replaces** — 17.8 ms
against 154.3 ms — because it never compares and never dereferences the arena. It
reads the key array in order, eight times, and writes into 256 buckets.

The catch is in the gap between the last two rows. **94% of these names share
their first eight bytes**, so the tie repair costs 79 of the 96 ms. Widening the
key to sixteen bytes was measured and does not help: ties fall to 60% and the
total moves 97.2 ms → 94.8 ms, because the second key's eight radix passes cost
what the smaller repair saves.

That tie rate is partly an artefact of a generator with thirty stems. Only
partly: real filenames collide on long prefixes constantly — `IMG_20240101_`,
`Screenshot from 2024-`, `part-00001-of-00512`. **A fixed-width prefix key is the
wrong shape for filenames**, and the direction that follows is an MSD string
radix over the arena that never falls back to comparison at all. That is
unmeasured, and it is where the remaining 79 ms lives.

#### Search: parallelism is worth 8×, and that is enough

| Query | 1 thread | 24 threads | speedup |
| --- | --- | --- | --- |
| `zzqx` — matches nothing | 18.0 ms | **2.6 ms** | 6.8× |
| `snapshot-render-70` | 23.1 ms | **2.8 ms** | 8.3× |
| `e` | 12.9 ms | 1.9 ms | 6.8× |
| `rpt` | 24.9 ms | **3.1 ms** | 8.0× |

> **Worst first result across every query, parallel: 2.075 ms.** The bar is 50 ms.

**8× on 24 threads, not 24×** — the scan is memory-bandwidth bound, exactly as
the 28 MB in 28 ms figure implied, and no amount of threads changes how fast the
arena can be read. Eight is enough: it takes the worst case from 57% of the
budget to 4% of it, and a machine three times slower still lands at 12%.

The early exit matters as much as the parallelism. A query that matches early
costs almost nothing because every thread stops as soon as one of them finds a
hit; the 2.075 ms worst case is `zzqx`, which matches nothing and therefore has
to read everything before it can say so.

### Measured on a Raspberry Pi 5 — and the extrapolation was wrong

Everything above is the 24-thread laptop, with conclusions projected to reference
hardware by Spike B's "roughly 3× slower per core". That projection was measured
and it is wrong, in both directions, by up to 4×.

| Operation, 1M entries | laptop | Pi 5 | real factor | I predicted |
| --- | --- | --- | --- | --- |
| search scan, serial (`rpt`) | 24.9 ms | 65.8 ms | **2.6×** | 3× |
| index, cold load | 48.1 ms | 195.0 ms | **4.1×** | — |
| flatten | 14.1 ms | 77.4 ms | 5.5× | — |
| sort by name, comparison | 164.0 ms | 916.2 ms | **5.6×** | 3× |
| sort by name, radix + ties | 96.6 ms | 752.8 ms | **7.8×** | 3× |
| radix over keys alone | 17.8 ms | 213.3 ms | **12.0×** | 3× |

**A single per-core multiplier is not a model.** The factor runs from 2.6× to 12×
on the same pair of machines, and it tracks how much memory the operation moves
per unit of work, not how fast the cores are. The scan — which streams 28 MB
linearly — is the *cheapest* to port at 2.6×. The radix sort — which scatters a
million writes into 256 buckets — is the most expensive at 12×. Cache size and
memory bandwidth differ from clock speed independently, and this project has now
guessed wrong about that twice.

#### Search passes on the Pi, and only because the scan is parallel

| Query | 1 thread | 4 threads | speedup |
| --- | --- | --- | --- |
| `zzqx` — matches nothing | 47.4 ms | **13.8 ms** | 3.4× |
| `snapshot-render-70` | 61.7 ms | 16.8 ms | 3.7× |
| `rpt` | 65.8 ms | 18.6 ms | 3.5× |

> **Worst first result across every query, parallel: 13.3 ms.** The bar is 50 ms.

Serial, the same worst case is 47.4 ms — inside the bar by 5%, which is not
inside it at all. **The parallel scan is not an optimisation here, it is the
feature.** As-you-type serial is worse still: 72.3 ms for the sixth keystroke of
`report`, against a 50 ms budget, and 360 ms to type the word.

The speedup is 3.4–3.7× on four cores, against 8× on twenty-four. Both are far
below the core count and both are enough, which is what a bandwidth-bound scan
looks like.

#### Sorting fails on the Pi by every route measured

| Sort, 1,021,338 entries | Pi 5 | vs 120 ms target | vs 300 ms hard fail |
| --- | --- | --- | --- |
| by size (`u64` column) | **6.1 ms** | 20× inside | — |
| by mtime | 19.9 ms | 6× inside | — |
| radix over keys alone | 213.3 ms | **1.8× over** | inside |
| radix + tie repair | **752.8 ms** | 6.3× over | **2.5× over** |
| radix, 16-byte keys | 875.6 ms | 7.3× over | 2.9× over |
| by name, precomputed keys | 837.2 ms | 7× over | 2.8× over |
| by name, through the arena | 916.2 ms | 7.6× over | 3.1× over |

**Nothing measured sorts a million filenames inside the hard fail on reference
hardware.** The radix pass that fixed this on the laptop is 213 ms on the Pi
before any ties are repaired — already over the target — and the repair takes it
to 753 ms. The 16-byte key is *worse* here than the 8-byte one, because on a
machine this memory-bound the extra eight radix passes cost more than the halved
tie count saves.

Sorting by a metadata column is 6.1 ms, 123× faster than by name. The entire
difference remains memory access pattern, and on the Pi it is starker, not
milder.

#### Making the flat directory work

A single directory of a million entries takes 8.4× a realistic tree, and the
obvious fix is not available: `getdents64` returns a cookie that can be *resumed*
from, but the cookies are only discoverable by reading forward, so there is no way
to give thread two a starting point without thread one having already been there.
**A directory's name stream cannot be split.**

The other half can be. Reading names is one sequential stream; the million
`statx` calls behind them are independent. Batching those onto the pool and
letting the reader run ahead is also exactly what rule 1's progressive listing
wants — names appear immediately, size and mtime fill in behind.

| 1M entries | per-directory jobs, `statx` inline | `statx` deferred to the pool |
| --- | --- | --- |
| **flat, Pi 5** | 2,346 ms · p99 **11,896 ms** | **1,164 ms** · p99 **1,213 ms** |
| **flat, laptop** | 1,036 ms | **305 ms** |
| realistic, Pi 5 | **1,246 ms** | 1,374 ms |
| realistic, laptop | **131 ms** | 399 ms |

**Neither strategy wins on both shapes, and they are not close.** Deferring is
3.4× faster on a flat directory and 3× slower on a realistic tree. The reason is
symmetric: a flat directory has one job and needs the `statx` work taken off it,
while a realistic tree has 21,338 jobs already and deferring only serialises the
traversal that was parallel.

The Pi shows something the laptop hides. **The p99 on the flat directory collapses
from 11.9 s to 1.2 s** — a factor of ten in the tail, against two in the median.
A million `statx` calls issued serially from one thread means any stall stops
everything; overlapping them means a stall costs one worker. **The tail is the
number a user feels**, and it is the stronger half of this result.

So the implementation is a per-directory choice, not a global one: **defer when a
directory is large enough to be its own bottleneck, keep `statx` inline when it is
one of thousands.** Both measurements support that, and the threshold is a number
`sk-fs` should carry rather than a constant anyone should guess.

One caveat the numbers depend on: the deferred jobs issue the `statx` calls and
discard the results rather than writing them back into the store, so this prices
the syscalls and not the bookkeeping. Spike B's equivalent arm has the same
caveat, and a real implementation writes into columns it already reserved.

#### Sorting, chased down from 948 ms to 191 ms

Having found that no approach met the sort budget on a Pi, six more were tried.
Every row was verified to produce a correctly ordered result, because a sort that
is fast and wrong is not a result.

| Approach, 1,021,338 entries | Pi 5 | laptop | vs 120 ms target | vs 300 ms fail |
| --- | --- | --- | --- | --- |
| comparison, through the arena | 948.3 | 156.7 | 7.9× over | 3.2× over |
| comparison, precomputed keys | 842.1 | 153.0 | 7.0× over | 2.8× over |
| radix over keys, then repair ties | 774.8 | 96.1 | 6.5× over | 2.6× over |
| radix over 16-byte keys, then ties | 890.4 | 99.0 | 7.4× over | 3.0× over |
| recursive radix, never compares | 367.1 | 69.4 | 3.1× over | 1.2× over |
| recursive radix, buckets threaded | 215.8 | 23.2 | 1.8× over | inside |
| MSD split, then buckets threaded | 202.7 | 24.7 | 1.7× over | inside |
| **MSD split parallel too** | **191.5** | 23.8 | **1.6× over** | **inside** |
| *(one packed radix pass, ties left unsorted)* | *131.2* | *15.0* | *1.1× over* | *inside* |

**948 ms to 191 ms — 5.0× — and the row moves from 3.2× outside the hard fail to
comfortably inside it.** The 120 ms target is still missed by 1.6×.

What worked, in order of what it was worth:

- **Never fall back to comparison.** Repairing ties by string comparison costs
  more than the radix before it — 600 of the 775 ms — because 94% of filenames
  share their first eight bytes. Recursing onto the *next* eight instead, down to
  runs of 24 where comparison genuinely beats a histogram, is **775 → 367 ms**.
- **Sort the buckets in parallel.** After the first pass the runs share nothing:
  each owns a disjoint slice of the order array and reads an immutable arena.
  Longest run first, or one bucket of 134,000 decides the wall clock.
  **367 → 216 ms.**
- **Pack the key beside the index.** A permutation sort reads `keys[order[i]]`, a
  random probe into 8 MB per element per pass. Pairs make every read sequential
  for 8 MB of footprint: on the Pi **232 → 131 ms** for a single pass, on the
  laptop 17 → 15 ms. The memory-bound machine shows what the cache-rich one
  hides.
- **Split on the most significant byte only.** Buckets need one byte, not an
  eight-pass sort. **216 → 203 ms.**
- **Parallelise that split as well.** **203 → 191 ms.**

**And an estimate that was wrong.** The arithmetic said the serial split was 76%
of the remaining time — 367 ms serial against 216 ms with parallel buckets implies
a serial phase near 165 ms. Parallelising it bought **11 ms**. The model was
wrong, so the conclusion drawn from it was too.

What the numbers say instead: serial recursion is 367 ms and four threads bring it
to 191 ms, a speedup of **1.9× on four cores** — the same shortfall the parallel
*search* shows at 3.4×. **The sort is memory-bandwidth bound**, and 191 ms is
close to what this hardware does for an access pattern that reads a 28 MB arena
at random. Bucket skew is not the limit: 29 non-empty first-byte buckets, the
largest 13.4% and the top four 33.2%, so at four threads a balanced schedule is
still available.

**Getting to 120 ms therefore needs less memory traffic, not more threads.** The
lower bound in the table is the packed single pass at 131 ms, which is already
1.1× over target and does not finish the job. That is the honest ceiling for this
representation, and beating it means changing what is sorted — not how.

#### Warm is not reliably warm on 8 GB

| | p50 | p99 | mean |
| --- | --- | --- | --- |
| walk, realistic | 1,136 ms | **6,332 ms** | 2,290 ms |
| walk, flat-1M | 2,368 ms | **10,485 ms** | 4,016 ms |

Five repeats, machine idle, page cache supposedly warm from the repeat before.
The p99 is five times the median and the mean sits between them, which is the
signature of a couple of runs having to go back to the device.

The likely cause is that three trees and their indices — 2.1M inodes and 105 MB
of index — do not stay resident in 8 GB alongside everything else, so the dentry
cache is partly evicted between repeats. That is a hypothesis; what is measured
is that **on a memory-constrained machine, a "warm cache" benchmark is not
reliably measuring a warm cache**, and every warm number in this project's spikes
was taken on a machine with 60 GB.

## Verdict

**Go on the index. The search and sort budgets do not survive reference
hardware, and both need a design change rather than tuning.**

Measured on both machines, with the Pi 5 as the one that decides:

| Question | Laptop | **Raspberry Pi 5** |
| --- | --- | --- |
| Is the dull index format fast enough? | 48 ms cold | **195 ms cold — yes, 12–19× faster than the walk it replaces** |
| Does flattened search hit 50 ms to first result? | 2.1 ms | **13.3 ms — yes, and only because the scan is parallel** |
| Does sorting hit 120 ms? | 23.8 ms | **191 ms — no, but comfortably inside the 300 ms hard fail, down from 948 ms** |
| What breaks a walk? | one directory of a million entries, 8.4× a realistic tree — **halved by deferring `statx`** | the same; deferring is 2× on the median and **10× on the p99**, plus a warm cache that does not stay warm in 8 GB |

### What this settles

[ADR 0008](../../docs/adr/0008-persistent-index-is-required.md) deliberately left
the on-disk format undecided, on the grounds that a format is expensive to get
wrong and no numbers existed. The numbers now exist on both machines and the
answer holds on the one that matters: a header, the arena, and the parallel
arrays written as they sit in memory load a million entries in **195 ms cold on a
Pi 5**, against a cold walk of 2.3–3.7 s. **The format ADR can be written and it
can be short** — mmap, compression and key ordering are each complexity spent
against a number that is already 12–19× ahead.

One correction to make while writing it: an earlier revision of this document
proposed a budget row of "load the index for 1M entries, target 100 ms". That
figure came from the laptop's 48 ms. The Pi measures **195 ms**, so the target as
proposed is unmeetable on reference hardware and the row needs a number taken
from the Pi rather than from the fastest machine to hand.

50 bytes per entry is the figure for the conversation ADR 0008 flagged: a million
files is 50 MB on the user's disk, and ten million is half a gigabyte.

### What it breaks

Three things this spike found that are recorded against the documents that claim
otherwise:

1. **`Sort 1M entries < 120 ms` is not met on reference hardware, at 191 ms** —
   though comfortably inside the 300 ms hard fail, down from 948 ms across eight
   approaches. The limit is memory bandwidth, not parallelism: four cores buy
   1.9× on a pattern that reads a 28 MB arena at random, and a single packed pass
   that does not even finish the job is already 131 ms. Closing the last 1.6×
   means moving less memory, not adding threads. Sorting by a metadata column is
   8 ms, so this is specifically a *filename* sorting problem.
2. **`First search result < 50 ms` is met on a Pi at 13.3 ms, and only with a
   parallel scan.** Serial on the same machine the worst case is 47.4 ms, inside
   the bar by 5%, which is not inside it; and as-you-type is 72.3 ms per
   keystroke, outside it. The speedup is 3.4–3.7× on four cores and 8× on
   twenty-four — both far below the core count, because the scan is
   memory-bandwidth bound. The parallel scan is the feature, not an optimisation
   of it.
3. **A directory is the unit of parallel work, and the fix is asymmetric.** One
   directory holding a million entries takes 8.4× a realistic tree. Its name
   stream cannot be split — `getdents64` cookies are only discoverable by reading
   forward — but its `statx` calls can be deferred to the pool, which is 3.4×
   faster on a flat directory and **3× slower on a realistic tree**. `sk-fs` needs
   both strategies and a per-directory choice between them, which is a shape
   decision rather than a tuning constant.

### What it does not license

- **Two machines, and the second one changed three answers.** The laptop-only
  version of this document projected reference hardware with a single 3×
  multiplier taken from Spike B. Measured, the factor runs **2.6× to 12×** on the
  same pair of machines, tracking how much memory an operation moves rather than
  how fast the cores are. Every projection in this project should now be treated
  as a hypothesis until a slow machine has run it.
- **`homelab` and `leto` are unmeasured for Spike F.** Two machines is enough to
  disprove a single multiplier and not enough to characterise the curve.
- **A cold floor, not a cold measurement.** `fadvise` evicts what the kernel is
  willing to drop. A genuinely cold index read — after a reboot, on a busy disk —
  can only be slower than 40 ms.
- **Nothing about freshness.** This measures a cold-built index against a cold
  walk. Keeping one current is inotify's problem and belongs to
  [Spike C](../../docs/08-roadmap.md#spike-c--watching-at-scale). An index that
  takes 43 ms to build and cannot be kept fresh is not a solution to anything.
- **ASCII case folding.** The matcher folds ASCII only. Correct matching for a
  Turkish or Greek user costs more per byte, on the hot path of a scan that is
  already the tightest budget in this spike.
