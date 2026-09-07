# 0032 — An archive is unpacked by a child that can only write where the files go

**Status:** Accepted

## Context

*"Add the ability to look into and extract common archives."* Two features that
sound like one, and the plan already separates them:
[11-non-goals.md](../11-non-goals.md) declines "virtual filesystems **beyond
archive listing**" — so listing is the named extent of what a virtual path may
be here — and declines "a bundled archive manager", so creating and editing
archives is somebody else's program. [M5](../08-roadmap.md) owes "archive
contents" in the inspector. What is left to decide is three things, and they are
independent.

**What is inside.** A zip says so in its central directory. A tar does not say
at all: the only way to know is to read every 512-byte header to the end, and if
the tar is compressed that means decompressing the whole thing.

**Who decodes.** [ADR 0006](0006-out-of-process-decoders.md) put every decoder
in a sandboxed helper because "a decoder is the largest attack surface in a file
manager and it runs on whatever landed in a download directory". An archive out
of a download directory is that sentence's most literal case.

**Where the bytes land.** Extraction is the first operation in this program
whose *destination is named by the input*. Every other mutation writes where a
person said; an archive entry called `../../.ssh/authorized_keys` writes where
the archive said. That is zip-slip, it is twenty years old, and it is still
found in shipping extractors every year.

### What each option costs

**A library, in this process.** `zip` with only its deflate feature, `tar`, and
a decompressor each. It is the shortest path to working and it fails the first
question: it puts an attacker-controlled parser in the address space that owns
the window, which is exactly what ADR 0006 exists to forbid. It also puts the
*writing* there — so path safety would rest entirely on our own string
checking, which is the thing that keeps being got wrong.

**libarchive, through `bsdtar`.** One battle-tested decoder for zip, 7z, tar,
cpio, iso and rar, driven as an external program the way `skerry-preview-doc`
drives `pdftoppm` — the precedent is already here and it is a good one.
Measured, and it lost on facts:

- **`bsdtar` is not installed on the reference machine.** Ubuntu 26.04 ships
  GNU `tar`, `unzip` and `7z`; `libarchive-tools` is not part of a default
  install. So the "one tool" is three tools, with three human-readable listing
  formats to parse and no machine-readable one between them except `7z -slt`.
- **The tool would do the writing**, which means the confinement below and the
  list of paths an undo has to remove would both come from parsing `-v` output.
  An undo entry derived from a text stream is an undo entry that is wrong the
  first time a filename contains a newline.

**Our own container, the machine's compressor.** A tar is 512-byte headers and a
zip is a central directory — 182 and 213 lines of arithmetic as built, which is
the trade [07-dependencies.md](../07-dependencies.md) already made explicitly for
GIF ("four hundred lines of arithmetic against two crates"). A compressor is the
opposite: xz is a range coder and zstd is a research project, and both are
already installed as single-purpose filter programs whose entire contract is
`-dc`, stdin to stdout — nothing to parse and no output format to drift.

**And the container costs nothing.** Listing the largest archive that could be
built on this machine — 4.3 GB of `/usr/lib/x86_64-linux-gnu` as a tar, `xz -6`
to 1.0 GB, 19,082 entries, warm page cache on tmpfs, two runs each:

| | |
| --- | --- |
| `tar -tf lib.tar.xz` — container **and** filter | 2.46 s, 2.49 s |
| `xz -dc lib.tar.xz > /dev/null` — the filter alone | 2.48 s, 2.51 s |

The walk is free. All of it is the decompressor, which is the half we are not
writing — and the decompressor's cost turns out to be a *memory* decision rather
than a fixed price:

| `xz` flags | peak RSS | 4.3 GB decoded |
| --- | --- | --- |
| `-T1` | 10 MB | 26.5 s |
| `-T0 -M128MiB` | 167 MB | 9.4 s |
| **`-T0 -M256MiB`** | **332 MB** | **5.6 s** |
| `-T0 -M512MiB` | 616 MB | 3.4 s |
| `-T0`, unbounded | 1.5 GB | 2.2 s |

**The bottom row does not run under this sandbox at all.** `Limits::PREVIEW`
caps address space at a gigabyte, and threaded `xz` left to itself reserves past
that: `xz: lib.tar.xz: Cannot allocate memory`. Told `-M256MiB` it fits in that
gigabyte with room to spare — 329 MB of RSS — which is what makes the budget the
fix rather than a bigger cap. Found by running it, which is the only way it
could have been found: every layer above would have reported an undecodable
archive.

## Decision

**One helper, `skerry-archive`, spawned per request, that lists an archive and
unpacks it — and during an unpack the only writable directory it has is the one
the files are going into.**

- **Landlock does the path safety, and our string checks are the second
  layer.** `sk_preview::sandbox::confine` already takes a writable set — the
  thumbnailer passes the XDG cache — so an unpack passes the destination and
  nothing else. An entry called `../../.ssh/authorized_keys` is refused by the
  kernel, in a process that has no other writable path to reach. Entries whose
  path is absolute or contains `..` are *also* rejected before the open, because
  a machine with no Landlock is a real machine and this is the one place where
  the fallback matters.
- **Every file is created with `O_EXCL` and no entry is written through a
  symlink.** The other half of zip-slip is an archive that extracts a symlink
  to `/` and then writes "through" it, which no path check catches because the
  path has no `..` in it. `O_EXCL` makes the second write fail; Landlock makes
  it fail again.
- **Nothing arrives with more rights than a file this process wrote itself.**
  The mode *asked for* is 0644, or 0755 where the entry's own execute bit is
  set, and the umask takes it from there — so somebody who works at 077 gets
  0600 out of an archive as they do out of everything else. setuid, setgid and
  sticky are dropped, and device nodes, fifos and sockets are skipped and
  counted. An archive is not a way to install a setuid binary.
- **A container we can parse, we parse. A compressor is the machine's.** Zip
  (stored and deflate) and tar are read here; gzip is `flate2`, which is already
  in both helpers' trees through `png` and so costs no crate; xz, zstd, bzip2 and
  lzma come from `xz`, `zstd`, `bzip2` and `lzma` on `PATH`, run as `-dc`
  filters over a pipe, **with `-T0 -M256MiB` where the filter is `xz`**: a
  decompressor told its budget picks its own thread count to fit, and one left to
  guess is eleven times faster than a single thread and dies under the cap the
  sandbox already applies.
- **A format nothing here reads is named, and named by the format.** 7z, rar and
  cab are archivers rather than filters, so each would be a second tool with its
  own listing format; what the panel says instead is *"a 7z archive is not
  something Skerry reads"*. That is `Refusal::Elsewhere`, and it is deliberately
  **not** `Refusal::NoUnpacker`: the first version of this had one refusal for
  both and told somebody with 7-Zip installed that 7z was not installed. A
  missing `xz` is a package to install and a 7z is a program to open it with, and
  the two sentences have nothing in common but their tone.
- **A listing is bounded in what it sends and complete in what it counts.** The
  first 100,000 entries cross the wire, or as many as fit in 28 MB of table,
  whichever comes first — **the child stops below the ceiling the host enforces**,
  because a bound only one end knows about turns a legitimate archive with long
  paths into a reply the other end refuses whole. The entry count and the
  unpacked total are always the real ones, so a panel over a million-entry tar
  says how many it is showing out of how many there are rather than implying it
  showed them all.
- **An unpack is one queue entry, one undo entry, and one new directory.**
  `Queued::Unpack` joins Trash, Erase, Copy and Move, so it inherits the
  progress line, the pause latch and the cancel token that already exist. It
  extracts into a directory named after the archive, beside it, under a free
  name — never into the current folder, because a tarbomb's forty top-level
  entries are forty things to undo and a person who asked for one gesture gets
  one. `Change::Unpacked { archive, root, only }` is reverted by removing that
  directory, which is `Change::Copied`'s inverse and the same argument:
  everything in it was made by the operation.
- **`only` is there because one entry is a whole gesture too.** A row chosen in
  the panel and Extract pressed takes that entry — or that folder's worth of
  them — and nothing else, into a folder of its own under the same rule. It is
  the *same* command with a smaller target rather than a second registry entry,
  for the reason rule 2 exists: "Extract this entry" in the palette would be a
  row that means nothing when no panel is open. The field is recorded on the
  change because a redo has to be the same change — without it, redoing a
  one-file extraction would unpack the other nine hundred into the folder that
  one file is in.
- **No ETA, on purpose.** Every other queue walks its sources first so the line
  can say how long is left; what an unpack will write is only knowable by
  decompressing the archive, so a walk beside it would decompress everything
  twice to put an estimate on the line. Rule 7 would rather say what has
  happened than pay for a guess.
- **A bomb is stopped by free space, not by a ratio.** The child checks the
  destination filesystem every 32 MiB and stops with `Refusal::NoRoom` when it
  would leave less than 64 MiB free. A compression-ratio ceiling was written
  first and thrown away: a legitimate fixture built for this work — 156 MB of
  source tar, `xz -6` to 896 KB — is **174:1**, which is inside the range where
  42.zip lives, so a ratio cannot separate them and would refuse real archives.
- **An archive is not a directory.** No pane navigates into one, no breadcrumb
  names a path inside one, and no operation takes an entry as a source. Looking
  into an archive is the quick look panel, which is where the roadmap put it,
  and the panel navigates *within* the listing it already has.

## Consequences

**A compressed tar has no cheap listing, and `Space` on a 4 GB one costs 5.6
seconds.** No amount of engineering here changes that: the table above says the
cost *is* the decompressor, and the only dial is memory. So a listing is an
ordinary job with a cancel token rather than a request on the preview host —
that host's five-second deadline would turn the archives most worth looking into
into refusals, and what a person actually has is a panel that says it is reading
and closes when they press `Escape`. A zip of the same size lists in
milliseconds, because it has an index; that is a property of the format and not
of this code.

**Redo of an extraction is the app's, not `ops::apply`'s.** Every other change
can be re-done by `sk_fs::ops` alone; this one needs a child process, and
`sk-fs` is level 1 with no business spawning one. So `Edits::redo` routes
`Change::Unpacked` back through the queue instead, and `ops::apply` refuses it
in so many words. That is one asymmetry in a module whose whole value is that
the inverse lives beside the operation, and it is the price of the sandbox.

**A stopped unpack offers no resume**, which every other queue here does. Picking
one up would mean a fresh child merging into a half-written folder where `O_EXCL`
refuses every name already there — including the file that was half written when
the stop landed, which would stay short and be counted as skipped. The answer is
`Ctrl+Z` and a fresh start, and both are one press.

**A failed unpack removes what it wrote, and a stopped one does not.** This is
the one place stricter than the copy engine, and the reason is that the folder is
the operation's own: nothing was in it before, so removing it loses nothing and
leaves no half-unpacked tree to mistake for a finished one. A *stopped* one is a
person's decision and its files stay, recorded, for `Ctrl+Z` — exactly as a
cancelled copy's do.

**The sandbox's CPU cap had to become a parameter.** Ten seconds is right for a
preview and wrong for unpacking four gigabytes, so `confine` now takes its
limits rather than holding them as constants. Two existing call sites changed and
neither changed behaviour, but a shared, tested lockdown grew a knob — and a knob
on a sandbox is a thing to get wrong later, so `Limits::PREVIEW` and
`Limits::UNPACK` are named values rather than numbers at the call site.

**Fidelity is deliberately lost.** No setuid bits, no device nodes, no ownership,
no extended attributes: extracting a root filesystem tarball here produces
something that is not a root filesystem. `tar -x` as root is the tool for that,
and the non-goals already decline being it.

**A second wire, and it is not small.** The preview wire is request-then-reply
with pixels in it; an unpack reports progress as it goes and could not be one
reply. Rather than growing a payload kind onto a header built for images,
`sk_preview::archive` carries its own framing — 516 lines of it with the host
side included, against the preview wire's 606. That is a real cost and the
alternative was worse: a reply header with three payload shapes in it, where
every reader has to know which ask it came from.

**And a `poll` on a pipe, which no other helper needed.** A listing can take
seconds and a person can close the panel during them, so the read waits in
tenth-of-a-second steps and checks a cancel token between them — otherwise a
closed panel leaves an `xz` decompressing four gigabytes into a pipe nobody will
read. The same loop makes Stop land inside a hundred milliseconds during an
unpack rather than when the child next has something to say. It is the first
place in this program where a *blocking* read had to become an interruptible one,
and the buffered-reader trap came with it: `poll` reports nothing readable while
records are already in hand, so the buffer is checked first.

## Revisit trigger

**A person asking for 7z or rar**, which is the report this record most expects.
The answer is one `Unpacker` implementation over `7z -slt` and `7z x`, priced at
a listing parser and a second execute grant — not a change to anything above.

**Or a wait somebody complains about**, which the table prices: a listing that
streams its table in chunks instead of sending one, so the first hundred rows
are on screen while the decompressor is still working. That is the same shape
[ADR 0019](0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md)
gives a page — a request that names a range — and it is not built here because
the panel it feeds shows forty rows and every archive under about half a
gigabyte lists in well under a second.

**Or a container shape these readers get wrong**, which is measured rather than
guessed. Both were pointed at somebody else's corpus before this landed:

| | |
| --- | --- |
| Go's `archive/zip` testdata — thirty deliberately awkward files | all thirty listed; **27 extracted byte-identical to `unzip`**, and the other three `unzip` will not open at all |
| every `.crate` in this machine's cargo cache, which is a real tar.gz each | **1,107 of 1,107 archives, 57,859 files, byte-identical to GNU `tar`** |

Four of those thirty are the reason the zip reader is what it is: a prefixed
zip, a wrong base offset, a wrong directory size and two zip64s. A report that
survives that corpus is a report about a shape none of it holds.

**Or `flate2` leaving `png`'s tree**, at which point deflate stops being free
and the row in [07-dependencies.md](../07-dependencies.md) is a real dependency
to argue rather than a name for something already there.
