// SPDX-License-Identifier: Apache-2.0
//! What a preview child does to itself before it reads a byte.
//!
//! **[ADR 0006](../../../docs/adr/0006-out-of-process-decoders.md)'s second half.**
//! Out of process is what stops a malformed JPEG reaching the window; this is what
//! stops it reaching the home directory. Applied by the child rather than by the
//! host — [ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)
//! — because a host that set it through `pre_exec` would be running this code
//! between `fork` and `exec`, where almost nothing is allowed and a failure has
//! nowhere to be reported.
//!
//! Four layers, weakest first, and each one is applied even when a stronger one is
//! available:
//!
//! * **`PR_SET_NO_NEW_PRIVS`**, without which Landlock is not permitted at all and
//!   a setuid binary reached through a decoder is still a way up.
//! * **An address-space cap**, which turns a decoder that allocates without bound
//!   into a failed allocation rather than into the machine swapping.
//! * **A CPU cap**, which turns a decompression bomb into a dead child. The host's
//!   own deadline is the other half of that and neither replaces the other: a
//!   child blocked on a slow disk burns no CPU, and a child in a tight loop
//!   answers no deadline.
//! * **Landlock**, which is the only one that says *which files*.
//!
//! **Where the kernel has no Landlock this says so on `stderr` and carries on**,
//! which is ADR 0006's "reduced isolation is logged — never silently accepted".
//! The three caps above it need no kernel feature and are applied regardless.
//!
//! What is not here is `seccomp`. ADR 0006 names it and it is not written: a
//! syscall filter is a BPF program, and a wrong one is a child that dies on a
//! syscall its decoder needed on somebody else's libc. It is the next layer and it
//! is absent rather than pretended — `report()` says so, so the log names what is
//! actually in force.

use std::ffi::CString;
use std::path::Path;

/// What the kernel will let a child spend on itself.
///
/// **A named pair rather than two numbers at the call site**, because the two
/// jobs a sandboxed child does here have honestly different appetites and the
/// wrong one is a silent failure in both directions: a preview allowed ten
/// minutes of CPU is a wedged decoder nobody kills, and an unpack allowed ten
/// seconds is a four-gigabyte archive that dies two thirds of the way through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// How much address space the child may map, in bytes.
    pub address_space: u64,
    /// How many CPU seconds it may burn before the kernel ends it.
    pub cpu_seconds: u64,
}

impl Limits {
    /// What a decoder answering one request gets.
    ///
    /// A gigabyte, because a 4096×4096 RGBA image is 64 MiB and the decoders hold
    /// a few multiples of their output — far above anything honest and far below
    /// what makes a machine swap. Ten CPU seconds, which is the case the host's
    /// own deadline cannot see: a child killed for missing a deadline is killed
    /// by the host, and a child in a loop inside a decoder is killed by this.
    pub const PREVIEW: Self = Self { address_space: 1024 * 1024 * 1024, cpu_seconds: 10 };

    /// What a child unpacking an archive gets.
    ///
    /// **Two gigabytes, which is headroom rather than a requirement**, and the
    /// distinction is measured. Decoding the 4.3 GB fixture in [ADR
    /// 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md):
    ///
    /// * `xz -T0` left to itself peaks at 1.5 GB of RSS and **fails under a
    ///   gigabyte of address space** — `Cannot allocate memory`, which is what
    ///   sent the filter its own `-M256MiB` budget in the first place.
    /// * With that budget it peaks at 329 MB and runs inside a gigabyte fine, on
    ///   this machine's twenty-four cores.
    ///
    /// The doubling is for the machine with a hundred: `-M` bounds the
    /// dictionary, not the stacks and arenas a thread pool *reserves*, and
    /// address space counts reservations. A cap that fits here and not there is
    /// a feature that silently refuses on somebody else's hardware, and the cost
    /// of the headroom is nothing — it is a ceiling, not an allocation.
    ///
    /// **Ten minutes** of CPU: the same archive is 5.6 seconds of it, so this is
    /// two orders of magnitude of headroom and still a ceiling that ends a
    /// decoder gone circular.
    pub const UNPACK: Self = Self { address_space: 2 * 1024 * 1024 * 1024, cpu_seconds: 600 };
}

/// What ended up in force, for the line a child prints to `stderr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Report {
    /// Whether `PR_SET_NO_NEW_PRIVS` took.
    pub no_new_privs: bool,
    /// Whether the address-space and CPU caps took.
    pub limits: bool,
    /// The Landlock ABI in force, or `None` where the kernel has none.
    pub landlock: Option<i32>,
}

impl Report {
    /// The line a child prints, which names what is *not* in force as well.
    #[must_use]
    pub fn line(&self) -> String {
        let landlock = match self.landlock {
            Some(abi) => format!(
                "landlock abi {abi}, truncation {}",
                if abi >= 3 { "restricted" } else { "UNRESTRICTED" }
            ),
            None => "landlock ABSENT".to_owned(),
        };
        let privs = if self.no_new_privs { "no-new-privs" } else { "no-new-privs FAILED" };
        let limits = if self.limits { "rlimits" } else { "rlimits FAILED" };
        format!("{privs}, {limits}, {landlock}, seccomp not implemented")
    }
}

/// Locks this process down to reading `readable`, running `executable` and writing
/// `writable`.
///
/// Called once, before the first request is read. Never fails: every layer that
/// cannot be applied is reported instead, because a child that refuses to start
/// is a preview kind that silently does not exist.
///
/// **`executable` is separate from `readable`, and the separation is load-bearing.**
/// `skerry-thumbnailer` decodes in-process and passes none: with no execute right
/// anywhere it cannot start a program at all, which is the strongest thing this
/// can say about a process that handles hostile input. `skerry-preview-doc` drives
/// `pdftoppm` and passes the root, for a reason recorded there.
///
/// Learned twice by the sandbox working, both times as `NoRenderer` from a helper
/// whose `--probe` said the renderer was installed. First: `execve` needs its own
/// Landlock right, which the original ruleset granted nowhere. Then: a
/// *dynamically linked* program needs that right on its loader and every library
/// it maps as well, so granting it on `PATH` alone is not enough for any real
/// binary. Neither was guessed — both were a program that would not start.
#[must_use]
pub fn confine(
    limits: Limits,
    readable: &[&Path],
    executable: &[&Path],
    writable: &[&Path],
) -> Report {
    let mut report = Report { no_new_privs: no_new_privs(), limits: caps(limits), landlock: None };
    report.landlock = landlock(readable, executable, writable);
    report
}

/// `prctl(PR_SET_NO_NEW_PRIVS, 1)`.
fn no_new_privs() -> bool {
    // SAFETY: `prctl` with this option reads no pointers and writes nothing; the
    // four trailing arguments are ignored and passed as zero, as the manual page
    // requires.
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 }
}

/// The address-space and CPU ceilings.
fn caps(limits: Limits) -> bool {
    let set = |resource, value: u64| {
        let limit = libc::rlimit { rlim_cur: value, rlim_max: value };
        // SAFETY: `setrlimit` reads the struct and writes nothing.
        unsafe { libc::setrlimit(resource, &raw const limit) == 0 }
    };
    set(libc::RLIMIT_AS, limits.address_space) & set(libc::RLIMIT_CPU, limits.cpu_seconds)
}

/// The Landlock ABI this kernel offers, or nothing.
fn abi() -> Option<i32> {
    // SAFETY: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
    // is the documented probe: with a null attribute and a zero size it reads
    // nothing and returns the supported ABI version.
    let version = unsafe { libc::syscall(CREATE_RULESET, std::ptr::null::<u8>(), 0usize, 1u32) };
    i32::try_from(version).ok().filter(|abi| *abi > 0)
}

/// Restricts the filesystem to `readable` and `writable`, or reports no Landlock.
///
/// Handles filesystem rights known by this build and supported by the kernel.
/// ABI 3 adds truncation independently of writes; omitting it leaves `O_TRUNC` open.
/// Executable paths receive a separate grant from writable output directories.
fn landlock(readable: &[&Path], executable: &[&Path], writable: &[&Path]) -> Option<i32> {
    let abi = abi()?;
    let rights =
        ABI_1_RIGHTS | if abi >= 2 { 1 << 13 } else { 0 } | if abi >= 3 { 1 << 14 } else { 0 };
    let attribute = RulesetAttribute { handled_access_fs: rights };
    // SAFETY: the attribute outlives the call and its size is passed alongside it,
    // which is the syscall's whole contract.
    let ruleset = unsafe {
        libc::syscall(CREATE_RULESET, &raw const attribute, size_of::<RulesetAttribute>(), 0u32)
    };
    let ruleset = i32::try_from(ruleset).ok().filter(|fd| *fd >= 0)?;

    for (paths, rights) in [
        (readable, READ_RIGHTS),
        (executable, READ_RIGHTS | LANDLOCK_ACCESS_FS_EXECUTE),
        (writable, rights & !LANDLOCK_ACCESS_FS_EXECUTE),
    ] {
        for path in paths {
            allow(ruleset, path, rights);
        }
    }
    // SAFETY: `restrict_self` takes the ruleset descriptor and two zeroes.
    let applied = unsafe { libc::syscall(RESTRICT_SELF, ruleset, 0u32) } == 0;
    // SAFETY: the descriptor came from this function and is not used again.
    unsafe { libc::close(ruleset) };
    applied.then_some(abi)
}

/// Adds one path to a ruleset. A path that cannot be opened is simply not granted.
fn allow(ruleset: i32, path: &Path, rights: u64) {
    let Ok(name) = CString::new(path.as_os_str().as_encoded_bytes()) else { return };
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return;
    }
    let rule = PathBeneath { allowed_access: rights, parent_fd: fd };
    // SAFETY: the rule outlives the call and its type tag says what it is.
    unsafe {
        libc::syscall(ADD_RULE, ruleset, RULE_PATH_BENEATH, &raw const rule, 0u32);
        libc::close(fd);
    }
}

// `linux/landlock.h`. Three syscall numbers and two structs, rather than a crate:
// this is the whole of the interface and it is stable kernel ABI.
const CREATE_RULESET: libc::c_long = 444;
const ADD_RULE: libc::c_long = 445;
const RESTRICT_SELF: libc::c_long = 446;
const RULE_PATH_BENEATH: u32 = 1;

/// Everything ABI 1 can handle, which is every filesystem right a decoder could
/// want and several it could not.
const ABI_1_RIGHTS: u64 = (1 << 13) - 1;

/// The subset a child needs over the files it is asked about: open for reading,
/// and read a directory to resolve a path through it.
const READ_RIGHTS: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;

#[repr(C)]
struct RulesetAttribute {
    handled_access_fs: u64,
}

#[repr(C)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Report;

    /// The line names what is missing as loudly as what is present, which is the
    /// difference between a log and a reassurance.
    #[test]
    fn the_report_says_what_is_not_in_force() {
        let full = Report { no_new_privs: true, limits: true, landlock: Some(8) };
        let line = full.line();
        assert!(line.contains("landlock abi 8"), "{line}");
        assert!(line.contains("seccomp not implemented"), "{line}");
        assert!(!line.contains("FAILED"), "{line}");

        let none = Report::default();
        let line = none.line();
        assert!(line.contains("landlock ABSENT"), "{line}");
        assert!(line.contains("no-new-privs FAILED"), "{line}");
        assert!(line.contains("rlimits FAILED"), "{line}");
    }

    #[test]
    fn landlock_denies_input_truncation_and_output_execution() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::{Path, PathBuf};
        const MARKER: &str = "SKERRY_LANDLOCK_REGRESSION_ROOT";
        if let Some(root) = std::env::var_os(MARKER) {
            let root = PathBuf::from(root);
            let out = root.join("out");
            assert!(super::no_new_privs());
            let Some(abi) = super::landlock(
                &[&root, Path::new("/usr"), Path::new("/lib"), Path::new("/lib64")],
                &[Path::new("/usr"), Path::new("/lib"), Path::new("/lib64")],
                &[&out],
            ) else {
                eprintln!("skerry: skip: Landlock unavailable in this environment");
                return;
            };
            assert_eq!(std::fs::read(root.join("input")).unwrap(), b"valuable");
            std::fs::write(out.join("allowed"), "output").unwrap();
            if abi >= 3 {
                assert!(std::fs::File::create(root.join("input")).is_err());
                assert_eq!(std::fs::read(root.join("input")).unwrap(), b"valuable");
            }
            assert!(std::process::Command::new(out.join("program")).status().is_err());
            return;
        }
        let root = std::env::temp_dir().join(format!("skerry-landlock-{}", std::process::id()));
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("input"), "valuable").unwrap();
        std::fs::copy("/bin/true", root.join("out/program")).unwrap();
        std::fs::set_permissions(root.join("out/program"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::tests::landlock_denies_input_truncation_and_output_execution",
                "--nocapture",
            ])
            .env(MARKER, &root)
            .status()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(status.success());
    }
}
