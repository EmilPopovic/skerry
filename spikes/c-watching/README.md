# Spike C — watching at scale

**Status:** method fixed, not yet run.

## Question

Does inotify hold up across the tens of thousands of directories a real tree
contains — within the per-user watch limit, through a forced queue overflow, and
fast enough that an external change is on screen before the user notices?

## Bar

Quoted from [08-roadmap.md](../../docs/08-roadmap.md#phase-0--spikes-34-weeks),
written before the code and not adjusted afterwards:

> **Bar:** an external `rm -rf` or `git checkout` reflected within 100 ms · no
> lost entries after a forced overflow · the watch budget documented as a number.

## What it decides

Two things, one of which arrived after the roadmap was written.

The roadmap's own question: **whether `fanotify` is ever worth its privilege
requirement** — expected answer no, and an expectation is not a measurement.

And [ADR 0008](../../docs/adr/0008-persistent-index-is-required.md)'s open flank.
That ADR made the persistent index required rather than optional, and
[Spike F](../f-index-and-search/README.md) showed it loads a million entries in
195 ms on reference hardware against a 2.3–3.7 s cold walk. It also said,
explicitly, that **freshness is Spike C's problem**. An index that takes 43 ms to
build and cannot be kept current is not a solution to anything, so this spike
decides whether ADR 0008's decision is affordable in practice.

## Method

### The constraint, before any code

The numbers the kernel gives us, read from `/proc/sys/fs/inotify`:

| | laptop | Pi 5 |
| --- | --- | --- |
| `max_user_watches` | 65,536 | 62,164 |
| `max_user_instances` | 128 | 128 |
| `max_queued_events` | 16,384 | 16,384 |

Spike F's realistic tree — a million files — holds **21,338 directories**. So one
such tree costs **33% of the entire per-user watch budget**, and three of them
exhaust it. A developer's home directory with several `node_modules` trees and a
few large checkouts is well past it already.

That is the shape of this spike: the interesting failure is not latency, it is
running out of watches, and what the program does when it has.

### The workloads

Spike F's trees, reused so the numbers are comparable: the realistic tree with
21,338 directories, and the flat million-entry directory which needs exactly one
watch and generates a million events.

### What is measured

| Number | Why |
| --- | --- |
| Watches established per second | Adding 21,338 watches is a startup cost paid on every launch |
| Peak RSS per watch | The kernel charges ~1 KB per watch; measured rather than assumed |
| Directories watched before `ENOSPC` | The watch budget, as the number the bar asks for |
| Latency, single change | `touch` to event delivered |
| Latency, `rm -rf` of a subtree | The bar's own case |
| Latency, bulk rename | A `git checkout` in the shape that matters: many renames at once |
| Overflow threshold | How many events it takes to provoke `IN_Q_OVERFLOW` |
| Entries lost after overflow | Must be zero, via a targeted rescan |
| Rescan cost after overflow | Because the recovery is what a user actually feels |

### The overflow path, which is the point

`max_queued_events` is 16,384. A `git checkout` across a large tree, an
`npm install`, or a `cargo build` produces more than that in a burst, and the
kernel's response is one `IN_Q_OVERFLOW` and then silence about everything it
dropped. **There is no way to learn what was missed** — the events are gone, not
queued elsewhere.

So the only correct response is to rescan, and the only question is *what*. This
spike measures three strategies:

- **Rescan everything.** Correct, and costs a full walk — Spike B's 1.2 s on a Pi.
- **Rescan the watches that were still delivering when the overflow hit**, on the
  theory that a burst is usually localised to one subtree.
- **Rescan nothing and rely on the next user action.** Fast, and wrong: it is the
  bug where a file you deleted in a terminal stays on screen until you press F5,
  which is exactly the failure mode rule 1's "correct change tracking as the disk
  mutates underneath you" exists to forbid.

The bar says no lost entries, so the third is measured only to price the
temptation.

### `fanotify`, and the expectation to test

`fanotify` with `FAN_MARK_FILESYSTEM` watches a whole mount with one descriptor
and no per-directory watches, which would dissolve the budget problem entirely.
It requires `CAP_SYS_ADMIN`. A file manager that asks for that capability, or
ships setuid, has made a security decision far larger than the feature it buys —
and rule 9 wants a binary that runs unprivileged on any distribution.

The measurement is therefore not "is `fanotify` faster" but **"what exactly does
inotify cost us that `fanotify` would not"**, so the refusal is priced rather than
assumed. If inotify's cost turns out to be unaffordable, that is a finding worth
having before the architecture depends on it.

### Deliberate departures

- **No `notify` crate.** The question is what the kernel interface costs and how
  it fails, and a cross-platform wrapper answers a different question — one about
  the wrapper's queueing and coalescing rather than the kernel's.
- **One watcher thread**, per the threading diagram in
  [04-architecture.md](../../docs/04-architecture.md#threading), with a 16 ms
  coalescing window. Not the job pool: a watcher that shares a pool with a copy
  engine inherits its latency.

## Result

Three machines: the 24-thread laptop, a 4-vCPU Zen 3 VM, and a Raspberry Pi 5.
All watching Spike F's realistic tree — a million files in 21,338 directories.
Raw output in [runs/](runs/).

### Latency, and what the number is actually measuring

| Case | laptop | homelab | **Pi 5** | vs the 100 ms bar |
| --- | --- | --- | --- | --- |
| one file created, p50 | 0.039 ms | 0.043 ms | **0.063 ms** | 1,600× inside |
| one file created, p99 | 0.060 ms | 5.41 ms | 1.19 ms | still 19× inside |
| `rm -rf` 2,000 files, all events | 7.13 ms | 25.4 ms | **40.1 ms** | 2.5× inside |
| 500 renames, 1,000 events | 3.84 ms | 11.1 ms | **16.9 ms** | 6× inside |

**Notification itself is free** — 63 microseconds on the slowest machine. Watches
establish at 125,000–206,000 per second and cost 119 bytes each on our side.

**But the `rm -rf` figure is not a notification latency and should not be read as
one.** The clock starts before `remove_dir_all` and stops when the last event
arrives, so it includes the deletion. That is the right thing to measure for the
bar as written — "an external `rm -rf` reflected within 100 ms" is about when the
listing is correct again — but it means **the number scales with the size of the
deletion, not with inotify.**

On the Pi, 2,000 files take 40 ms, so roughly **50 files per millisecond**. A
`rm -rf node_modules` is routinely 30,000 files: **around 600 ms, six times the
bar.** The bar is met for the case measured and is not met for the case people
actually run, and that is a property of how fast a Pi can unlink files rather
than anything inotify does.

### The watch budget, confirmed exactly on three machines

| | `max_user_watches` | obtained while already holding 21,338 | sum | as % of the limit |
| --- | --- | --- | --- | --- |
| laptop | 65,536 | 41,539 | 62,877 | 96% |
| homelab | 60,793 | 39,446 | 60,784 | **99.99%** |
| Pi 5 | 62,164 | 40,815 | 62,153 | **99.98%** |

**The limit is exactly per-user**, to within 0.02% on two of three machines. One
million-file tree costs 34–35% of it. Three exhaust it.

And **the limit is not a constant**: 65,536, 62,164 and 60,793 on three machines,
because the kernel derives it from available memory. Any policy that hard-codes
65,536 is wrong on two of these three.

### Overflow recovery: 12× the bar on reference hardware

Identical behaviour everywhere — a burst of 52,000 creations across 4,000
directories delivered **exactly 16,384 events from 316 distinct watches** on all
three machines, and dropped 35,616. The queue depth is the constant; what differs
is only what recovery costs.

| | laptop | homelab | **Pi 5** |
| --- | --- | --- | --- |
| rescan the 4,000 directories touched | 132 ms | 148 ms | **701 ms** |
| **rescan all 21,338, which correctness requires** | **331 ms** | **569 ms** | **1,234 ms** |
| against the 100 ms bar | 3.3× over | 5.7× over | **12.3× over** |

The earlier laptop-only result projected "over a second on a Pi" from Spike B's
walk numbers. Measured: **1.234 s.** For once a projection in this project was
right, which is worth noting only because [the last three were not](../f-index-and-search/README.md#a-prediction-the-cache-finding-made-and-measurement-refused).

The 316 figure is the one to keep. It is identical on all three machines because
it is a property of the queue, not the hardware: the queue fills from the first
directories a burst reaches and then drops everything else in silence. **A recovery
that rescans "the directories we heard from" covers 316 of 4,000 and reports
itself finished.**

### Recovering by pruning on directory mtime

A full rescan is the only *correct* recovery, and it is 1,234 ms on a Pi. There is
a cheaper one that is correct about the thing it claims.

A directory's mtime moves whenever an entry is created, deleted or renamed in it.
So `statx` every directory — cheap — and re-read only the ones that moved.

| | laptop | homelab | **Pi 5** |
| --- | --- | --- | --- |
| baseline: 21,338 directory mtimes | 20 ms | 38 ms | **62 ms** |
| changed directories detected | 4,000 / 4,000 | 4,000 / 4,000 | **4,000 / 4,000** |
| false negatives | **0** | **0** | **0** |
| stat pass | 19 ms | 38 ms | 62 ms |
| read the changed directories | 76 ms | 146 ms | 349 ms |
| **recovery total** | **96 ms** | **184 ms** | **411 ms** |
| full rescan, for comparison | 233 ms | 564 ms | 882 ms |
| speedup | 2.4× | **3.1×** | **2.1×** |

**Zero false negatives on all three machines**, and the recovery drops from
882 ms to 411 ms on the Pi — from 8.8× the latency bar to 4.1×. Still outside it,
and no longer in a different category.

**The ratio here is pessimistic.** This burst changed 18.7% of the tree's
directories; a real `git checkout` or `npm install` is localised to one subtree,
where the changed fraction is a percent or two and pruning approaches the cost of
the stat pass alone — **62 ms on a Pi, inside the bar.**

**What it is sound for, precisely.** Directory mtime tracks *structural* change:
entries appearing, disappearing, being renamed. It does not move when a file
already in the directory is written to or `chmod`ed, so a lost `MODIFY` or
`ATTRIB` event stays lost. **It recovers the listing's shape, not every column in
it.** The columns are recoverable separately by re-`statx`ing the entries of the
handful of directories actually on screen, which is a tiny set — so the two halves
compose into a correct recovery, and neither half alone is one.

### As close as ext4 gets to an MFT

Worth stating alongside, because it is the question this spike kept running into.

**An NTFS MFT record contains the filename** — `$FILE_NAME` is an attribute of the
record — so reading the MFT yields paths. **An ext4 inode has no name field at
all.** Names exist only in `ext4_dir_entry_2` structures inside directory data
blocks.

So there is nothing on ext4 that "reading the index" could give us. Building a
path index means reading directory blocks, which is the work `getdents64` already
does; reading them raw off the device would make the access sequential rather than
VFS-mediated, a constant factor at the cost of root and filesystem-specific
parsing. `STATX_CHANGE_COOKIE`, which would be a per-inode change counter, is not
in this kernel's headers — checked, not assumed.

**Directory mtime is the closest thing available: a change filter the kernel
maintains for free, sound for structure and silent about content.**

## Verdict

**Go on inotify, with two things that need designing rather than tuning.**

| Condition | Result |
| --- | --- |
| An external `rm -rf` or `git checkout` reflected within 100 ms | **PASS for the case measured** — 40 ms on a Pi for 2,000 files — and **fails at scale**: the figure is dominated by the deletion at ~50 files/ms, so a 30,000-file `node_modules` is ~600 ms |
| No lost entries after a forced overflow | **PASS.** A full rescan is 1,234 ms on a Pi; **mtime pruning is 411 ms with zero false negatives**, and ~62 ms for a localised burst |
| The watch budget documented as a number | **21,338 directories per million-file tree = 34% of a per-user budget that is 60,793–65,536 depending on the machine's memory, and is shared with every other application** |

### `fanotify` is still the wrong answer, and now the cost is priced

The roadmap expected to decline `fanotify` and this spike agrees, but not because
the alternative is free. `FAN_MARK_FILESYSTEM` watches an entire mount with one
descriptor: no per-directory watches, no `ENOSPC`, no budget shared with the user's
IDE, and no overflow-then-rescan problem of this shape. **It would dissolve two of
the three findings above.**

It requires `CAP_SYS_ADMIN`. A file manager that asks for that, or ships setuid,
has made a far larger security decision than the feature justifies, and rule 9
wants a binary that runs unprivileged on any distribution. **The refusal stands,
and it costs us a shared 65,536-watch budget and a 331 ms recovery path.** That is
the price, stated, rather than an expectation confirmed.

### A third thing, which the scaling exposed

**The bar's own wording does not survive its own test.** "An external `rm -rf`
reflected within 100 ms" is met at 2,000 files and missed at 30,000, because the
number is the deletion rather than the notification. A budget row that passes or
fails depending on how large the user's `node_modules` is, is not measuring the
thing it names.

What the measurements support instead: **notification latency** — microseconds,
and worth a tight bar — and **time for a bulk external change to be fully
reflected**, which is bounded by the kernel's unlink rate and belongs in the same
family as overflow recovery: progressive, visible, and not instantaneous.

### What has to be designed

1. **Running out of watches is a normal condition, not an error.** The budget is
   per-user and shared, so a file manager will be refused watches on a machine
   that is otherwise working perfectly. `ENOSPC` from `inotify_add_watch` needs a
   real policy: which directories get watches when there are not enough — visible
   panes first, then recently visited, then nothing — and a fallback for the rest
   that does not pretend to be live. The failure this prevents is the worst kind:
   a listing that looks current and is not.

   **And the limit must be read, never assumed.** It is 65,536, 62,164 and 60,793
   on the three machines measured, because the kernel derives it from available
   memory. Any code with 65,536 in it is wrong on two of these three.
2. **Overflow recovery cannot meet the latency bar for a tree-wide burst, so it
   must not pretend to.** Pruning on directory mtime gets a Pi from 882 ms to
   **411 ms** with zero false negatives, and to roughly the 62 ms stat pass when
   the burst is localised — which is the common case. A tree-wide burst is still
   outside the bar. A rescan that blocks the listing for a
   second is a frozen window; one that silently rescans the wrong subset is a lie.
   What the measurement supports is a *visible, progressive, cancellable* rescan —
   the same machinery rule 1 already requires for a first listing — with the panes
   the user is looking at rescanned first. **The 100 ms bar applies to observing a
   change, not to recovering from having missed 35,000 of them**, and
   [09-performance-budgets.md](../../docs/09-performance-budgets.md) should say so
   rather than leaving a row that cannot be met.

### What this means for ADR 0008

[ADR 0008](../../docs/adr/0008-persistent-index-is-required.md) made the persistent
index required and left freshness to this spike. The answer is that **freshness is
affordable for a tree you are watching and not for a filesystem you are not.**

Watching 21,338 directories costs 0.112 s and 2.7 MB, and change latency is
microseconds — so an index of the *current* tree stays current essentially for
free. But a whole-filesystem index, which is what "flatten an entire drive" needs,
would want watches on every directory on the disk, and the budget is 65,536 shared
with everything else. **The index can be kept fresh where it is watched, and
everywhere else it is a snapshot with a timestamp**, which is a user-visible
property the interaction design has to carry rather than a detail.

That does not invalidate ADR 0008 — a stale index is still 195 ms against a
3.7 second walk — but it does mean the ADR's "accelerator for a walk that still
works" clause is load-bearing, and results from an unwatched region must be
visibly distinguishable from live ones.

### What it does not license

- **Three machines, all local ext4 on NVMe.** The thing left unmeasured across
  hardware is a slow *device*: every rescan figure here is NVMe, and the same
  recovery on a spinning disk or an SD card — which is what a great many Pis
  actually boot from — would be far worse than 1.2 s.
- **One filesystem, no network.** ext4 on NVMe. inotify does not work across NFS
  at all — a change made on another host is invisible, with no event and no error
  — and this spike did not touch that. A file manager that shows a stale network
  mount as though it were live has the same defect as one that ignores `ENOSPC`.
- **No `fanotify` measurement.** It is declined on privilege, so its performance
  was never measured. If the watch budget ever becomes the thing that blocks a
  release, that decision is worth revisiting with numbers rather than reasoning.
- **Nothing about coalescing.** The harness reports raw events. The 16 ms
  coalescing window in [04-architecture.md](../../docs/04-architecture.md#threading)
  is unmeasured, and it is the mechanism that decides whether 16,384 events become
  one repaint or sixteen thousand.
