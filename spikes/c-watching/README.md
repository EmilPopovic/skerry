# Spike C — watching at scale

**Status:** method fixed, not yet run.

## Question

Does inotify hold up across the tens of thousands of directories a real tree
contains — within the per-user watch limit, through a forced queue overflow, and
fast enough that an external change is on screen before the user notices?

## Bar

Quoted from [08-roadmap.md](../../docs/08-roadmap.md#phase-0--five-spikes-34-weeks),
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

24-thread laptop, idle at load 1.21, watching Spike F's realistic tree — a million
files in 21,338 directories. Raw output in [runs/laptop.txt](runs/laptop.txt).

### Latency passes, and not narrowly

| Case | median | vs the 100 ms bar |
| --- | --- | --- |
| one file created | **0.039 ms** | 2,500× inside |
| `rm -rf` of 2,000 files — first event | 7.09 ms | 14× inside |
| `rm -rf` of 2,000 files — last event | 7.13 ms | 14× inside |
| 500 renames (1,000 paired events) | 3.84 ms | 26× inside |

Establishing the watches is not a startup problem either: **21,338 watches in
0.112 s**, about 190,000 per second, and 127 bytes of our own memory per watch.

The bar's first condition is met with room that makes it uninteresting. **The
other two are where this spike earns its place.**

### The watch budget, as the number the bar asks for

| | |
| --- | --- |
| `max_user_watches` on this machine | 65,536 |
| Directories in one million-file tree | **21,338 — 32.6% of it** |
| Watches actually obtainable **while already holding those 21,338** | **41,539, then `ENOSPC`** |
| Our own memory per watch | 100–127 bytes |

That third row is the finding, and it corrects the test that produced it. The
probe expected to reach 65,536 and stopped at 41,539 — because **the limit is per
*user*, not per process or per instance**, and this program was already holding
21,338 of them. 21,338 + 41,539 = 62,877, which is the budget minus whatever else
this login session had open.

**So a file manager never has 65,536 watches. It has 65,536 minus the IDE, minus
the sync client, minus the language server, minus the desktop's own indexer** —
and it finds out by being refused, one directory at a time, with no signal that
anything is wrong beyond an errno it could have ignored.

Three million-file trees exhaust the budget outright. A developer's home directory
with a few `node_modules` and a couple of large checkouts is past it before a file
manager starts.

### Overflow: the bar is met only by a full rescan, and that misses the latency budget

One directory first, the easy case:

| | |
| --- | --- |
| Files created in a burst | 49,152 in 0.35 s |
| Events delivered | **16,384 — exactly `max_queued_events`** |
| Events lost | 32,768 — **66.7%** |
| Overflow signalled | yes |
| Targeted rescan of the one directory | 49,152 entries in **7.1 ms**, recovers everything |

Then the case that actually happens — a burst spread across the tree, which is
what `git checkout`, `cargo build` and `npm install` produce:

| | |
| --- | --- |
| Files created | 52,000 across **4,000 directories** in 0.26 s |
| Events delivered | 16,384, from **316 distinct watches** |
| Events lost | 35,616, across an unknown subset of 4,000 directories |
| Rescan of the 4,000 directories actually touched | 132 ms |
| **Rescan of all 21,338, which is what correctness requires** | **331 ms** |

**The surviving-event set is not just insufficient, it is misleading.** 16,384
events arrived from **316** watches — the queue filled up from the first few
directories the burst reached and then dropped everything else in silence. A
recovery that rescans "the directories we heard from" would rescan 316 of the
4,000 that changed and confidently report itself finished.

The kernel offers nothing better. `IN_Q_OVERFLOW` is one bit; the dropped events
are gone, not buffered elsewhere, and there is no way to ask what they were. **The
only correct recovery is to rescan everything watched, and that is 331 ms against
a 100 ms latency budget** — 3.3× over, on the fastest machine measured in this
project. Spike B's walk numbers put the same rescan at over a second on a
Raspberry Pi.

## Verdict

**Go on inotify, with two things that need designing rather than tuning.**

| Condition | Result |
| --- | --- |
| An external `rm -rf` or `git checkout` reflected within 100 ms | **PASS**, 7.1 ms and 3.8 ms, 14–26× inside |
| No lost entries after a forced overflow | **PASS only via a full rescan**, which costs 331 ms and therefore misses the latency bar |
| The watch budget documented as a number | **21,338 directories per million-file tree, against a per-user budget of 65,536 shared with every other application** |

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

### What has to be designed

1. **Running out of watches is a normal condition, not an error.** The budget is
   per-user and shared, so a file manager will be refused watches on a machine
   that is otherwise working perfectly. `ENOSPC` from `inotify_add_watch` needs a
   real policy: which directories get watches when there are not enough — visible
   panes first, then recently visited, then nothing — and a fallback for the rest
   that does not pretend to be live. The failure this prevents is the worst kind:
   a listing that looks current and is not.
2. **Overflow recovery cannot meet the latency bar, so it must not pretend to.**
   331 ms here, over a second on a Pi. A rescan that blocks the listing for a
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

- **One machine.** Everything here is the 24-thread laptop. The latencies have
  three orders of magnitude of headroom and will survive; the 331 ms rescan will
  not, and Spike B's walk numbers put it over a second on a Pi. The rescan is the
  number to re-measure.
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
