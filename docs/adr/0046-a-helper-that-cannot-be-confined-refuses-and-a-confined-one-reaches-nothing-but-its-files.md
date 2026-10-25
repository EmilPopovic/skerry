# 0046 — A helper that cannot be confined refuses, and a confined one reaches nothing but its files

**Status:** Accepted
**Supersedes:** the sentence of [0006](0006-out-of-process-decoders.md) that reads
"Where the kernel cannot sandbox (Landlock absent), helpers still run out of process
and the reduced isolation is logged — never silently accepted", and the sentence of
[0034](0034-helper-execution-and-network-rights-are-explicit.md) that reads
"Landlock remains a probed capability with reduced isolation reported, as 0006
requires". 0034's seccomp policy stands and is extended below.

## Context

0006 put every decoder in a helper and, where the kernel has no Landlock, chose to
keep decoding and say so. The security review of 2026-10-25 forced Landlock's three
syscalls to `ENOSYS` and watched what "say so" meant: the helper printed `landlock
ABSENT` to a `stderr` nobody reads, then decoded with every write right the user has.
A poppler or xz bug reached through a PDF preview can then write `~/.bashrc`. The
reduced isolation was never silently *accepted* — it was silently *used*, which is
the outcome the sentence existed to prevent.

This is not a rare kernel. [06-portability.md](../06-portability.md) names 5.10 as
the floor and Landlock arrived in 5.13; a kernel booted without `landlock` in `lsm=`
has none either.

The same review found that Landlock alone does not keep a compromised helper inside
its files. Landlock governs opening paths, not metadata, signals or keys, and
0034's deny-list covers none of those. Reproduced at ABI 8: `chmod 000` outside the
root, `utimensat`, `prlimit64` on another process, `SIGSTOP` to the window,
`keyctl` on the session keyring — and `kill(-1, SIGKILL)` follows from the same
gap. Below ABI 3 (kernels 5.13 to 6.1) truncation is not a Landlock right at all,
so any file the user can write can be emptied. And the helpers shared Skerry's
terminal and session: fd 2 inherited, so `TIOCSTI` before 6.2 and OSC 52 clipboard
escapes were one `write` away.

### What each option costs

**Keep the fallback and make it louder** — the window's `F2` line and a status-line
notice instead of `stderr`. It is the smallest change and it fixes the reporting.
It loses because a notice is read after the decode it describes: the file that
wrote `~/.bashrc` has written it by the time anybody sees the sentence.

**Refuse unconditionally.** The strongest answer, and it lost on the machines it
exists for. A Debian 11 desktop on its own kernel would have no thumbnails, no
previews and no archive listing, with nothing its owner could do about it short of
a new kernel. Whether to decode a download with full user rights is a trade the
person whose machine it is may make — so it is a setting, off by default, that
says what it costs.

**A second sandbox for kernels without the first** — a user namespace with
`pivot_root`. It is a fallback that is itself a probed capability: unprivileged user
namespaces are disabled or restricted by policy on several distributions, and the
helper's own seccomp filter denies `unshare` for good reason. Two sandboxes to keep
correct, for the machines least likely to have either, lost to one sandbox and an
honest refusal.

**A syscall allowlist instead of a longer deny-list.** 0034 declined it and its
reason still holds: the external renderers and decompressors are whatever version
the distribution ships, linked against whatever libc it ships, and an allowlist that
misses one syscall kills `pdftoppm` on somebody's machine with no error attached.
The deny-list grows by exactly what reaches past the helper's files.

**`RLIMIT_NPROC` against a fork bomb.** It counts every task of the real uid, not
the helper's children: 1,807 tasks of this user's uid on the development machine
while it was building, of which the helper is three. A cap low enough to stop a fork
bomb refuses `pdftoppm` on a busy desktop, and one high enough not to is no cap. The
thumbnailer starts nothing, so it is denied process creation outright; the other two
start one tool per request and are bounded by the host's deadline and their CPU cap.

## Decision

**A helper without usable Landlock refuses.** `sk_preview::sandbox::confine` applies
Landlock and, where the ruleset did not take, exits with status 78 before reading a
request — unless its environment carries `SKERRY_WITHOUT_LANDLOCK=allow`. Mandatory
no-new-privileges, resource limits and seccomp still exit with status 1, as 0034
decided.

**The opt-in is one setting**, `decode_without_landlock`, a config line and a panel
row under [0027](0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)'s
precedence, off by default. The host — `sk_preview::host` — sets the variable at
spawn only when the setting is on; it never passes one through from its own
environment.

**The host asks first, the helper decides.** Before spawning, the host asks the
kernel's Landlock ABI with the version query `landlock_create_ruleset(NULL, 0,
VERSION)`, which reads no file, and does not start a helper it knows would refuse. The
helper's own check is the one that holds, and its exit status is read as a refusal
too.

**The window says so.** A refused preview answers `Refusal::Unconfined` and a refused
archive `archive::Refusal::Unconfined`, each with its own sentence; the status line
says it once per run; and the `F2` capability line carries `sandbox=landlock-N`,
`sandbox=refused` or `sandbox=UNCONFINED`.

**What a confined helper may not do, beyond 0034's list**, in `sk_preview::seccomp`:

- change any file's mode, owner or extended attributes — the `chmod`, `chown` and
  `setxattr`/`removexattr` families, `fchmodat2` and the `*xattrat` calls included;
- set any file's times, except the archive helper unpacking, which stamps what it
  wrote with the archive's dates through `utimensat`;
- signal another process: `tkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` and
  `pidfd_send_signal` are denied, `tgkill` reaches only the helper itself, and
  `kill` reaches only the helper itself — except in the archive helper, which kills
  the decompressor it started and so may signal a positive pid, never a group;
- act on another process: `prlimit64`, `sched_setaffinity`, `sched_setparam`,
  `sched_setscheduler`, `sched_setattr`, `migrate_pages` and `move_pages` with a
  nonzero pid, and `setpriority`, `ioprio_set`, `process_madvise` and
  `process_mrelease` at all — the first two because a zero target with
  `PRIO_USER` is every process the user has;
- use the kernel keyrings: `keyctl`, `add_key`, `request_key`;
- below Landlock ABI 3, or without Landlock: truncate — `truncate`, `ftruncate`,
  `creat`, and `O_TRUNC` in `open`/`openat` flags are denied, and `openat2`, whose
  flags are behind a pointer, answers `ENOSYS`;
- in the thumbnailer, which runs nothing: `execve`, `execveat`, `fork`, `vfork`,
  and `clone` without `CLONE_THREAD`.

Where Landlock is ABI 6 or newer, the ruleset also scopes signals and abstract Unix
sockets to the helper's own domain, which is what keeps the archive helper's
decompressor killable and the window not.

**`RLIMIT_FSIZE` per helper**: one MiB for the thumbnailer, whose largest honest
write is a 128-pixel PNG under 80 KB; zero for the document helper and an archive
listing, which write nothing; unlimited for an unpack, whose entries are as large as
the archive says and whose disk is guarded by its free-space check. `SIGXFSZ` is
ignored, so a write past the ceiling is `EFBIG` rather than a dead renderer.

**A helper is its own session, with no terminal.** `confine` calls `setsid` first,
so the helper has no controlling terminal and its process group is its own pid — the
group the host's `kill(-pid)` already targets. The host no longer puts it in a group
of its own, because `setsid` refuses a group leader. Doing it in the child, not in a
`pre_exec` hook, is 0017's reason for every other step of confinement.

**`stderr` is a pipe the host drains** on the job that already holds the child —
after each preview batch, after an archive answer — and forwards to Skerry's own
`stderr` with every control character but a tab replaced. Not to 0041's log: a pipe
is not subject to `RLIMIT_FSIZE`, and a log file is, so a helper with a zero ceiling
could not write its own confinement line there.

**The environment is an allowlist** — `PATH`, `HOME`, the XDG base directories,
fontconfig's own variables, the locale and `RUST_BACKTRACE`, each named in
`sk_preview::host` with the reader that needs it. Nothing else crosses.

**A helper beside the binary that exists is the helper.** `PATH` is tried only when
nothing by that name is there; one that is there and fails to start is a failure,
not a reason to run a different binary.

## Consequences

- On a kernel without Landlock, a fresh install shows no thumbnails, previews or
  archive listings until somebody turns a setting on. The window says why and where
  the setting is; [06-portability.md](../06-portability.md)'s capability table says
  the same.
- Below ABI 3, any helper code that opens with `O_TRUNC` fails there. The thumbnail
  cache must be written through `O_EXCL` temporaries, and the thumbnailer's `--write`
  diagnostic cannot replace a file on those kernels.
- No helper can `chmod`. A writer that needs a mode sets it at creation.
- Below ABI 6, the archive helper can still signal another of the user's processes
  by pid. That is the price of killing its own decompressor without a scope, and the
  preview helpers, which reap rather than kill, do not pay it.
- A renderer that needs a variable off the list — `LD_LIBRARY_PATH` for a tool
  installed under a private prefix — does not get it, and fails as a missing tool.
- The filter grows from 75 instructions to between 239 and 285 on x86-64,
  depending on the helper and the ABI. A call it never inspects by
  argument — `read`, `mmap`, `write`, everything a decoder does in a loop — is
  answered from the kernel's per-syscall action cache without running it:
  `getppid` measured 47–53 ns a call against 48–51 ns under 0034's filter and
  41–42 ns unfiltered. A call it does inspect runs all of it: `kill` to the
  caller measured 167–322 ns against 147–242 ns and 128–238 ns. Median of seven
  runs of a million calls, three interleaved rounds, on the 24-thread
  development machine at a load average of 10 to 15 from other builds — so the
  spread is the machine's, and the order of the three is the finding.
- Every denial is asserted by a test that confines a disposable child and tries it,
  in the shape 0034 set.

## Revisit trigger

When [06-portability.md](../06-portability.md)'s kernel floor reaches 6.12 (Landlock
ABI 6), the below-ABI-6 signal rules and the below-ABI-3 truncation rules are dead
code and go. When an external tool needs a syscall this denies, the filter changes in
a new record — not the helper. When a supported distribution's default kernel ships
without Landlock in `lsm=`, the default of the setting is argued again in a new
record with that distribution named.
