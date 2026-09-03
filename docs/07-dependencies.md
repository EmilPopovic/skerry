# Dependencies

Rule 10. A crate enters the graph only when writing that code ourselves is demonstrably
worse, and the reasoning is recorded here. This file is normative: a dependency without a
row is a review blocker.

## The test a dependency has to pass

1. **Would we have to write this anyway?** Font shaping, yes. A colour crate, no.
2. **Is it doing one thing?** A crate that also brings a runtime, a logging framework or a
   thread pool brings the whole thing, not the part we wanted.
3. **What does it drag in?** Measured as transitive crates and as compiled bytes, before
   the decision, not after.
4. **Can it live behind a seam?** If it can sit behind a trait we own, a bad bet later costs
   a swap instead of a rewrite. Every tier-2 entry below has one.
5. **Can it live in another process?** If so it should — see the helpers section.
6. **Does it build without a C toolchain?** If not, it belongs in a helper binary or
   nowhere.

## Tier 0 — the ones we would otherwise write badly

| Crate | Why | Seam |
| --- | --- | --- |
| `rustix` | Raw `getdents64`, `statx`, `copy_file_range`, `inotify`, `fallocate` with a `linux_raw` backend that bypasses libc entirely. Writing this by hand means hand-rolling syscall ABI per architecture — the exact work that makes a project non-portable. | `sk-fs` internals |
| `swash` | Shaping, font introspection and rasterization in one crate with a small tree. Text is the one thing in a file manager that is genuinely hard and genuinely solved. See [ADR 0004](adr/0004-swash-for-text.md). | `sk-text` |
| `wgpu` | Vulkan and GL behind one API, with the driver-quirk knowledge we would otherwise accumulate by suffering. The heaviest entry in the graph and the one with explicit revisit triggers — [ADR 0003](adr/0003-wgpu-first-backend.md). | `sk-gpu::Renderer` |
| `xkbcommon` (Rust bindings) | Keyboard mapping is a specification with decades of edge cases. Bindings only; the protocol handling above it is ours. | `sk-plat` |

## Tier 1 — small, sharp, obviously worth it

| Crate | Why |
| --- | --- |
| `nucleo` | Fuzzy matching that is already faster than what we would write, with an API that matches our arena layout. |
| ~~`resvg` / `tiny-skia`~~ | Moved to tier 3: it is in a helper rather than in the product, which is what ADR 0006 wanted and better than this row assumed. |
| `thiserror` | Error enums. Zero runtime cost, no tree. |
| `serde` + `toml` | Config and state. The one place where hand-rolling is worse for users, because error messages matter. |
| `bitflags`, `memchr`, `smallvec` | Each a single file's worth of well-tested primitive. |
| `libc` | One function: `localtime_r`. A local time is a timezone database, a DST history and a leap-second table, and the machine already has all three; the alternative is parsing TZif ourselves or taking a date-time crate. It is already in the graph through `rustix` and `xkbcommon`, so it adds **zero** crates to the count. Confined to `sk-ui::format`. |

## Tier 3 — the decoder tree, and it is in no library

**None of these is in the `skerry` binary's graph**, which is
[ADR 0006](adr/0006-out-of-process-decoders.md)'s dependency argument made
mechanical: a decoder runs in a helper executable, a helper is a separate package,
and `tools/dep-budget.py` measures the product's tree. They are named here because
`declared()` walks every manifest — a dependency is argued once wherever it lands.

Each still buys the same three things ADR 0006 wanted out of process: an attack
surface that cannot reach the window, a tree that cannot reach the budget, and a
hang that costs one preview.

| Crate | Why | Where |
| --- | --- | --- |
| `png` | The format the XDG thumbnail cache is written in, so it is needed to *share* the cache whether or not anything else decodes. Pure Rust, no C, and the encoder half is what puts a thumbnail where Nautilus can read it. | `skerry-thumbnailer`, `skerry-preview-doc` |
| `zune-jpeg` | JPEG, which is most of the pictures on any machine. Chosen over `jpeg-decoder` for having no `unsafe` in the hot path and a smaller tree; either would do and the exit is the other one. | `skerry-thumbnailer` |
| `resvg` (with `usvg` and `tiny-skia`) | SVG, which is most of the icons and diagrams. Already named in tier 1 for a purpose it was never used for; this is that entry becoming real, in a helper rather than in the product, which is strictly better than the tier-1 row assumed. | `skerry-thumbnailer` |

**A GIF decoder is deliberately not here either**, and that is a different
argument from the document renderer below: `gif` and `weezl` are small, pure-Rust
and would have been an easy row — but the format is a header, a palette, a block
loop and LZW, and it has not moved since 1990. Four hundred lines of arithmetic
against two crates, in a helper whose tree is already the largest in the
workspace, is the trade this file exists to make explicitly rather than by
reflex: `bin/skerry-thumbnailer/src/gif.rs`, with a fixture written by *Pillow*
so the encoder under test is somebody else's.

A document renderer is deliberately **not** here. `skerry-preview-doc` drives
`pdftoppm` or `mutool` found on `PATH` — an external tool, which ADR 0006 already
provides for — because every pure-Rust PDF *renderer* is a research project and
every complete one is a C library that
[06-portability.md](06-portability.md) forbids linking into anything we ship by
default. A machine with neither tool gets `Refusal::NoRenderer` and says so.

## Tier 2 — accepted with a stated exit

| Crate | Why now | Exit |
| --- | --- | --- |
| `accesskit` | AT-SPI without becoming an AT-SPI expert. | None expected; the alternative is no accessibility, which is not an option. |
| `wayland-client`, `wayland-protocols`, `x11rb` | Protocol plumbing and code generation. The *policy* above them — DnD, clipboard, IME, scaling — is ours, which is the point of [ADR 0002](adr/0002-own-platform-layer.md). `wayland-client` is taken with `system` and `dlopen`: the first gives a real `wl_display` pointer for the GPU surface, the second loads `libwayland-client` at run time so an X11-only machine still starts. That second feature is rule 9 as a build decision and it costs three crates — see the table below. | None expected. |

**`zbus` was in this table and was declined when the code arrived.** The exit
condition written against it — "it brings an async executor, which sits uneasily
with [ADR 0005](adr/0005-no-async-runtime.md)" — was met on the day it would have
been added rather than later, and the in-house client this row priced at "roughly
1,500 lines" came in at 1,197 in `sk-desktop` including its tests, and 311 more in `sk-app` for what the messages mean:
[ADR 0023](adr/0023-d-bus-is-a-socket-we-speak-ourselves.md). A dependency whose
exit condition is already true on arrival is a dependency taken by inertia.

## What we write ourselves, on purpose

Each of these is a case where the available crate solves a more general problem than ours,
and generality costs latency or control:

- **The job pool and cancellation.** One pool for the process, with per-device queues. A
  `rayon` global pool alongside it would mean uncoordinated oversubscription.
- **Arenas, interning, bitsets, the columnar store.** These *are* the performance design;
  a general-purpose container crate would undo it.
- **The immediate-mode UI layer, layout, virtualized lists.** The differentiator.
- **shared-mime-info, `.desktop`, `index.theme`, `mimeapps.list`, `.trashinfo` parsing.**
  All small, all well-specified, all otherwise reachable only through GIO. Fuzzed in CI.
- **The thumbnail cache and its XDG metadata.**
- **D-Bus itself** — the socket, `EXTERNAL` authentication, the marshaller and
  the service side of `FileManager1`. `sk_desktop::bus`,
  [ADR 0023](adr/0023-d-bus-is-a-socket-we-speak-ourselves.md).
- **Natural-order collation and sort-key derivation.** Tightly coupled to the store layout.
- **Localization.** A `t!` macro over an embedded catalogue is a hundred lines; a
  localization framework is a dependency and a build step.

## Helpers: dependency isolation by process boundary

Optional and dangerous work lives in separate executables, so its dependency tree never
enters the main binary and a missing helper degrades one feature instead of failing a
build:

| Binary | Brings | Sandbox |
| --- | --- | --- |
| `skerry-thumbnailer` | `png`, `zune-jpeg` and `resvg` — by far the largest tree in the workspace, and none of it in the product; GIF is ours, for the reason above | Landlock and rlimits: read-only everywhere, **no execute right anywhere**, and the thumbnail cache the one thing writable. `seccomp` is named by [ADR 0006](adr/0006-out-of-process-decoders.md) and is not written; the child's own report line says so |
| `skerry-preview-doc` | Nothing but `png`. The renderer is a program on `PATH` — `pdftoppm` or `mutool` — so a machine with neither loses one preview and builds the same | The same, except that execute is granted: a dynamically linked renderer needs it on its loader and every library it maps, so the grant is the root and what is load-bearing is that nothing is writable |
| *(external, optional)* | `ffmpeg` for video thumbnails, discovered on `PATH` at first use | Spawned with a timeout and a memory cap |

This is the same boundary that rule 1 and the security model already wanted. Getting
dependency isolation from it is the argument that makes it obviously correct rather than
merely defensible.

## Test tooling

`tools/skerry-drive` replays keyboard and pointer input into a compositor so that
`tools/gui-test` can drive the real window. It is not in the product: the budget
above is measured by walking `skerry`'s graph and nothing else, so what a test
tool carries cannot hide inside it.

| Crate | Why |
| --- | --- |
| `wayland-protocols-misc` | `zwp_virtual_keyboard_manager_v1`, which is where the keyboard half of a replayed input trace comes from. |
| `wayland-protocols-wlr` | `zwlr_virtual_pointer_manager_v1`, the pointer half. Measured on this machine, `kwin_wayland` advertises neither protocol, so the fixture is Hyprland-only and the portability answer for KWin comes from running the product there by hand. |

Both are the same code generator as `wayland-protocols` and bring nothing new.

One scenario looks at pixels rather than at the state line, and it needs two
programs from outside the workspace: **`grim`**, to photograph the window through
the nested compositor's screencopy protocol, and **Pillow**, to crop and count. It
is worth the tooling because it is the only test in the repository that can see
what the window actually drew — the reported defect was a shaped run served from a
cache, so every layer below the frame buffer reported the right answer while the
screen showed a dash. `tools/gui-test` skips that scenario when either is
missing and says so; nothing else in the harness depends on them.

## What the accepted set actually costs

Measured with `tools/dep-budget.py` on the first commit that had crates in it, at
M0 on Wayland. Each row is the set reachable **only** through that dependency,
excluding workspace members and proc-macro crates — neither is in the binary,
which is what the row is about.

| Dependency | Subtree | What is in it |
| --- | --- | --- |
| `rustix` | 5 | `rustix`, `bitflags`, `errno`, `libc`, `linux-raw-sys` |
| `swash` | 8 | `swash`, `skrifa`, `read-fonts`, `font-types`, `bytemuck`, `zeno`, `yazi`, `once_cell` |
| `xkbcommon` | 4 | `xkbcommon`, `xkeysym`, `memmap2`, `libc` |
| `wayland-client` + `wayland-protocols` | 17 | the two, plus `wayland-backend`, `wayland-sys`, `downcast-rs`, `scoped-tls`, `smallvec`, `log`, and `dlib` + `libloading` + `cfg-if` for `dlopen` |
| `x11rb` | 5 | `x11rb`, `x11rb-protocol`, `as-raw-xcb-connection`, `gethostname`, `xcursor` — `libloading`, `cfg-if`, `once_cell`, `libc`, `rustix` and `bitflags` are already here and are not counted twice |
| **Union, deduplicated** | **32** | the non-GPU budget below |

Two numbers here are worth stating plainly rather than leaving implied.

**The 25 in the table below was two too small, and it was a guess.** It was
written before a single crate existed. The four tier-0 dependencies were each
accepted by an ADR that priced them in prose and not in crates, and their union
with the protocol crates is 27. The gate is now a **ratchet at the measured
value**: raising it needs a row here and a re-measure in the same commit. A
budget nobody has measured is decoration; a ratchet fails on the next addition
that arrives without an argument.

**The second display server costs five, and four of them were unavoidable.**
[ADR 0009](adr/0009-x11-backend-behind-a-runtime-selected-enum.md) prices it:
`x11rb` and `x11rb-protocol` are the crate, `gethostname` is a hard dependency of
it, and `as-raw-xcb-connection` comes with `dl-libxcb` — which is taken for the
reason `wayland-client` takes `dlopen` and which is also the only way to get the
`xcb_connection_t *` that `VK_KHR_xcb_surface` needs. The fifth, `xcursor`, is a
choice: X11 has no `wp_cursor_shape_v1`, and without a theme parser the pointer
is the core cursor font, which is a set of bitmaps from 1987. One crate for the
pointer looking like the rest of the desktop.

Two things were **not** taken and each saved a crate or a link-time dependency.
`gethostname` is a direct dependency of `x11rb` and is used through `rustix`'s
`uname` here rather than directly, and `xkbcommon`'s `x11` feature — the textbook
way to fetch a keymap from an X server — would link `libxkbcommon-x11` at build
time, so a machine without it would fail to start `skerry` even on Wayland. The
keymap comes from `_XKB_RULES_NAMES` instead.

**`dlopen` costs three of the 32 and is not negotiable for a number.** Linking
`libwayland-client` at load time would put the count at 24 and inside the
original budget — and would make the binary refuse to start on an X11-only
machine that has no Wayland libraries, which is verbatim the failure condition
rule 9 names. Loading it at run time *is* the capability probe
[ADR 0007](adr/0007-runtime-capability-detection.md) requires.

Two things the count deliberately excludes, both for the same reason the existing
check already excludes build and dev edges — they are not in the shipped binary:

- **Workspace members.** Counting our own crates against a dependency budget
  would mean the crate table in [04-architecture.md](04-architecture.md) consumes
  a third of the budget before an external crate is chosen. The first run against
  a real workspace reported 86 against a budget of 25, and 9 of those were ours.
- **Proc-macro crates.** `bytemuck_derive` and the `syn`/`quote`/`proc-macro2`
  chain under it run in the compiler. Excluding them also removes a reported
  `syn` duplicate that exists only because `swash` and `wgpu` pin different major
  versions of a compile-time crate.

`x11rb` has not landed yet, and M0 is not finished without it. When it does, that
commit raises the ratchet to the number it measures.

## Budgets and enforcement

| Gate | Limit | Tool |
| --- | --- | --- |
| Transitive crates, main binary excluding the GPU stack | **27**, ratcheted at the measured value | `cargo tree` in `tools/dep-budget.py`; excludes workspace members and proc-macro crates |
| Transitive crates, main binary total | **120** | same — the GPU stack dominates; if it exceeds this, [ADR 0003](adr/0003-wgpu-first-backend.md)'s revisit trigger fires |
| Duplicate versions of any crate | **0** | `cargo tree --duplicates` |
| C toolchain required for `skerry` | **no** | `tools/no-cc`, and the `no-c-toolchain` job |
| Licences | Permissive only; copyleft rejected at the gate | `cargo-deny` |
| Advisories | No known-vulnerable versions | `cargo-deny` |
| New dependency | Requires a row in this file in the same commit | Review checklist, plus a CI diff check on `Cargo.lock` |

**The C-toolchain row does not say "clean-container build" any more, because a
container cannot answer it.** `rustc` drives linking through `cc` on Linux, so an
image with no compiler fails at the link step and says nothing about whether a
dependency *compiled* anything. What separates the two is that the `cc` crate
honours `$CC` and `rustc`'s linker does not, so `tools/no-cc` points `$CC` at
something that refuses and runs `cargo check` — no linking at all — in a cold
target directory, because a warm one re-runs no build scripts and a gate over
build scripts that does not run them passes by not looking.

The answer today is no, and it is **contingent rather than structural**:
`wayland-backend` compiles two C shims when its `log` feature is on, and `sk-plat`
takes `client_system,dlopen` without it. That is exactly the kind of thing that
changes under somebody enabling a feature for an unrelated reason, which is why
it is a gate and not a paragraph.

The budgets are deliberately tight enough to hurt occasionally. That is the mechanism: a
dependency that is worth breaking a budget for is worth an ADR, and one that is not gets
declined without a debate.
