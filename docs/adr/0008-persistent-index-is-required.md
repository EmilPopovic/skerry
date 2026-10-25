# 0008 — A persistent filename index in `sk-index`, not an optional extra

**Status:** Accepted

## Context

[04-architecture.md](../04-architecture.md) lists `sk-index`'s responsibilities as
"recursive walk, flattened views, fuzzy matching, ignore rules, recursive size aggregation,
**the optional persistent index**". That word was written before anything was measured.

[01-research-file-pilot.md](../01-research-file-pilot.md) ranks flattened search `core` —
"collapse a subtree, or an entire drive, into one flat list in milliseconds, then
fuzzy-filter it fzf-style". [09-performance-budgets.md](../09-performance-budgets.md) turns
that into a row: first search result under 50 ms on a warm 1M-file tree.

[Spike B](https://github.com/EmilPopovic/skerry/blob/spikes-final/spikes/b-enumeration/README.md) measured what a walk costs on four machines.
A million files, producing names, sizes and mtimes:

| | warm | cold |
| --- | --- | --- |
| 24-thread laptop | 0.130 s | 0.80–0.85 s |
| 4-core x86 | 0.72 s | 2.3–2.4 s |
| 4-core aarch64 (Pi 5) | 0.91–1.07 s | 3.7 s |

Those numbers are good — 4× better than `read_dir` plus `metadata()`, and the design is
worth having regardless of this decision. They are also three to four orders of magnitude
away from "milliseconds", and no amount of further tuning closes that. The remaining levers
in the walk are worth tens of percent, not thousands.

**The gap is not our code; it is a missing kernel primitive.** On Windows the standard
technique reads the NTFS Master File Table in bulk to build a whole-volume filename
database in about a second, and then keeps it current from the USN change journal, which
reports every create, rename and delete on the volume. ext4 has an inode table but no
bulk-readable volume index and no subscribable change journal. The Linux equivalents of
Everything — FSearch, plocate — all do the same thing: walk once, store the result, and
rescan to stay current, because there is nothing else to do.

So the choice is not "index or go faster". It is "index, or do not ship flattened search
over anything larger than a subtree".

## Decision

`sk-index` carries a persistent filename index. It is a required component of the 1.0
feature set, not an optional accelerator, and the flattened-search feature is specified
against it rather than against a live walk.

Three constraints follow immediately, and they are the part of this decision that is
expensive to reverse:

- **In-process, not a daemon.** The index is built and queried by the file manager's own
  job pool. A background indexing service is a second lifecycle, a second failure mode and
  a second thing to explain to a user whose disk is busy — and it is the design most likely
  to be blamed for a machine feeling slow. Building only while the application is running
  is a real cost and is accepted.
- **Ours, not the desktop's.** Not `tracker`/`tinysparql`, not Baloo, not `plocate`'s
  database. Rule 9 forbids depending on a desktop's services, `plocate`'s database is
  updated by a cron job and is routinely hours stale, and both put the freshness of our
  results outside our control.
- **An accelerator for a walk that still works.** Every flattened view must be correct with
  no index, degrading to the Spike B walk and its progressive results. A missing, stale or
  corrupt index is a performance event, never a correctness one, and never a reason to show
  the user an error.

The on-disk format, the update mechanism and the freshness policy are **not** decided here.
They depend on measurements that do not exist yet, and a format is the most expensive thing
in this area to get wrong. A later ADR settles them, informed by
[Spike F](https://github.com/EmilPopovic/skerry/blob/spikes-final/spikes/f-index-and-search/README.md).

## Consequences

- A persistence format enters the project, with everything that implies: versioning,
  forward compatibility, corruption detection, and a decision about what happens when the
  format changes under an existing user.
- Disk space and privacy become user-visible concerns. A filename index is a list of
  everything a user has, in a file, and it needs a documented location, a documented size,
  and a way to turn it off and delete it.
- Staleness becomes a correctness surface in the interaction design even though it is not
  one in the data: results that no longer exist must be visibly distinguishable, and rule
  1's progressive streaming has to reconcile indexed hits with live ones.
- `sk-index` grows from a walker into a component with state that outlives the process,
  which makes it the second-most-likely place after `sk-plat` for a subtle bug to hide.
- The 25-crate non-GPU budget is under pressure: this is the kind of feature that invites a
  serialization crate. It should not need one.

## Revisit trigger

Any of: a Linux kernel interface arrives that reports filesystem changes volume-wide
without a watch per directory, making a live walk competitive; measurements in Spike F show
the un-indexed walk reaching the 50 ms first-result budget on reference hardware; the index
cannot be kept fresh within a bounded and documented staleness on a tree that changes as
fast as a build directory; or flattened search over a whole filesystem is moved to
[11-non-goals.md](../11-non-goals.md), which would remove the requirement entirely.
