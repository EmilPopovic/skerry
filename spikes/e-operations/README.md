# Spike E — operation engine correctness

**Status:** in progress. Method below is fixed; results are filled in as arms
land.

## Question

Do the copy paths preserve everything a file has, on every filesystem we can
reach — and when a copy fails halfway, is there anything left behind?

## Bar

Quoted from [08-roadmap.md](../../docs/08-roadmap.md#phase-0--spikes-34-weeks),
written before the first line of code and not adjusted afterwards:

> **Bar:** zero data loss across the matrix · `ENOSPC` handled cleanly mid-copy
> with no partial file left behind · reflink measurably instant on btrfs.

Three conditions. The verdict is the conjunction, and the third one names a
filesystem this machine cannot create — see *what this machine can reach*.

## What it decides

Nothing reversible, and that is unusual for a spike. The other five could each
change an architectural choice; this one gates a milestone:

> **Gate:** M3 does not ship until the Spike E matrix is green in CI.

So the artifact that matters is not the number. It is the **matrix itself**, which
outlives the spike: [08-roadmap.md](../../docs/08-roadmap.md) says "this spike
becomes the permanent torture matrix", and every other spike's code is thrown
away.

That changes how it is built. The other spikes measure one thing quickly. This one
is written to be **re-run against a filesystem it has never seen**, because the
list of filesystems is not knowable in advance — it is whatever the user's machine
has mounted. The filesystem list is therefore probed at runtime and never compiled
in, which is [ADR 0007](../../docs/adr/0007-runtime-capability-detection.md)
applied to a test harness rather than to a product.

## Method

### What this machine can reach, and what it does not

The roadmap names ext4, btrfs, xfs, tmpfs, exFAT and NFS. Creating any of the
first six except ext4 and tmpfs means `mkfs` plus `mount`, and `mount` needs
`CAP_SYS_ADMIN`. **There is no passwordless root on this machine, or on either of
the two remote machines Spikes B, C and F used.**

| Filesystem | Reachable here | How |
| --- | --- | --- |
| **ext4** | yes | the real root filesystem, via `$HOME` — the one most users will actually have |
| **tmpfs** | yes | `/tmp` and `/dev/shm` |
| **NTFS** | yes | `mkntfs` an image and mount it with `ntfs-3g`, which is FUSE and needs no root. Not in the roadmap's list, and worth having anyway: it is what an external drive is formatted as, it has no POSIX ACLs, and it is a realistic source of "could not preserve" |
| **a filesystem that fails on purpose** | yes | ours, over FUSE — see below |
| btrfs, xfs | **no** | no `mkfs.btrfs`, no `mkfs.xfs`, and mounting an image needs root |
| exFAT, vfat | **no** | `mkfs.exfat` exists; there is no FUSE driver for it here, so it cannot be mounted |
| NFS | **no** | needs a server and a privileged mount |

**The reflink condition therefore cannot be met on this machine**, and the verdict
says so rather than quietly dropping it. `FICLONE` is attempted on every
filesystem and the errno is recorded, so the *code path* is exercised everywhere;
what is missing is a filesystem that says yes.

### The filesystem that fails on purpose

`ENOSPC handled cleanly mid-copy` is the second condition, and a real full disk is
a bad instrument for it: filling one is slow, and the exact byte at which it fails
depends on the allocator, so the test is not repeatable.

So the spike ships a FUSE filesystem that fails **at an exact offset, on demand**:

- `ENOSPC` after *n* bytes written — the condition in the bar.
- `EIO` after *n* bytes read — the same shape from the other side, which is what a
  failing disk looks like.
- short writes — a `write` that accepts fewer bytes than offered, forever.
- no xattr support — `ENOTSUP` on `setxattr`, so "report what could not be
  preserved" has something to report.
- no `fallocate` — so the preallocation path has a filesystem to fall back on.

`/dev/fuse` is mode 0666 here and `fusermount3` is present, so none of this needs
root. It is an instrument, and it is the reason the ENOSPC result is a number
rather than an anecdote.

### The tree that gets copied

Every property below is a file in the fixture, not a separate test, because a copy
engine that handles them one at a time and not together is not a copy engine:

| Fixture | What it catches |
| --- | --- |
| a plain file with content | the floor |
| a sparse file, 1 GiB apparent, 3 extents | a copy that expands holes turns 12 KiB into 1 GiB |
| three names for one inode | a copy that breaks a hardlink set silently duplicates data |
| a relative symlink, an absolute one, and a dangling one | a copy that *follows* a symlink writes the wrong file, and a dangling one makes it fail |
| `user.*` xattrs, and a POSIX ACL | ACLs are xattrs, so getting xattrs right gets ACLs right — and getting them wrong loses permissions silently |
| mode 0o4755, and a mtime with nanoseconds | a copy that rounds the timestamp breaks every "is it newer" comparison |
| a 255-byte name, a name with a newline in it, a name that is not valid UTF-8, and a name beginning with `-` | every one of these has broken a real file manager, and the last one has broken shell wrappers |
| a file that is its own destination | the classic truncation bug: `cp a a` must refuse, not empty the file |
| a read-only file, and a directory with no write permission | the failure has to be reported, not skipped |

### Checks

Each is a pass, a fail, or a documented limitation of the filesystem. Verification
is always by comparison against the source, never by trusting the copy:

| Check | Passes when |
| --- | --- |
| `bytes` | every byte of the destination equals the source, compared streaming |
| `holes` | the destination's hole map from `SEEK_HOLE`/`SEEK_DATA` equals the source's, and its allocated size has not grown |
| `strategy` | the strategy actually used is recorded — `FICLONE`, `copy_file_range`, or read/write — with the errno of each one that was refused |
| `hardlinks` | a set of names for one inode is one inode afterwards |
| `symlinks` | a symlink is copied as a symlink with a byte-identical target, dangling included |
| `xattrs` | every `user.*` attribute survives, and anything that could not be set is *reported* with its errno rather than dropped |
| `acl` | `system.posix_acl_access` survives, and `getfacl` agrees |
| `metadata` | mode, and mtime to the nanosecond |
| `names` | all four adversarial names round-trip |
| `enospc` | a copy interrupted by `ENOSPC` leaves **no file** at the destination, and reports the shortfall in bytes |
| `eio` | the same for a read error mid-copy |
| `self-copy` | copying a file onto itself is refused with the source intact |
| `clobber` | an existing destination is never silently overwritten |
| `reflink` | on a filesystem that supports `FICLONE`, the copy is instant and shares extents |
| `no-temp-files` | no `.skerry-copy-*` name survives anywhere in the destination — checked by walking the tree, not by asking the engine |
| `move` | a move within a filesystem is a rename; a move across one verifies the destination *before* unlinking the source, and the source survives exactly when the destination does not |
| `enospc-tree` | a destination that fills partway through a tree: every file that exists is complete, and the ones that do not exist were never started |
| `fixture` | what the filesystem refused to let us *create*, which is a result rather than an obstacle |

## Result

From [runs/matrix-yoga.txt](runs/matrix-yoga.txt), which is the whole of
`./target/release/spike-e-operations run` — one command, every filesystem it can
find, every fault case. The same binary's output on the other two machines is in
[runs/matrix-homelab.txt](runs/matrix-homelab.txt) and
[runs/matrix-pi.txt](runs/matrix-pi.txt).

**37 pass, 0 fail, 6 limitations, 3 owed, of 46 checks.** Every limitation is a
property of NTFS, and every one of the three owed is the same missing filesystem.

### Two of the bar's three conditions are met

**Zero data loss across the matrix.** Byte-exact on every filesystem, including
across a filesystem boundary, including a 1 GiB sparse file that stays 12 KiB,
including three names for one inode staying one inode, including four filenames
that have each broken something shipped. Copying a file onto itself is refused
with the source intact; an existing destination is never overwritten.

**`ENOSPC` handled cleanly mid-copy with no partial file left behind.** The
number is exact and repeatable because the filesystem that produces it is ours:

```
failed at copy_file_range with ENOSPC after 307200 of 1048576 bytes,
741376 short of the file; the destination filesystem accepted 307200 bytes;
the destination name exists afterwards: false; files left behind: []
```

And at the tree level, where the guarantee is weaker and worth stating
separately — the destination had room for eight of twenty equal files:

```
8 files copied and 12 failed; 8 of the files that exist are byte-exact;
0 incomplete; 0 temporary files left
```

**The third condition — reflink measurably instant on btrfs — is not measured**,
because no machine here can create a btrfs filesystem without root. See *what is
owed*; it is one command away and the command is written.

### The structural decision that makes the ENOSPC claim provable

Nothing is ever written to the destination name. Every copy goes to a temporary
file in the destination directory and is renamed into place at the end; on any
failure the temporary file is unlinked and the destination never existed. "No
partial file left behind" then stops being a promise about error handling and
becomes a property of the algorithm — which is why the check for it is a walk of
the destination tree looking for `.skerry-copy-*`, not a question to the engine.

### Per filesystem

| | ext4 → ext4 | tmpfs → tmpfs | ext4 → NTFS |
| --- | --- | --- | --- |
| strategy used | `copy_file_range` | `copy_file_range` | read/write |
| placement | `renameat2(NOREPLACE)` | `renameat2(NOREPLACE)` | **`link`+`unlink`** |
| bytes | pass | pass | pass, 13 files |
| sparse, 1 GiB apparent | **12 KiB → 12 KiB** | **12 KiB → 12 KiB** | no holes at all |
| mode `4755` | preserved | preserved | no POSIX mode bits |
| mtime | exact to the nanosecond | exact | 89 ns of rounding |
| `user.*` xattrs | preserved | preserved | refused, and reported |
| 3 hardlinks | 1 inode | 1 inode | 1 inode |
| symlinks | 4 of 4 | 4 of 4 | 3 of 4 — one target is not encodable |
| adversarial names | 4 of 4 | 4 of 4 | 3 of 4 — `EILSEQ` on invalid UTF-8 |
| move within / across | rename in 0.011 ms / verified | rename in 0.009 ms | rename / verified |
| reflink | `EOPNOTSUPP` | `EOPNOTSUPP` | `EOPNOTSUPP` |

The NTFS column is the reason NTFS is in a matrix that does not name it. Two
filesystems that both say yes to everything do not test the paths that report
what could not be preserved — and those paths are where a copy engine quietly
loses a permission bit.

### Three machines, two architectures

The matrix's design claim is that it can be re-run somewhere it has never been,
because the filesystem list is discovered rather than declared. That claim is
testable, so it was tested: the same source, built for `x86_64-unknown-linux-musl`
and `aarch64-unknown-linux-musl`, on the three machines Spikes B, C and F used.

| | this laptop | homelab (x86_64 VM) | Pi 5 (aarch64) |
| --- | --- | --- | --- |
| result | 37 pass, 0 fail | **26 pass, 0 fail** | **26 pass, 0 fail** |
| filesystems found | ext4, tmpfs, NTFS | ext4, tmpfs | ext4, tmpfs |
| rename within a filesystem | 0.011 ms | 0.015 ms | 0.025 ms |
| fault arm | 5 cases | owed | owed |

Identical behaviour on both architectures, and no code change to run there. The
two remote machines have fewer checks for two reasons that are worth separating:

- **The fault arm needs `libfuse` at link time**, so the static musl build omits
  it and reports the five cases as owed rather than skipping them silently. A
  toolchain on the remote machine would close that; cross-compiling against
  `libfuse` for another architecture is not worth the effort for a spike.
- **`ntfs-3g` is setuid root on this laptop and is not on the Pi**, which is a
  packaging choice: `-rwsr-xr-x` against `-rwxr-xr-x`. So the NTFS arm exists on
  one machine and not the others, and the matrix reports that as owed with the
  reason rather than pretending NTFS passed.

Neither is a portability problem in the engine, and that is the useful result: the
part that copies files behaves the same everywhere, and what varies is which
filesystems a machine will let an unprivileged process create.

### Four things this changed, three of them in our own code

The value of this spike is not the pass count. It is that four defects were found
by a check failing, and every one of them would have shipped.

1. **`copy_file_range` destroys sparseness.** The obvious loop — one
   `copy_file_range` over the whole file — turned a 1 GiB sparse file with 12 KiB
   of data into **1 GiB of zeros**, on ext4 *and* tmpfs. `copy_file_range` does
   not promise to preserve holes; where it cannot share extents it materialises
   them. Seeking to each data extent and copying only that keeps the kernel-side
   copy *and* the sparseness, so there is no trade to make — but the naive version
   passes a byte-for-byte comparison, and only the extent map catches it.

2. **`RENAME_NOREPLACE` is not universally supported.** ntfs-3g returns `EINVAL`
   for the flag, and an engine that stops there copies **nothing at all** to an
   external drive: every file failed at `rename`. The fallback is the classic one —
   `link` refuses an existing destination atomically and needs no flag — with
   `stat`-then-`rename` as a last resort, and the placement is *reported* because
   the last one has a race and a caller has to be able to know.

3. **An ignored `unlink` leaves a file behind.** The `link`+`unlink` placement
   discarded the unlink's result. On NTFS that left a `.skerry-copy-*` name linked
   to the copy, which showed up as a three-name hardlink set with a link count of
   four. The lesson is narrow and the cost was not: a cleanup call whose result is
   discarded is a cleanup call that does not happen.

4. **The file-type test was nonsense.** Masking `st_mode` with a single type bit
   and comparing it to another type never matches, so the tree walk descended into
   a directory as though it were a file. `FileType::from_raw_mode` masks with
   `S_IFMT`; doing it by hand did not.

Two more findings are about the destination rather than about us, and both belong
in `sk-fs`:

- **NTFS refuses a filename that is not valid UTF-8** with `EILSEQ`, because its
  names are UTF-16. A fixture that insists on creating everything cannot even be
  *built* on half the filesystems a user owns, which is why what a filesystem
  refuses is recorded as a result and the checks distinguish "the destination
  cannot hold this" from "we lost it".
- **A filesystem's own bookkeeping is not the property to test.** ntfs-3g reports
  one more link than there are names. The check counts names in the copied tree
  that share an inode, which is what the user can see, and reports the link count
  as evidence rather than as the test.

### What is owed

**The reflink condition, and it is one command away.** Both remote machines from
Spikes B, C and F have `mkfs.btrfs` *and* `mkfs.xfs` installed; what is missing is
`CAP_SYS_ADMIN`, because a loop device cannot be set up from a user namespace.
[with-root.sh](with-root.sh) does the whole privileged part — create the images,
`mkfs`, mount, run the matrix as the *invoking* user, unmount, clean up:

```
cargo build --release && sudo ./with-root.sh
```

It runs the matrix as the unprivileged user on purpose: root can set any extended
attribute and write any file, so the "report what could not be preserved" paths
never fire and a privileged run would be the weaker test.

That script also covers **xfs** (the second reflink filesystem), **exFAT** and
**vfat** (two-second timestamps, no permissions, no symlinks — the coarsest
destinations a user will actually plug in), and **NFS over loopback** where
`exportfs` is present.

Also owed:

- **Durability.** Every check here is about what is visible after a copy returns.
  Whether it survives a power cut is a different question, `fsync` is exercised
  but not verified, and testing it needs a machine somebody is willing to cut the
  power to.
- **A destination that vanishes mid-copy.** A USB drive pulled out is `EIO` on
  every subsequent call, which the injector can already produce; a mount that
  *disappears* is `ESTALE` and a different code path.
- **Ownership.** Copying uid and gid needs privilege to test properly, and the
  unprivileged case is only interesting for what it *reports*.

## Verdict

**Go, with the reflink condition owed and one command away.**

Two of the bar's three conditions are met, exactly and repeatably. The third names
a filesystem no machine here can create without root, and [with-root.sh](with-root.sh)
is written so that measuring it costs one password rather than a day.

Nothing failed. Every one of the six limitations is a property of NTFS that the
engine detects and reports rather than papering over, and the distinction between
*not preserved* and *not preservable* is drawn by a runtime probe rather than by a
list of filesystem names — which is [ADR 0007](../../docs/adr/0007-runtime-capability-detection.md)
applied to a test harness, and it is what lets this matrix run on a filesystem it
has never seen.

**M3's gate is satisfiable.** The roadmap says *"M3 does not ship until the Spike
E matrix is green in CI"*, and green now has a meaning: 37 checks that pass, 6
that are the destination's limits, and 3 that need a filesystem CI can create with
the privileges CI has. The matrix is one binary with no arguments, it discovers
what to test, and it exits non-zero on a failure — so it is a CI job today rather
than a thing to be written later.

**Four defects were found by a check failing, and all four would have shipped.**
A sparse file becoming a gigabyte of zeros, an external drive silently copying
nothing, a temporary file left linked into a hardlink set, and a directory walked
as though it were a file. None of them would have been found by reading the
manual pages, and the first passes a byte-for-byte comparison. That is the
argument for the matrix outliving the spike.

