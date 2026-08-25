# 0007 — Runtime capability probing, never build-time features

**Status:** Accepted

## Context

The usual way a Linux application handles environmental variation is Cargo features and
`#[cfg]`: build with systemd support, build with udisks2 support. That hands the decision to
whoever packaged the binary, which means the same version behaves differently on two
distributions and bug reports become unreproducible. It also makes "distro-agnostic" false
by construction — there is no single binary, only a family of them.

Every variation we actually face is detectable at runtime and cheap to detect: a D-Bus name,
a Wayland registry entry, a syscall returning `ENOSYS`, a file in `/run`, a binary on
`PATH`.

## Decision

One binary. Every environmental capability is probed at startup or first use and cached, and
every absent capability has a documented degradation — the table in
[06-portability.md](../06-portability.md) is the contract. Build-time features exist only
for things that genuinely cannot be probed: developer tooling and the `tracing`
instrumentation.

Each degradation is forced in CI through `SKERRY_DISABLE=…` rather than trusted to compile.

## Consequences

- Slightly more code than `#[cfg]`, and every fallback path is live code that must keep
  working — which is the point, since untested fallbacks are the ones that fail in front of
  users.
- Bug reports are reproducible: one binary, one behaviour, plus a capability report that
  `skerry --diagnose` prints.
- Packagers cannot accidentally ship a crippled build.
- Startup cost of probing is bounded: D-Bus name checks and registry walks are microseconds,
  and syscall probes happen on first use, not at launch.
