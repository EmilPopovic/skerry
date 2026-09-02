# Roadmap

Estimates assume one experienced developer working steadily. They are ratios for ordering
the work, not a schedule.

---

## Phase 0 — spikes, ~3–4 weeks

Throwaway binaries, each answering one question that could invalidate the plan, each with a
number attached. They live in `spikes/` afterwards because their measurements are the
evidence behind the ADRs. **If a spike misses its bar, the architecture changes before
there is a codebase to rewrite.**

### Status

**All six resolved.** Full results in each spike's own README; the summary tables in
[09-performance-budgets.md](09-performance-budgets.md) carry the numbers that became gates.

| | Verdict |
| --- | --- |
| [A](../spikes/a-million-rows/README.md) | **Go.** 2.6 ms worst p99 of 18 runs against an 8 ms bar. RSS misses on the NVIDIA proprietary driver alone, whose floor is 332 MB before our data exists |
| [B](../spikes/b-enumeration/README.md) | **Go.** 0.13 s warm on a laptop, 0.91–1.07 s on a Pi 5 — sits on the 1 s bar rather than inside it. A parallel walk with an unlocked store is 4× the obvious design |
| [C](../spikes/c-watching/README.md) | **Go, three things to design.** Notification 0.063 ms; overflow recovery 411 ms by pruning on directory mtime; the watch budget is per-user, shared, and not a constant |
| [D](../spikes/d-platform-and-dnd/README.md) | **Go, three things to design.** Files land on disk from a drag we start; every Wayland pair passes and two of the peers pass on X11 too; four compositor limitations are documented and three of them report no error at all. `winit` reaches 5 of 9 capabilities, so [ADR 0002](adr/0002-own-platform-layer.md) is Accepted rather than pending |
| [E](../spikes/e-operations/README.md) | **Go.** All three conditions met on seven filesystems: 81 pass, 0 fail, 16 limitations, 5 owed, of 102 checks. `ENOSPC` clean at an exact byte; reflink consumes 0 KiB against 192 MiB. **Four** defects found by a check failing |
| [F](../spikes/f-index-and-search/README.md) | **Go on the index.** 195 ms cold load on a Pi against a 2.3–3.7 s cold walk; search 13.3 ms with a parallel scan. **Sorting misses at 191 ms** |

**Spike F is not one of the original five.** It exists because
[ADR 0008](adr/0008-persistent-index-is-required.md) made the persistent index required
rather than optional and then declined to choose a format without numbers.

### What Phase 0 changed in the plan

The point of a spike is to move the architecture before there is code. Five things moved:

- **[ADR 0008](adr/0008-persistent-index-is-required.md)** — the persistent index is
  required, not an optional accelerator, because Linux has no MFT to read and no
  volume-wide change journal to subscribe to.
- **`sk-fs` needs two walk strategies**, not one. Per-directory jobs are 3× faster on a
  realistic tree; deferring `statx` to the pool is 3.4× faster on a single directory holding
  a million entries, and takes its p99 from 11.9 s to 1.2 s.
- **Pool width is at least `ncpu`**, not `ncpu - 2`. The old figure is 1.626 s against
  0.91 s on a four-core machine.
- **Sort keys as [04-architecture.md](04-architecture.md#data-layout) described them did not
  work.** Precomputing them is worth 3%; the cost is the access pattern, and a radix pass
  that never compares is worth 5×.
- **Two budget rows are known-unmet** and one measures the wrong thing — see
  [09-performance-budgets.md](09-performance-budgets.md) and
  [12-measurement.md](12-measurement.md).
- **`copy_file_range` cannot be used unconditionally.** It does not preserve
  holes, and using it over a whole file turned a 1 GiB sparse file into a gigabyte
  of zeros on ext4 and tmpfs. `sk-fs` copies per data extent, which keeps the
  kernel-side copy and the sparseness at once.
- **`RENAME_NOREPLACE` needs a fallback chain.** ntfs-3g refuses the flag, and an
  engine that relies on it copies nothing at all to an external drive. `link` then
  `unlink` is the atomic fallback; `stat` then `rename` is the last resort and has
  to be reported as such, because it has a race.
- **Three drag-and-drop capabilities are absent with no error attached** — a
  discarded primary selection, an unreported drag action, an undelivered
  `dnd_finished`. `sk-plat` cannot detect any of them, so M3's move operation must
  not be built on `dnd_finished`, and the policy above `sk-plat` has to be correct
  when all three are missing.

### Spike A — a million rows at 120 fps
Virtualized list of 1,000,000 synthetic entries with text, icon and three columns. Built
twice: once on `wgpu` + `swash`, once on GPUI, so the comparison is measured rather than
argued. Run on Wayland and X11; on Intel, AMD, NVIDIA proprietary and llvmpipe; at 100% and
150% scaling.

> **Bar:** p99 frame under 8 ms while scrolling · crisp glyphs at fractional scale · under
> 300 MB RSS · zero CPU at idle.
> **Decides:** the renderer, permanently. Also the first golden-frame harness.

### Spike B — enumeration throughput
Walk a 1M-file tree with `getdents64` plus batched `statx` on a job pool. Compare against
`std::fs::read_dir` to quantify the tax rather than assume it. Cold cache, warm cache, and
an sshfs mount that stops responding halfway through.

> **Bar:** warm-cache full walk under 1 s · first 200 visible rows on screen within 30 ms of
> opening any directory · a hung mount never blocks the UI thread.

### Spike C — watching at scale
inotify across thousands of directories against the per-user watch limit, with a deliberate
`IN_Q_OVERFLOW` and a targeted rescan path. Decide whether `fanotify`'s privilege
requirement is ever worth it (expected answer: no).

> **Bar:** an external `rm -rf` or `git checkout` reflected within 100 ms · no lost entries
> after a forced overflow · the watch budget documented as a number.

### Spike D — platform layer and drag-and-drop
The highest-uncertainty item in the plan, and the one that decides
[ADR 0002](adr/0002-own-platform-layer.md). A minimal Wayland and X11 window that is a
drag-and-drop **source and target**, handles clipboard including primary selection, and
receives `text-input-v3`. Dragged both directions against Nautilus, Dolphin, Firefox,
Chrome and Thunderbird, covering copy, move and link.

> **Bar:** every pair works on both display servers, or the failure is documented as a
> compositor limitation with a fallback · IME composes CJK text into a field.

**Resolved: Go.** Built twice, on `wayland-client` + `x11rb` and on `winit`, so the
revisit trigger is answered rather than argued. Full result in
[d-platform-and-dnd](../spikes/d-platform-and-dnd/README.md). What it changed is above;
what it left owed is Dolphin, a second compositor, and the peer-to-us direction on X11.

### Spike E — operation engine correctness
Reflink, `copy_file_range` and fallback paths across ext4, btrfs, xfs, tmpfs, exFAT and NFS,
with sparse files, hardlinks, symlinks, xattrs, ACLs and a full destination disk. Verify
integrity after each. This spike becomes the permanent torture matrix.

> **Bar:** zero data loss across the matrix · `ENOSPC` handled cleanly mid-copy with no
> partial file left behind · reflink measurably instant on btrfs.

**Resolved: Go.** All three conditions are met, on seven filesystems — ext4, tmpfs, btrfs,
xfs, exFAT, vfat and NTFS, plus five injected faults.
Full result in [e-operations](../spikes/e-operations/README.md). Unlike every other spike
its code stays: the matrix is a CI job today, it discovers what to test rather than being
told, and it ships a filesystem that fails at an exact offset on demand.

---

## Milestones

Each ends in something a real person can use, because a file manager you cannot dogfood
cannot be designed.

### M0 · 6 wk — Platform layer
`sk-plat`: Wayland and X11 windowing, keyboard mapping through xkbcommon, pointer and
gesture input, monitors and fractional scaling, clipboard, drag-and-drop as source and
target, IME, window lifecycle. Plus the CI scaffolding it needs: headless compositor
fixture, input-trace replay, the layering check, the dependency budget.

> The most expensive milestone and the one with the least to show. It is first because
> every later milestone sits on it and because Spike D has already de-risked it.

**In progress. The workspace exists and there is a window on screen.** Twelve
crates and two sandboxed helpers, 674 tests, 113 GUI scenarios on two display servers and fifty-three golden frames, every gate green. What "the least to show" turned out to mean in practice:
the milestone with nothing visible would also have been the milestone with nothing
*measurable*, so the first artifact carries enough of M1 to put a real directory in a
real window — which is what produced the numbers in
[09-performance-budgets.md](09-performance-budgets.md#what-m0-measured) and the four
defects they exposed.

| Done | Owed |
| --- | --- |
| Wayland windowing, `xdg_shell`, decoration negotiation and the client-drawn frame that GNOME requires — **and X11**, behind a runtime-selected seam ([ADR 0009](adr/0009-x11-backend-behind-a-runtime-selected-enum.md)): `x11rb` with `dl-libxcb`, EWMH state, `_MOTIF_WM_HINTS`, a keymap from `_XKB_RULES_NAMES` rather than a linked `libxkbcommon-x11`, selections with `INCR`, and scale from `Xft.dpi` | — |
| Exact fractional scale through `wp_fractional_scale_v1` + `wp_viewporter`, reconfigured per change, **frame 1 included** — a one-pixel buffer maps the surface first, because the scale arrives at map and not before it | — |
| Keyboard through `xkbcommon`, with key repeat implemented client-side **and shared by both backends**, so a held key behaves identically on either; **dead keys and Compose sequences** from the user's own table, on both servers; and **`text-input-v3`** on Wayland — a real input method composes into any of the three fields, with the preedit underlined at the caret and not committed until the method says so. Tested against `skerry-drive --ime`, an input method the harness runs beside the window | **An input method on X11.** The equivalent is XIM, a protocol over properties and `ClientMessage`s, or ibus over D-Bus; neither is written, and the capability is reported absent rather than pretended |
| Pointer with counted clicks, coalesced high-resolution scroll, rubber-band selection, the **pointer-shape protocol** where the compositor has one, and **touchpad gestures**: a three-finger swipe navigates, a pinch zooms the list, and `Ctrl` with a wheel does what the pinch does | Gestures on X11. `XInput` 2.4 has the events and this backend reads core ones; reported as an absent capability rather than worked around |
| **Clipboard and primary selection**, as a source and as a target, with three of Spike D's four ordering invariants structural, and a paste that pastes **files** | Nothing. `x-special/gnome-copied-files` is published and read, so a cut here is a move in Nautilus and a cut there is a move here; and files drag **both ways** between this window and a real GTK application, on both display servers, asserted by two scenarios that run a GTK peer beside the window under test |
| A frame loop that blocks on the display socket and the pool's eventfd — **0 CPU ticks over 8 s idle**, on both servers | — |
| `/proc/self/mountinfo` polled for `POLLPRI` on the watcher's own thread, so a disk plugged in reaches the sidebar without a restart | The volume's label and its free space, which are `udisks2` and M7's |
| **GitHub Actions**: `tools/check` verbatim, the golden frames, the declared MSRV, Spike E's matrix nightly on four filesystems it builds itself, and **`tools/check` on `aarch64`** — the second architecture [06-portability.md](06-portability.md) claims | Spike E's matrix is still `x86_64` only, and so are the frames: the ARM image's fonts are not the ones the references were blessed on |
| Thirty-five golden frames against committed references on lavapipe, **in CI**, pinned to the hash of every font file they were blessed on, compared by **CIE76 ΔE against the just-noticeable difference** — and a second pass over both images blurred by a pixel, which separates a glyph that moved from one that blended differently and says which in the failure | — |
| `--bench`, a scripted scroll through the real frame path | The regression gate that fails a build on it (M8). **Deliberately not in CI**: a shared runner's worst frame says more about its neighbours than about this code, and [12-measurement.md](12-measurement.md#the-machine-must-be-idle-and-the-load-must-be-recorded) prices that at 6× |

The two capabilities left are the expensive ones, and Spike D already knows what
each of them costs and which ordering invariants they have to hold. Drag-and-drop
was deliberately put after X11 — XDND has five actions where Wayland has three,
and an API designed against one dialect would be wrong in a way that is expensive
to correct. Both dialects exist now and drag-and-drop is done: `wl_data_device`
and XDND version 5, source and target, with the bytes travelling through each
server's selection machinery so a directory's worth of URIs goes by `INCR`. A
drag **out** of this window is always a copy, whatever a peer asks for — Spike D
measured the negotiated action reported zero times and `dnd_finished` delivered by
some peers and not others, and deleting an original on a confirmation that may
never arrive is how a drag loses somebody's files.

**The GUI scenarios run on both servers, and the X11 half needs no seat.**
That was the reason they were out of CI: Wayland's virtual-input protocols need a
compositor that will hand out a seat, and a runner has none. `XTEST` under `Xvfb`
needs neither, so the same scenarios — not a second set written against X11 — are
runnable on a plain runner. Running them on a second server immediately found a
harness bug that had been latent since it was written, recorded in
[12-measurement.md](12-measurement.md#a-helper-can-be-correct-only-because-of-the-fixture-it-grew-up-in).

**Ten things arrived ahead of their milestone**, each into the crate the table
in [04-architecture.md](04-architecture.md#crate-responsibilities) already names
for it, because each one is what a person reaches for on the first day of
dogfooding:

| Pulled forward | From | Why now |
| --- | --- | --- |
| `sk-fs::watch` — inotify | M1 | A listing that goes stale is a listing you cannot trust, and Spike C had already resolved the design |
| `sk-cmd` — the command registry | M6 | Rule 2 is one of the eight expensive-to-undo invariants, and its cost grows with every command added before it |
| `sk-desktop` — MIME, `.desktop`, launch | M7 | A file manager that cannot open a file is a directory listing |
| `sk-index::size` — recursive folder totals | M4 | A folder with no size is a folder you have to open to understand |
| A breadcrumb path bar, `Ctrl+L` to type into it, and a text field that edits like one — selection, word operations, its own undo, and a scope that stops a chord meaning two things at once | M2, M6 | One bar per pane, under that pane's tab strip: a pane has a path, a history and a folder to reload, and a window has as many of each as it has panes — a single bar describing whichever pane last took the keyboard leaves the other half of a split a listing with no address. An inert bar is a strip of pixels that looks clickable and is not. Typing a path is how a person goes somewhere they cannot see, and it needs no completion to be worth having — the read fails with a message naming what it looked for, which is the same answer a directory deleted a moment ago gives |
| **Properties**, and **a terminal here** | M5, and the one thing [11-non-goals.md](11-non-goals.md) says instead of an embedded terminal | A panel of what a file *is* is what a person opens when a listing has not answered their question, and it is `statx` and a `readlink` on a job rather than a milestone. The terminal is one command, honouring `$TERMINAL`, and it is the substitute the non-goals list already promised |
| A mark coloured by the kind of file | M7 | An icon theme is a spec, a parser and a dependency on the user having one installed. Nine hues over the mark that was already there is legible at thirteen pixels, costs 0.012 ms a frame, and is what M7 replaces rather than what it starts from |
| The sidebar, and back / forward / parent / reload | M2 | A file manager without them is a demo. What goes in the sidebar comes from three files every other file manager on the machine already reads, so it cost a parser each and no dependency — and the tree, the drag-to-reorder and the resizable divider that M2 owes are still owed |
| Filter-as-you-type | M4 | A substring over a listing already in memory is thirty lines and the narrowing is what a directory of ten thousand needs before search exists. **Not** search: no fuzzy matching, no index, no recursion — those are M4's and cost what M4 costs |
| New folder, rename, **copy, cut, paste, duplicate**, Move to Trash over a whole selection, one undo stack over all of it, a counted progress line, a stop, and Copy the text inside | M3 | Rule 3 is one of the eight expensive-to-undo invariants, and every mutation added before the stack exists is a mutation that has to be retrofitted onto it. The engine is Spike E's, ported rather than rewritten. **Not** the whole queue: one operation at a time, no ETA, no pause, and no conflict *dialog* — but a copy under a taken name now lands beside it as `report (copy).pdf` rather than being refused, because pasting into the folder you copied from is how a person duplicates something and refusing it is a refusal to do the ordinary thing. A **move** under a taken name is still refused and said out loud: two files under names nobody chose is worse than a refusal. The XDG Trash is written by hand, so it brought no dependency and it interoperates with every other file manager on the machine |
| The context menu **and the fuzzy palette**, both built out of the command registry | M6 | Every action reachable by keyboard alone *and* by pointer alone, which is a promise that gets more expensive with every command that arrives without it. `every_command_can_be_reached_without_a_keyboard` is the other half of the test rule 2 already had, and the exemption table it allows names ten commands and the reason a pointer has a better way of doing each |

None of them brought a dependency: every parser is ours, which is what
[07-dependencies.md](07-dependencies.md) said it would cost and what it did cost.

### M1 · 4 wk — Walking skeleton
One window, one pane, Details view over the columnar store. Navigation, sorting, selection,
rubber-band, scrolling, icons from the real theme, inotify refresh. Accessibility tree and
`--script` headless mode wired in now, not later. No mutations yet — read-only, and already
faster than everything installed.

> **Dogfood test:** browse `/usr`, `~/Downloads` and a large git checkout for a day without
> reaching for Nautilus.

### M2 · 4 wk — Panes, tabs, views
The split tree: nested horizontal and vertical panes, tabs, drag-to-rearrange with snap
overlays, middle-click to split. Grid with thumbnails, inline Tree, List columns, Group By.
Per-folder view settings with subtree inheritance. Full session persistence.

> Ships the layout model, which is the hardest thing to retrofit later.

**The layout model landed early**, in M0, because the cost of waiting was
measurable and rising: `App` held one `Pane` and named it 129 times, and every
feature added before the tree added more.
[ADR 0010](adr/0010-a-split-tree-of-tab-strips.md) decides the shape — a binary
split tree whose leaves are tab strips — and what exists is the tree, a tab strip
that is always there with a `+` at the end of its tabs, the divider drag,
**drag-to-rearrange with snap overlays** — a tab dragged to a pane's edge splits
it and dragged into a strip joins it, with the landing place shaded before the
button comes up — `Ctrl+T` / `Ctrl+W` / `Ctrl+Shift+T` / `Ctrl+Tab`, `F3` /
`Shift+F3` / `Ctrl+F3` / `F6`, and a watcher that watches every visible pane
rather than one directory. **Session persistence landed too**, in
[ADR 0011](adr/0011-one-plain-text-state-file-written-atomically.md)'s terms: the
tree serializes to the recursive form ADR 0010 chose the shape for, and the window
comes back after `kill -9` with its splits, its tabs, its sorts and its scroll
offsets. **A second view mode landed as well** — a Grid, behind the cell geometry
[ADR 0013](adr/0013-one-cell-geometry-behind-every-uniform-view.md) put under
every uniform view, with the mode saved per tab.

**Group By landed too**, as ADR 0013 said it should: over the cell geometry
rather than inside it, so the same headings work in both views, and as a sort
prefix rather than a second pass, so each band is ordered by whatever the pane is
sorted by. All three quantities: **by time, by size and by kind**, the last of
them banding on `sk_desktop::Category` — the same taxonomy that colours a row's
mark, so a heading and the marks under it cannot disagree.

**Per-folder view settings landed with subtree inheritance**, in a file beside
the session: the mode and the grouping are a decision about a folder rather than
about a window, so they are keyed by path and a folder with no record of its own
takes the nearest ancestor's.

**The Details columns have a ceiling and boundaries that drag.** The name column
took whatever was left over, which on a 2,878-pixel window was two thousand of
them with a file's size at the far right of the screen; it stops at a width the
person sets by dragging the boundary in the header, and the two metadata columns
then pack against it rather than against the pane's edge. **The boundaries between the
columns are drawn**, because eight logical pixels of invisible grab band is a
feature only somebody who was told about it can use — which is [rule
4](03-principles.md). And the list is a table with an edge: the row fill and the
hit test read one value, so a selected row stops where the columns stop and a
press past them clears the selection the way a press below the last row does. The
scrollbar stays at the pane's right edge, where a viewport's scrollbar goes, and
the eleven pixels it needs are **reserved out of the columns** — the grid always
did that, Details did not, and the difference was a thumb drawn across the focus
ring of every selected row. The widths are the
window's rather than the folder's — a width is a decision about how much of a
name somebody wants to read — and they live in the session file beside the tabs
and the splits, on a line an older file simply does not have.

**Each pane's bar carries a view switcher**, at the right of it, one small mark
per mode with the current one filled — because a mode reached only by `Ctrl+1` or
a menu row is a mode most people never find, which is [rule
4](03-principles.md). The buttons run the same registry commands the chords do,
and they shed after the navigation buttons: a bar with room for one group keeps
the one that moves rather than the one that looks.

**The column view is built**, and the switcher's third mark is it. One column per
level of the path, the pane's own rightmost, each ancestor showing which of its
entries the next column came from, and a press in one of them navigating.

The wheel scrolls the column under the pointer, ancestors included, and each of
them has an extent, an easing and a fling of its own. `Shift` and the wheel move
along the run, which is how every other program is asked to go sideways and the
only way to reach a deep trail with a mouse; a touchpad's horizontal delta —
dropped everywhere until there was a mode that could use it — does the same. The
pane's own column is brought into view when a column appears or goes and **not on
every frame**, so scrolling back to look at an ancestor stays where it was put.

**A folder's record says Details or Grid, and cannot say columns.** The record is
how a folder's *contents* are laid out; a column view shows a path, so "this folder
is a column view" is a statement about how somebody was moving around rather than
about what is in the folder — and under
[subtree inheritance](04-architecture.md) it is worse than meaningless. Recorded
against whatever folder a pane happened to be standing in, it was inherited by
every directory beneath, so setting one destination back to a details list and
stepping out put the whole subtree into columns.

`views::Layout` is that sentence with a type: two variants, and a pane in columns
records nothing at all. A trail also declines a record on arrival, because the
columns are the path and every directory on it is a step rather than a
destination — while arriving from a details list still applies one, since that
*is* coming to look at a folder.

The shape is [ADR 0015](adr/0015-a-column-is-a-directory-and-a-choice.md)'s, and
it is the second answer to the question:
[0014](adr/0014-a-column-view-is-a-trail-of-panes.md) said a column *is* a `Pane`
and was wrong, because a pane is an independent view and columns are one path
decomposed — the relation between two neighbours is the feature, and a `Vec<Pane>`
cannot hold it. What a read is about turns out to be neither: a `Level` is a
directory, a scroll and a chosen entry, with no sort, no mode and no selection to
get out of step. The trail is a function of the path, so one method keeps it equal
to the ancestors and every navigation goes through it — a click in a column, a
breadcrumb, `Alt ←`, a restored session. A level fetches no metadata, so eight
columns cost eight enumerations and no `statx` fan-out.

What 0015 predicted it would cost — moving `Pane`'s own fields behind a
`Directory` — did not happen, and that belongs beside the decision rather than
inside it: it would have renamed a hundred and sixty-nine `pane.listing.rows()`
call sites with no reader any the wiser. The guarantee the extraction existed for
is one read and one place a reply lands, and `pane::spawn_read` and
`Panes::take_reply` are those.

**Still owed here:** middle-click to split, the **List** columns — and the inline
**Tree**,
which ADR 0013 explicitly does not reach: it is not a listing laid out
differently but a listing whose rows come from several directories at varying
depth, with an expansion state nothing models yet.

### M3 · 5 wk — Operations and undo
Copy, move, rename, create, trash, permanent delete on the Spike E engine, behind a queue
with honest progress, conflict resolution, pause, cancel and crash resume. One undo stack
across all of it with visible history. Clipboard interop with GTK and Qt applications.
Interactive batch rename.

> Ships rule 3 — the thing File Pilot still does not have.
> **Gate:** M3 does not ship until the Spike E matrix is green in CI.

### M4 · 3 wk — Search and flattened views
Recursive flattening of a subtree or a whole filesystem, fuzzy filtering as you type,
extension facets, ignore rules, sort by recursive size with live computation. Two scopes:
global, and current-folder-with-subfolders.

> **Bar:** first results within 50 ms of the first keystroke; results keep arriving without
> ever stalling a frame.

### M5 · 3 wk — Inspector and quick look
`Space` quick look and a docked inspector: text with syntax highlighting, images with EXIF
orientation and zoom, **folder listings that are clickable and navigable**, archive
contents, media metadata, hex. Decoders sandboxed in helpers. Thumbnails to the XDG shared
cache.

> Includes the PDF preview File Pilot lacks, via `skerry-preview-doc`.

**Started, and the spine of it is in.** Two helpers under
[ADR 0006](adr/0006-out-of-process-decoders.md)'s sandbox — `skerry-thumbnailer`
for PNG, JPEG and SVG in pure Rust, `skerry-preview-doc` for a PDF's first page
through whatever renderer is on `PATH` — talking to a host that decodes nothing,
over the protocol
[ADR 0017](adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)
settles. Thumbnails go to the XDG shared cache, so one Nautilus wrote is one this
reads and the other way round, and they are drawn from a slot atlas
([ADR 0018](adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)) in
Details and in the grid. `Space` opens a quick look at a picture, a page or a text
file; `Space` or `Escape` closes it.

**The product's dependency count did not move**, which is ADR 0006's whole
argument: a helper is a separate binary and `tools/dep-budget.py` measures the
product's tree. Still 32.

Owed here: **syntax highlighting**, whose seam is written and whose highlighter is
not — every line is already cut into runs carrying a tone, there is one tone, and
adding real ones replaces one function. Then EXIF orientation, zoom, a *docked*
inspector rather than an overlay, archive contents, media metadata, hex, and
navigable folder listings. `seccomp` is named by ADR 0006 and is not written; the
child's own report line says so.

### M6 · 3 wk — Command palette and keymap
The registry made visible: fuzzy palette over every action and setting, inline hotkey
binding, key sequences and chords, aliases, contextual shortcut hints, an optional vim
layer, and a config file that is a serialization of the same registry with hot reload.
Searchable and pinnable context menu, reading desktop-entry actions and KDE service menus so
third-party integrations work without a plugin API.

> Ships rule 4 — discoverability, the thing Dolphin never solved.

### M7 · 4 wk — Desktop citizenship
FileManager1, default-handler registration, single instance, udisks2 devices with mount and
eject, the GoTo popup over bookmarks/recents/system paths, network mount behaviour, portal
integration, the settings surface, theming and DPI, localization catalogues.

> **Bar:** a fresh install feels native on GNOME, on KDE, and in a bare Sway session with no
> systemd.

### M8 · 4 wk — Hardening and 1.0
Performance regression CI with the budgets as failing tests, fuzzing of every parser, the
full filesystem matrix nightly, the forced-degradation matrix from
[06-portability.md](06-portability.md), crash reporting that is off by default, packaging
for both architectures.

---

## The next twelve things, in order

The work below does not run down the milestone list, and neither did the last two
months: **ten things have already arrived ahead of their milestone**, each because it
was what a person reached for on the first day of dogfooding. This is the same
argument continued with one rule added — *a step that makes the next step cheaper
comes first* — and one observation that decides most of the ordering: **the product
cannot copy a file.** New folder, rename and Move to Trash are in; copy and move are
not, so the single largest gap between this and a file manager is one milestone's
worth of work that Spike E has already proved out on seven filesystems.

Every row names the milestone it belongs to, so the table above stays the map. Sizes
are ratios for ordering, not a schedule.

### First — three that are cheap and close a red cell each

| | What | From | Bar |
| --- | --- | --- | --- |
| 1 | ~~**An MSD string radix over the name arena.**~~ **Done, and the row is still missed.** 1,460 ms → **166.0 ms** at 973,759 entries, 8.8×, against a 120 ms target. The remaining gap is parallelism, which needs an API that hands buckets to a caller above `sk-model` | M0 budget | Sort 1M under 120 ms — **not met**. The radix agrees with the comparison order it replaces over every sort a person can ask for, which is met and is a test |
| 2 | ~~**The command palette.**~~ **Done.** A fuzzy filter over the one registry, in the text field that already existed, drawing the list the keyboard overlay already drew. `Ctrl+Shift+P` or `Ctrl+P`, and a row on the background menu so a pointer alone reaches it | M6 | Every registry entry reachable in three keystrokes — met, and the ranking is tested against the case that broke the first version. Pointer-reachable to the last of forty-one results, which is what the wheel is for |
| 3 | ~~**The 1×1 pre-map probe**~~ **Done.** The scale is known before the first frame instead of after it | M0 | Frame 1 drawn at the final scale — met, and asserted by a GUI scenario against the scale the compositor reports, at 1, 1.25, 1.5 and 2. A golden frame could not have proved it: a capture is told its scale |

The palette was out of order on purpose. It is
[rule 4](03-principles.md#4-discoverable-not-just-configurable)'s headline feature
and it cost a fuzzy matcher, because `sk-cmd`'s registry, `Field` and the overlay
were all built before it. Every command added after it is discoverable for free;
every command added before it was not.

What it cost that was not planned: a **greedy** subsequence matcher ranks
*Show the sidebar* above *Show hidden files* for `hid`, because it takes the `h`
inside *Show* and earns no word-start bonus. That is a dynamic program, not a
heuristic, and it is forty lines. And the palette needed the **wheel**, because
eleven of forty-one results are drawn and a pointer with no keyboard could
otherwise reach only the first eleven — which is where `skerry-drive`'s scroll
turned out never to have worked, and the scenario that covered it turned out never
to have run.

### Then — the operations engine, which is the actual gap

| | What | From | Bar |
| --- | --- | --- | --- |
| 4 | ~~**Copy, move and permanent delete**~~ **Done.** Spike E's engine is `sk-fs::copy`: reflink, `copy_file_range` per extent, a hole-preserving floor, temp-then-rename, the three-step placement chain, recursive trees, symlinks recreated, and cancellation | M3 | Per-extent `copy_file_range` and the placement chain survive as tests rather than being rediscovered — met. The sparse test was watched failing against the naive loop: **131,072 blocks against 32** |
| 5 | ~~**The queue: honest progress, cancel, an ETA and pause**~~ **Done, and conflict resolution with it.** A walk runs beside the copy and counts what there is, so the line adds a rate and then a time left, and the status strip's divider becomes a determinate bar. `sk_util::Pause` is a latch the job waits on between files, which is where a cancel also lands. A taken name stops the queue and asks, with both files' sizes and dates | M3, [rule 7](03-principles.md#7-honest-progress) | Real throughput, a real ETA and per-file conflict resolution — met. **Replace trashes what it replaces**, as one `Change::Batch`, because [rule 3](03-principles.md#3-everything-is-reversible) does not let anything here destroy a file; where the trash refuses, the answer fails and says so. The one clause still unmet is *resumable*: a copy stopped part way is recorded as a change rather than as something to pick up |
| 6 | ~~**A paste that pastes files, and Cut**~~ **Done.** `Ctrl+V` pastes what the clipboard names, `Ctrl+X` cuts, both undoable, both on the menus. `Ctrl+V` took over from *Go to the copied folder*, which moved to `Ctrl+Shift+V` | M0 owed | `x-special/gnome-copied-files` is published *and* read, so a peer's verb wins: a scenario has `wl-copy` own the selection with a `cut` in it and asserts the original is gone and `Ctrl+Z` brings it back. Watched failing against a paste that ignored the verb. What is still owed is narrower than the row used to say: the bytes are the documented format rather than Nautilus's own, because PyGObject cannot offer custom clipboard targets — `set_with_data` and `set_with_owner` are both unbound — so the GTK peer that covers drag-and-drop cannot cover this |
| 7 | ~~**Undo across copy and move**~~ **and ~~a visible history~~ — both done.** A strip above the status line, collapsed to its header until somebody expands it: one row per edit oldest first, the present marked, the undone future still listed in grey, and a press on any row walking to the point *after* that edit — N single undos or redos, each the same inverse `Ctrl+Z` uses | M3, [rule 3](03-principles.md#3-everything-is-reversible) | `Ctrl+Z` walks back every operation: met, and asserted through the window for a file, a cut and a whole folder. Undoing a copy removes only what the copy made, which is why its inverse may be recursive at all |

Four and five were one decision apart and were not split: a copy with no progress
is a frozen window on a 4 GiB file, and [rule 1](03-principles.md#1-never-block-the-frame)
means the engine had to be on the pool from its first commit rather than moved
there afterwards.

**A cancelled copy is still a change**, which is the part that took the thinking.
It stopped part way and left what it wrote, and the only thing that can take that
back is the undo stack — so it is recorded as a copy rather than reported as a
failure with files on disk that nothing describes. This is also the group that makes the *worst frame in the
product* worth fixing — 3.5–6.1 ms today, in the window where a 200,000-entry read
lands, and [09-performance-budgets.md](09-performance-budgets.md#what-m0-measured)
already names it as the frame to fix when the queue makes a read concurrent with an
operation.

### Then — the M0 tail, in this order and not another

| | What | From | Bar |
| --- | --- | --- | --- |
| 8 | **X11, through `x11rb`** — window, keyboard through `xkbcommon-x11`, pointer, clipboard, scaling | M0 | Everything the Wayland backend does, at the same budgets, with the dependency ratchet in [07-dependencies.md](07-dependencies.md#budgets-and-enforcement) raised in the same commit |
| 9 | **Drag-and-drop, as source and as target, on both display servers** | M0 | Spike D's pairs work or the failure is a documented compositor limitation with a fallback |
| 10 | **IME through `text-input-v3`** | M0 | CJK composes into all three fields — filter, rename and the path bar |

**Drag-and-drop is third of the three, and that is a constraint rather than a
preference.** It sits behind X11 because XDND has five actions where Wayland has
three, and an API designed against one dialect is wrong in a way that costs weeks
to correct. It sits behind the operations engine because a drop that lands on a
directory has to *copy or move something*, and until item 4 exists a drag is a
gesture with nothing at the end of it. Its own design constraint is already
recorded: three capabilities are [absent with no error attached](../spikes/d-platform-and-dnd/README.md#what-this-changed),
so no policy above `sk-plat` may depend on `dnd_finished` arriving.

### Then — the layout model, because it is the one that cannot be retrofitted

| | What | From | Bar |
| --- | --- | --- | --- |
| 11 | **Tabs, the split-pane tree, and per-folder view settings** | M2 | A pane tree of any shape survives a resize and a scale change with no reflow above 8 ms |
| 12 | ~~**Session persistence**~~ **Done**, decided by [ADR 0011](adr/0011-one-plain-text-state-file-written-atomically.md); the **config file** that is a serialization of the same registry is M6's and is not | M2, [rule 6](03-principles.md#6-state-survives-everything) | Every tab, pane, scroll offset and sort survives `kill -9` — met, and the scenario uses that signal rather than a clean exit, which writes the file on its way out. **The filter and the selection are deliberately not saved**, and the ADR says why rather than leaving it looking like an oversight |

M2 says the layout model is the hardest thing to retrofit later, which is an
argument for doing it earlier than this. The reason it is not earlier: a split pane
whose two halves cannot copy a file between them is the demo version of the feature,
and the operations engine is what makes the second pane worth having.

### ~~One thing the port left behind~~ Closed

**Spike E's matrix tested a copy of the engine rather than the engine**, so
*"M3 does not ship until the Spike E matrix is green in CI"* was a gate over a
fork. It now takes `sk-fs` as a path dependency and its own `copy.rs` is deleted:
being a standalone `[workspace]` was never the obstacle this section assumed,
because a path dependency resolves workspace inheritance from its own directory.

The port paid for itself immediately. `sk-fs::copy` gained the three things the
matrix needs and the product does not — `reflink`, `kernel_copy` and `fsync`
switches, so the floor can be forced and a clone compared against a true byte
copy, and a `refused` list, which is *the only way to tell "this filesystem
cannot" from "we never asked"*. It also gained a `ProblemKind::NameRefused`:
`EILSEQ` from a destination that will not encode a name — which is what copying to
a FAT stick does — was reported as `Unreadable`, whose advice is to check
permissions the person already has.

And the drift the fork was going to cause showed up inside the port: the matrix's
own symlink walker kept a second errno-to-kind table, classified an `EILSEQ` as
unreadable where the engine called it `NameRefused`, and turned a limitation into
a failure. `sk_fs::copy::problem_for` is now the one table.

**37 pass, 0 fail, 6 limitations, 3 owed** over tmpfs, ext4 and NTFS on this
machine, plus four injected faults; the four loop-device filesystems are
[torture.yml](../.github/workflows/torture.yml)'s.

### Owed, and cheap now that there is somewhere to run them

~~Three rows claim a gate that does not exist.~~ **All three are closed**, each a
job in [check.yml](../.github/workflows/check.yml) rather than a milestone:

- ~~**A clean-container build**~~ **Done**, and not as a container: `rustc` drives
  linking through `cc`, so an image with no compiler fails at the link step and says
  nothing about whether a dependency compiled C. `tools/no-cc` asks the question
  that can be answered — the `cc` crate honours `$CC` and the linker does not — and
  the `no-c-toolchain` job runs it on a cold target directory.
- ~~**`cargo-deny`**~~ **Done**: the `supply` job runs `licenses`, `bans`, `sources`
  and `advisories`, the last of which needs the network CI has and a person does
  not. `tools/check` runs the offline half where the tool is installed.
- ~~**The sixty GUI scenarios**~~ **Done**, and by the X11 backend rather than by
  a headless compositor: `XTEST` under `Xvfb` needs no seat, so
  [check.yml](../.github/workflows/check.yml)'s `scenarios` job runs every one of
  them at 100% and at 125%. What that job was quietly not getting is a *reliable* answer —
  one or two scenarios per sweep failed on timing, and
  [12-measurement.md](12-measurement.md#a-suite-is-a-load-test-of-whatever-else-is-running)
  records both the four defects and the wrong conclusion that had excused them.
  The Wayland half stays local, because that one does need the seat.

### After those, and in this order

**Search over the index** (M4) [Spike F](../spikes/f-index-and-search/README.md) said
go on. **One subtree is searchable now** — `Ctrl+Shift+F`, a walk on the pool,
results arriving live with the folder each is in, first result at **0.7 ms over
817,288 entries** against a 50 ms bar
([09](09-performance-budgets.md#what-first-search-result-measures-and-what-it-does-not)).
What that scope does *not* need is the index, which is exactly why it exists
first; the whole filesystem does, and the index is still not built. **The
flattened view is done**: `Ctrl+Enter` — or the label at the end of the search's
summary line — puts every result into the pane as rows, and then they are rows.
Selecting, copying, dragging, trashing and undoing all work on them with no
second implementation, which is what [rule 8](03-principles.md) asks for.

What made it a bigger change than it looks was the sentence above: every
`path.join(name)` assumed one directory behind it, at ten call sites. A `Listing`
now carries an optional home per entry — empty for a directory read, so a
million-row folder pays nothing for a column it never reads — and `Pane::path_of`
is the one place that joins a name to a directory. A scenario trashes one of two
results that share a name, finds it gone from the folder it actually lived in,
and undoes it back there; watched failing against a `path_of` that used the
pane's own path, which removed neither file.

Extension facets and ignore rules are untouched.

Then the **rest of** quick look and the inspector (M5 — the two helpers, the
atlas, the cache and the three previews are in; syntax highlighting, EXIF, zoom
and the docked form are not), then **desktop citizenship** (M7)
— FileManager1, udisks2, portals, the GoTo popup — then **M8**, whose first job is the
performance regression gate that CI deliberately does not have yet.

**Gestures** are further down than their obvious priority, and **the grid view
with thumbnails** was there beside them: both large, both visible, neither serving
a rule that was unserved. A thumbnail is a decoder, a cache format and a sandbox —
[ADR 0006](adr/0006-out-of-process-decoders.md) — and it is worth that when the file
manager underneath it can already move a file. Which it now can, so that half is
built: the grid landed in M0 and the pictures in it are M5's. Gestures are still
where they were, and still for the same reason.

## What comes next

Spike A returned its number: the differentiator is reachable on this stack, with 3× of
headroom on the frame budget and the GPU consuming 0.02 ms of an 8 ms frame. The plan above
is sound and the rest is ordinary engineering.

**Phase 0 is over and `crates/` exists.** M0 is in progress; what is done and what
is owed is in the M0 section above.

All six spikes are resolved and none of them said no. What they changed is listed above:
eight architectural moves, two known-unmet budget rows, and a set of invariants that M0
and M3 have to hold rather than rediscover.

Two of those are worth repeating here because they are the ones a milestone can quietly
skip. M0 owes four ordering invariants from
[Spike D](../spikes/d-platform-and-dnd/README.md#what-this-changed) — devices before the
first commit, the offer surviving `leave`, a fresh serial per selection, a source that
keeps dispatching after the drop — each of which was a silent failure that looked like a
compositor bug. M3 owes the copy engine's two, from
[Spike E](../spikes/e-operations/README.md#four-things-this-changed-three-of-them-in-our-own-code):
`copy_file_range` per extent, and a placement fallback chain. Both sets are already
written as tests; what M0 and M3 have to do is not delete them.

Six measurements are still owed. One of them blocked a budget row rather than a
milestone; the sort row is now 8.8× better and still missed by 1.4×, and what it
needs next is threads rather than a better key — see
[09-performance-budgets.md](09-performance-budgets.md#what-m0-measured).

- **An Intel integrated GPU** for Spike A. Absent from every machine available, and the
  most common Linux GPU.
- **Network mounts.** Every enumeration and watching figure is local ext4 on NVMe. inotify
  does not work over NFS at all — no event, no error — and a listing that shows a stale
  network mount as live has the defect rule 1 exists to prevent.
- **A slow device.** Every rescan and cold-load figure is NVMe. A Pi booting from an SD
  card is the common case and is unmeasured.
- **A second compositor** for Spike D. Three of its four limitations are Hyprland's
  rather than Wayland's, and mutter and kwin would say which.
- **Dolphin**, for the Qt half of rule 5. Konsole and Qt Designer stand in for it today,
  and KIO's drop handling is its own dialect.
- **NFS**, the one filesystem in Spike E's list still unmeasured: `exportfs` is not installed
  on any machine here, and its failure modes are unlike the others — no hardlink guarantees
  worth trusting, and a server that can disappear mid-copy.
