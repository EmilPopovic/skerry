// SPDX-License-Identifier: Apache-2.0
//! Inherited restrictions for helpers; see ADR 0034 and ADR 0046.
//!
//! **A deny-list, every rule self-contained.** Each rule loads the syscall number
//! itself and either returns its errno or falls through to the next rule, so no
//! jump crosses a rule and a rule can be read, added or removed on its own line.
//! Anything no rule names is allowed, which is 0034's choice and the reason the
//! list grows by what reaches past a helper's files rather than shrinking to what
//! a renderer is known to call.

use libc::c_long;

/// What the filter is installed for, decided by the helper's role and the
/// Landlock ABI that took.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Policy {
    /// Whether it starts programs at all.
    pub(crate) spawns: bool,
    /// Whether it sets file times — an unpack stamping the archive's dates.
    pub(crate) stamps: bool,
    /// Whether it kills processes it started, which needs `kill` on their pid.
    pub(crate) reaps: bool,
    /// Whether truncation is left to seccomp, because Landlock below ABI 3 has no
    /// right for it.
    pub(crate) truncation: bool,
    /// This process's pid, the only one a helper that reaps nothing may signal.
    pub(crate) pid: u32,
}

// `linux/bpf_common.h` and `linux/seccomp.h`. `seccomp_data` is `nr` at 0, `arch`
// at 4, the instruction pointer at 8 and `args[i]` at `16 + 8i` — the same layout
// on x86-64 and aarch64, whose little-endian order puts an argument's low word
// first.
const LOAD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JGE: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const JSET: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RET: u16 = 0x06; // BPF_RET | BPF_K
const NR: u32 = 0;
const ARCH: u32 = 4;
const KILL_PROCESS: u32 = 0x8000_0000;
const ERRNO: u32 = 0x0005_0000;
const ALLOW: u32 = 0x7fff_0000;

const _: () = assert!(cfg!(target_endian = "little"), "argument offsets assume the low word first");

/// The low 32 bits of `args[index]`, which is all of a `pid_t`, an `int` flag
/// word or `clone`'s flags that any rule here reads.
const fn argument(index: u32) -> u32 {
    16 + 8 * index
}

// Numbers 424 and above are shared by every architecture, and the `libc` crate
// does not yet name these on aarch64, or at all.
//
// **The table grows with every kernel, and a deny-list does not.** Read through
// 471, `rseq_slice_yield`, on 7.0: `file_setattr` arrived in 6.17 and set by path
// what `FS_IOC_FSSETXATTR` is denied setting through a descriptor. When the kernel
// floor or the development kernel moves, read the numbers past the last one this
// list names for anything that changes metadata, mounts or reaches another
// process. Left allowed on that reading: `file_getattr`, `getxattrat`,
// `listxattrat`, `statmount`, `listmount` and `listns`, which only read;
// `pidfd_open`, inert while every call that acts on a pidfd is denied; and the
// futex, memory and LSM-self calls, which act on the helper alone.
const FCHMODAT2: c_long = 452;
const SETXATTRAT: c_long = 463;
const REMOVEXATTRAT: c_long = 466;
const OPEN_TREE_ATTR: c_long = 467;
const FILE_SETATTR: c_long = 469;

const CLONE_THREAD: u32 = 0x0001_0000;
const CLONE_PARENT: u32 = 0x0000_8000;
// `asm-generic/fcntl.h`, `asm-generic/ioctls.h` and `asm-generic/sockios.h`, which
// x86-64 and aarch64 both use; a 64-bit kernel has no `fcntl64` to cover too.
// The `libc` crate does not name the second and third on every target.
const F_SETOWN: u32 = 8;
const F_SETSIG: u32 = 10;
const F_SETOWN_EX: u32 = 15;
const F_SETLEASE: u32 = 1024;
const FIOSETOWN: u32 = 0x8901;
const SIOCSPGRP: u32 = 0x8902;
/// Every `CLONE_NEW*` flag `clone` itself accepts.
const CLONE_NEW: u32 = 0x7e02_0000;
const O_TRUNC: u32 = 0o1000;

/// What a rule does with the argument it reads.
#[derive(Clone, Copy)]
enum Test {
    /// Denied where any of these bits is set.
    Any(u32),
    /// Denied where none of these bits is set.
    None(u32),
    /// Denied unless the word is exactly this.
    Except(u32),
    /// Denied where the word is exactly this.
    Is(u32),
}

struct Program(Vec<libc::sock_filter>);

impl Program {
    fn push(&mut self, code: u16, jt: u8, jf: u8, k: u32) {
        self.0.push(libc::sock_filter { code, jt, jf, k });
    }

    fn deny(&mut self, syscall: c_long, errno: i32) {
        self.push(LOAD, 0, 0, NR);
        self.push(JEQ, 0, 1, number(syscall));
        self.push(RET, 0, 0, ERRNO | errno.unsigned_abs());
    }

    fn deny_all(&mut self, syscalls: &[c_long]) {
        for syscall in syscalls {
            self.deny(*syscall, libc::EPERM);
        }
    }

    /// Denies `syscall` where argument `index` passes `test`.
    fn deny_where(&mut self, syscall: c_long, index: u32, test: Test) {
        let (code, k, jt, jf) = match test {
            Test::Any(bits) => (JSET, bits, 0, 1),
            Test::None(bits) => (JSET, bits, 1, 0),
            Test::Except(value) => (JEQ, value, 1, 0),
            Test::Is(value) => (JEQ, value, 0, 1),
        };
        self.push(LOAD, 0, 0, NR);
        self.push(JEQ, 0, 3, number(syscall));
        self.push(LOAD, 0, 0, argument(index));
        self.push(code, jt, jf, k);
        self.push(RET, 0, 0, ERRNO | libc::EPERM.unsigned_abs());
    }
}

fn number(syscall: c_long) -> u32 {
    u32::try_from(syscall).unwrap_or(u32::MAX)
}

/// Installs the filter, inherited by everything the helper starts.
///
/// Callers set no-new-privileges first; without it the kernel refuses.
pub(crate) fn install(policy: Policy) -> bool {
    let mut program = build(policy);
    let Ok(len) = u16::try_from(program.0.len()) else { return false };
    let fprog = libc::sock_fprog { len, filter: program.0.as_mut_ptr() };
    // SAFETY: the BPF program and backing array outlive prctl, which copies them.
    unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &raw const fprog, 0, 0) == 0 }
}

/// Networking, process inspection, kernel programming, mounts and namespaces —
/// ADR 0034's list, with the three calls of the new mount API it did not name.
const KERNEL_INTERFACES: [c_long; 34] = [
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
    libc::SYS_kcmp,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_open_tree,
    OPEN_TREE_ATTR,
    libc::SYS_mount_setattr,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_open_by_handle_at,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_reboot,
    libc::SYS_kexec_load,
];

/// What reaches past a helper's files without opening a path, so Landlock never
/// sees it — ADR 0046: a file's mode, owner and attributes, signals by every
/// route but `kill` and `tgkill`, priorities, another process's memory, and the
/// keyrings.
///
/// **`setpriority` and `ioprio_set` at all**, not only for another pid: a zero
/// target with `PRIO_USER` or `IOPRIO_WHO_USER` is every process the user has.
///
/// **`file_setattr`** is `FS_IOC_FSSETXATTR` by path — see [`INODE_IOCTLS`] —
/// and reaches every file the user owns at every Landlock ABI.
const PAST_ITS_FILES: [c_long; 25] = [
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    FCHMODAT2,
    libc::SYS_fchown,
    libc::SYS_fchownat,
    FILE_SETATTR,
    libc::SYS_setxattr,
    libc::SYS_lsetxattr,
    libc::SYS_fsetxattr,
    libc::SYS_removexattr,
    libc::SYS_lremovexattr,
    libc::SYS_fremovexattr,
    SETXATTRAT,
    REMOVEXATTRAT,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_tkill,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_rt_tgsigqueueinfo,
    libc::SYS_pidfd_send_signal,
    libc::SYS_setpriority,
    libc::SYS_ioprio_set,
    libc::SYS_process_madvise,
    libc::SYS_process_mrelease,
];

/// The older forms of [`PAST_ITS_FILES`]' metadata calls, which aarch64 never had.
#[cfg(target_arch = "x86_64")]
const PAST_ITS_FILES_LEGACY: [c_long; 3] = [libc::SYS_chmod, libc::SYS_chown, libc::SYS_lchown];

/// Calls whose first argument is a pid and whose zero means this process.
const BY_PID: [c_long; 7] = [
    libc::SYS_prlimit64,
    libc::SYS_sched_setaffinity,
    libc::SYS_sched_setparam,
    libc::SYS_sched_setscheduler,
    libc::SYS_sched_setattr,
    libc::SYS_migrate_pages,
    libc::SYS_move_pages,
];

fn build(policy: Policy) -> Program {
    let mut program = Program(Vec::with_capacity(256));
    native_only(&mut program);
    program.deny_all(&KERNEL_INTERFACES);
    program.deny_all(&PAST_ITS_FILES);
    #[cfg(target_arch = "x86_64")]
    program.deny_all(&PAST_ITS_FILES_LEGACY);
    for syscall in BY_PID {
        program.deny_where(syscall, 0, Test::Except(0));
    }
    for request in INODE_IOCTLS {
        program.deny_where(libc::SYS_ioctl, 1, Test::Is(request));
    }
    signals(&mut program, policy);
    stays_in_its_group(&mut program);
    no_leases(&mut program);
    if !policy.stamps {
        no_times(&mut program);
    }
    if !policy.spawns {
        no_processes(&mut program);
    }
    if policy.truncation {
        no_truncation(&mut program);
    }
    // clone3 hides its flags behind a pointer; ENOSYS makes libc fall back to
    // clone, whose flags the rules here can inspect directly.
    program.deny(libc::SYS_clone3, libc::ENOSYS);
    program.deny_where(libc::SYS_clone, 0, Test::Any(CLONE_NEW));
    program.push(RET, 0, 0, ALLOW);
    program
}

/// Kills a process calling through another architecture's table, whose numbers
/// mean other calls, and answers x32's alternate numbers `ENOSYS`.
fn native_only(program: &mut Program) {
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let arch = 0;
    program.push(LOAD, 0, 0, ARCH);
    program.push(JEQ, 1, 0, arch);
    program.push(RET, 0, 0, KILL_PROCESS);
    program.push(LOAD, 0, 0, NR);
    program.push(JGE, 0, 1, 0x4000_0000);
    program.push(RET, 0, 0, ERRNO | libc::ENOSYS.unsigned_abs());
}

/// No file's times may be set, for every helper but an unpack.
fn no_times(program: &mut Program) {
    program.deny(libc::SYS_utimensat, libc::EPERM);
    #[cfg(target_arch = "x86_64")]
    program.deny_all(&[libc::SYS_utime, libc::SYS_utimes, libc::SYS_futimesat]);
}

/// No process may be started — a thread may — for the thumbnailer.
fn no_processes(program: &mut Program) {
    program.deny_all(&[libc::SYS_execve, libc::SYS_execveat]);
    #[cfg(target_arch = "x86_64")]
    program.deny_all(&[libc::SYS_fork, libc::SYS_vfork]);
    program.deny_where(libc::SYS_clone, 0, Test::None(CLONE_THREAD));
}

/// No file may be truncated, where Landlock below ABI 3 has no right for it.
fn no_truncation(program: &mut Program) {
    program.deny_all(&[libc::SYS_truncate, libc::SYS_ftruncate]);
    #[cfg(target_arch = "x86_64")]
    {
        program.deny(libc::SYS_creat, libc::EPERM);
        program.deny_where(libc::SYS_open, 1, Test::Any(O_TRUNC));
    }
    program.deny_where(libc::SYS_openat, 2, Test::Any(O_TRUNC));
    // Its flags are behind a pointer this cannot follow. `ENOSYS` is what an
    // older kernel answers, and no libc's `open` reaches for it first.
    program.deny(libc::SYS_openat2, libc::ENOSYS);
}

/// Who a helper may signal.
///
/// **Itself, and for the archive helper the decompressor it started.** `abort`
/// is a `tgkill` to the caller, so that much stays; a pid the helper did not
/// start is the window, or anything else the user runs. The archive helper has
/// to `kill` its decompressor when a listing stops early, and that pid is not
/// known when the filter is built — so below Landlock ABI 6, whose signal scope
/// is what closes this, it may signal a positive pid and never a group.
///
/// **And no descriptor may name an owner to signal.** `F_SETOWN` or
/// `F_SETOWN_EX` with a pid, `F_SETSIG` and `O_ASYNC` on a pipe the helper holds
/// is a signal to that pid whenever the pipe is written — `kill` by another
/// route, which below ABI 6 nothing else stops. `FIOSETOWN` and `SIOCSPGRP` are
/// the same thing spelled for a socket.
fn signals(program: &mut Program, policy: Policy) {
    program.deny_where(libc::SYS_tgkill, 0, Test::Except(policy.pid));
    if policy.reaps {
        // A negative pid is a group, -1 is every process the user has, and 0 is
        // the caller's own group.
        program.deny_where(libc::SYS_kill, 0, Test::Any(0x8000_0000));
        program.deny_where(libc::SYS_kill, 0, Test::Is(0));
    } else {
        program.deny_where(libc::SYS_kill, 0, Test::Except(policy.pid));
    }
    for command in [F_SETOWN, F_SETOWN_EX, F_SETSIG] {
        program.deny_where(libc::SYS_fcntl, 1, Test::Is(command));
    }
    for request in [FIOSETOWN, SIOCSPGRP] {
        program.deny_where(libc::SYS_ioctl, 1, Test::Is(request));
    }
}

/// No lease on any file.
///
/// **A read lease needs only a descriptor opened to read**, and holds up every
/// other process's open of that file for writing for `lease-break-time` — 45
/// seconds by default — each time one tries. No helper needs one. `flock` and
/// POSIX locks stay: fontconfig and the tools a helper starts lock their own
/// caches, and a lock stops only another locker, never somebody's open.
fn no_leases(program: &mut Program) {
    program.deny_where(libc::SYS_fcntl, 1, Test::Is(F_SETLEASE));
}

/// No process the helper starts may leave the group [`crate::host::terminate`]
/// signals.
///
/// **`setsid` has already run** — it is the first step of confinement — so
/// after it, `setsid` and `setpgid` are only a way out of the group, and
/// `CLONE_PARENT` makes a child the window's rather than the helper's.
fn stays_in_its_group(program: &mut Program) {
    program.deny_all(&[libc::SYS_setsid, libc::SYS_setpgid]);
    program.deny_where(libc::SYS_clone, 0, Test::Any(CLONE_PARENT));
}

/// What changes a file's inode through a descriptor opened only to read it, so
/// Landlock's read right is all it needs.
///
/// **By the low 32 bits**, which is all of the `unsigned int` the kernel reads.
/// Flags are `chattr`'s `noatime`, `nodump` and `append`; the attributes are
/// XFS's project and extent hints on every filesystem; verity makes a file
/// permanently read-only; an encryption policy claims an empty folder for a key
/// the helper chose; a version is the inode's generation.
pub(crate) const INODE_IOCTLS: [u32; 9] = [
    0x4008_6602, // FS_IOC_SETFLAGS
    0x4004_6602, // FS_IOC32_SETFLAGS
    0x401c_5820, // FS_IOC_FSSETXATTR
    0x4080_6685, // FS_IOC_ENABLE_VERITY
    0x800c_6613, // FS_IOC_SET_ENCRYPTION_POLICY
    0x4008_7602, // FS_IOC_SETVERSION
    0x4004_7602, // FS_IOC32_SETVERSION
    0x4008_6604, // EXT4_IOC_SETVERSION
    0x4004_6604, // EXT4_IOC32_SETVERSION
];

/// Makes Landlock look absent: its three syscalls answer `ENOSYS`, which is what
/// a kernel without it says.
///
/// For the tests that need a helper to meet a kernel without Landlock on a
/// machine that has one — the way the security review reproduced ADR 0046's
/// finding.
#[cfg(test)]
pub(crate) fn hide_landlock() -> bool {
    let mut program = Program(Vec::new());
    for syscall in [444, 445, 446] {
        program.deny(syscall, libc::ENOSYS);
    }
    program.push(RET, 0, 0, ALLOW);
    let Ok(len) = u16::try_from(program.0.len()) else { return false };
    let fprog = libc::sock_fprog { len, filter: program.0.as_mut_ptr() };
    // SAFETY: as in `install`.
    unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &raw const fprog, 0, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::{Policy, build};

    /// **Every policy fits `BPF_MAXINSNS`**, which is 4096: past it the kernel
    /// refuses the filter, and every helper exits with a mandatory layer failed.
    /// And no jump leaves its own rule, which is what lets a rule be added
    /// without recounting the ones after it.
    #[test]
    fn every_policy_fits_the_kernel_and_no_jump_leaves_its_rule() {
        for bits in 0..16u8 {
            let policy = Policy {
                spawns: bits & 1 != 0,
                stamps: bits & 2 != 0,
                reaps: bits & 4 != 0,
                truncation: bits & 8 != 0,
                pid: 4242,
            };
            let program = build(policy);
            assert!(program.0.len() <= 4096, "{policy:?} is {} long", program.0.len());
            assert!(program.0.iter().all(|it| it.jt <= 3 && it.jf <= 3), "{policy:?}");
            assert_eq!(program.0.last().map(|it| it.k), Some(super::ALLOW));
        }
    }
}
