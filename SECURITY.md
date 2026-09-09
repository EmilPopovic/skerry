# Security policy

Skerry is preparing its first public alpha. Security fixes target the current
release and main branch; older alpha builds do not receive separate backports.

## Reporting a vulnerability

Use GitHub's **Report a vulnerability** action on this repository's Security tab
when it is available. If private reporting is unavailable, open an issue asking
for a private contact, without including the vulnerability, exploit, affected
private paths, or sample documents. A confirmed private channel is required
before the first public release; no security email address is advertised yet.

Include the Skerry version/build commit, distribution, kernel, desktop backend,
steps to reproduce, expected behavior, and impact. Minimize sample files and
remove personal information. Do not test against somebody else's files or system.

## Security boundaries

Skerry runs with the logged-in user's filesystem permissions. File operations
refuse unsafe replacements and preserve undo information where possible, but
undo needs usable trash and is not a backup or an atomic filesystem transaction.

Untrusted image, document and archive parsing runs in helper processes. They
require resource limits, no-new-privileges and a seccomp filter that denies
networking and selected process/kernel interfaces. This is a targeted deny
filter, not a complete syscall allowlist. Landlock adds filesystem restrictions
when available, including exact executable/loader grants and writable output
roots. Without usable Landlock, filesystem isolation is reduced and reported by
helper probes. Ordinary user-readable files remain readable by helpers; their
confinement is not a guarantee that private home-directory data is inaccessible.

External renderers and decompressors come from the user's PATH. Keep them and
the operating system updated. Unsupported archive variants are refused, and a
failed extraction may retain partial output which can be inspected or undone.

See [release readiness](docs/13-release-readiness.md) for dated validation and
remaining publication checks.
