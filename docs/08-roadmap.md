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
| [E](../spikes/e-operations/README.md) | **Go.** All three conditions met on seven filesystems: 81 pass, 0 fail, 16 limitations, 5 owed, of 102 checks. `ENOSPC` clean at an exact byte; reflink consumes 0 KiB against 192 MiB. **Eight** defects found by a check failing |
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
crates, 310 tests, every gate green. What "the least to show" turned out to mean in practice:
the milestone with nothing visible would also have been the milestone with nothing
*measurable*, so the first artifact carries enough of M1 to put a real directory in a
real window — which is what produced the numbers in
[09-performance-budgets.md](09-performance-budgets.md#what-m0-measured) and the four
defects they exposed.

| Done | Owed |
| --- | --- |
| Wayland windowing, `xdg_shell`, decoration negotiation and the client-drawn frame that GNOME requires | **X11**, and the whole of `x11rb` |
| Exact fractional scale through `wp_fractional_scale_v1` + `wp_viewporter`, reconfigured per change | The 1×1 pre-map probe that would make **frame 1** right and not just frame 2 |
| Keyboard through `xkbcommon`, with key repeat implemented client-side | **Drag-and-drop and IME** — two of the four capabilities `winit` could not reach, and the reason ADR 0002 exists |
| Pointer with counted clicks, coalesced high-resolution scroll, rubber-band selection | Gestures, and the pointer-shape protocol |
| **Clipboard and primary selection**, as a source and as a target, with three of Spike D's four ordering invariants structural | A paste that pastes *files*, which needs M3's operation engine |
| A frame loop that blocks on the display socket and the pool's eventfd — **0 CPU ticks over 8 s idle** | A headless compositor fixture and input-trace replay in CI |
| Offscreen rendering, PNG capture, and a comparable glyph stream — the golden-frame harness minus its references | The reference images, the perceptual diff, and llvmpipe in CI |
| `--bench`, a scripted scroll through the real frame path | The regression gate that fails a build on it (M8) |

The two capabilities left are the expensive ones, and Spike D already knows what
each of them costs and which ordering invariants they have to hold. Drag-and-drop
is deliberately after X11: XDND has five actions where Wayland has three, and an
API designed against one dialect would be wrong in a way that is expensive to
correct.

**Eight things arrived ahead of their milestone**, each into the crate the table
in [04-architecture.md](04-architecture.md#crate-responsibilities) already names
for it, because each one is what a person reaches for on the first day of
dogfooding:

| Pulled forward | From | Why now |
| --- | --- | --- |
| `sk-fs::watch` — inotify | M1 | A listing that goes stale is a listing you cannot trust, and Spike C had already resolved the design |
| `sk-cmd` — the command registry | M6 | Rule 2 is one of the eight expensive-to-undo invariants, and its cost grows with every command added before it |
| `sk-desktop` — MIME, `.desktop`, launch | M7 | A file manager that cannot open a file is a directory listing |
| `sk-index::size` — recursive folder totals | M4 | A folder with no size is a folder you have to open to understand |
| A breadcrumb path bar | M2 | The path bar is already the title bar the client-drawn frame needed, and an inert one is a strip of pixels that looks clickable and is not |
| A mark coloured by the kind of file | M7 | An icon theme is a spec, a parser and a dependency on the user having one installed. Nine hues over the mark that was already there is legible at thirteen pixels, costs 0.012 ms a frame, and is what M7 replaces rather than what it starts from |
| The sidebar, and back / forward / parent / reload | M2 | A file manager without them is a demo. What goes in the sidebar comes from three files every other file manager on the machine already reads, so it cost a parser each and no dependency — and the tree, the drag-to-reorder and the resizable divider that M2 owes are still owed |
| Filter-as-you-type | M4 | A substring over a listing already in memory is thirty lines and the narrowing is what a directory of ten thousand needs before search exists. **Not** search: no fuzzy matching, no index, no recursion — those are M4's and cost what M4 costs |
| New folder, rename, Move to Trash, and one undo stack over all three | M3 | Rule 3 is one of the eight expensive-to-undo invariants, and every mutation added before the stack exists is a mutation that has to be retrofitted onto it. **Not** the queue: one operation at a time, no progress, no ETA, no copy and no move — those are M3's and are what M3 costs. The XDG Trash is written by hand, so it brought no dependency and it interoperates with every other file manager on the machine |
| The context menu, built out of the command registry | M6 | Every action reachable by keyboard alone *and* by pointer alone, which is a promise that gets more expensive with every command that arrives without it. `every_command_can_be_reached_without_a_keyboard` is the other half of the test rule 2 already had, and the exemption table it allows names ten commands and the reason a pointer has a better way of doing each |

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

Six measurements are still owed. One of them now blocks a budget row rather than a
milestone — see the sort row in
[09-performance-budgets.md](09-performance-budgets.md#what-m0-measured), which needs
an MSD string radix over the name arena and cannot use the prefix key Spike F
proposed.

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
