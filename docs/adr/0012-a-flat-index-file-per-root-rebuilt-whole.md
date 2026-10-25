# 0012 — One flat index file per root, rebuilt whole, verified at the point of use

**Status:** Accepted

## Context

[ADR 0008](0008-persistent-index-is-required.md) decided that `sk-index` carries a
persistent filename index and that it is required rather than optional. It
deliberately left three things open — the on-disk format, the update mechanism
and the freshness policy — on the grounds that "a format is the most expensive
thing in this area to get wrong" and that none of the numbers existed. It named
the document that would spend them: a later ADR, informed by
[Spike F](https://github.com/EmilPopovic/skerry/blob/spikes-final/spikes/f-index-and-search/README.md).

Spike F has run, on a 24-thread laptop and on a Raspberry Pi 5, and the Pi is the
machine that decides. Its answer to the format question is unambiguous:

| | 1M entries, laptop | 1M entries, **Pi 5** |
| --- | --- | --- |
| Cold load | 40.2 ms | **195 ms** |
| Cold walk it replaces | 0.80–0.85 s | **3.7 s** |
| On disk | 50.4 MB | 50 bytes per entry |

**195 ms against 3.7 s is 19×**, from the dullest format that can work: a header,
the name arena written whole, and fixed-width parallel arrays written as they sit
in memory. Spike F's own conclusion is that "the format ADR can be written and it
can be short — mmap, compression and key ordering are each complexity spent
against a number that is already 12–19× ahead."

Two further findings constrain what is written here rather than what is stored.
The scan over a loaded index must be **parallel**: on a Pi, serial worst case is
47.4 ms against a 50 ms bar, which Spike F records as "inside the bar by 5%, which
is not inside it", and as-you-type is 72.3 ms, outside it. And every projection in
this project is now suspect — the laptop-to-Pi factor ran 2.6× to 12× on the same
pair of machines, tracking memory moved rather than core speed.

What no measurement covers is freshness. Spike F says so explicitly: it compared a
cold-built index against a cold walk and "an index that takes 43 ms to build and
cannot be kept fresh is not a solution to anything." That is the part of this
decision taken on design grounds rather than on numbers, and it is the part most
likely to be revisited.

## Decision

### The format

One file per indexed root, little-endian throughout, in three parts:

- **A header**: magic `SKIX`, a format version, the entry count, the arena
  length, the root path, the device the root sat on, the wall-clock time the
  build finished, and a checksum of everything after the header.
- **The name arena**: every entry's name, concatenated, no separators.
- **Fixed-width parallel arrays**, one element per entry: parent index `u32`,
  name offset `u32`, name length `u16`, flags `u16`, size `u64`, modified `i64`.

Twenty-eight bytes per entry plus its name, which is Spike F's 50 bytes at its
measured mean name length. A path is reconstructed by walking parent indices, so
no path is stored twice; the root is stored once, in the header.

The checksum is FNV-1a over the body. It detects a truncated or corrupted file,
which is the only thing it is for — an index is not a security boundary, it is
a cache of things the user can already read.

**No serialization crate**, which ADR 0008 named as the pressure this feature puts
on the 25-crate budget and Spike F confirmed is unnecessary.

### Where it lives

`$XDG_CACHE_HOME/skerry/index/`, one file per root named by a hash of the root's
path. **A cache, not state** — the basedir specification's distinction is exactly
whether losing the file loses anything the user cannot regenerate, and this file
is a slower copy of the filesystem. It is not [ADR
0011](0011-one-plain-text-state-file-written-atomically.md)'s neighbour: losing
the session loses a layout somebody arranged, and losing the index costs a walk.

That answers ADR 0008's requirement of "a documented location, a documented size,
and a way to turn it off and delete it": the location is above, the size is 50
bytes per entry, deleting the directory at any moment is safe, and building is
never automatic.

### How it is updated

**Rebuilt whole, only when asked.** There is no incremental update and no
background indexer. Two reasons, and the first is ADR 0008's: an index that
builds itself while the user is doing something else is the design most likely to
be blamed for a machine feeling slow, and ADR 0008 has already refused the daemon
that would make it defensible. The second is that incremental update is where the
correctness lives, and there is no measurement behind it yet — Spike F measured a
cold build against a cold walk and explicitly did not measure maintenance.

So: a command builds an index for a root, on the job pool, cancellable, and says
when it finished. Nothing builds one on its own.

### How stale results are handled

**Verified at the point of use, not at load.** A hit taken from the index is
confirmed with one `statx` before it is shown, and one that no longer exists is
dropped. The verification is bounded by the same `MOST` cap the walk already has,
so it is at most two thousand `statx` calls whatever the tree's size.

This makes one direction of staleness invisible and the other honest. A file
deleted since the build never appears, because it was checked. A file *created*
since the build does not appear either, and no check can fix that — so the search
says how old the index is, and that sentence is the freshness policy's whole user
interface.

The alternative — verifying every entry at load — is a `statx` per entry, which is
the walk this feature exists to avoid.

### What it must never do

ADR 0008's third constraint is restated here because it is the one this format
could quietly break: **an index is an accelerator for a walk that still works.**
Absent, truncated, wrong-magic, wrong-version, failing its checksum, or built for
a different root — every one of those falls back to
[`find`](../../crates/sk-index/src/find.rs) and none of them is an error the user
is shown. A search with no index is slower and equally correct.

### The scan is parallel

Spike F found that the parallel scan "is the feature, not an optimisation of it",
so it is specified here rather than left to the implementation. Shards are
claimed from an atomic counter by helper jobs *and by the job that spawned them*,
which is what makes it safe on a pool that does not steal work: a helper that is
never scheduled claims nothing, and the coordinating job finishes the whole scan
by itself. There is no configuration in which this waits on a job that cannot
run.

## Consequences

- **The window between builds is the user's to manage, and that is a real cost.**
  A person who indexes their home directory and then downloads a file will not
  find it until they rebuild. This is the least satisfying part of the decision
  and the one the revisit trigger below is aimed at.
- A rebuild is all-or-nothing, so a large tree costs its whole build again for one
  new file. At Spike F's 43 ms per million entries that is cheap in CPU and not
  cheap in I/O on a cold tree.
- One file per root means two overlapping roots are indexed twice and stored
  twice. Accepted: the alternative is a containment check on every query and a
  shared store whose invalidation is the hard problem this ADR is avoiding.
- The format is versioned and a version mismatch is a silent rebuild, so a format
  change costs users one walk and costs us no migration code. This is only true
  while the index is a cache; the moment anything unrecoverable is stored in it,
  it stops being true.
- `sk-index` gains file I/O and therefore a second way to fail. Every failure path
  in it must reach the same place — the walk — and a test asserts that for each.
- Verification at the point of use puts up to two thousand `statx` calls between
  the scan and the first drawn result, which is inside the budget on the numbers
  available and is a real addition to a path that is otherwise pure memory.

## Revisit trigger

Any of: a measurement shows incremental update from the existing inotify watcher
keeping an index correct within a bounded staleness on a tree that changes as fast
as a build directory, which would remove the manual rebuild; the parallel scan
misses 50 ms to first result on reference hardware at 1M entries, which would make
the format's simplicity the thing to spend against; 50 bytes per entry proves
wrong by more than 25% on real home directories, which would reopen compression;
or the verification `statx` pass measures above 10 ms at the `MOST` cap, which
would move freshness back to load time.
