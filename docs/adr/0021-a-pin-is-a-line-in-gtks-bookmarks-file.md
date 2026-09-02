# 0021 — A pin is a line in GTK's bookmarks file, and it is rule 6 state rather than rule 3 state

**Status:** Accepted

## Context

The sidebar reads three files and writes none: `user-dirs.dirs` for the
well-known directories, GTK's `bookmarks` for the list under them, and
`/proc/self/mountinfo` for the volumes. Reading is enough to *show* a place and
not enough to *keep* one, and keeping one is the feature — a person's fourth
directory is not Downloads and no specification knows its name.

So the list under the well-known directories has to be writable, and there are
three places to write it.

- **A list of our own, in the session file.** [ADR
  0011](0011-one-plain-text-state-file-written-atomically.md) already writes one
  plain-text file atomically and would take a `pin` line without argument. It
  produces the worst possible sidebar: a folder pinned in Nautilus appears here,
  because `bookmarks` is already read, and a folder pinned here appears nowhere
  else. Two kinds of row that look identical, live in different files and have
  different lifetimes, in a list eleven rows long.
- **A file of our own, beside the session.** The same asymmetry, plus a second
  format to parse and a second thing to explain.
- **GTK's `bookmarks`.** One list, one lifetime, and a pin made here is a pin in
  Thunar, Nemo, Caja and `PCManFM` — because all of them read and write this file
  too. It is not a specification and it is not GTK the library; it is a list of
  URIs, one per line, and [rule
  9](../03-principles.md) is about linking against nothing, not about refusing to
  interoperate. The XDG trash and `x-special/gnome-copied-files` are the same
  argument already won twice: the format is the contract.

The cost of the third is that we write a file we do not own, whose grammar we
know only from the files on real machines. That is the thing this record is for.

The second question is which stack a pin belongs on.
[Rule 3](../03-principles.md#3-everything-is-reversible) says every mutation is
reversible and its gate is that `Ctrl+Z` walks it back; a pin writes a file, so
the gate appears to apply. It does not, and rule 6 says why:
[rule 6](../03-principles.md#6-state-survives-everything) lists "tabs, split
layouts, scroll offsets, selections, per-folder view settings, sort orders,
**bookmarks** and recents", and its own failure condition is *"`kill -9` loses a
bookmark"*. Rule 3's list is the file mutations — "rename, move, trash, create
and batch-rename". A pin is named by one rule and not the other, and putting it
on the undo stack would mean `Ctrl+Z` after pinning a folder and copying a file
takes back the pin, which is not what anybody pressing it meant.

## Decision

**A pin is a line in `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, written whole and
atomically at the moment it is made, and it is not on the undo stack.**

- **`sk-desktop` owns the file in both directions.** It reads it now; it gains
  `pin`, `unpin` and `reorder`. The
  [architecture table](../04-architecture.md#crate-responsibilities) already
  gives that crate "XDG user directories and bookmarks", and a format with a
  reader in one crate and a writer in another is a format with two grammars.
- **Every line we do not understand survives, in place and byte for byte.** A
  `sftp://` bookmark is a GVfs mount this program cannot open and has no business
  deleting; so is `trash://`, so is a line with a label we did not write. The
  file is read, the one line that is ours to add or remove is added or removed,
  and everything else is written back as it arrived. The round trip is a test
  over every form found in the wild.
- **Written the way rule 6 asks**: to a neighbouring name, `fsync`ed, then
  `rename`d — the three steps [ADR
  0011](0011-one-plain-text-state-file-written-atomically.md) takes, in one
  function in `sk-fs` that the session file, the per-folder view records and this
  now share. There were two copies of those three steps before this and one of
  them had lost the `fsync`.
- **What can be pinned is a directory that is not already a place.** Not a
  well-known directory — it is in the list already, from another file, and
  `sidebar::build` drops a bookmark that duplicates one, so the line would be
  written and change nothing. Not a volume: a mount is a fact about the machine
  rather than a choice about it. Not a file. The command's `enabled` is that
  sentence, so the row is dimmed rather than absent and the menu is the same
  shape every time it opens.
- **What can be unpinned is exactly what is pinned**, which is why `Entry` grew a
  `pinned` flag rather than the sidebar inferring one from `Kind::Other`. The
  well-known directories and the volumes have no unpin, and that is drawn.
- **The target is state, not an argument.** A command runs against `&mut App` and
  takes nothing, so `App` holds the place a pointer or a keyboard is acting on —
  the same shape the selection has, which is how `file.trash` knows what to
  trash. The comment in `open_menu` that said a place's menu "needs a target,
  which the registry has no shape for yet" is answered without the registry
  changing shape: the target is a field, set before the menu opens.
- **The inverse is Unpin, and it is one press away in the same menu.** Rule 3's
  own exemption is for "an explicit, differently-shaped action"; this is not even
  destructive, and no undo entry can be lost to a program that was killed while a
  pin was on a stack in memory.

## Consequences

**We write a file other programs own, and a bug here damages their data.** That
is the real price. It is bounded in one direction — the write is atomic, so a
crash leaves the old file whole — and in the other only by the round-trip test,
which is therefore the most important test in `sk-desktop` and is written over
the forms a real machine has: a bare URI, a URI with a label, a percent-encoded
name, a `sftp://` mount, a blank line, and a trailing line with no newline.

**A pin is durable and not undoable, and the undo history will not mention it.**
Somebody who pins by accident sees the row appear and can unpin it; somebody who
expects `Ctrl+Z` to do it will be surprised once. The alternative surprise is
`Ctrl+Z` after a copy taking back a pin made ten minutes earlier, which is worse
and is what one stack over both would give.

**`sk-desktop` gains an edge to `sk-fs`** for the atomic write — level 2 to level
1, which the layering allows and `tools/check-layering.py` will confirm. No
external dependency arrives, so [07-dependencies.md](../07-dependencies.md) does
not move.

**The order of the pinned rows is the order of the lines**, so drag-to-reorder is
a rewrite of the whole list rather than an edit of one line, and it inherits
"every line we do not understand survives" — a reorder must not lose the
`sftp://` bookmark it cannot show. That is the same test with a different verb.

**A folder pinned here and deleted elsewhere leaves a line that shows nothing.**
`bookmarks()` already drops a bookmark whose directory is gone rather than
drawing it broken, and this decision does not change that: the line stays in the
file, because deleting other people's lines on the strength of one `stat` is how
a removable disk's bookmarks disappear while it is unplugged.

## Revisit trigger

**A round trip that loses a line.** The test carries every form found in the
wild; if a file manager ships a form it cannot preserve — a comment syntax, a
continuation, a second URI on one line — the answer is not a cleverer parser but
a list of our own with GTK's read as a second source, and the asymmetry accepted
because the alternative has become data loss.

**Or a freedesktop specification for the list**, which would make this a
specification to follow rather than a format to match, and would decide the
questions this record answers by observation.
