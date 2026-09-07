# tools

Repository gates. Standard library Python and POSIX `sh` only — the same
dependency policy the product has, applied to the thing that enforces it.

| | Decides | Needs |
| --- | --- | --- |
| `check` | Runs everything below, then `fmt`, `clippy` and `test` once a workspace exists | — |
| `test-package.py` | Tests reproducible staging, version/architecture checks, and installation/uninstallation without Cargo | Python 3.11+, make |
| `package.py --arch x86_64` | Produces a binary tarball, ABI metadata and SHA-256 checksum; `--tag` verifies the release version | Python 3.11+, readelf, four release binaries |
| `check-archives.py` | Exercises the production archive helper with independently generated tar and ZIP fixtures, including malformed input and traversal | Python 3, release archive helper |
| `check-links.py` | Every relative link and heading anchor in the markdown resolves | — |
| `check-layering.py` | Crate dependency edges point strictly downward; `--diagram` prints the level diagram | manifests |
| `dep-budget.py` | Every dependency has a row in [07-dependencies.md](../docs/07-dependencies.md); transitive counts inside budget, with the GPU stack found by reachability from the `wgpu` edge | manifests, `cargo`, `rustc` |
| `check-invariants.py` | Banned crates, capability feature flags, crate lints, the GPU seam, ADR immutability | manifests, sources |
| `make-listing-fixture` | Not a gate. Builds a directory of N files so the frame and listing numbers in [09-performance-budgets.md](../docs/09-performance-budgets.md) have a method that can be re-run | — |
| `golden` | Renders the interface on lavapipe and compares it to `tests/golden`, which is the only assertion here about *pixels* rather than about a number the program reports. `--bless` accepts what it renders now | Pillow, lavapipe, a release build, the fonts in `tests/golden/faces.txt` |
| `gui-test` | Drives the real window with real input and asserts on the state `F12` prints, and in one scenario on the pixels. **Two display servers, the same scenarios:** a nested Hyprland by default, and `--x11` runs them against a private `Xvfb` through `XTEST` — which needs no seat, and is therefore the half that can reach CI. `--scale=1.25` sets a fractional scale, as the compositor's on Wayland and as `Xft.dpi` on X11, which is the only way one scenario can fail at all. Refuses to run if it registers fewer scenarios than `FEWEST_SCENARIOS`, or if a name on the command line matches nothing | `Hyprland` and `grim` for the Wayland half; `Xvfb`, `xwininfo`, `xrdb` and ImageMagick's `import` for the X11 half; Pillow, and a release build |
| `gtk-drag-peer` | A GTK application on the same display, for the two interop scenarios: one big drop target, or one big drag source. A **separate process** — GTK is banned by rule 9 and stays banned, but the thing being measured is interop with GTK, and Spike D made the same trade for the clipboard. Not Nautilus, because driving a file manager's own view means guessing where it drew an icon, which is exactly where Spike D's X11 half failed | `python3-gi`, GTK 3 |
| `skerry-drive` | The input half of `gui-test`: replays a script of keys, clicks, drags and scrolls into a display server — virtual-input protocols on Wayland, `XTEST` on X11, one playback function over both so a scenario presses the same sequence either way. Also `--paste MIME` and `--close`, which are the X11 answers to `wl-paste` and `hyprctl closewindow` | — |

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

`tools/golden` renders fourteen frames — four scales, both palettes, the keyboard
overlay, the context menu, the command palette, the find popup, a name being typed,
the path bar as a field at two widths, a window too narrow for the metadata
columns, and the frame GNOME requires — and compares them to committed references. It is what catches a glyph that moved half a pixel, a
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

Two inputs come from the machine and both are pinned: the timezone, because the
`Modified` column is `localtime_r` of a fixed second and west of UTC that second
is the previous day; and **the fonts**, which decide most of the pixels. Hebrew,
Arabic and an emoji are in the fixture on purpose, so the whole fallback chain is
in the image. `tests/golden/faces.txt` records that chain **by the hash of each
file**, and a run that would shape with anything else refuses to compare and says
which file changed. The hash rather than the name: Ubuntu 24.04 ships
`NotoColorEmoji.ttf` at 2.047 where 26.04 ships 2.051, and the beach umbrella is a
different bitmap in each. Without that, the frames only ever passed on one laptop.

It found a bug on its first run. `--capture` drained every folder walk and then
broke out of the loop *before* applying what the last one reported, so a frame
drew a dash for a folder whose total was already known — depending on which side
of the last drain the walk landed. Two runs of the same binary produced two
different images.

## What runs on a push, and what does not

[`.github/workflows/check.yml`](../.github/workflows/check.yml) calls `tools/check`
verbatim on every push and pull request, then the golden frames on lavapipe in a
job of their own, then `cargo check` on the oldest toolchain `rust-version`
claims. [`torture.yml`](../.github/workflows/torture.yml) runs Spike E's matrix
nightly, and on any change to it or to `sk-fs`, over btrfs, xfs, exFAT and vfat
that the job builds on loop devices for it — which is what makes *"M3 does not
ship until the Spike E matrix is green in CI"* a gate rather than a sentence.

Both workflows pin the runner image, the toolchain and every action by commit. The
image is pinned because it is an input: the golden frames need its font packages
to the byte.

Two things stay local deliberately. `tools/gui-test` needs a compositor that hands
out `zwp_virtual_keyboard_v1` and `zwlr_virtual_pointer_v1`, and a runner has no
seat to give one. And `--bench` on a shared runner measures the runner's
neighbours — [12-measurement.md](../docs/12-measurement.md#the-machine-must-be-idle-and-the-load-must-be-recorded)
prices a number taken on a loaded machine at 6× less, so the frame budget is
defended by a person on an idle machine and the regression gate is M8's.

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
