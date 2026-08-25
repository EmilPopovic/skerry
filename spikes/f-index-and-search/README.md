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

## Verdict

**Go on the index. The search and sort budgets do not survive reference
hardware, and both need a design change rather than tuning.**

| Question | Answer |
| --- | --- |
| Is the dull index format fast enough? | **Yes, decisively.** 40 ms cold for a million entries against a 2.3–3.7 s cold walk — 57× to 92× |
| Does flattened search hit 50 ms to first result? | **Here, yes. On a Pi, no.** A keystroke costs 28.7 ms on the fastest machine measured; reference hardware is 3× that |
| Does sorting hit 120 ms? | **No.** 152 ms by name on the fastest machine, and the architecture's stated remedy is worth 3% |
| What breaks a walk? | **A single directory with a million entries.** 8.4× the realistic tree, because a directory is the unit of parallel work |

### What this settles

[ADR 0008](../../docs/adr/0008-persistent-index-is-required.md) deliberately left
the on-disk format undecided, on the grounds that a format is expensive to get
wrong and no numbers existed. The numbers now exist and they are not close:
a header, the arena, and the parallel arrays written as they sit in memory loads
a million entries in 40 ms cold. **The format ADR can be written, and it can be
short.** Nothing in mmap, compression or key ordering is needed to beat a walk by
two orders of magnitude, and each would be complexity spent against a number that
is already 57× ahead.

50 bytes per entry is the figure for the conversation ADR 0008 flagged: a million
files is 50 MB on the user's disk, and ten million is half a gigabyte.

### What it breaks

Three things this spike found that are recorded against the documents that claim
otherwise:

1. **`Sort 1M entries < 120 ms` is not met** — 152 ms on the fastest machine
   available, and the architecture's stated mechanism for meeting it (precomputed
   sort keys) is worth 3%. The cost is the random access pattern of sorting a
   permutation, not the comparison. A radix sort over the keys is the obvious
   direction and is unmeasured.
2. **`First search result < 50 ms` is met here and will not be on reference
   hardware.** 28.7 ms per keystroke on a 24-thread laptop, against a Pi that
   Spike B measured at 3× slower per core. The scan is already near memory
   bandwidth — 28 MB in 28 ms — so the remedy is parallelism, an early exit at
   the first screenful, or a prefilter.
3. **A directory is the unit of parallel work, and that is a problem.** One
   directory holding a million entries takes 8.4× a realistic tree of the same
   size. `getdents64` returns a resumable cookie, so a large directory *can* be
   split across jobs; `sk-fs` has to be shaped for that from the start.

### What it does not license

- **One machine.** Every number here is the 24-thread laptop. Spike B's four
  machines varied by 7× on comparable work, and the two conclusions above that
  depend on a 3× extrapolation to a Pi should be measured on one rather than
  projected. The harness runs anywhere and the trees are seeded.
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
