# tools

Repository gates. Standard library Python and POSIX `sh` only — the same
dependency policy the product has, applied to the thing that enforces it.

| | Decides | Needs |
| --- | --- | --- |
| `check` | Runs everything below, then `fmt`, `clippy` and `test` once a workspace exists | — |
| `check-links.py` | Every relative link and heading anchor in the markdown resolves | — |
| `check-layering.py` | Crate dependency edges point strictly downward; `--diagram` prints the level diagram | manifests |
| `dep-budget.py` | Every dependency has a row in [07-dependencies.md](../docs/07-dependencies.md); transitive counts inside budget, with the GPU stack found by reachability from the `wgpu` edge | manifests, `cargo`, `rustc` |
| `check-invariants.py` | Banned crates, capability feature flags, crate lints, the GPU seam, ADR immutability | manifests, sources |

Each check abstains rather than guesses: with no crates yet, layering reports
"nothing to check" and exits zero. They exist before the code so that the first
crate cannot be the one that violates them.

## Levels come from the manifests

`check-layering.py` reads each crate's own declaration:

```toml
[package.metadata.skerry]
level = 1
```

The diagram in [04-architecture.md](../docs/04-architecture.md) is generated from
those declarations, so the documentation cannot drift from the manifests.

## Opting out

A line may suppress one source rule, with the reason on the same line:

```rust
let device = self.device.as_ref().unwrap();  // skerry-allow: no-unwrap — set in new(), never cleared
```

A suppression without a stated invariant is a review finding.

## What counts as the GPU stack

`docs/09-performance-budgets.md` budgets the GPU stack separately, so
`dep-budget.py` has to decide which crates it contains. It does that by
reachability from the `wgpu` and `naga` edges of the product's graph, read out of
`cargo metadata` and filtered to the host platform.

It used to match on name prefixes, which put `ash`, `glow`, `gpu-alloc`,
`gpu-descriptor`, `khronos-egl`, `spirv`, `renderdoc-sys`, `codespan-reporting`
and about fifteen more on the wrong side: 6 crates classified as GPU where the
honest figure is around 50. Measured on a workspace carrying `wgpu`, `swash`,
`rustix`, `bitflags`, `thiserror` and `smallvec`, the difference is a non-GPU
count of **21** against a budget of 25, rather than a spurious failure at 63.

A crate reachable from both the GPU stack and our own code counts as **non-GPU**,
because it is one we would carry anyway. That direction is deliberate: it stops
the GPU stack from concealing our own dependencies.
