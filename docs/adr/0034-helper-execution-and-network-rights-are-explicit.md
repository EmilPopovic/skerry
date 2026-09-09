# 0034: Helper execution and network rights are explicit

## Context

[0006](0006-out-of-process-decoders.md) called for syscall confinement, and
[0032](0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md)
restricts archive writes. Granting Landlock execute access on `/` to accommodate
external renderers also permits executing extracted files. Separating writable
grants cannot subtract an execute right already granted by an ancestor.

## Decision

Resolve the known external tools from PATH before confinement and grant execute
access to those regular files and their ELF interpreters. Shared libraries need
read access, not a blanket executable-directory grant. Script-based replacements
are unsupported by this policy; missing tools continue to produce the existing
capability refusal. No executable directory receives a grant.

Add an inherited seccomp filter, checked against the native syscall architecture,
which denies socket creation/connection, process inspection, kernel-programming
and mount/namespace entry points. Reject alternate syscall ABIs. This is a targeted
restriction, not a claim to a complete syscall allowlist. Ordinary file, memory,
thread and external-renderer operations remain available subject to Landlock and
resource limits. No new dependency or scheduler is introduced.

The kernel interface is documented in
[Landlock](https://docs.kernel.org/userspace-api/landlock.html) and
[seccomp BPF](https://www.kernel.org/doc/html/latest/userspace-api/seccomp_filter.html).
Mandatory no-new-privileges, resource limits and seccomp failures stop the helper.
Landlock remains a probed capability with reduced isolation reported, as 0006
requires; it must not be advertised as active when unavailable.

## Consequences

Tests must run confinement in disposable child processes and demonstrate both
denials and working external tools. The release record must distinguish tests
with Landlock active from tests on kernels without it. PATH programs and loaders
must be installed outside the requested writable output; overlapping paths are
not granted execution.

## Revisit trigger

An external tool requires a denied syscall, a new architecture is supported, or
confidentiality requires restricting helper reads to each individual request.
Review and test that policy before broadening grants.

## Status

Accepted, 2026-09-08. Refines 0006 and 0032 without changing the feature set.
