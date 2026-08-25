---
name: new-crate
description: Scaffold a workspace crate at a declared level, with the lint preamble, the layering metadata and a test module. Use only once Phase 0 is resolved — the roadmap puts the first spike before the first crate.
---

# /new-crate

> **Check first:** [08-roadmap.md](../../../docs/08-roadmap.md) puts Phase 0's
> spikes before any crate exists. If Spike A has not returned a number, the
> renderer is undecided and `sk-gpu` is premature. Say so rather than scaffolding.

The twelve crates and their responsibilities are already decided in the crate
table in [04-architecture.md](../../../docs/04-architecture.md#crate-responsibilities).
A crate outside that table needs an ADR and an argument that a seam is missing —
ask `architect` before creating one.

## Steps

1. Confirm the crate and its level against the crate table.
2. Create `crates/<name>/` from the template below.
3. Add it to the workspace `members`.
4. `./tools/check` — layering and invariants both read the new manifest.

## Cargo.toml

```toml
[package]
name = "sk-<name>"
version.workspace = true
edition.workspace = true
license.workspace = true

# Read by tools/check-layering.py. A crate may depend on lower levels only.
[package.metadata.skerry]
level = <0..4>

[dependencies]
```

Every dependency needs its row in [07-dependencies.md](../../../docs/07-dependencies.md)
in the same commit — run `/add-dep` rather than adding one by hand.

## src/lib.rs

```rust
//! <One sentence: what this crate owns.>
//!
//! Does not own: <the "explicitly does not own" column, verbatim.>

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![warn(missing_docs, clippy::pedantic)]
```

The deny is checked by `tools/check-invariants.py`, and the second line of the
doc comment is load-bearing: most misplaced code is placed by someone who read
what a crate owns and not what it refuses.

## What belongs in it on day one

- **An error enum with `thiserror`.** No `anyhow` below level 4. Errors that can
  reach a human carry a user-facing message — what happened and what to do —
  separate from the `Debug` chain that goes to the log.
- **The test module named in the testing-strategy table.** Property tests for
  arena and interning invariants; golden frames for anything that paints; the
  filesystem matrix for `sk-fs`. A crate whose test style is undecided is a crate
  whose boundary is undecided.
- **Nothing speculative.** No trait with one implementation and no second caller
  in sight. The seams are already chosen; inventing more is how twelve crates
  becomes thirty.

## Level 1 and above

Level 1 crates own everything that can fail because the outside world is hostile
— a vanishing mount, a broken font, a compositor missing a protocol. That is the
whole reason the boundary is there: above it, failures are policy decisions and
can be unit-tested with fakes.

Anything touching a disk, a mount or a decoder is a job with a cancellation
token, not a function the UI thread calls.
