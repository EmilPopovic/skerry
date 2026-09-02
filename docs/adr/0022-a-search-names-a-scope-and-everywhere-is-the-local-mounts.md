# 0022 — A search names a scope, and everywhere is the local mounts: one walk over several roots, index-first per root

**Status:** Accepted

## Context

`Ctrl+Shift+F` searches everything under the folder you are in.
[`sk_index::find`](../../crates/sk-index/src/find.rs) walks it depth-first, one
filesystem, symlinks unfollowed, reporting as it goes — and the first result
arrives in **0.7 ms over 817,288 entries**
([09](../09-performance-budgets.md#what-first-search-result-measures-and-what-it-does-not)).
That is the scope a person asks for most often. It is not the scope they ask for
when they do not know where the file is, which is the case a search exists for.

"Everywhere" needs three questions answered, and each has a wrong answer that
looks reasonable.

**What is everywhere?** `/` is the tempting one and it is wrong twice: the walk
refuses to cross a mount — `du -x`, for the reason
[`size`](../../crates/sk-index/src/size.rs) gives, that crossing one takes an
idle network mount's timeout — so a second disk would be silently missing; and
`/proc`, `/sys`, `/dev` and `/run` are not places, so a walk that did cross would
spend its time in `/sys/devices` and report `bpf` as a result. Every mount is
worse: this machine has **sixty-four** and four of them are places.
[`sk_fs::mounts`](../../crates/sk-fs/src/mounts.rs) already answers exactly this
question for the sidebar's Volumes section, with an allow-list of filesystem
types and a `remote` flag on each.

**How long does it take?** Measured, on the machine
[09](../09-performance-budgets.md#what-m0-measured) describes, at a one-minute
load average of 1.2–1.35, warm page cache, `/` at **6,901,145 entries**:

| | |
| --- | --- |
| A complete walk of `/`, for `config` | **8,559 ms** |
| The same answer from an index of `/` | **27.45 ms** |
| Loading that index, warm | **413.6 ms** |
| Building it | 8,577 ms |
| Its size on disk | 342,234,511 bytes — **49.6 bytes an entry** |

**312×**, and it is the difference between a scope that works and a scope that
does not: 8.6 s is not a search, it is a `find` you have to wait for. This is
precisely what [ADR 0008](0008-persistent-index-is-required.md) made a persistent
index mandatory for and what [ADR
0012](0012-a-flat-index-file-per-root-rebuilt-whole.md) built one for, and the
index has had no caller that needed it until now.

**What does it cost while it is open?** 326 MB of resident memory for that index,
against an RSS row already missed at 117.6 MB. That is the price of this scope on
a 6.9-million-entry filesystem and it is the reason the third bullet below exists.

## Decision

**A search carries a scope. `Here` is the folder, and `Everywhere` is every local
mount — walked as one walk over several roots, with any root that has an index
answered from it first.**

- **The roots of `Everywhere` are `sk_fs::mounts()` minus the remote ones.** The
  same allow-list the sidebar's Volumes section draws, so what the sidebar calls
  a place is what the search calls a root and neither can grow a filesystem the
  other has not heard of. A remote mount is excluded because reaching one costs
  its timeout — [Spike B](../../spikes/b-enumeration/README.md) measured 28,000
  ms for one unresponsive mount — and a search that hangs is a worse answer than
  a search that says what it covered. Where `/proc/self/mountinfo` cannot be read
  at all the scope is `/` alone, because a search with no roots finds nothing and
  says nothing about why.
- **One walk, several roots: the walk's "one filesystem" rule becomes "the
  filesystems you asked for".** `find` takes a set of devices rather than one
  device, and `find(root)` is `across(&[root])` — so the single-root scope is
  unchanged, and a pseudo-filesystem is still excluded by not being a root rather
  than by a list of names to keep up with. The **seeds** are the roots no other
  root contains: `/boot` under `/` is not walked twice, because the walk reaches
  it by descending and its device is in the set.
- **An index is loaded for a root when a search wants it and dropped when the
  search closes.** [ADR 0020](0020-a-preview-is-a-few-textures-bounded-by-bytes.md)'s
  sentence, for the same reason: it is what makes 326 MB a peak rather than a
  floor. The window holds one index per root of the scope in flight — the
  generalisation of "the one that is loaded is the one the last search wanted" —
  and re-reading it costs the 413.6 ms above, on a job, started when the panel
  opens rather than on the first keystroke, because typing three characters takes
  longer than that.
- **Index-first per root, then one walk over the rest, in one job.** The indexed
  roots answer in tens of milliseconds and their results are on screen before the
  walk of the others starts. Serially, and not in parallel across roots: [Spike
  B](../../spikes/b-enumeration/README.md) measured a parallel walk at 4× and it
  needs a store shared between threads, which is a different design — and the
  thing a person waits for here is the *first* result, which arrives in about a
  millisecond either way.
- **The summary says "indexed" only when every root in the scope answered from
  an index.** An index's age is a promise about what could have been found; two
  ages averaged is a promise about neither, and a mixed answer takes the plain
  complete-or-capped line, which promises nothing false.
- **The cap is over the whole scope.** [`find::MOST`](../../crates/sk-index/src/find.rs)
  is two thousand hits and stays two thousand: it is reported rather than hidden,
  and a cap per root would make the number depend on how somebody's disks are
  partitioned.
- **`Ctrl+Shift+F` opens the folder scope and `Ctrl+Alt+F` opens everywhere**;
  either command switches the scope of an open panel without losing what has been
  typed, and the scope is a control in the panel so a pointer reaches it too,
  which is [rule 5](../03-principles.md#5-pointer-and-keyboard-are-peers).

## Consequences

**326 MB of resident memory while a whole-filesystem search is open on this
machine**, freed when it closes, on top of a 117.6 MB floor. There is no version
of this scope that is fast and does not hold the index somewhere, and the
alternative is 8.6 seconds. It is a peak tied to a panel somebody opened, and it
is now a row in [09-performance-budgets.md](../09-performance-budgets.md) rather
than a surprise.

**And a third of that memory is provably unread.** The index is 49.6 bytes an
entry, of which `size` (8) and `modified` (8) are written by
[`store::build`](../../crates/sk-index/src/store.rs) and read by nothing:
`store::search` `statx`es every hit before drawing it, which is ADR 0012's
freshness policy, so the stored copies are dead weight — **110 MB of the 326**.
`flags` (2) is the same. Removing them is a format change, which is a decision of
its own and is not this one; it is recorded here because the number was measured
here.

**A walk of a bind mount is a walk of it twice.** Two mount points on one device
put both under the seed's tree, and nothing tracks visited inodes — which is
already true of one subtree containing a bind mount of itself, so this scope
inherits the limitation rather than introducing it. The cost is duplicate rows,
not a hang, because the depth is still bounded by the tree.

**There is no one action that indexes every root.** `index.build` acts on the
folder you are standing in, as it always has; the scope uses whatever indexes
exist. Indexing the whole filesystem is therefore "go to Filesystem in the
sidebar, build the index" — two steps rather than one, and the alternative was a
command whose meaning changed depending on whether a panel was open.

**A search over everywhere reports `partial` almost always**, because a walk from
`/` as an ordinary user cannot read `/root` or another account's home. That flag
already exists and already means "something was skipped"; what changes is that it
is now the normal case rather than the exception, so it must not be drawn as an
error.

## Revisit trigger

**A complete indexed answer over 100 ms** in this scope, measured as the scan of
every root's index for a query that matches at the cap. It is 27.45 ms at 6.9
million entries; 100 ms means either a tree four times this one or a scan that
has stopped being bound by the memory it moves, and the answer is a different
scan rather than a different scope.

**Or an unindexed complete answer over 30 s**, which is where "the list is still
filling" stops being an honest description of what the panel is doing. The answer
then is the parallel walk Spike B measured at 4×, not a narrower scope.
