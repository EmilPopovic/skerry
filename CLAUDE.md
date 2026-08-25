# Working on Skerry

A file manager for Linux, in Rust. The differentiator is latency and interaction
design, not features. Read [docs/03-principles.md](docs/03-principles.md) before
your first change; everything else in this file assumes it.

## Where the project is right now

**Planning. There is no code, and the next artifact is not a crate skeleton.**

[docs/08-roadmap.md](docs/08-roadmap.md) is explicit: the first thing to exist is
**Spike A** — a million-row virtualized list at a hard 8 ms p99, built twice so
the renderer comparison is measured rather than argued. Building `crates/` before
that spike returns a number is building on an unverified premise. Use `/spike`.

Spikes live in [spikes/](spikes/), are exempt from most rules, and are kept for
their measurements after they are thrown away.

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
