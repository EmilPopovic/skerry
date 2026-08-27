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
| `golden` | Renders the interface on lavapipe and compares it to `tests/golden`, which is the only assertion here about *pixels* rather than about a number the program reports. `--bless` accepts what it renders now | Pillow, lavapipe, a release build |
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

Three drive the chrome above and beside the list: the four navigation buttons,
the sidebar's `F9` toggle, and a click on a sidebar row. The sidebar's contents
come from this machine, so those assertions are about the shape rather than the
contents — Home is always there, and clicking it lands in `$HOME`. Its row
positions are found by hovering down it rather than calculated, because a row's y
depends on how many section headings came before it.

One clicks a **breadcrumb** in the path bar, which is the first thing here whose
hit regions come from the frame that drew them: the width of a path component is
the shaped width of a directory name and is known nowhere else.

Two of them drive the window frame the program draws for itself — the one GNOME
requires, because mutter advertises no decoration manager at all. They start the
window with `--own-frame`, which asks the compositor for a client-side frame *and*
draws one whatever it answers, since Hyprland refuses the request. The pointer is
aimed with the compositor's own geometry rather than by nudging: a row has a name
to check against and a title bar does not.

One scenario goes further and reads the **pixels**, with `grim` and Pillow: it
walks a twenty-thousand-file subtree, waits for the model to report the total,
and then counts the ink in the size cell, because a dash is one stroke and
`146 KiB` is six glyphs. It exists because the defect it covers was a shaped run
served from a cache — every layer under the frame buffer had the right answer and
the screen did not. It skips itself where `grim` or Pillow is missing.

Each check abstains rather than guesses: with no crates yet, layering reports
"nothing to check" and exits zero. They exist before the code so that the first
crate cannot be the one that violates them.

## Golden frames

`tools/golden` renders seven frames — four scales, both palettes, the keyboard
overlay and a window too narrow for the metadata columns — and compares them to
committed references. It is what catches a glyph that moved half a pixel, a
colour that lost its alpha, a column that stopped shedding, and a row that draws
under the status bar at 200%.

**Rendered on lavapipe**, so the reference does not depend on whose GPU produced
it. Measured on this machine, the same frame differs by at most **two channel
levels of 255** between llvmpipe, RADV and the NVIDIA proprietary driver, and by
one between the two hardware drivers — blending into an sRGB target is not
bit-exact across implementations. The comparison allows exactly that much: a real
regression moves a glyph or changes a colour, which is tens or hundreds of levels
and never two. References blessed on lavapipe pass unchanged on both hardware
drivers, which is how the tolerance was chosen rather than guessed.

The fixture is at a **fixed absolute path** with modification times fixed in
2001, because the path bar and the `Modified` column are both part of the frame.
Its names are the adversarial set: right-to-left script, a combining acute, an
emoji, a name long enough to be elided, and two that are hidden.

It found a bug on its first run. `--capture` drained every folder walk and then
broke out of the loop *before* applying what the last one reported, so a frame
drew a dash for a folder whose total was already known — depending on which side
of the last drain the walk landed. Two runs of the same binary produced two
different images.

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
