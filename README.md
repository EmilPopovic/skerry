# Skerry

A file manager for Linux, written in Rust, built on the premise that a file manager
is a **latency product and an interaction-design product** — not a feature checklist.

Status: **M0, in progress.** Phase 0's six spikes are resolved and there is a window
on screen: twelve crates, a Wayland platform layer, a GPU renderer, real text shaping,
and a Details view over a real directory that stays live under inotify, shows folder
totals as they are computed, filters as you type, navigates by breadcrumb and by typed
path, makes and renames and trashes things with one undo stack over all of it, opens
files in whatever the desktop says owns them, copies files, paths and the text
inside a text file to the clipboard, lists the
volumes as they are mounted, and draws its own window frame where the compositor
declines to. **Every action is reachable with the keyboard alone and with the pointer
alone**, and a test fails the build for one that is not. What is done and what is owed
is in [docs/08-roadmap.md](docs/08-roadmap.md#m0--6-wk--platform-layer).

```
cargo run --release -- ~/Downloads      # a window; F1 lists the keyboard
cargo run --release -- --bench 900 /usr/lib     # a scripted scroll, timed
cargo run --release -- --capture frame.png --scale 125 /usr/lib
```

The last two need no display server, which is the point of them: the renderer is
inspectable and measurable in CI. At 200,000 entries the frame p99 is **0.34 ms**
against an 8 ms budget, and an idle window uses **no CPU at all** — the numbers, the
method, and the three budget rows that are currently missed are in
[docs/09-performance-budgets.md](docs/09-performance-budgets.md#what-m0-measured).

Three harnesses assert about things a unit test cannot reach, and each of them found a
defect nothing else could: `tools/golden` compares fourteen rendered frames to committed
references on lavapipe, `tools/gui-test` presses keys and moves a pointer through
fifty-four scenarios in a nested compositor — one of which mounts a filesystem in a
namespace of its own to prove the sidebar notices — and `spikes/e-operations` is a
filesystem torture matrix that is a CI job rather than thrown-away spike code.
Two of the three run on every push; the scenarios need a compositor that will hand out
the virtual-input protocols, which a runner has no seat to do.

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

## Working on it

All six Phase 0 spikes are resolved and M0 is in progress: eleven crates, 384 tests, a
window on screen over a real directory. What is done and what is owed is the table in
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

## Name

A skerry is a small rocky island — the kind you navigate around rather than into.
