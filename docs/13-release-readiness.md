# Public alpha release readiness

Release scope: `0.1.0-alpha.1`, with the current feature set. Updated 2026-09-09.
This record describes local evidence; it does not claim that remote CI, a tag,
or publication has occurred. The unfinished roadmap is outside this alpha.

## Fixes completed

The initial review fixes cover filesystem replacement and symlink safety, undo
and replay recovery, bounded protocol decoding, helper separation and accurate
partial-operation reporting. Release builds now unwind worker panics so recovery
works in the production profile, verified by a real release executable.

Archive parsing validates tar checksums, payload/padding lengths, PAX framing,
and compression trailers. Piped decompressors must exit successfully. ZIP
central-directory corruption cannot silently truncate the reported member list;
legacy ZIP names use CP437 and flagged UTF-8 is validated. Archive byte totals
saturate rather than overflow. Unsupported tar name encodings, sparse/PAX
variants and ZIP compression/encryption variants are explicitly refused instead
of being extracted under a misleading name or reported as a missing program.
Malformed PAX fractional times are refused and negative times round down.

Helpers require seccomp, resource limits and no-new-privileges. The syscall
filter denies networking and selected process/kernel interfaces. Landlock
executable grants now name only resolved external tools and their ELF
interpreters, excluding writable output trees. The live-kernel tests prove both
successful dynamic execution and denied output execution/input truncation.
The exact policy and its limits are in [ADR 0034](adr/0034-helper-execution-and-network-rights-are-explicit.md)
and [SECURITY.md](../SECURITY.md). Landlock absence still means reduced filesystem
isolation; helpers can read ordinary user-readable files even with Landlock.

User-installed fonts now win equal matches over system fonts, as documented.
The golden harness fingerprints the PDF Base-14 substitute as well as UI fonts.
GUI tests retain failure logs, count every diagnostic response, and wait for
painted menu/palette hit targets. Headless Sway provides a reproducible Wayland
backend alongside Xvfb; its virtual input devices persist between script batches.
No golden reference images were changed to accommodate these fixes. Fractional
Wayland testing also exposed a first-frame scale race: the transparent startup
probe now commits the configured logical size and waits for a real preferred-scale
event, bounded at 100 ms for compositors which never answer. Two sync replies
were not evidence that Sway had completed its layout transaction.

Distribution archives include dependency and Rust standard-library notices,
build identity, measured GLIBC requirements, checksums and exact-file installers.
The generated notice graph covers 141 external packages across both Linux
architectures. Packaging rejects missing helpers and mismatched versions or ELF
architectures. Reusable release gates now include the filesystem workflow,
mandatory injected-fault results, live-kernel confinement, and GUI backend/scale
matrices. The standalone filesystem spike's stale lockfile is repaired.

## Local validation

The tested environment is x86-64 Ubuntu 24.04 userspace, llvmpipe/Vulkan software
rendering, private Xvfb and headless Sway sessions. Rust 1.96.1 is the CI pin;
Rust 1.90 is the declared minimum. Tests were run outside the tool sandbox when
sockets or process isolation were required.

| Check | Result |
| --- | --- |
| `tools/check` | All gates passed: formatting, clippy with warnings denied, repository rules, package/notice checks, release panic recovery and 86 golden frames. The suite was 980 tests at that run and is 996 now, five explicitly ignored; the added tests cover trash recovery and queue retention and were run separately. Golden frames need the recorded font bytes, so only a font-matched host reproduces that part. |
| ARM cross-check | All workspace targets compiled for aarch64 Linux with Rust 1.96.1; native execution remains a CI check. |
| Minimum Rust | `cargo check --workspace --all-targets --locked` passed on 1.90. |
| Supply audit | cargo-deny 0.20.2: advisories, licenses, bans and sources passed against the fetched database on 2026-09-09. |
| Independent archives | 24 valid/malformed fixtures, including damaged gzip/xz/bzip2 trailers, ZIP directory corruption and legacy names. |
| Real D-Bus | Private-daemon integration test passed separately. |
| Kernel confinement | Linux 6.8.0-139 guest: mandatory Landlock and seccomp tests passed, including a dynamically linked executable. |
| Filesystem guest | 80 passes, zero failures, five FAT limitations, five explicitly owed checks across rootfs/tmpfs/ext4/btrfs/XFS/FAT. All five injected faults passed. |
| Degradation injection | `SKERRY_DISABLE` forces absence at the eight runtime probes (vulkan, portal, helpers, udisks2, watch, notifications, names, and a named helper) — where `names` is the one that simulates a facility that *never answers* rather than one that is absent, because that is how a name service on a machine joined to a directory fails. A GUI scenario asserts the visible outcome on both a missing image decoder — the look ends in `refused` rather than `waiting`, and the status line names the unavailable facility — and a refused watch, which reports the stale listing and the refresh key. Both halves were confirmed to fail when the injection is removed. |
| Parser campaigns | 500,000 mutations each against tar, ZIP and the D-Bus wire on seed 20260909, and 20,000 each on the default seed 380535: no panic, no timeout, no memory-limit kill. Bounded mutation fuzzing over six valid seeds ([ADR 0040](adr/0040-bounded-parser-mutation-campaigns.md)), not coverage-guided; the markdown, `.desktop`, MIME and keymap parsers are not yet targets. |

The filesystem VM runs as UID 1000 after guest-only mount setup. Disk-full and
read-error cases leave no incomplete final file or temporary file; the partial
tree case preserves eight byte-exact completed files and reports twelve failures.
Short writes and metadata refusals are verified. Both btrfs and XFS reflinks
passed, and the release gate requires those results rather than accepting skips. Four owed checks are reflinks on filesystems without that facility; the
fifth is NTFS, which this guest does not provide. FAT cannot preserve POSIX
symlinks, hardlinks, xattrs or all names. These are explicit limitations.

| Desktop run | Result and follow-up |
| --- | --- |
| X11, 100% | 145 passed, zero failed, three explicit skips. |
| X11, 125% | 145 passed, zero failed, three explicit skips. |
| Sway, 100% | 140 passed, one palette test synchronization failure, seven skips. The corrected palette scenario passed separately. |
| Sway, 125% | 141 passed, one first-frame scale failure, six skips. The startup fix passed the scale/palette/keyboard/close checks; ten consecutive warm-start scale checks also passed. |

Re-run on 2026-09-09 after the degraded-runtime scenario and the two new
diagnostic fields were added: **X11 at 100%, 148 passed, zero failed, three
explicit skips** over 151 registered scenarios. Only that backend and scale were
re-run; the Sway and fractional rows above stand on their original sessions.

The full-run failures above were investigated and corrected, not discarded or
converted into skips. Follow-up runs target the changed behavior; they are not
represented as a fresh full-suite run.
The host cannot expose FUSE to the GUI, so its hung-mount scenario is explicitly
skipped. Five Sway scenarios inspect state during a held drag; Sway suppresses
F12 during that grab, so those diagnostic inspections abstain. They run on X11;
completed Wayland drags and GTK drag/clipboard interoperation still run.

The real x86-64 tarball passed checksum verification, installation into a prefix
containing spaces, executable/helper probes, byte comparison of all binaries and
notices, and uninstall preserving an unrelated file. Its measured GLIBC floor
is 2.39. Identical binary inputs produce identical archives; independent compiler
builds have not been shown to produce identical binaries.

## Reproduction

See [tools documentation](../tools/README.md#release-validation-additions) for
package/font prerequisites and disposable-kernel instructions. Core commands:

```sh
cargo build --release --workspace --locked
./tools/check
cargo deny --workspace check
python3 tools/check-archives.py
./tools/gui-test --x11
./tools/gui-test --x11 --scale=1.25
./tools/gui-test --sway
./tools/gui-test --sway --scale=1.25
python3 tools/package.py --arch x86_64 --output /tmp/skerry-artifacts
```

Select Rust 1.96.1 for the pinned gates. Golden frames require the recorded font
bytes, including the Ubuntu 26.04 emoji face and Nimbus Sans; a different font
package is not a reason to bless new reference pixels. Guest timing and shared
host timings do not establish the interactive performance budgets.

## Publication checks and declared limits

- Run the reusable release workflows on the exact committed revision. Native ARM
  tests and both packaged architectures still need their CI results; local x86
  evidence is not ARM validation.
- Verify downloaded package checksums, build provenance (`gh attestation verify
  <tarball> --repo EmilPopovic/skerry --signer-workflow
  EmilPopovic/skerry/.github/workflows/release.yml`) and installation on each
  advertised distro.
  Local binaries require GLIBC **2.39**. No claim of a 2.28 binary floor is made;
  achieving that floor requires an older build baseline and runtime testing.
- Confirm a private security-reporting channel before publication. The policy
  describes a safe fallback without inventing a contact address.
- Perform the final hardware/desktop smoke pass, including Orca and a real task.
  Software-rendered automated tests do not substitute for every compositor,
  driver, network filesystem or removable device. NTFS/NFS and the GUI hung-mount
  case are not covered by this local VM evidence.

[RELEASE-NOTES.md](../RELEASE-NOTES.md) is the user-facing companion to this
record and must ship with the release; it states the glibc 2.39 floor, the
recovery behaviour of trash and the operation queue, and the narrowed fuzzing and
`--bench` claims.

Known feature limits remain in the [README](../README.md#what-this-alpha-does-not-do):
accessibility actions/tables are incomplete, X11 lacks input-method/touchpad
protocols, encrypted volumes and some archive/preview formats are unsupported,
and recorded performance budget misses remain. AppImage, automatic updates and
musl distribution artifacts are later work. These limits must accompany the
alpha release; they are not claims of completed roadmap functionality.

Tagging, uploading and publication remain separate actions. A manual release
workflow builds reviewable artifacts; a tag must exactly match `v` plus the
workspace version. Existing release assets are not overwritten. Nothing in this
record authorizes publishing without the final checks above.
