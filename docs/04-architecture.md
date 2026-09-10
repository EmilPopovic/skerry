# Architecture

Two questions this document answers: *how does the code reach one-frame latency*, and
*how does it stay organized once there are twelve crates, four contributors and a hundred
features*. The second question is the one that kills projects at this size, so it comes
first.

---

## Part 1 — Organization

### Layers

The workspace is a strict DAG in five levels. A crate may depend on lower levels only —
never sideways within its own level, never upward.

```
level 4   sk-app          panes, tabs, split tree, sessions, config, views
                │
level 3   sk-ui           immediate-mode layer, layout, virtual lists, focus, a11y
                │
level 2   sk-gpu   sk-index   sk-desktop   sk-preview   sk-cmd
                │
level 1   sk-plat  sk-fs      sk-text
                │
level 0   sk-model  sk-util
```

**Two things moved between the level-0 crates on the first commit that had
callers**, and the layering check is why. The bump arena and the bitset were
`sk-util`'s in the first draft; each turned out to have exactly one consumer —
the arena backs `Listing`'s name column and the bitset backs `Selection` — so
leaving them there made `sk-model` depend on `sk-util`, which is a sideways edge
inside level 0 that `tools/check-layering.py` refuses. The refusal was right:
they are not general-purpose containers, they *are* the data layout, and a
general-purpose container crate would undo it. The string catalogue moved the
same way and for a plainer reason: every level above needs `t!`, and level 0 is
the only place all of them can reach.

Why these seams and not others: each boundary is where a *different kind of failure*
lives. Level 1 crates own everything that can fail because the outside world is hostile —
a vanishing mount, a broken font, a compositor missing a protocol. Level 2 crates own
policy over those primitives and can be unit-tested with fakes. Level 3 owns pixels and
input. Level 4 owns product behaviour and nothing else, which is what keeps feature work
from reaching into the renderer.

### Layering is enforced, not encouraged

`tools/check-layering.py` parses every `Cargo.toml` in the workspace, resolves internal
edges, and fails the build on any edge that is not strictly downward. It runs on every
commit. This is thirty lines of code and it is the single highest-leverage thing in the
repository: without it, level 4 grows a dependency on `sk-gpu` in month four and the
architecture becomes a diagram that used to be true.

The same tool emits the level diagram above, so the documentation cannot drift from the
manifests either.

### Crate responsibilities

| Crate | Owns | Explicitly does not own |
| --- | --- | --- |
| `sk-model` | `EntryId`, the columnar `Listing`, `Selection`, `Sort`, natural-order collation, the string catalogue, and `Problem` — the error type that carries a message a human can act on | Any I/O, any UI concept |
| `sk-util` | The one job pool, its cancellation tokens, and the eventfd a finished job wakes the frame loop with | Domain types |
| `sk-plat` | Wayland and X11 protocol handling, keyboard mapping, pointer and gesture input, clipboard, drag-and-drop as both source and target, IME, monitors and fractional scaling, window lifecycle | Drawing anything |
| `sk-fs` | `getdents64` enumeration, batched `statx`, the watcher, the mount table, the operation engine (copy/move/rename/create/trash), xattr and ACL preservation | Deciding *what* to enumerate, or why |
| `sk-text` | Font discovery, shaping, line layout, the layout cache, glyph rasterization | The glyph atlas (that is GPU memory, so `sk-gpu`) |
| `sk-gpu` | Device and surface setup, glyph and image atlases, instanced quad batching, damage tracking, present pacing | Layout, hit-testing, widget state |
| `sk-index` | Recursive walk, flattened views, fuzzy matching, ignore rules, recursive size aggregation, and the persistent index — required, not optional, per [ADR 0008](adr/0008-persistent-index-is-required.md). Size aggregation, subtree search and the persistent index exist — the index's format is [ADR 0012](adr/0012-a-flat-index-file-per-root-rebuilt-whole.md); extension facets and ignore rules are M4 | Rendering results, or the query UI |
| `sk-desktop` | shared-mime-info, XDG user directories and bookmarks, XDG icon themes, `.desktop` parsing and launch, D-Bus (FileManager1, udisks2, portals), runtime capability probing | Any policy about *when* to launch something |
| `sk-preview` | The preview host: child-process lifecycle, the wire protocol ([ADR 0017](adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)), the sandbox a child applies to itself, and the queue of what is wanted | Decoding anything in-process (see [ADR 0006](adr/0006-out-of-process-decoders.md)); **kind detection**, which needs the MIME database and so lives in `sk-app` — both crates are level 2 and the edge is forbidden; and the thumbnail cache, which the child owns because the child is what reads and writes it |
| `sk-cmd` | The command registry, keymap resolution including scopes and sequences, the palette's fuzzy ranking, the undo stack. Generic over the context a command runs against, which is what lets a level-2 crate hold level-4 behaviour with no upward edge | Any command's implementation |
| `sk-ui` | Immediate-mode pass, layout primitives, virtualized lists, focus and hit-testing, animation clocks, the accessibility tree | Which widgets a file manager needs |
| `sk-app` | Pane and tab tree, session persistence, config load and hot reload, the concrete views, command implementations | Anything reusable |

### Extension points: how features are added without editing shared files

The failure mode at this scale is not bad code, it is a handful of central `match`
statements that every feature must edit — and that every contributor therefore conflicts
in. Skerry has four registries instead, each populated at startup by the feature's own
module:

```rust
// crates/sk-app/src/views/tree.rs — a whole feature, touching no shared file
pub fn register(reg: &mut Registry) {
    reg.view(ViewSpec {
        id: "view.tree",
        name: t!("view.tree.name"),
        build: |ctx| Box::new(TreeView::new(ctx)),
    });
    reg.command(CommandSpec {
        id: "tree.expand_all",
        name: t!("cmd.tree.expand_all"),
        group: Group::View,
        default_key: Some("ctrl+shift+e"),
        visible_when: Ctx::VIEW_TREE,
        run: |app| app.view_mut::<TreeView>().expand_all(),
    });
}
```

The four registries:

| Registry | Registers | Consumed by |
| --- | --- | --- |
| **Commands** | `CommandSpec` — id, display name, group, default binding, visibility predicate, closure | Palette, menus, keymap, config schema, and the docs generator — all four views of rule 2 |
| **Views** | `ViewSpec` — a `View` trait implementation | The pane, which knows nothing about Details vs Grid vs Tree |
| **Previews** | `PreviewKind` — MIME patterns plus either an in-process renderer or a helper binary | The inspector and the thumbnailer |
| **Columns** | `ColumnSpec` — header, width policy, extractor, sort key derivation | Details view, Group By, and the sort machinery |

Consequences worth stating: adding a view mode touches one new file plus one line in a
`register_all`. The command palette and the config schema populate themselves. The user
manual's shortcut table is generated from the registry, so it cannot be wrong.

### Error policy

- Libraries define their own error enums with `thiserror`. No `anyhow` below level 4.
- Every error that can reach a human carries a *user-facing* message written per the copy
  rules in [03-principles.md](03-principles.md) rule 7 — what happened, and what to do —
  separate from the `Debug` chain that goes to the log.
- `unwrap` and `expect` are denied by lint outside tests and outside genuine invariants,
  which must carry a comment naming the invariant.
- Panics in the UI thread are a bug; panics in a job are caught at the pool boundary and
  surfaced as a failed job, because one unreadable directory must not take down a window.

### Configuration and state

Two separate stores, because they have different durability needs and different owners.

| | Config | State |
| --- | --- | --- |
| Path | `$XDG_CONFIG_HOME/skerry/config.toml` | `$XDG_STATE_HOME/skerry/` |
| Contains | Keymap, colours, view defaults, behaviour toggles | Sessions, tabs, layouts, bookmarks, recents, per-folder view settings |
| Written by | The human, or the app when the human changes a setting in the UI | The app, constantly |
| Format | TOML, schema generated from the command registry | Append-only log plus periodic compaction |
| Durability | Hot-reloaded on change; a syntax error keeps the last good config and shows a fixable error | `write` to a temp file, `fsync`, `rename` — rule 6 is a `kill -9` test |
| Migration | `version` key with a migration chain; the old file is backed up before rewrite | Same |

### Internationalization from day one

Every user-visible string goes through `t!()` and lives in a catalogue, from the first
commit. Not because localization is an early priority — it is not — but because
retrofitting string extraction across twelve crates costs weeks, and because the discipline
catches concatenated sentences that cannot be translated. The `t!` macro resolves from an
embedded catalogue at build time with a runtime override directory.

### Testing strategy, by layer

| Layer | Test style | Runs where |
| --- | --- | --- |
| `sk-model`, `sk-util` | Ordinary unit tests, plus property tests for arena and interning invariants | Every commit |
| `sk-fs` | A filesystem torture matrix over loopback images: ext4, btrfs, xfs, tmpfs, exFAT, plus an NFS loopback. Sparse files, hardlinks, symlinks, xattrs, ACLs, ENOSPC mid-copy, adversarial filenames | Every commit for tmpfs/ext4; nightly for the full matrix |
| `sk-plat` | The 152 GUI scenarios against a real window, on **both** display servers: a nested Hyprland through the virtual-input protocols, and a private `Xvfb` through `XTEST`. Only the second needs no seat, so only the second is in CI | Every commit |
| `sk-gpu`, `sk-ui`, `sk-text` | Golden-frame tests: render offscreen, compare against reference PNGs with a perceptual tolerance, at 100/125/150/200% scaling. Run on llvmpipe so CI needs no GPU | Every commit |
| `sk-index`, `sk-cmd` | Unit tests against synthetic trees. Fuzzing the filter grammar and the keymap parser is planned and not yet written | Every commit |
| `sk-desktop` | The D-Bus wire runs a bounded mutation campaign ([ADR 0040](adr/0040-bounded-parser-mutation-campaigns.md)) and is tested against a mock bus. `.desktop`, MIME globs and magic and `index.theme` have adversarial unit tests, not yet a campaign | Every commit; longer campaigns scheduled |
| `sk-app` | Scripted end-to-end sessions via a `--script` mode that drives the real app headlessly | Every commit |
| Whole binary | Frame-time and memory budgets from [09-performance-budgets.md](09-performance-budgets.md), asserted as failing tests | Every commit |

The two entries doing the most work here: **golden-frame tests on llvmpipe** mean a custom
renderer stays reviewable by people without the reviewer's GPU, and **`--script` headless
mode** means product behaviour is testable without a display server. Both need designing
in M1, not bolted on at M8.

### Observability

An always-on internal frame profiler with a ring buffer of the last few seconds, dumpable
to a file with a keystroke, so a user's "it stuttered" becomes a timeline. `tracing` spans
behind a default-off feature flag for deeper work. No telemetry that leaves the machine,
ever, in any build.

### How this scales to contributors

- The registries mean most features are additive: one new file, no shared edits, no merge
  conflicts.
- The layering check means a newcomer physically cannot wire a feature into the renderer.
- `sk-preview`, `sk-desktop` and `sk-index` are the natural contribution surfaces — high
  value, self-contained, testable, and reachable without understanding the frame loop.
  `sk-gpu`, `sk-plat` and `sk-ui` are where the project's identity lives and are reviewed
  hardest.
- Every level-1 crate is publishable standalone if it turns out to be useful elsewhere;
  that is a side effect of the seams, not a goal, but it is a good check on whether a
  boundary is real.

---

## Part 2 — Runtime design

### Rendering

A small immediate-mode UI layer over the GPU, not a retained-widget toolkit. GTK and Qt are
excluded on mechanism rather than ideology: per-row widget instantiation, their scroll
implementations, and their blocking-I/O conventions are exactly what rule 1 forbids. See
[ADR 0001](adr/0001-custom-gpu-ui.md).

- **GPU:** `wgpu` for the first release — Vulkan where available, GL fallback for llvmpipe
  and old drivers — behind a `Renderer` trait thin enough that a hand-written `ash` backend
  is a contained swap if the dependency weight or a driver bug forces it.
  [ADR 0003](adr/0003-wgpu-first-backend.md) records the revisit triggers.
- **Platform:** our own Wayland and X11 layer rather than `winit`, because drag-and-drop
  *source* support, primary selection, `text-input-v3` and `wp_fractional_scale_v1` are
  requirements and are exactly where the general-purpose windowing crates are thinnest.
  This is the most expensive single decision in the plan; [ADR
  0002](adr/0002-own-platform-layer.md) argues it and prices it.
- **Text:** `swash` for shaping and rasterization in one dependency, our own line layout
  and layout cache on top. Real shaping, bidi and fallback from the first commit — File
  Pilot's missing Unicode support is a defect to learn from, not to inherit.
  [ADR 0004](adr/0004-swash-for-text.md).
- Everything visible is instanced quads: one draw call per batch, damage-tracked, redrawn
  only on input or animation, paced by Wayland frame callbacks. Zero CPU at idle is a
  budget, not a hope.

### Threading

```
        ┌──────────────────────────────────────────┐
        │  UI THREAD — 8 ms budget                 │
        │                                          │
        │  input  →  immediate-mode pass over       │
        │            *visible rows only*            │
        │         →  damage  →  atlas  →  submit    │
        │                                          │
        │  issues ZERO filesystem syscalls          │◄──────┐
        └──────────────────────────────────────────┘       │
                     ▲                                     │ versioned
                     │ reads                                │ snapshot
        ┌────────────┴───────────────┐        ┌─────────────┴──────────────┐
        │  STORE                     │        │  JOB POOL — ncpu-2          │
        │  columnar SoA:             │◄───────│  getdents64 enumerate       │
        │  name arena + parallel     │ writes │  statx batch, per-mount     │
        │  size/mtime/flag arrays,   │        │  recursive size walk        │
        │  bitset selection          │        │  fuzzy match over arena     │
        └────────────────────────────┘        │  parallel sort by key       │
                     ▲                        │  copy engine                │
                     │ invalidate             │  preview/thumb child spawn  │
        ┌────────────┴───────────────┐        └────────────────────────────┘
        │  WATCHER THREAD            │          every job holds a cancel token
        │  inotify, 16 ms coalesce   │
        │  IN_Q_OVERFLOW → rescan    │
        └────────────────────────────┘
```

The whole design in one constraint: **the UI thread reads immutable snapshots and never
calls into the filesystem.** A hung NFS mount degrades to a stale row, never to a frozen
window. Snapshots are versioned and reference-counted; a scroll in progress keeps reading
the snapshot it started with, so results arriving mid-gesture never make the list jump.

One job pool for the whole process, in `sk-util`, with cancellation tokens and per-device
queues — parallel on NVMe, serialized on rotational disks and network mounts. The lane
is chosen by a runtime probe in `sk-fs::device`: the filesystem magic from `statfs`
identifies NFS, CIFS, 9p and FUSE, and `/sys/dev/block/MAJOR:MINOR/queue/rotational`
answers the rest. It runs once per directory, not once per entry. One pool, not
two: a second thread pool (a `rayon` global, an async runtime's blocking pool) means
uncoordinated oversubscription, which is a latency bug that only appears under load.
See [ADR 0005](adr/0005-no-async-runtime.md).

**Width is at least `ncpu`, and the exact value matters less than not going below it.**
The `ncpu - 2` in the diagram above is wrong on small machines, where subtracting two is
most of the machine:
[Spike B](../spikes/b-enumeration/README.md#ncpu---2-is-wrong-again-for-a-second-and-unrelated-reason)
measured a million-file walk at **1.626 s** with `ncpu - 2` on a four-core Raspberry Pi 5
against **0.91–1.01 s** anywhere from four workers upward. Every machine measured has its
optimum at or above `ncpu`, and past that point the curve is flat enough that the choice is
inside the run-to-run noise. The four-core machines are content with more threads than they
have cores, which fits work that waits on inode fetches rather than computing.

A short probe at first use would beat any formula, and per-device queues remain necessary
regardless — every one of these numbers is NVMe.

An earlier revision of this section said the opposite, that metadata work saturates at three
workers because it is "kernel-bound on inode locks", on the strength of a spike arm that had
a single-threaded tree walk and a store that held a mutex across 34 MB of copying. Both
defects were the spike's. That claim is retracted; the episode is kept because a design
document carried a wrong statement about the kernel for two commits, and the reason was a
measurement that did not suspect itself.

### Data layout

A directory entry is not a struct in a `Vec`.

```
names:     one bump arena of UTF-8 bytes
name_ref:  [(u32 offset, u32 len)]     ── parallel arrays, index = EntryId
size:      [u64]
mtime:     [i64]
flags:     [u16]                        ── dir, symlink, hidden, exec, …
sort_key:  [u64]                        ── precomputed once per listing
selected:  bitset
```

This is what makes sorting and filtering a million entries a memory-bandwidth problem
instead of a pointer-chasing one, and it is the most important decision after "don't block
the UI thread". Selection as a bitset makes "select all, then deselect 200" instant and
makes the selection cheap to snapshot.

**A sort of any size runs off the frame loop.** M0 measured a 200,000-entry
reorder at 150 ms, which is not a frame; the first sort therefore happens inside
the job that read the directory, and a later re-sort above 8,192 entries runs on
a job over a *clone* of the listing and sends back only the permutation. The
threshold is measured rather than chosen: 6,167 entries reorder in 7.1 ms, inside
one frame and cheaper than a round trip through the pool. Cloning is what makes
the job possible at all — the pane cannot lend out a listing that `statx` results
are still landing on — and the permutation stays valid because an `EntryId` is an
index into arrays that only grow at the end.

**Sort keys are derived once at listing time — and [Spike F](../spikes/f-index-and-search/README.md#sorting-misses-its-target-and-the-architectures-stated-reason-is-wrong)
measured that this is worth 3%, not the order of magnitude an earlier revision of this
paragraph implied.** Sorting a million entries by name takes 152 ms whether the comparison
reads the arena or a precomputed `u64`, against a 120 ms budget; sorting by a metadata
column takes 0.93 ms. The cost is the random access pattern of sorting a *permutation*, not
the comparison function, so making the comparison cheaper cannot fix it. What is needed is
a sort that reads sequentially: a radix pass over the keys is **17.8 ms** and brings the
full name sort to **96.6 ms**, inside the budget. The remaining 79 ms is repairing ties,
because 94% of filenames share their first eight bytes and widening the key to sixteen was
measured and does not help. A fixed-width prefix key is the wrong shape for names that
collide on long prefixes. What shipped instead is an MSD radix over a **collation key
whose byte order is `natural_cmp` exactly**, in `sk_model::radix`: 1,460 → 166 ms at
973,759 entries, and never a comparison between two names. See
[09-performance-budgets.md](09-performance-budgets.md#what-m0-measured).

Enumeration goes through raw `getdents64` (via `rustix`) rather than `std::fs::read_dir`,
because `d_type` yields file-versus-directory for free and lets `statx` be deferred to a
background pass that fills columns in as they arrive.
[Spike B](../spikes/b-enumeration/README.md) measured the tax at a million entries: a warm
walk producing size and mtime takes **0.530 s** this way and **1.195 s** through `read_dir`
plus `metadata()`, which misses the 1 s bar.

The spike also corrected the shape of that claim. Deferring `statx` to the job pool is worth
0.614 s of the 0.665 s difference and `getdents64` itself is worth 0.051 s, so **the batching
is the decision and the syscall is the trim**. And the per-entry allocation is not a
footprint cost at all — peak RSS is flat across every arm, because the allocator hands the
same pages back. It is CPU time.

### The frame loop, precisely

1. Drain platform events. Keyboard, pointer, clipboard, DnD, configure, scale change.
2. Advance animation clocks by real elapsed time. Never by frame count.
3. Immediate-mode pass over *visible* rows only, producing draw commands and hit regions.
4. Diff against the previous frame's damage rectangles.
5. Upload any newly needed glyphs and thumbnails from the atlas staging queue — bounded
   per frame, so a thousand new thumbnails cannot blow the budget.
6. Submit, present, wait for the frame callback.
7. If nothing changed and no animation is running, sleep on the event queue. No timer
   wake-ups.

Step 5's bound is the kind of detail that separates a smooth renderer from a nearly smooth
one, and it belongs in the design rather than in a later bug fix.
