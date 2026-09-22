# 0043 — Recents are two stores read as one list, and ours is the one rule 6 counts

**Status:** Accepted

## Context

[Rule 6](../03-principles.md#6-state-survives-everything) lists "tabs, split
layouts, scroll offsets, selections, per-folder view settings, sort orders,
bookmarks and recents", and there are no recents. The
[state table](../04-architecture.md#configuration-and-state) puts them in
`$XDG_STATE_HOME/skerry/`; the desktop keeps them in
`$XDG_DATA_HOME/recently-used.xbel`, which is what GNOME's *Recent*, KDE's
*Recent Documents* and every GTK file chooser read. Those two sentences do not
conflict until somebody asks which one is the list.

[ADR 0021](0021-a-pin-is-a-line-in-gtks-bookmarks-file.md) answered the same
shape for pins — the format is the contract, so a pin is a line in GTK's
`bookmarks` and there is no list of our own. This record follows that reasoning
on one question and departs from it on three, and every departure is a fact about
this file rather than a change of mind about that one.

**The specification is thin.** Desktop Bookmark Specification 0.8.5 exists on the
freedesktop wiki and not in the published index, which still publishes the Recent
File Storage Specification 0.2 that 0.8.5 replaced. Its entire normative content
on concurrency is one sentence: an implementor "should" avoid concurrent access,
"for instance, by using file locking techniques" — no mechanism, no lock file
named, no ordering rule. The superseded 2002 specification was concrete and said
`lockf`; 0.8.5 dropped it. So the contract is what the implementations do, and
the two that matter do opposite things.

**GLib takes no lock and cannot see our write.** `GtkRecentManager` loads the
file once at construction, keeps a `GBookmarkFile` in memory for the life of the
process, coalesces writes on a 250 ms timer, and on that timer dumps its
in-memory copy without re-reading. Reproduced on this machine: after our atomic
whole-file write our entry is present, and after the next GTK write it is absent
— neither writer reports an error. The write itself is careful:
`g_file_set_contents` with `CONSISTENT | ONLY_EXISTING`, which is temp, `fsync`,
`rename`, followed by a `chmod` to 0600. The file's durability is good and its
concurrency is last-writer-wins over a window a quarter of a second wide.

**KIO does the opposite and says why.** `KRecentDocument` takes a `QLockFile` on
`recently-used.xbel.lock` — `O_EXCL` plus `flock(LOCK_EX|LOCK_NB)`, tried for
100 ms — re-reads under the lock, splices through `QXmlStreamReader` and
`QXmlStreamWriter` so that what it does not understand survives, and commits
through `QSaveFile`. Its own comment, three times in one file: "Won't help for
GTK applications and whatnot, but we can be good citizens ourselves." Its
`removeOldestEntries` is the counter-example in the same codebase — a truncating
in-place write, which is a corruption chain in code that ships today.

**A bad byte loses the file.** GLib rejects the whole document on a duplicate
`href`, an unknown attribute on `bookmark`, a foreign `metadata` block with any
child element, an XBEL `folder`, an internal entity declaration, a `version`
other than `1.0`, or a timestamp it cannot read — and then discards its
container, so the next application to add anything writes a file holding only its
own entry. The only trace is a `g_warning` on a stderr nobody reads. In
`bookmarks` a bad line loses a line; here a bad byte loses everybody's list.

**Other programs are licensed to delete our entries.** GTK clamps to 1,000
entries and 30 days on every write, KDE clamps to 300 and drops any `href`
containing `/.`, `KRecentDocument::clear()` deletes the file outright, and
GNOME's File History switch makes the next GTK write empty it. Nothing does that
to a `bookmarks` line. A rule 6 store another desktop is licensed to empty is not
a rule 6 store, and `kill -9` was never the threat here.

**Reading it costs a third of a frame.** GLib's own C parser, 300 iterations,
load-contaminated and therefore indicative rather than budget-grade: 0.165 ms
over this machine's real file — 39,845 bytes, 62 bookmarks — and 2.727 ms over a
synthesised file at GTK's 1,000-entry cap. Against an 8 ms frame that is a job,
and so is the per-entry existence check the display filter needs, which on a hung
mount is precisely the hazard [`tools/hung-mount`](../../tools/hung-mount) exists
to prove this program survives.

**The URI is the key, and we escape it differently from GLib.**
`g_filename_to_uri` percent-encodes one set and `g_markup_escape_text`
XML-escapes another, and neither covers the other: `/tmp/Q&A 'notes' <draft>.txt`
is stored as `href="file:///tmp/Q&amp;A%20&apos;notes&apos;%20%3Cdraft%3E.txt"`.
[`places::file_uri`](../../crates/sk-desktop/src/places.rs) escapes more than
`g_filename_to_uri` does, deliberately, and the reason its comment gives is sound
for `bookmarks` — a URI that escapes more still decodes to the same path. A
recent is matched on the exact URI string, so the same encoder here gives
`Q&A.txt` a second bookmark under `%26` and two rows in every GTK Recent view.
Its `percent_decode` also ends in `String::from_utf8_lossy`, and a recent entry
legitimately carries `%FF%FE` for a name that is not UTF-8.

## Decision

**Recents are two stores read as one list. `$XDG_STATE_HOME/skerry/recents` is
ours and is the one rule 6 counts; `$XDG_DATA_HOME/recently-used.xbel` is a
second source we read. R1 ships that half; writing the shared file is staged
behind a measurement this record names as its own precondition.**

- **The merge is keyed by the exact URI byte string**, timestamped by the later
  of the two `visited` times, and filtered the way the ecosystem already filters
  — gvfs's own `recent:///`: not private, `file:` scheme only, not
  `inode/directory`, and the file exists. Both the parse and the existence check
  run on the job pool, the merged list is a job's result that the frame reads,
  and an entry whose `statx` has not come back is not drawn. The watch is on the
  directory rather than on the file, because every writer publishes by `rename`
  and a watch on the inode stops hearing after the first one.

- **Ours is authoritative, and complete on its own.** It holds every entry we
  recorded and is written the way rule 6 asks, through
  [`sk_fs::save::atomically`](../../crates/sk-fs/src/save.rs) — its third caller,
  beside the session and the per-folder view records. It is never derived from
  the shared file and never repaired out of it. The shared file is additive: what
  survives the filter and is not tombstoned appears in the list, and a shared
  file that is missing, empty or unreadable costs the list nothing that is ours.

- **A removal is a tombstone with a timestamp, not a deletion.** Removing an
  entry drops our record and writes a tombstone carrying the URI and the moment
  of the removal, because a union over two stores resurrects every removal on the
  next read. The merge hides a shared entry whose URI is tombstoned — unless the
  shared `visited` is newer than the tombstone, which means somebody opened that
  file again after we were told to forget it, and the tombstone is then spent and
  dropped. Clearing the list is one tombstone per row it was showing. A tombstone
  whose URI is no longer in the shared file is pruned on the next write of our
  store, so the file does not grow without an end.

- **A recent is rule 6 state, so a removal is not on the undo stack.** That is
  ADR 0021's answer for ADR 0021's reason: rule 3's list is the file mutations —
  "rename, move, trash, create and batch-rename" — and forgetting a row in a list
  of things you opened moves no bytes on disk. `Ctrl+Z` after forgetting an entry
  and copying a file takes back the copy.

- **The reader is ours, and it is a single-pass scan over one fixed shape.** The
  [dependency budget](../07-dependencies.md) is a ratchet at 27 transitive
  crates, so an XML crate is not a row to add but a ratchet to move, bought for a
  grammar that does not vary. It must handle `xbel` at `version="1.0"`;
  `bookmark` with `href`, `added`, `modified` and `visited`; `title` and `desc`;
  `info` and `metadata owner="http://freedesktop.org/"` carrying
  `mime:mime-type`, `bookmark:applications` with `bookmark:application` and its
  `name`, `exec`, `modified` and `count`, `bookmark:groups` and
  `bookmark:private`; the five XML entities and numeric character references,
  undone before the percent-decoding and not after; and both timestamp forms —
  the ISO 8601 with a `Z` that GLib writes today and the older one with an
  offset. It provably need not handle a duplicate `href`, an unknown attribute on
  `bookmark`, a foreign `metadata` block with child elements, an XBEL `folder`,
  an internal entity declaration or a `version` other than `1.0`, because GLib
  rejects each of them outright: a document holding one is a document no GTK
  application can read and the next one to write will replace with a single entry
  of its own. Such a document is unreadable, said once in the log, and the list is
  then ours alone — never half-read, because a half-read list is
  indistinguishable from a short one.

- **Two helpers are new rather than reused.** The encoder agrees with
  `g_filename_to_uri` byte for byte, sub-delimiters left literal, and is not
  `places::file_uri`, which stays exactly as it is for `bookmarks` and for the
  bus, for the reason its comment already gives. The decoder yields bytes, so a
  name that is not UTF-8 round-trips as the path it names instead of as
  replacement characters.

- **The writer, if it ships, splices.** One contiguous byte range is replaced and
  every other byte of the file is copied through unchanged — not re-serialised,
  which is what a stream writer does and what silently normalises somebody else's
  attribute order and whitespace. The invariant is the test: for any input the
  reader accepts, the output differs from the input in exactly one contiguous
  span. It re-reads immediately before splicing, publishes through
  `sk_fs::save`'s four steps, and takes `recently-used.xbel.lock` in KIO's shape
  — `O_EXCL` plus `flock(LOCK_EX|LOCK_NB)`, bounded — which excludes KDE and
  costs nothing against GTK, because GLib takes no lock and will not see ours. A
  write of ours inside a running GTK application's 250 ms window is discarded by
  that application's next dump, silently. That cannot be fixed from this side,
  and it is one of the two reasons the write is staged.

- **R1 ships the read and our store; the shared write ships only if a measurement
  says it is worth its blast radius.** The other reason to stage it is the first
  fact in this Context: a splice is one badly-formed byte away from emptying
  every recent list on a machine that belongs to somebody. The question that
  decides it is how often the application we launched has already registered the
  file itself.

  **The measurement**, quoted before the code and not adjusted after
  ([12-measurement](../12-measurement.md#the-bar-is-quoted-before-the-code-and-never-adjusted-after)):
  watch `IN_MOVED_TO` on `$XDG_DATA_HOME` for one week of ordinary use, and for
  every file the window opens record whether that exact URI appears in the shared
  file within ten seconds with nothing written by us. On this machine's own file
  most applications already do.

  **The decision rule**: the shared write ships only if fewer than two in three
  opens are already registered, over at least 200 opens and at least five
  distinct applications. At two thirds or above it does not ship — not in R1 and
  not later — and reversing that takes a new record rather than a second opinion
  about the same number. The outcome is recorded where measurements are recorded,
  in [13-release-readiness](../13-release-readiness.md), and not by editing this
  record, which is immutable.

  **What the measurement cannot say**: one machine's application mix is one
  sample, and a desktop of Qt applications would answer differently from this
  one. The rule is a floor on this machine's evidence, and a second machine's
  number is worth more than a third significant figure on the first.

## Consequences

**Two stores give the list a failure mode that looks like success.** A shared
file another program has emptied is indistinguishable, in the file, from a
machine nobody has opened anything on. Our store is what tells the two apart, and
that is the whole reason authority is settled here rather than left to whichever
file has more entries.

**Entries disappear from under the merge and rows move.** GTK's clamp runs on
every write and KDE's on every add, so a file the shared half names today can be
absent from it tomorrow while our own record of it stays. Our half never shrinks
by somebody else's rule, so the list gets shorter from the bottom instead of
going blank.

**The existence filter is a `statx` per entry, on a job.** At GTK's cap that is a
thousand of them and any one can be on a mount that never answers. Rule 1 has no
exemption for a small file, and this is the discipline the listing already has:
the frame draws what has come back.

**Nothing new is depended on.** `sk-desktop` gains the reader and the encoder,
`sk-app` owns the merge and the tombstones — a parse is `sk-desktop`'s and a
policy never is — and the edge to `sk-fs` for the atomic write is the one ADR
0021 already added. [07-dependencies](../07-dependencies.md) does not move and the
ratchet at 27 is untouched.

**A removal is durable and not undoable.** Somebody who expects `Ctrl+Z` to bring
a row back is surprised once, exactly as with a pin, and the alternative surprise
— `Ctrl+Z` after a copy restoring a row to a list — is worse.

**Until the write ships, a file opened from Skerry may not appear in GNOME's
Recent list.** That is the quantity the measurement is about, and it belongs in
[the README's own list of what this alpha does not do](../../README.md#what-this-alpha-does-not-do)
rather than in somebody's bug report — the difference between a rule stated unmet
and one silently unmet.

**We carry a second grammar we did not design**, so the round-trip test over the
forms a real machine holds is the most important test the reader has, which is
ADR 0021's consequence one format along. The corpus is this machine's own file, a
file at the 1,000-entry cap, a file KDE wrote last, an entry whose name is not
UTF-8, an entry whose name needs both escaping layers, and one document for each
thing GLib rejects — every one of which must be refused whole.

## Revisit trigger

**The measurement.** Below two thirds and the writer ships on the terms above —
the splice invariant, the lock, the atomic publish — with no further decision
needed. At two thirds or above the shared write is declined, and a later reversal
is a new record linking back to this one.

**GLib re-reading before it dumps, or 0.8.5 arriving in the published index with
a concurrency mechanism.** Either turns last-writer-wins over a 250 ms window
into a protocol, which removes the second reason the write is staged and makes
this a decision worth retaking.

**A file in the wild that GLib rejects and another reader accepts.** The reader's
"provably need not" clause is an argument about what can exist on a working
machine, and one counter-example ends it — the answer then is a tolerant reader,
not a wider refusal.
