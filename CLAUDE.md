# Working on Skerry

A file manager for Linux, in Rust. The differentiator is latency and interaction
design, not features. Read [docs/03-principles.md](docs/03-principles.md) before
your first change; everything else in this file assumes it.

## Where the project is right now

**M0, in progress, and something you can use for a day.** Eleven crates, 378
tests, 52 GUI scenarios, twelve golden frames, every gate green: a Details view
over a real directory at a **0.342 ms frame p99 with 200,000 entries in it**, live
under inotify, with folder totals, Back and Forward, a sidebar, a command registry
behind every key, a context menu and a fuzzy palette over the same registry,
new-folder / rename / **copy / cut / paste / Move to Trash** with one undo stack
over all of it and a status line that counts what a copy has done, files that open
in the application the desktop says owns them, and a clipboard that takes files,
paths, and the text inside a text file. Every command is reachable by keyboard
alone and by pointer alone, and a test fails the build for one that is not. What
is done and what is owed:
[08-roadmap.md](docs/08-roadmap.md#m0--6-wk--platform-layer). The numbers and the
three budget rows currently missed:
[09-performance-budgets.md](docs/09-performance-budgets.md#what-m0-measured).

Two things about working on this code that are not obvious from reading it:

- **`--bench` and `--capture` need no display server**, and they run the same
  immediate-mode pass, renderer and glyph atlas the window does. Use them. Four
  of the six defects M0 found were found by looking at a captured PNG or reading
  a benchmark line, not by running the window.
- **Everything above `sk-plat` works in logical pixels; everything below the
  `Painter` works in device pixels**, and the conversion happens in exactly one
  place. Mixing them produces arithmetic that is correct at 100% and wrong
  everywhere else — which is how one baseline ended up four thousand pixels below
  the window.

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
quantity, and an accelerator disagreed with the order it was accelerating.

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
| The UI thread issues zero filesystem syscalls | any frame exceeds 8 ms with a mount hung mid-listing | rule 1 |
| The frame loop sleeps on descriptors, never on a timer | `skerry` accrues any CPU with an idle window | rule 1, [09](docs/09-performance-budgets.md#what-m0-measured) |
| Dependencies point strictly downward through the levels | `tools/check-layering.py` reports an edge | [04](docs/04-architecture.md#layers) |
| No GLib, GTK, Qt, KIO, GVfs, GStreamer, libdbus — ever | any appears in the graph | rule 9, [06](docs/06-portability.md) |
| One job pool for the process; no async runtime, no `rayon` | a second scheduler enters the workspace | [ADR 0005](docs/adr/0005-no-async-runtime.md) |
| Capabilities are probed at runtime, never compiled in | a `#[cfg(feature)]` decides environmental behaviour | [ADR 0007](docs/adr/0007-runtime-capability-detection.md) |
| A new dependency arrives with its row in the same commit | `tools/dep-budget.py` finds no row | rule 10, [07](docs/07-dependencies.md) |
| Every action is one entry in one registry | a menu item or setting the palette cannot find | rule 2 |
| Every mutation is undoable | `Ctrl+Z` cannot walk it back | rule 3 |

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
oldest toolchain `rust-version` claims. **Two harnesses stay local on purpose.**
`tools/gui-test` needs a compositor advertising the virtual-input protocols and a
runner has no seat to give one, and `--bench` on a shared runner measures the
runner's neighbours — so a scenario or a frame time is *your* job before you push,
not CI's after.

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
