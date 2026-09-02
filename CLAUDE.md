# Working on Skerry

A file manager for Linux, in Rust. The differentiator is latency and interaction
design, not features. Read [docs/03-principles.md](docs/03-principles.md) before
your first change; everything else in this file assumes it.

## Where the project is right now

**M0, in progress, and something you can use for a day.** Twelve crates and two
sandboxed helpers, 715 tests, 122 GUI scenarios **on both Wayland and X11**,
fifty-eight golden frames, every gate green: a Details view
over a real directory at a **0.342 ms frame p99 with 200,000 entries in it**, live
under inotify, with folder totals, a sidebar **whose places open into a tree, whose divider
drags, and whose pinned rows are a line each in GTK's `bookmarks` file**
([ADR 0021](docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md)) — so a
folder pinned here is pinned in Nautilus, the well-known directories cannot be
unpinned, and the rows drag into an order — **tabs and a
binary split tree** ([ADR 0010](docs/adr/0010-a-split-tree-of-tab-strips.md))
where a tab dragged to a pane's edge splits it, **each pane carrying its own
tab strip and, under it, its own Back / Forward / Parent / Reload and its own
breadcrumbs** — so the window draws a title bar only where the compositor draws
no frame — a command registry
behind every key, a context menu and a fuzzy palette over the same registry,
new-folder / rename / **copy / cut / paste / duplicate / Move to Trash** with one
undo stack over all of it — **and the stack is visible**: a strip above the status
line, collapsed to its header until somebody expands it, one row per edit oldest
first with the present marked and the undone future still listed in grey, where a
press on any row walks to the point *after* that edit — and **delete for good**, which asks in a panel that names what goes and says
it cannot be undone, and is on no stack, because rule 3 calls it "an explicit,
differently-shaped action"
— a status line that counts what a copy has done, files that open in the
application the desktop says owns them or in **the one you pick**, **Properties**
and **a terminal here**, a clipboard that takes files, paths, and the text
inside a text file, and text from **a real input method** — dead keys and
Compose on both servers, `text-input-v3` on Wayland. Files drag between the halves
of a split and between the tabs of one, with a card under the pointer saying what
is held and a folder — or a tab — held over for nine hundred milliseconds opening
to be dropped into; a taken name **stops the queue and asks**, and Replace trashes what it
replaces so `Ctrl+Z` walks both halves back; a copy shows a **counted ETA** and can
be **paused** on a latch; `Ctrl+Shift+F` searches everything under the folder, first
result at **0.7 ms over 817,288 entries**, `Ctrl+Alt+F` searches **every local
filesystem** — the sidebar's own Volumes, as
[ADR 0022](docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)
settles, where a complete answer over 6.9 million entries is **8,472 ms walked
against 36.2 ms from an index** — and `Ctrl+Enter` puts every result
into the pane as rows that can be selected, dragged and trashed where they are;
scrolling belongs to the pane under the
pointer and a touchpad **flick throws the list** and coasts to a stop, which is
where the frame loop learned that an animation is a wake-up with an owner
([12](docs/12-measurement.md#an-accessor-written-for-a-caller-that-never-arrived));
the columns have **a ceiling and drawn boundaries that drag**, so an ultrawide
window packs them left into a table with an edge instead of putting a file's size
a screen away from its name; each pane's bar carries a **switcher** for its view
mode, so a mode is one press rather than a menu — and its third mark is a **column view** —
one column per level of the path, the pane's own rightmost, a press in an ancestor
navigating ([ADR
0015](docs/adr/0015-a-column-is-a-directory-and-a-choice.md), which supersedes
0014: a column is not a pane, because a pane is independent and columns are one
path decomposed);
a pane lays out as a **details list or a grid**, per tab and remembered, behind one
cell geometry ([ADR 0013](docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md))
that every future view plugs into, and **groups by time, by size or by kind** —
today, yesterday and the seven days before that, gigantic down to empty, or images
and archives and code —
orthogonally to either, with each band sorted on its own
because the band is a prefix byte ahead of the sort's column, **remembered per
folder** with the nearest ancestor inherited, so Downloads can be a grouped list
while Pictures is a grid; **a heading is a control**, so a press on one collapses
that band — filtering by group, at no sort, with the heading left holding the count
of what is behind it and the status line saying `9 of 12 items`; **a check box over
an entry's mark** selects with the pointer alone, the same `Selection::toggle`
`Ctrl`-click has always had and `Ctrl+Space` now has too;
**a picture is its own mark** — PNG, JPEG and SVG decoded in
`skerry-thumbnailer`, a process with no execute right anywhere and nothing writable
but the XDG cache it shares with every other file manager on the machine, drawn
from a grid of equal slots ([ADR 0018](docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md))
that is one texel until the first thumbnail exists — and **`Space` is a quick
look**: the picture, a PDF's page through whatever renderer is on `PATH`, or
the file's own text — **mono, numbered, and selectable**, where a drag and
`Ctrl+C` put it on the clipboard through
[`Scope::Look`](crates/sk-cmd/src/registry.rs), the third scope the resolver was
written to expect — drawn as runs carrying a tone so a highlighter replaces one
function and nothing else, where **`←`/`→` walk the files and `↑`/`↓` the pages**
([ADR 0019](docs/adr/0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md):
a request names a page and the child holds nothing, because page 400 of 1200 costs
what page 1 costs) — and the **two pages ahead are already on the GPU**, a few
whole textures bounded by 24 MiB rather than by a count
([ADR 0020](docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md)),
so a turn onto one is a field assignment;
a folder can be **indexed** ([ADR 0012](docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md)),
after which the same complete search costs **6.99 ms instead of 639.9 ms** over
563,605 entries — one file per root, 45 bytes an entry, rebuilt when asked and
never on its own; and the window **comes back the way it was
left** after `kill -9`
([ADR 0011](docs/adr/0011-one-plain-text-state-file-written-atomically.md)).
Every command is reachable by keyboard
alone and by pointer alone, and a test fails the build for one that is not. What
is done and what is owed:
[08-roadmap.md](docs/08-roadmap.md#m0--6-wk--platform-layer). The numbers and the
three budget rows currently missed:
[09-performance-budgets.md](docs/09-performance-budgets.md#what-m0-measured).

Three things about working on this code that are not obvious from reading it:

- **`--bench` and `--capture` need no display server**, and they run the same
  immediate-mode pass, renderer and glyph atlas the window does. Use them. Four
  of the six defects M0 found were found by looking at a captured PNG or reading
  a benchmark line, not by running the window.
- **Everything above `sk-plat` works in logical pixels; everything below the
  `Painter` works in device pixels**, and the conversion happens in exactly one
  place. Mixing them produces arithmetic that is correct at 100% and wrong
  everywhere else — which is how one baseline ended up four thousand pixels below
  the window.
- **There are two display backends and nothing above `sk-plat` may know which it
  has.** `tools/check-invariants.py` fails a build that names one
  ([ADR 0009](docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md)). Run
  the scenarios both ways — `tools/gui-test` and `tools/gui-test --x11` — because
  the X11 half needs no seat and finds things the Wayland half cannot: it caught
  a harness bug that had been latent since the harness was written.

Phase 0's six spikes are all resolved and none of them said no:

| | | |
| --- | --- | --- |
| [A](spikes/a-million-rows/README.md) | a million rows at 8 ms p99 | **Go**, 2.6 ms worst p99 |
| [B](spikes/b-enumeration/README.md) | `getdents64` + batched `statx` | **Go**, but sits *on* the 1 s bar on a Pi 5 |
| [C](spikes/c-watching/README.md) | inotify at scale | **Go**, three things to design |
| [D](spikes/d-platform-and-dnd/README.md) | platform layer and drag-and-drop | **Go**, and three limitations report no error at all |
| [E](spikes/e-operations/README.md) | operation engine correctness | **Go**, all three conditions on seven filesystems; four defects found that would have shipped |
| [F](spikes/f-index-and-search/README.md) | index and flattened search | **Go on the index**; sorting misses its budget |

Spikes live in [spikes/](spikes/), are exempt from the layering, lint and
dependency rules — never the banned-toolkit rule — and are kept for their
measurements after the code is thrown away. Use `/spike`.

**Read [docs/12-measurement.md](docs/12-measurement.md) before taking a number.**
Every rule in it was learned by getting one wrong here, and three of them cost a
day each: load contamination is worth 6×, a single per-core multiplier is not a
portability model, and a gate nobody has watched fail is decoration. M0 added
three more, and each began as a green test: an assertion between two derived
numbers passed while both were 13× wrong, a per-frame budget counted the wrong
quantity, and an accelerator disagreed with the order it was accelerating. A
fourth began as a *red* one that named the wrong process for four rounds: a
counter that was one short made every wait run out, the wait then gave up
quietly, and the poller behind it read the answer to the previous question — and
a stale state dump is pristine, which reads exactly like a window that ignored
the keys.

Phase 0 moved the architecture in eight places, listed in
[docs/08-roadmap.md](docs/08-roadmap.md#what-phase-0-changed-in-the-plan). Two
performance budget rows are **known-unmet** and recorded as such rather than
widened, and three drag-and-drop capabilities are **absent with no error
attached**, which is a design constraint rather than a bug to fix.

**Spike E's code is the one exception to "spike code is thrown away."** It is the
permanent torture matrix, M3 is gated on it, and it is a CI job today — one binary
with no arguments that discovers which filesystems to test, run nightly by
[torture.yml](.github/workflows/torture.yml) over four filesystems the job builds
on loop devices for it.

## The craft bar

The three below are not style preferences. They are the reason this project can
have twelve crates and stay reviewable.

### Code explains itself; comments explain why

A comment that describes *what* the code does is a bug report against the code.
Rename it, split it, restructure it until the mechanism is legible. Three kinds
of comment survive review: a syscall or protocol quirk with a spec citation, a
performance decision with a measured number, and a named invariant behind a
suppression.

> **Fails if:** a comment longer than three lines explains mechanism rather than
> motive. A function that needs a section comment wants to be two functions.

### Every claim about performance is a number with a method

"Fast" is not a word that appears in a commit message, a doc, or a review here.
[docs/09-performance-budgets.md](docs/09-performance-budgets.md) is the format:
a measure, a target, a hard fail, and how it was measured. p99 over a realistic
trace, never an average over an idle window.

> **Fails if:** an optimization lands without a before and an after.

### Beautiful is a budget, not a taste

Zero CPU at idle, 8 ms frames, crisp glyphs at fractional scale, a visible focus
ring, real progress with a real ETA. Each is a row in the budget table or a
golden-frame test. Aesthetic judgement goes into the parts that cannot be
measured — spacing, motion, copy — and never into excusing a missed number.

## The eight things that are expensive to undo

Each is written as a detectable failure because that is how this project settles
arguments. The full reasoning is one link away; do not re-derive it in review.

| Invariant | Fails if | Where |
| --- | --- | --- |
| The UI thread issues zero filesystem syscalls | any frame exceeds 8 ms with a mount hung mid-listing — `tools/hung-mount`, and `a-hung-mount-never-blocks-the-frame-loop` runs it | rule 1 |
| The frame loop sleeps on descriptors, never on a timer | `skerry` accrues any CPU with an idle window | rule 1, [09](docs/09-performance-budgets.md#what-m0-measured) |
| Dependencies point strictly downward through the levels | `tools/check-layering.py` reports an edge | [04](docs/04-architecture.md#layers) |
| No GLib, GTK, Qt, KIO, GVfs, GStreamer, libdbus — ever | any appears in the graph | rule 9, [06](docs/06-portability.md) |
| One job pool for the process; no async runtime, no `rayon` | a second scheduler enters the workspace | [ADR 0005](docs/adr/0005-no-async-runtime.md) |
| Capabilities are probed at runtime, never compiled in | a `#[cfg(feature)]` decides environmental behaviour | [ADR 0007](docs/adr/0007-runtime-capability-detection.md) |
| A new dependency arrives with its row in the same commit | `tools/dep-budget.py` finds no row | rule 10, [07](docs/07-dependencies.md) |
| Every action is one entry in one registry | a menu item or setting the palette cannot find | rule 2 |
| Every mutation is undoable | `Ctrl+Z` cannot walk it back | rule 3 |
| The layout survives the process | `kill -9` loses a tab, a split or a sort | rule 6, [ADR 0011](docs/adr/0011-one-plain-text-state-file-written-atomically.md) |

Two more that only bite once code exists: `wgpu` types must not appear above
`sk-gpu` ([ADR 0003](docs/adr/0003-wgpu-first-backend.md)), and every
user-visible string goes through `t!()` from the first commit, because
retrofitting extraction across twelve crates costs weeks.

## Before you propose a change

```
./tools/check          # links, layering, dependency budget, invariants
```

That is also all CI runs — [check.yml](.github/workflows/check.yml) calls the same
script, because a gate that exists only in a workflow file is a gate nobody can
reproduce. Alongside it: the golden frames on lavapipe, and `cargo check` on the
oldest toolchain `rust-version` claims. **One and a half harnesses stay local on purpose.** `tools/gui-test --x11`
is in CI — `XTEST` under `Xvfb` needs no seat — but the Wayland half needs a
compositor advertising the virtual-input protocols and a runner has none to give,
and `--bench` on a shared runner measures the runner's neighbours. So a frame time
is *your* job before you push, and so is the Wayland half of a scenario.
**Run the suite under load before you trust it**: twelve spinning processes is
what found four scenarios that were guessing with a `sleep` where a wait belonged,
after four quiet sweeps had called them the machine's fault.

Then: **is this in [docs/11-non-goals.md](docs/11-non-goals.md)?** That list
exists so declined features are declined by pointing, not by re-arguing. A
feature belongs in 1.0 only if it serves one of the ten rules.

For anything irreversible — a dependency, a seam, a threading decision, a
protocol — write an ADR with `/adr` *before* the code, not after. ADRs are
immutable: a decision that changes is superseded by a new one that links back.
`tools/check-invariants.py` enforces that mechanically.

## Documents, by what they settle

| Question | Document |
| --- | --- |
| How do I take a number honestly? | [12-measurement](docs/12-measurement.md) |
| Is this feature worth building? | [02-positioning](docs/02-positioning.md), [11-non-goals](docs/11-non-goals.md) |
| Is this design allowed? | [03-principles](docs/03-principles.md), [04-architecture](docs/04-architecture.md) |
| Where does this code go? | [04-architecture](docs/04-architecture.md) crate table |
| Can I add this crate? | [07-dependencies](docs/07-dependencies.md) |
| Will this run on their machine? | [06-portability](docs/06-portability.md) |
| Is it fast enough? | [09-performance-budgets](docs/09-performance-budgets.md) |
| Why was this decided? | [docs/adr/](docs/adr/) |
| What is being built next? | [08-roadmap](docs/08-roadmap.md) |

Numbered documents are living and edited in place. ADRs are not.

## Agents and skills

Delegate to the agent whose judgement the task actually needs, rather than
reasoning about all of it yourself.

| Agent | Use it for |
| --- | --- |
| `architect` | Placing new work in the layering, choosing seams, planning a milestone |
| `auditor` | Reviewing a diff against the ten rules, the budgets and the craft bar |
| `dep-warden` | Any change to a `Cargo.toml`, or "can we use crate X" |
| `platform-scout` | Wayland, X11, D-Bus and freedesktop specification detail |
| `perf-analyst` | Designing a measurement, reading a number, defending a budget |

| Skill | Does |
| --- | --- |
| `/spike` | Create or resolve a Phase 0 spike: question, bar, method, number, verdict |
| `/adr` | Write the next ADR and update the index |
| `/new-crate` | Scaffold a crate at a level, with the lints and the metadata |
| `/add-dep` | Run the dependency gate and record the justification |
| `/check` | Run every gate and explain what failed |

## Writing style, for docs and for copy

The documents in `docs/` have a voice: declarative, specific, numbers over
adjectives, one idea per paragraph, em dashes over parentheses. Match it. In the
product, error messages say what happened and what to do, and are separate from
the debug chain that goes to the log.
