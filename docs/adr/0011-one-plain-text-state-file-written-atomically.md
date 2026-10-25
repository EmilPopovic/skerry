# 0011 — One plain-text state file, written whole and renamed into place

**Status:** Accepted

## Context

[Rule 6](../03-principles.md#6-state-survives-everything) says tabs, split
layouts, scroll offsets, sort orders and per-folder view settings are restored on
relaunch, "written durably enough to survive a hard power cut", and fails if
`kill -9` loses a bookmark. Nothing in the program writes anything at all today:
every launch is a factory reset, down to the theme and whether the sidebar is
open. It is the only one of the ten rules with no implementation whatsoever.

[ADR 0010](0010-a-split-tree-of-tab-strips.md) already chose the tree's shape
partly for this: *"the tree serializes to the state store M2 owes, and its shape
is chosen here to make that a recursive four-line form rather than to make it
possible."* What that ADR deliberately did not decide was the file: where it
lives, what is in it, how it is written, and what happens when it is from a
different version. All four are expensive to reverse once anyone has run a build
that writes one, because from then on every later build has to read what the
earlier one wrote.

## Decision

### One file, in `$XDG_STATE_HOME`

`$XDG_STATE_HOME/skerry/session`, falling back to `~/.local/state` — which is
what the basedir specification says state is for: *"state data that should
persist between (application) restarts, but that is not important or portable
enough to the user that it should be stored in `$XDG_DATA_HOME`"*. That is
exactly what a window layout is.

Not `$XDG_CONFIG_HOME`: config is what a person edits and this is what the
program writes. Mixing them means a hand-edited file being overwritten on exit,
which is the single most reliable way to make people distrust a program's config.
The config file M6 owes is a separate file in a separate directory, and it is a
serialization of the command registry rather than of the window.

One file rather than one per window: a second window is a second process today,
and two processes writing two files means neither of them can be the answer to
"what was I doing". The single-instance work in M7 is what makes more than one
window one process, and this decision is revisited there.

### Plain text, one record per line, ours

Not a serialization crate. [07-dependencies.md](../07-dependencies.md) prices
`serde` plus a format at four to six crates for something whose whole grammar is
below. Not a binary format: a state file that cannot be read with `cat` is a
state file nobody can tell you is corrupt, and every parser in this repository is
already ours for the same reason.

```
skerry 1
theme dark
sidebar on
hidden off
zoom 2
window 1100x720
split beside 0.5
  leaf 1
    tab /home/tester/notes name+ 0
    tab /home/tester/Downloads size- 340
  leaf 0
    tab / modified+ 0
```

Four kinds of line, which is the four-line recursion ADR 0010 promised: `split`
with an axis and a ratio, `leaf` with the index of its showing tab, `tab` with a
path, a sort and a scroll offset, and the flat settings above them. Indentation
is decoration for a person reading it; the parser reads the tree from the
`split`/`leaf` sequence, because indentation that carries meaning is a grammar
that fails silently when something reformats it.

**A path is the last field on its line and is not escaped.** A newline is the one
byte that cannot appear in a path component, so a line holds any path there is.
Every other field is an identifier from a closed set.

### Written whole, renamed into place

Written to `session.new` in the same directory, `fsync`ed, then `rename`d over
`session`. That is the same three-step placement `sk-fs` already uses for a copy,
and it is what makes rule 6's "hard power cut" clause true rather than
aspirational: `rename` within a directory is atomic, so a reader sees the whole
old file or the whole new one and never a half-written one.

**Not written on every change.** A scroll offset changes sixty times a second
while a wheel is spinning. The file is written when the window closes, and
otherwise at most once every few seconds and only when something has actually
changed — on a job, because rule 1 has no exemption for a small file.

### A version line, and a refusal rather than a guess

The first line is `skerry 1`. A file whose first token is not `skerry`, or whose
version this build does not know, is **ignored in full** — the program starts as
it does today, on the directory it was given, and says nothing. It is then
overwritten by the next save.

Ignored rather than partially read: a state file is not a document somebody wrote
and it holds nothing that cannot be reconstructed by opening a folder again. A
half-understood layout is a worse outcome than a fresh one, and an error message
about a file the person has never heard of is worse than both.

A line inside a known version that this build does not recognise is skipped, so a
newer build's extra settings do not stop an older one from restoring the layout
it does understand.

### What is not in it

**Selections.** A selection is about files that may not be there any more, and
restoring one means a `Ctrl+X` from before a restart acting on a set nobody can
see was chosen. Rule 6 names selections; this decision declines that half of it
and records the reason here rather than leaving it to look like an oversight.

**Filters.** A filter restored on relaunch is a window that opens showing four of
nine hundred files with no visible cause, which is the shape of the bug people
report as "my files are gone".

**Bookmarks and recents.** Rule 6 names both. They are M7's, they are content
rather than layout, and they belong in `$XDG_DATA_HOME` where the basedir
specification puts data a person would miss.

## Consequences

- `sk-app` gains a `session` module: a parser, a writer, and the tree's
  round-trip. It is level 4, which is right — the shape being written *is*
  product state, and nothing below it should know a window has tabs.
- The tree grows `Panes::describe`/`Panes::restore`. `describe` is total;
  `restore` is fallible and falls back to one leaf on one tab.
- Every field added to the file from now on is a compatibility obligation for as
  long as version 1 is read. The cheap escape is the skip-unknown-lines rule
  above, which means a field can be *removed* freely and only *renaming* one is
  expensive.
- Restoring happens before the first read is issued, so the window opens on the
  restored tree rather than opening on `$HOME` and then jumping.
- A path in the file may not exist any more — a removed USB drive, a deleted
  folder. That is not an error: the pane opens on it, the read fails, and the
  pane shows the problem it already shows for a directory that is gone.

## Revisit trigger

If the single-instance work in M7 makes a second window a second set of panes in
one process, "one file" stops being a description of the state and becomes a
choice between windows. The signal is the first save that has to decide which of
two windows it is describing. That is a new ADR, not an edit to this one.

If per-folder view settings arrive and turn out to want a record per folder
rather than per tab — thousands of them, looked up by path on every navigation —
they are a different file with a different access pattern, and putting them in
this one would make a session save proportional to how many folders have ever
been visited.
