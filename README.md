# Skerry

A file manager for Linux, written in Rust, built on the premise that a file manager
is a **latency product and an interaction-design product** — not a feature checklist.

Status: **`0.1.0-alpha.1` — a public pre-release.** Twelve crates and two sandboxed
helpers, 801 tests, 137 GUI scenarios on both Wayland and X11, seventy-five golden
frames. It is a file manager you can use for a day: a Details view, a grid and a
column view over a real directory at a **0.34 ms frame p99 with 200,000 entries**,
live under inotify; tabs and a split tree of panes; copy, cut, paste, duplicate,
rename, new folder and Move to Trash with **one undo stack over all of it** and a
visible history; a sidebar whose pins are lines in GTK's `bookmarks` file, so a
folder pinned here is pinned in Nautilus; search under a folder or over every local
filesystem, first result at **0.7 ms over 817,288 entries**; quick look at pictures,
PDFs — with selectable text — and source, with a highlighter and an editor; drag and
drop to and from GTK applications; a command palette and a context menu over one
registry; and a keymap that is a file you own. **Every action is reachable with the
keyboard alone and with the pointer alone**, and a test fails the build for one that
is not.

What is done and what is owed is in
[08-roadmap.md](docs/08-roadmap.md#m0--6-wk--platform-layer). What an alpha does not
have yet is [below](#what-this-alpha-does-not-do).

![Skerry](captures/dark.png)

## Installing

```
make                                   # cargo build --release --workspace
sudo make install                      # PREFIX=/usr/local, honours DESTDIR
make install PREFIX=$HOME/.local       # or into your own prefix
```

Three binaries and one desktop entry, and nothing else: no post-install script and
nothing written outside `DESTDIR` and `PREFIX`. The two helpers install beside the
binary because that is where it looks for them first. Tagged builds also produce a
tarball per architecture — see [release.yml](.github/workflows/release.yml).

Building needs a Rust toolchain at or above the `rust-version` in
[Cargo.toml](Cargo.toml) and `libxkbcommon`'s development package for the linker
symlink. Nothing else: no GLib, no GTK, no Qt.

```
cargo run --release -- ~/Downloads      # a window; F1 lists the keyboard
cargo run --release -- --bench 900 /usr/lib     # a scripted scroll, timed
cargo run --release -- --capture frame.png --scale 125 /usr/lib
```

The last two need no display server, which is the point of them: the renderer is
inspectable and measurable in CI. At 200,000 entries the frame p99 is **0.34 ms**
against an 8 ms budget, and an idle window uses **no CPU at all** — the numbers, the
method, and the four budget rows that are currently missed are in
[docs/09-performance-budgets.md](docs/09-performance-budgets.md#what-m0-measured).

Three harnesses assert about things a unit test cannot reach, and each of them found a
defect nothing else could: `tools/golden` compares seventy-five rendered frames to
committed references on lavapipe, `tools/gui-test` presses keys and moves a pointer
through 137 scenarios on two display servers — one of which mounts a filesystem in a
namespace of its own to prove the sidebar notices — and `spikes/e-operations` is a
filesystem torture matrix that is a CI job rather than thrown-away spike code. All
three run on every push, the scenarios under `Xvfb`; the Wayland half of them stays
local, because a compositor that hands out the virtual-input protocols needs a seat
and a runner has none.

## The thesis in one paragraph

Linux has no shortage of file managers. It has no file manager that is *simultaneously*
GPU-composited with a real frame budget, non-blocking on every filesystem call,
keyboard-first **and** excellent with a mouse, and discoverable rather than merely
configurable. Nautilus is minimal by policy; Dolphin is powerful with no discovery
surface; Thunar and Nemo carry a toolkit's scroll behaviour; Yazi and lf are the closest
in spirit but terminal-bound and hostile to a pointer; Krusader is frozen in 1998.
Skerry aims at exactly that gap. The prior art it learns from is
[File Pilot](https://filepilot.tech/) on Windows — one developer, C, no dependencies,
a hand-written renderer, and a reputation built entirely on the fact that every
interaction resolves in one frame.

## Two constraints that shape everything else

**Distro-agnostic.** Skerry targets *Linux*, not a desktop environment and not a
distribution. No GLib, GTK, Qt, KIO, or GVfs anywhere in the dependency graph. No
build-time assumption that systemd, udisks2, xdg-desktop-portal, PipeWire, or any
particular compositor exists — every such integration is probed at runtime and degrades
to a documented fallback when absent. Interoperability comes from freedesktop *specs*
(trash, thumbnails, icon themes, desktop entries, shared MIME info, base directories),
which every desktop implements, rather than from any desktop's API. Wayland and X11 are
both first-class. x86-64 and aarch64 are both first-class. See
[docs/06-portability.md](docs/06-portability.md).

**No unnecessary dependencies.** Every crate in the graph has to justify itself against
the work of writing that code ourselves, and the justification is recorded. The
dependency budget is a number in CI, not a vibe. Optional capabilities (PDF rendering,
video thumbnails) live in separate helper executables so their dependency trees never
enter the main binary, and so a missing helper degrades one feature instead of failing a
build. See [docs/07-dependencies.md](docs/07-dependencies.md).

Neither constraint is asceticism for its own sake. A file manager that pulls in a
desktop's object system inherits that desktop's blocking-I/O conventions and its main
loop, which is precisely the thing this project exists to avoid.

## Documentation

Read in order; each document is self-contained enough to review on its own.

| Document | What it settles |
| --- | --- |
| [01-research-file-pilot.md](docs/01-research-file-pilot.md) | What File Pilot is, what it does, and which of its features actually carry its reputation |
| [02-positioning.md](docs/02-positioning.md) | The Linux landscape and the specific gap Skerry occupies |
| [03-principles.md](docs/03-principles.md) | Ten rules, each written so a violation is detectable |
| [04-architecture.md](docs/04-architecture.md) | Workspace layout, layering rules, threading model, data layout, extension points |
| [05-platform-integration.md](docs/05-platform-integration.md) | The Linux desktop integration work that has no Windows counterpart |
| [06-portability.md](docs/06-portability.md) | Distro-agnostic policy: kernel floor, runtime probing, packaging |
| [07-dependencies.md](docs/07-dependencies.md) | Dependency policy, the allowlist, and what we write ourselves |
| [08-roadmap.md](docs/08-roadmap.md) | Phase 0 spikes with go/no-go bars, then milestones M0–M8 |
| [09-performance-budgets.md](docs/09-performance-budgets.md) | The numbers that decide whether it worked, and the method behind each |
| [10-risks.md](docs/10-risks.md) | What is most likely to go wrong, and the mitigation for each |
| [11-non-goals.md](docs/11-non-goals.md) | What Skerry deliberately will not do |
| [adr/](docs/adr/) | Architecture decision records — the *why* behind each irreversible choice |

## Planned repository layout

```
skerry/
├── crates/
│   ├── sk-model/         level 0  core types: paths, entries, ids, selection
│   ├── sk-util/          level 0  arenas, small collections, string interning
│   ├── sk-plat/          level 1  Wayland + X11, input, clipboard, DnD, IME, monitors
│   ├── sk-fs/            level 1  enumeration, statx, watching, operations, trash
│   ├── sk-text/          level 1  font discovery, shaping, layout cache, rasterization
│   ├── sk-gpu/           level 2  device, glyph atlas, batching, damage tracking
│   ├── sk-index/         level 2  recursive walk, flattening, fuzzy match, size aggregation
│   ├── sk-desktop/       level 2  MIME, icon themes, launching, D-Bus, devices, portals
│   ├── sk-preview/       level 2  preview host + child protocol
│   ├── sk-cmd/           level 2  command registry, keymap, undo stack
│   ├── sk-ui/            level 3  immediate-mode layer, layout, virtual lists, a11y
│   └── sk-app/           level 4  panes, tabs, sessions, config, views
├── bin/
│   ├── skerry/                    the application
│   ├── skerry-thumbnailer/        sandboxed image decode, own dependency tree
│   └── skerry-preview-doc/        optional PDF/document rendering helper
├── spikes/                        throwaway measurement binaries, kept for their numbers
├── tools/                         the gates: layering, dependency budget, golden frames, GUI
├── .github/workflows/             what runs them on every push, and the matrix nightly
├── tests/                         cross-crate integration and filesystem torture matrix
├── docs/
└── .claude/                       agents, skills and hooks — see CLAUDE.md
```

Dependencies point strictly downward by level. A CI check parses the workspace manifests
and fails the build on any upward or sideways edge — see
[04-architecture.md](docs/04-architecture.md#layering-is-enforced-not-encouraged).

## What this alpha does not do

Stated here rather than discovered, because a pre-release that hides its gaps wastes
the first bug report on one of them.

- **Accessibility is new.** A screen reader can walk the window — the
  application, its frame, the listing and its rows with names, roles, states and
  extents, the sidebar, the status line, the open field and the open menu — over
  AT-SPI, and it is told when focus moves. What has *not* happened is a pass with
  Orca over a real task, which is
  [ADR 0026](docs/adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md)'s
  own revisit trigger. Tables, selections and actions are not implemented yet.
- **No input method or touchpad gestures on X11.** Both work on Wayland;
  `text-input-v3` has no X11 equivalent this program implements, and the capability
  is reported absent rather than pretended.
- **No mounting.** A volume's label and its free space are drawn without `udisks2`,
  but mounting and ejecting one are not written yet.
- **Settings are keys.** `Ctrl ,` writes the whole keymap to a file you own; anything
  else is a chord or a drag, and there is no preferences surface yet.
- **Four performance budget rows are missed**, and each is recorded with its number
  rather than widened: cold start, the 1M sort, one scroll run in five at a million
  entries, and the resident floor. See
  [09-performance-budgets.md](docs/09-performance-budgets.md#what-m0-measured).
- **No archive, media or hex previews.** Pictures — the right way up, from the
  EXIF tag — PDFs and text are what a quick look shows.

## Working on it

All six Phase 0 spikes are resolved. What is done and what is owed is the table in
[08-roadmap.md](docs/08-roadmap.md#m0--6-wk--platform-layer). [CLAUDE.md](CLAUDE.md) is
the working brief — the craft bar, the invariants that are expensive to undo, and which
document settles which question.

```
./tools/check     # links, layering, dependency budget, invariants, fmt, clippy, tests, frames
```

One script, run by a person before proposing a change and by
[check.yml](.github/workflows/check.yml) on every push — CI adds nothing of its own.
Each gate abstains where it has nothing to check, and says so rather than passing
quietly. See [tools/README.md](tools/README.md).

## Licence

MIT or Apache-2.0, at your option: [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE). Every source file carries an
`SPDX-License-Identifier` line saying the same thing, and `tools/check` fails a build
for one that does not.

## Name

A skerry is a small rocky island — the kind you navigate around rather than into.
