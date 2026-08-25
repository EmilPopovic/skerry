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
| `resvg` / `tiny-skia` | SVG icon rendering. Writing an SVG renderer to draw a theme's icons is not a file manager project. |
| `thiserror` | Error enums. Zero runtime cost, no tree. |
| `serde` + `toml` | Config and state. The one place where hand-rolling is worse for users, because error messages matter. |
| `bitflags`, `memchr`, `smallvec` | Each a single file's worth of well-tested primitive. |

## Tier 2 — accepted with a stated exit

| Crate | Why now | Exit |
| --- | --- | --- |
| `zbus` | D-Bus for FileManager1, udisks2 and portals. Correct SASL, marshalling and name handling for free. | It brings an async executor, which sits uneasily with [ADR 0005](adr/0005-no-async-runtime.md). It is confined to `sk-desktop` on its own thread. If it turns out to be the largest single chunk of the dependency tree, replace it with an in-house client — the protocol is a Unix socket, `EXTERNAL` auth and a marshaller, roughly 1,500 lines. |
| `accesskit` | AT-SPI without becoming an AT-SPI expert. | None expected; the alternative is no accessibility, which is not an option. |
| `wayland-client`, `x11rb` | Protocol plumbing and code generation. The *policy* above them — DnD, clipboard, IME, scaling — is ours, which is the point of [ADR 0002](adr/0002-own-platform-layer.md). | None expected. |

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
- **The D-Bus service side of FileManager1.**
- **Natural-order collation and sort-key derivation.** Tightly coupled to the store layout.
- **Localization.** A `t!` macro over an embedded catalogue is a hundred lines; a
  localization framework is a dependency and a build step.

## Helpers: dependency isolation by process boundary

Optional and dangerous work lives in separate executables, so its dependency tree never
enters the main binary and a missing helper degrades one feature instead of failing a
build:

| Binary | Brings | Sandbox |
| --- | --- | --- |
| `skerry-thumbnailer` | `image` and the codec crates — by far the largest tree in the workspace | seccomp + Landlock, no network, write access to the thumbnail cache only |
| `skerry-preview-doc` | A PDF/document renderer, built only if its toolchain is available | Same |
| *(external, optional)* | `ffmpeg` for video thumbnails, discovered on `PATH` at first use | Spawned with a timeout and a memory cap |

This is the same boundary that rule 1 and the security model already wanted. Getting
dependency isolation from it is the argument that makes it obviously correct rather than
merely defensible.

## Budgets and enforcement

| Gate | Limit | Tool |
| --- | --- | --- |
| Transitive crates, main binary excluding the GPU stack | **25** | `cargo tree` in `tools/dep-budget.py` |
| Transitive crates, main binary total | **120** | same — the GPU stack dominates; if it exceeds this, [ADR 0003](adr/0003-wgpu-first-backend.md)'s revisit trigger fires |
| Duplicate versions of any crate | **0** | `cargo tree --duplicates` |
| C toolchain required for `skerry` | **no** | Clean-container build in CI |
| Licences | Permissive only; copyleft rejected at the gate | `cargo-deny` |
| Advisories | No known-vulnerable versions | `cargo-deny` |
| New dependency | Requires a row in this file in the same commit | Review checklist, plus a CI diff check on `Cargo.lock` |

The budgets are deliberately tight enough to hurt occasionally. That is the mechanism: a
dependency that is worth breaking a budget for is worth an ADR, and one that is not gets
declined without a debate.
