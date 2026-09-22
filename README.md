# Skerry

A file manager for Linux, written in Rust, built on the premise that a file manager
is a **latency product and an interaction-design product** — not a feature checklist.

Status: **`0.1.0-alpha.1` — preparing for public pre-release.**
[RELEASE-NOTES.md](RELEASE-NOTES.md) is the user-facing summary: compatibility,
recovery behaviour and known limits. The
[release checklist](docs/13-release-readiness.md) records test evidence and remaining
publication checks. See the [security policy](SECURITY.md) for reporting and helper
isolation limits. Twelve
crates and three isolated helpers, unit and integration tests, GUI scenarios on
both Wayland and X11, and golden
frames. It is a file manager you can use for a day: a Details view, a grid and a
column view over a real directory at a **0.34 ms frame p99 with 200,000 entries**,
live under inotify; tabs and a split tree of panes; copy, cut, paste, duplicate,
rename, new folder and Move to Trash with **one undo stack over all of it** and a
visible history; a sidebar whose pins are lines in GTK's `bookmarks` file, so a
folder pinned here is pinned in Nautilus; search under a folder or over every local
filesystem, first result at **0.7 ms over 817,288 entries**; quick look at pictures,
PDFs — with selectable text — and source, with a highlighter and an editor; **looking
inside a zip or a tar and unpacking one as a single undoable gesture**, in a child
whose Landlock write restriction, where supported, limits writes to the destination; drag and
drop to and from GTK applications; a command palette and a context menu over one
registry; a settings panel over `Ctrl ,` that says where each value came from, and
that can make Skerry the desktop's handler for folders; and a keymap that is a file
you own. **Every action is reachable with the
keyboard alone and with the pointer alone**, and a test fails the build for one that
is not.

What is done and what is owed is in
[08-roadmap.md](docs/08-roadmap.md#m0--6-wk--platform-layer). What an alpha does not
have yet is [below](#what-this-alpha-does-not-do).

## Installing

```
make                                   # cargo build --release --workspace
sudo make install                      # PREFIX=/usr/local, honours DESTDIR
make install PREFIX=$HOME/.local       # or into your own prefix
```

Four binaries and one desktop entry, and nothing else: no post-install script and
nothing written outside `DESTDIR` and `PREFIX`. The three helpers install beside the
binary because that is where it looks for them first. Tagged builds also produce a
tarball per architecture — see [release.yml](.github/workflows/release.yml).
Binary tarballs have their own Makefile and need no Rust installation; see the
[binary installation instructions](packaging/README.binary.md).

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
defect nothing else could: `tools/golden` compares eighty-three rendered frames to
committed references on lavapipe, `tools/gui-test` presses keys and moves a pointer
through 145 scenarios on two display servers — one of which mounts a filesystem in a
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
│   ├── skerry-preview-doc/        optional PDF/document rendering helper
│   └── skerry-archive/            sandboxed archive listing and unpacking
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
- **No encrypted volumes.** A disk appears in the sidebar whether or not anything
  has mounted it, and a press mounts it through udisks2 — but a LUKS container is
  not offered, because unlocking one is a passphrase field and its own surface.
- **Six settings, and no more.** `Ctrl ,` opens a panel listing the theme, hidden
  files, the sidebar and its width, what a new folder opens as, and whether Move to
  Trash asks first — each with its value and where that value came from. Everything
  else is a chord, a drag, or per-folder and remembered where you set it.
- **Four performance budget rows are missed**, and each is recorded with its number
  rather than widened: cold start, the 1M sort, one scroll run in five at a million
  entries, and the resident floor. See
  [09-performance-budgets.md](docs/09-performance-budgets.md#what-m0-measured).
- **No media or hex previews, and no 7z or rar.** Pictures — the right way up,
  from the EXIF tag — PDFs, source with a highlighter, **markdown rendered**, and
  **what is inside an archive** are what a quick look shows. The markdown parser
  is ours and deliberately small: anything it does not recognise, HTML included,
  draws as its own text rather than disappearing ([ADR
  0029](docs/adr/0029-markdown-renders-from-our-own-parser-and-unknown-syntax-is-its-own-text.md)).
  A 7z or a rar is listed as a sentence naming the format rather than as an empty
  panel — the line is that a container Skerry can parse it parses, and a
  compressor is the machine's ([ADR
  0032](docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md)).
- **No icon themes.** An application icon ships and the desktop entry names it,
  but nothing is resolved out of `index.theme`: the window draws its own built-in
  glyphs, and a file's mark is a page coloured by its MIME category rather than
  the themed icon the rest of the desktop would draw for it.
- **Creating things is thin.** New folder and a single rename. No new file, no
  Templates, no symlinks, and no batch rename — which M3 names and this is not.
- **Properties is read-only**, and it shows owners as numeric ids rather than
  account names. Nothing about a file's permissions can be changed from it.
- **Rebinding means editing the config file.** Every command and its chords are
  written to `$XDG_CONFIG_HOME/skerry/config` and a line rebinds one
  ([ADR 0025](docs/adr/0025-the-config-file-is-the-keymap-written-out.md)), but
  there is no panel that does it, no hot reload, and no sequences, aliases or vim
  layer. Rule 4 is not fully met, and saying so is better than a caveat that
  claims it is.
- **English only.** The catalogue is keyed from the first commit and every string
  goes through `t!()`, which is the groundwork — but it is compiled in, there is
  no `LANG` lookup, and the plural rule is English's. A contributed translation
  cannot be dropped in today.
- **Sorting, columns and selection are the basics.** Name, size and modified,
  three fixed columns with no picker, no sort by type or extension, and no invert
  or select-by-pattern.
- **The clipboard speaks GNOME and KDE, not the portal.** Both conventions are
  published and read, so a cut stays a cut in either direction
  ([ADR 0039](docs/adr/0039-file-clipboard-intent-belongs-to-one-offer.md)) —
  tested against a real GTK peer and a real Qt peer, which is not the same as
  testing against Nautilus and Dolphin themselves. Portal file transfers are not
  implemented, so a sandboxed Flatpak peer receives nothing.
- **A drag cannot ask.** The middle button opens a folder in a new tab and
  pastes the primary selection into a field — `Shift+Insert` does the second
  too — and a drag says whether it will move or copy. There is no right-drag
  menu, no link action by drag, and no drop onto an executable.
- **No "always open with this."** Open With opens once. The only default this
  writes is the folder handler, from the settings panel.
- **No recents.** `Ctrl+L` completes the directory being typed — `Tab` takes
  what the matches share, the arrows walk them — but remembers nowhere it has been.
- **Search has no ignore rules**, so a search over a checkout walks `.git`, and
  there are no extension facets. Both are M4.
- **No desktop notification** when a copy finishes with the window unfocused. The
  result and its warnings stay in the app.
- **Fuzzing covers tar, ZIP and the D-Bus wire only**, as bounded mutation
  campaigns rather than coverage-guided fuzzing
  ([ADR 0040](docs/adr/0040-bounded-parser-mutation-campaigns.md)). The markdown,
  `.desktop`, MIME and keymap parsers have adversarial unit tests and no campaign.
- **The binaries need glibc 2.39.** That is measured from the artifacts, not a
  target — it excludes Debian 12 and Ubuntu 22.04. Building from source on an
  older distribution is unaffected. See [RELEASE-NOTES.md](RELEASE-NOTES.md).

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

Apache-2.0: [LICENSE](LICENSE), with the attribution notice in
[NOTICE](NOTICE) that the licence asks a distribution to carry. Every source
file carries an
`SPDX-License-Identifier` line saying the same thing, and `tools/check` fails a build
for one that does not.

## Name

A skerry is a small rocky island — the kind you navigate around rather than into.
