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
crates, 401 tests, 60 GUI scenarios and fourteen golden frames, every gate green. What "the least to show" turned out to mean in practice:
the milestone with nothing visible would also have been the milestone with nothing
*measurable*, so the first artifact carries enough of M1 to put a real directory in a
real window — which is what produced the numbers in
[09-performance-budgets.md](09-performance-budgets.md#what-m0-measured) and the four
defects they exposed.

| Done | Owed |
| --- | --- |
| Wayland windowing, `xdg_shell`, decoration negotiation and the client-drawn frame that GNOME requires | **X11**, and the whole of `x11rb` |
| Exact fractional scale through `wp_fractional_scale_v1` + `wp_viewporter`, reconfigured per change, **frame 1 included** — a one-pixel buffer maps the surface first, because the scale arrives at map and not before it | — |
| Keyboard through `xkbcommon`, with key repeat implemented client-side | **Drag-and-drop and IME** — two of the four capabilities `winit` could not reach, and the reason ADR 0002 exists |
| Pointer with counted clicks, coalesced high-resolution scroll, rubber-band selection | Gestures, and the pointer-shape protocol |
| **Clipboard and primary selection**, as a source and as a target, with three of Spike D's four ordering invariants structural, and a paste that pastes **files** | `x-special/gnome-copied-files`, which is what makes a cut arrive in Nautilus as a move rather than a copy — and interop in either direction, which is unverified |
| A frame loop that blocks on the display socket and the pool's eventfd — **0 CPU ticks over 8 s idle** | The sixty GUI scenarios **in** CI: they drive a real nested compositor for the virtual-input protocols, and a runner has no seat to give one |
| `/proc/self/mountinfo` polled for `POLLPRI` on the watcher's own thread, so a disk plugged in reaches the sidebar without a restart | The volume's label and its free space, which are `udisks2` and M7's |
| **GitHub Actions**: `tools/check` verbatim, the golden frames, the declared MSRV, and Spike E's matrix nightly on four filesystems it builds itself | A second runner architecture — the matrix and the frames are both `x86_64` today, and [06-portability.md](06-portability.md) names `aarch64` |
| Eleven golden frames against committed references on lavapipe, **in CI**, pinned to the hash of every font file they were blessed on | A perceptual diff. Two channel levels of 255 is a threshold, not a metric, and it cannot tell a moved glyph from a re-blended one |
| `--bench`, a scripted scroll through the real frame path | The regression gate that fails a build on it (M8). **Deliberately not in CI**: a shared runner's worst frame says more about its neighbours than about this code, and [12-measurement.md](12-measurement.md#the-machine-must-be-idle-and-the-load-must-be-recorded) prices that at 6× |

The two capabilities left are the expensive ones, and Spike D already knows what
each of them costs and which ordering invariants they have to hold. Drag-and-drop
is deliberately after X11: XDND has five actions where Wayland has three, and an
API designed against one dialect would be wrong in a way that is expensive to
correct.

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
| A breadcrumb path bar, `Ctrl+L` to type into it, and a text field that edits like one — selection, word operations, its own undo, and a scope that stops a chord meaning two things at once | M2, M6 | The path bar is already the title bar the client-drawn frame needed, and an inert one is a strip of pixels that looks clickable and is not. Typing a path is how a person goes somewhere they cannot see, and it needs no completion to be worth having — the read fails with a message naming what it looked for, which is the same answer a directory deleted a moment ago gives |
| A mark coloured by the kind of file | M7 | An icon theme is a spec, a parser and a dependency on the user having one installed. Nine hues over the mark that was already there is legible at thirteen pixels, costs 0.012 ms a frame, and is what M7 replaces rather than what it starts from |
| The sidebar, and back / forward / parent / reload | M2 | A file manager without them is a demo. What goes in the sidebar comes from three files every other file manager on the machine already reads, so it cost a parser each and no dependency — and the tree, the drag-to-reorder and the resizable divider that M2 owes are still owed |
| Filter-as-you-type | M4 | A substring over a listing already in memory is thirty lines and the narrowing is what a directory of ten thousand needs before search exists. **Not** search: no fuzzy matching, no index, no recursion — those are M4's and cost what M4 costs |
| New folder, rename, **copy, cut, paste**, Move to Trash over a whole selection, one undo stack over all of it, a counted progress line, a stop, and Copy the text inside | M3 | Rule 3 is one of the eight expensive-to-undo invariants, and every mutation added before the stack exists is a mutation that has to be retrofitted onto it. The engine is Spike E's, ported rather than rewritten. **Not** the whole queue: one operation at a time, no ETA, no pause, no conflict resolution — a name that is taken is refused, which is what every other mutation here does. The XDG Trash is written by hand, so it brought no dependency and it interoperates with every other file manager on the machine |
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
| 5 | **The queue: honest progress and cancel** — done. **An ETA and pause** — not | M3, [rule 7](03-principles.md#7-honest-progress) | The status line counts entries and reports a measured rate; `Ctrl+.` stops a tree copy between files and the partial copy is one undo entry. **No ETA**: a rate needs a total and the total is not known until the walk finds the last file, so what is shown is what has happened. **No pause**: stopping and restarting is not pausing, and a real one needs the job to block |
| 6 | ~~**A paste that pastes files, and Cut**~~ **Mostly done.** `Ctrl+V` pastes what the clipboard names, `Ctrl+X` cuts, both undoable, both on the menus. `Ctrl+V` took over from *Go to the copied folder*, which moved to `Ctrl+Shift+V` | M0 owed | A file copied in Nautilus pastes here — **unverified**, and the reverse needs `x-special/gnome-copied-files` for a cut to arrive as a move rather than a copy. That marker is what is left |
| 7 | **Undo across copy and move** — done — **and a visible history**, which is not | M3, [rule 3](03-principles.md#3-everything-is-reversible) | `Ctrl+Z` walks back every operation: met, and asserted through the window for a file, a cut and a whole folder. Undoing a copy removes only what the copy made, which is why its inverse may be recursive at all |

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
| 12 | **Session persistence**, and the config file that is a serialization of the same registry | M2, [rule 6](03-principles.md#6-state-survives-everything) | Every tab, pane, scroll offset, sort and filter survives `kill -9` |

M2 says the layout model is the hardest thing to retrofit later, which is an
argument for doing it earlier than this. The reason it is not earlier: a split pane
whose two halves cannot copy a file between them is the demo version of the feature,
and the operations engine is what makes the second pane worth having.

### One thing the port left behind

**Spike E's matrix now tests a copy of the engine rather than the engine.** The
code in `sk-fs::copy` is the spike's, ported; the matrix in
`spikes/e-operations` still exercises its own. So *"M3 does not ship until the
Spike E matrix is green in CI"* is now a gate over a fork, and two engines that
agree today can stop agreeing without anything failing.

The spike is standalone on purpose — its own `[workspace]`, exempt from the
layering and dependency rules — so it cannot simply depend on `sk-fs`. The fix is
to make the matrix an integration test over `sk-fs::copy` and keep the
filesystem-discovery half, which is the part that has no equivalent anywhere else.
Until that happens the product's engine is covered by 18 unit tests on tmpfs and
ext4 and by nothing on btrfs, xfs, exFAT, vfat or NTFS.

### Owed, and cheap now that there is somewhere to run them

Three rows claim a gate that does not exist. Each is a job in
[check.yml](../.github/workflows/check.yml) rather than a milestone:

- **A clean-container build**, which is the whole evidence behind *"no C toolchain
  required"* in [07-dependencies.md](07-dependencies.md#budgets-and-enforcement).
- **`cargo-deny`**, which is the whole evidence behind *"permissive licences only,
  rejected at the gate"* in the same table.
- **The sixty GUI scenarios**, which need a compositor that will hand out
  `zwp_virtual_keyboard_v1` on a runner with no seat. The answer is a headless
  backend or nothing; until then a scenario is a person's job before pushing.

### After those, and in this order

**Search** (M4) over the index [Spike F](../spikes/f-index-and-search/README.md) said
go on, then **quick look and the inspector** (M5), then **desktop citizenship** (M7)
— FileManager1, udisks2, portals, the GoTo popup — then **M8**, whose first job is the
performance regression gate that CI deliberately does not have yet.

Two things are further down than their obvious priority, and for the same reason:
**gestures** and **the grid view with thumbnails** are both large, both visible, and
neither serves a rule that is currently unserved. A thumbnail is a decoder, a cache
format and a sandbox — [ADR 0006](adr/0006-out-of-process-decoders.md) — and it is
worth that when the file manager underneath it can already move a file.

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
