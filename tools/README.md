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
| `make-listing-fixture` | Not a gate. Builds a directory of N files so the frame and listing numbers in [09-performance-budgets.md](../docs/09-performance-budgets.md) have a method that can be re-run | — |
| `gui-test` | Not in `check`, because it needs a compositor. Drives the real window with real input through a nested Hyprland and asserts on the state `F12` prints, and in one scenario on the pixels | `Hyprland`, a release build; `grim` and Pillow for the pixel scenario |
| `skerry-drive` | The input half of `gui-test`: replays a script of keys, clicks, drags and scrolls into a compositor | — |

## Testing the window itself

Every gate above stops at the edge of `sk-plat`. `tools/gui-test` does not: it
starts a **nested** compositor, runs the product inside it, and presses keys and
moves a pointer through `zwp_virtual_keyboard_v1` and `zwlr_virtual_pointer_v1`.
What that covers is the path no unit test reaches — `wl_keyboard`, `xkbcommon`,
the click counter, the scroll coalescer, hit-testing, and a frame loop that has to
wake up for any of it to be seen.

Nested, and it refuses to run otherwise: synthetic input goes wherever the focus
is, and pointed at a live session it would type into whatever window a person had
open.

It found three defects on its first run, one of which no unit test could have:
`Ctrl+A` reported ten items selected in a folder showing nine, because
`select_all` filled the whole bitset rather than the display order and the tenth
was a dotfile.

One scenario goes further and reads the **pixels**, with `grim` and Pillow: it
walks a twenty-thousand-file subtree, waits for the model to report the total,
and then counts the ink in the size cell, because a dash is one stroke and
`146 KiB` is six glyphs. It exists because the defect it covers was a shaped run
served from a cache — every layer under the frame buffer had the right answer and
the screen did not. It skips itself where `grim` or Pillow is missing.

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
honest figure is around 50.

A crate reachable from both the GPU stack and our own code counts as **non-GPU**,
because it is one we would carry anyway. That direction is deliberate: it stops
the GPU stack from concealing our own dependencies.

Two more things it learned from meeting a real workspace, both of which reported
a number that was not wrong so much as about the wrong thing:

- **An entry point is looked for anywhere in the graph, not only among the
  product's direct dependencies.** `skerry` depends on `sk-app`, which depends on
  `sk-gpu`, which depends on `wgpu` — so filtering the *root's* direct edges
  excluded nothing at all, and the first run against real crates reported 86
  non-GPU crates against a budget of 25.
- **Workspace members and proc-macro crates are excluded.** Neither is in the
  shipped binary, which is what the row is about; the check already excluded dev
  and build edges on exactly that reasoning. Nine of that 86 were our own crates,
  and five more were `bytemuck_derive` and the `syn` chain beneath it — which was
  also the source of a reported `syn` duplicate that exists only because `swash`
  and `wgpu` pin different major versions of a compile-time crate.
