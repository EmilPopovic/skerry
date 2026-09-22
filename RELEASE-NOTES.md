# Skerry 0.1.0-alpha.1

The first public pre-release. It is a file manager you can use for a day, and it
is an alpha: the list of what it does not do is as much a part of this document
as the list of what it does.

The engineering record behind these claims — what was measured, on what, and what
was skipped — is [13-release-readiness.md](docs/13-release-readiness.md).

## What works

A Details list, a grid and a column view over a real directory at a **0.34 ms
frame p99 with 200,000 entries**, live under inotify. Tabs and a split tree of
panes, each pane with its own tab strip, history and breadcrumbs. Copy, cut,
paste, duplicate, rename, new folder and Move to Trash with **one undo stack over
all of it** and a visible history strip. Search under a folder — first result at
**0.7 ms over 817,288 entries** — or over every local filesystem. Quick look at
pictures, PDFs with selectable text, source with a highlighter, markdown
rendered, and the inside of a zip or a tar. Drag and drop with GTK applications.
A command palette and a context menu over one registry, a settings panel that
says where each value came from, and a keymap that is a file you own.

## Compatibility

- **x86-64 and ARM64 Linux.** The tarballs are built in CI for both.
- **glibc 2.39 or newer** for the published binaries. This is the alpha's real
  floor, measured from the artifacts and recorded in each tarball's `BUILD-INFO`.
  It excludes Debian 12 and Ubuntu 22.04. The 2.28 target in
  [06-portability.md](docs/06-portability.md) needs an older build baseline and
  its own runtime testing; neither has happened, so no 2.28 claim is made here.
  Building from source on an older distribution is unaffected.
- **Wayland and X11**, chosen at runtime. Tested against headless Sway and Xvfb
  at 100% and 125% scale.
- **Rust 1.90** is the declared minimum to build; 1.96.1 is the CI pin.

## Recovery behaviour, and what to expect when something fails

These are the parts most worth knowing before you trust the alpha with real work.

- **Move to Trash** picks the trash on the file's own filesystem — the home trash
  when it is the same mount, otherwise a checked `.Trash/$uid` or `.Trash-$uid` on
  the volume ([ADR 0035](docs/adr/0035-trash-recovery-uses-filesystem-local-records.md)).
  When no safe trash exists the file **stays where it is** and the error names it.
  Nothing is ever deleted as a substitute for trashing.
- **Trash is browsable.** Each trash is a sidebar place, and Restore returns an
  item to where it came from and is itself undoable. There is no Empty Trash yet.
- **A failed or warned operation keeps the rest of the queue**
  ([ADR 0036](docs/adr/0036-operation-failures-retain-pending-work.md)). A
  metadata-only warning — an xattr that a FAT stick cannot hold — does not stop
  later files. A hard failure stops and offers retry, skip or discard; the work
  already done stays on the undo stack. The retained queue is in memory and is
  not replayed after a restart.
- **The layout survives `kill -9`**, to the last checkpoint. A panic also writes a
  diagnostic file under the state directory
  ([ADR 0041](docs/adr/0041-panic-diagnostics-use-a-preopened-local-log.md)); it may
  name private paths and is never uploaded.
- **A stale listing says so.** When a watch is refused or the filesystem delivers
  no events — NFS and CIFS do not — the pane says why and offers a refresh rather
  than looking merely quiet
  ([ADR 0037](docs/adr/0037-runtime-degradation-is-visible-and-injectable.md)).
- **A missing helper draws a fallback and names itself.** Every decoder request
  gets a terminal answer instead of a cell that stays blank forever.

## What this alpha does not do

The README carries the [full list](README.md#what-this-alpha-does-not-do). The
ones most likely to be reached for in the first week:

- **No icon themes.** An application icon ships and the desktop entry names it,
  but nothing is resolved out of `index.theme`: a file's mark is a built-in glyph
  coloured by MIME category, not the themed icon your desktop would draw.
- **Creating things is thin.** New folder and single rename only — no new file,
  Templates, symlinks or batch rename.
- **Properties is read-only**, and owners are shown as numeric ids.
- **Rebinding is by editing the config file.** No in-app rebinding UI, no hot
  reload, no key sequences or aliases.
- **English only.** The catalogue is keyed and compiled in; there is no locale
  lookup, and the plural rule is English's.
- **Accessibility is partial.** The tree covers the focused pane; Table, Selection
  and Action interfaces, the tab strip and toolbar buttons are not in it, and no
  Orca pass over a real task has happened.
- **Clipboard interoperates with GNOME and KDE**, including KDE's cut convention
  ([ADR 0039](docs/adr/0039-file-clipboard-intent-belongs-to-one-offer.md)),
  tested against a real GTK peer and a real Qt peer rather than against Nautilus
  and Dolphin themselves. Portal file transfers are not implemented, so a
  sandboxed Flatpak peer receives nothing.
- **No middle-click open-in-tab and no PRIMARY paste.** An `Alt`-drag asks
  Move, Copy or Link only where Skerry takes the drop, and nothing drops onto an
  executable.
- **"Always open with this" writes the association and opens nothing**, and a
  desktop notification carries no buttons — a button needs `ActionInvoked`, which
  is a signal, and there is no subscribing bus connection. No recents and no path
  completion.
- **Search has no ignore rules**, so it walks `.git`, and no extension facets.
- **Four performance budget rows are missed** and recorded with their numbers
  rather than widened: cold start, the 1M sort, one scroll run in five at a
  million entries, and the resident floor.

## Testing, and what is claimed for it

996 unit and integration tests, 151 GUI scenarios on both display servers, 86
golden frames on llvmpipe, and a nightly filesystem torture matrix over ext4,
btrfs, XFS and FAT with injected faults.

Two claims are deliberately narrower than they may look:

- **Fuzzing is bounded mutation fuzzing over tar, ZIP and the D-Bus wire**
  ([ADR 0040](docs/adr/0040-bounded-parser-mutation-campaigns.md)), not
  coverage-guided fuzzing, and it does not yet cover the markdown, `.desktop`,
  MIME or keymap parsers.
- **`--bench` reports measurements; it is not a threshold gate.** Interactive
  performance numbers come from a controlled machine, never from shared CI.

Not covered by automated evidence: NTFS and NFS, the GUI hung-mount case, real
hardware GPUs, removable devices, and native ARM golden frames.

## Reporting problems

Security issues go through [SECURITY.md](SECURITY.md), which also documents what
the helper sandbox does and does not isolate — notably that helpers can read
ordinary user-readable files, and that without Landlock the filesystem isolation
is weaker. Everything else is a GitHub issue.
