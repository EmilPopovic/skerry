# Public alpha release readiness

This is the release record for `0.1.0-alpha.1`, updated 2026-09-07. The working
tree has passed the checks described below; that is not evidence that artifacts
have been published or that the outstanding host-dependent checks have passed.
The scope is a first public alpha, not completion of every roadmap milestone.

## Changes made during release preparation

- Release builds unwind panics so the worker pool can recover as its contract
  promises. `tools/check` runs a real release executable which panics in a worker
  and then proves another job on the same device runs. A release test harness
  alone would not verify the production panic strategy.
- Tar parsing checks each header checksum, rejects truncated blocks and payloads,
  frames PAX records by byte length, and uses PAX size overrides for both payload
  and padding. Unsupported sparse formats are refused. Extraction reports skipped
  entries as partial output; its warning accurately says that output is retained
  and can be undone.
- D-Bus decoding bounds nested variants and arrays, validates signatures, and
  rejects malformed strings, booleans and unconsumed message bodies. The relevant
  protocol constraints are in the [D-Bus specification](https://dbus.freedesktop.org/doc/dbus-specification.html).
  Tar header checks follow the [GNU tar format documentation](https://www.gnu.org/s/tar/manual/html_node/Standard.html).
- Binary distributions have a source-independent installer, exact-file uninstall,
  SHA-256 sidecar, build identity and measured GLIBC symbol requirement. Packaging
  rejects missing helpers and mismatched versions, tags or ELF architectures.
  Identical binary inputs produce identical archives; independent compiler/linker
  builds have not been shown to produce identical binaries.
- Release packaging depends on the reusable check workflow. GUI and frame jobs
  build all helpers, and the frame job runs independent archive fixtures. The
  dependency auditor is pinned to the tested cargo-deny 0.20.2. Its permissive
  allowlist now includes the BSD-3-Clause license of the existing tiny-skia crates.

## Reproducing the local checks

Run from the repository root with Python 3.11+, make, readelf and the Rust
toolchain available:

```sh
./tools/check
cargo deny --workspace check
cargo build --release --workspace --locked
python3 tools/check-archives.py
dbus-run-session -- cargo test -p sk-desktop bus::tests::talks_to_a_real_bus -- --ignored --exact --nocapture
python3 tools/package.py --arch x86_64 --output /tmp/skerry-artifacts
```

The local run used Rust 1.98.1 on x86-64 Linux. Formatting, clippy with warnings
denied, repository invariants, 976 passing workspace tests (five ignored), the production panic-recovery
example, three packaging tests and eleven independent archive cases passed.
The real D-Bus daemon test passed separately. A full cargo-deny run passed
advisories, licenses, bans and sources against the fetched RustSec database.
This is a dated audit result, not a guarantee about future advisories.

The actual release tarball was checksummed, installed into a temporary prefix
containing spaces, and its executable ran `--version`. All four installed binary
contents matched the archive. Uninstall preserved an unrelated license-directory
file. This host's binaries require GLIBC symbols through **2.39**; they do not
establish the portability document's intended 2.28 floor.

Local omissions were explicit: no Pillow for golden frames, no Xvfb for GUI
scenarios, no usable FUSE rig, and no enabled Landlock for live denial tests.
The Rust toolchains pinned by CI and the ARM runner were not available locally.
An `ok` test that internally skips a missing facility does not close that check.

## Remaining work before publication

| Requirement | Completion evidence still needed |
| --- | --- |
| Validate the exact release commit | Green reusable check workflow: pinned Rust, MSRV, ARM, golden frames, normal/fractional X11 scenarios and supply audit. Run the filesystem torture workflow on that same commit as well. |
| Validate supported desktop sessions | Wayland scenarios and real desktop smoke tests: opening, clipboard/drag interop, document previews, keyboard use and accessibility. Record compositor, GPU/driver and scale. |
| Exercise real filesystem failures | Cross-filesystem move/undo, unavailable trash, full disk, permissions, interrupted operations and stalled FUSE/NFS mounts on disposable fixtures. Retained partial work is not a transactional rollback guarantee. |
| Finish the helper threat model | Run denial tests on a Landlock-enabled host. Archive and document helpers currently grant executable access from `/` to support external programs and loaders; writable-output exclusion alone does not prevent execution there. Narrow the effective grants and validate distro loader compatibility. Network/syscall confinement also needs an explicit policy and implementation; process separation is not a complete sandbox. |
| Expand archive integrity coverage | Test damaged compression trailers and external decompressor failure after a complete tar payload. The current tar walker stops at its end marker and piped readers do not validate child exit status. Also cover remaining PAX/global-header semantics and non-UTF-8 member names before claiming broad tar compatibility. |
| Include dependency distribution notices | The license audit checks allowability, not completeness of redistributed copyright/license notices. The tarball currently includes only Skerry's LICENSE and NOTICE. Generate and verify notices for the shipped dependency graph and bundled assets before publication. |
| Establish binary compatibility | Test each packaged architecture on the oldest distro actually advertised. Build on an appropriately old baseline if claiming glibc 2.28; otherwise publish the measured requirement. Test runtime libraries and helper discovery from the extracted package. |
| Publish accurate alpha notes | Carry forward the known limits in the README and performance budgets. Record tested systems, unresolved defects and a confirmed private security-reporting route. Do not describe reduced helper isolation as complete confinement. |

AppImage, automatic updates, musl binary artifacts and the unfinished roadmap are
not implied by the tarball release. They can remain later work if the release
notes state that scope. The security, integrity and distribution requirements
above remain open; this document does not authorize publishing around them.

## Preparing and attaching artifacts

Update the workspace version and release notes, commit the changes, and run the
checks on that commit. The tag must be exactly `v` followed by the workspace
version. The release workflow validates that relation before packaging.

A manual workflow run validates and uploads workflow artifacts without attaching
them to a public release. A tag run uses `gh release upload`, so the corresponding
GitHub release must already exist (a draft is suitable while reviewing assets).
Existing assets are not overwritten. Download and verify both architectures and
their checksums before publishing the draft. Tagging, uploading and publication
are separate release actions; none were performed during this local preparation.
