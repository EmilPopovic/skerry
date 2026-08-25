---
name: add-dep
description: The dependency gate. Run before adding any crate to any Cargo.toml — answers rule 10's six questions, records the row in docs/07-dependencies.md, and reports the effect on both budgets.
---

# /add-dep

Rule 10: a crate enters the graph only when writing that code ourselves is
demonstrably worse, **and the reasoning is written down**. A dependency without a
row in [07-dependencies.md](../../../docs/07-dependencies.md) is a review
blocker, and `tools/dep-budget.py` fails the build on one.

## Steps

1. **Ask `dep-warden`.** It answers the six questions with evidence and returns
   a verdict and a drafted row. Do not skip to step 3 because the crate seems
   obviously fine — `serde` is obviously fine and still carries a stated reason.
2. **Measure the cost before deciding.** `cargo tree --edges normal` and a
   release build, as numbers: transitive crates added, and compiled bytes. Report
   both counts — non-GPU and total — against the budgets of 25 and 120.
3. **Add the row** to the right tier in `docs/07-dependencies.md`, in the same
   commit as the manifest change. Tier 2 entries state an exit: what replacing it
   later costs, in lines.
4. **Add the dependency**, with the narrowest feature set that works.
   `default-features = false` unless a default is needed and named.
5. `./tools/check`.

## The tiers

| | Means |
| --- | --- |
| **0** | We would otherwise write it badly. `rustix`, `swash`, `wgpu`, `xkbcommon`. |
| **1** | Small, sharp, obviously worth it. One file's worth of well-tested primitive, or a solved problem that is not our problem. |
| **2** | Accepted with a stated exit. Something is uncomfortable about it and we wrote down what leaving costs. |

## Answers that are not "add the crate"

Reach for these first; they are the right answer more often than the crate is.

- **A helper binary.** Optional or dangerous work lives in a separate executable,
  so its tree never enters the main binary and a missing helper degrades one
  feature instead of failing a build. This is where every codec belongs
  ([ADR 0006](../../../docs/adr/0006-out-of-process-decoders.md)).
- **Write it.** Small, well-specified parsers — `.desktop`, MIME globs,
  `index.theme`, `mimeapps.list`, `.trashinfo` — and anything coupled to the
  columnar store. The "what we write ourselves, on purpose" list is the
  precedent, and each entry is there because generality would cost latency or
  control.
- **No.** A budget exceeded is not a budget renegotiated. Moving one takes an
  ADR, and the fact that a dependency is worth that is itself the argument.

## Refused on sight

GLib, GObject, GIO, GTK, Qt, KIO, KDE Frameworks, GVfs, `libdbus`, GStreamer
(rule 9). `tokio`, `async-std`, `smol`, `rayon`, any second thread pool
([ADR 0005](../../../docs/adr/0005-no-async-runtime.md)). Any crate needing a C
toolchain in the main binary. `tools/check-invariants.py` enforces this list, so
the conversation ends at the citation.
