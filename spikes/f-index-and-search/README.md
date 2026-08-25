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

*Not yet run.*

## Verdict

*Pending the result.*
