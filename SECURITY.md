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

Untrusted image, document and archive parsing runs in helper processes. Each
helper leaves Skerry's session and terminal, gets a cleared environment with a
short allowlist, writes diagnostics to a pipe Skerry forwards with control
characters replaced, and requires resource limits (address space, CPU time and
file size), no-new-privileges and a seccomp filter. The filter denies
networking, signals and resource or scheduling changes aimed at other
processes and descriptor owners that would signal them, changes to any file's
mode, owner, extended attributes, inode flags and times, file leases, the
kernel keyrings, leaving the helper's process group, and selected kernel
interfaces; the
thumbnailer may not start processes at all. This is a targeted deny filter, not
a complete syscall allowlist.

**One exception reaches past an unpack's destination.** To give extracted files
the archive's dates, the archive helper may call `utimensat` while unpacking, and
because file times are not a Landlock right and its read access is the whole
filesystem, a compromised unpacker can set the access and modification times of
any file you own. It cannot change their contents, modes or owners.

Landlock restricts which files a helper may read, write and execute, with exact
executable/loader grants and writable output roots, and from Landlock ABI 6 it
also keeps a helper's signals inside its own processes. Below ABI 3 the seccomp
filter denies truncation, which Landlock cannot restrict there.

**Without usable Landlock, a helper refuses to decode.** Thumbnails, previews
and archive listing and unpacking stop, and the window says so. The setting
"Decode files without Landlock" (`decode_without_landlock` in the
config file) lets helpers decode anyway, with every filesystem right the user
has; turn it on only if you accept that a decoder exploit could then change your
files. Below Landlock ABI 6 the archive helper can still signal another process
of the same user, because it must be able to stop its own decompressor.

Ordinary user-readable files remain readable by helpers; their confinement is
not a guarantee that private home-directory data is inaccessible. The decisions
and their limits are in ADRs [0034](docs/adr/0034-helper-execution-and-network-rights-are-explicit.md)
and [0046](docs/adr/0046-a-helper-that-cannot-be-confined-refuses-and-a-confined-one-reaches-nothing-but-its-files.md).

External renderers and decompressors come from the user's PATH. Keep them and
the operating system updated. Unsupported archive variants are refused, and a
failed extraction may retain partial output which can be inspected or undone.

See [release readiness](docs/13-release-readiness.md) for dated validation and
remaining publication checks.
