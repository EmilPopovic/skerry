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
//! Five layers, weakest first, and each one is applied even when a stronger one is
//! available:
//!
//! * **A session of its own**, so the child has no controlling terminal to push
//!   keystrokes into and its process group is its own pid.
//! * **`PR_SET_NO_NEW_PRIVS`**, without which Landlock is not permitted at all and
//!   a setuid binary reached through a decoder is still a way up.
//! * **Resource caps** on address space, CPU and the size of a written file.
//!   [`Limits`] says why each is what it is; the CPU cap and the host's deadline
//!   are two halves and neither replaces the other, because a child blocked on a
//!   slow disk burns no CPU and a child in a tight loop answers no deadline.
//! * **Landlock**, which is the only one that says *which files* — and from ABI 6
//!   which processes a signal may reach.
//! * **seccomp**, for what Landlock does not govern because it is not opening a
//!   path: networking, other processes, file metadata, keyrings. ADR 0034's
//!   policy, extended by ADR 0046; it is not a syscall allowlist.
//!
//! **Where Landlock does not take, the child refuses** — [ADR
//! 0046](../../../docs/adr/0046-a-helper-that-cannot-be-confined-refuses-and-a-confined-one-reaches-nothing-but-its-files.md)
//! — and exits with [`REFUSED_WITHOUT_LANDLOCK`] before reading a request, unless
//! the host passed [`WITHOUT_LANDLOCK`] because somebody turned the setting on.

use std::ffi::CString;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};

/// The variable that lets a helper decode where Landlock did not take.
///
/// Its value must be `allow`. The host sets it from the `decode_without_landlock`
/// setting and never passes one through from its own environment.
pub const WITHOUT_LANDLOCK: &str = "SKERRY_WITHOUT_LANDLOCK";

/// The status a helper exits with when it refuses for want of Landlock.
///
/// `EX_CONFIG` from `sysexits.h`: the machine is not set up for this, and a
/// setting is what would change the answer. Distinct from the 1 a failed
/// mandatory layer exits with and the 101 of a panic, so the host can tell a
/// refusal from a crash.
pub const REFUSED_WITHOUT_LANDLOCK: i32 = 78;

/// What the kernel will let a child spend on itself.
///
/// **A named set rather than numbers at the call site**, because the jobs a
/// sandboxed child does here have honestly different appetites and the wrong one
/// is a silent failure in both directions: a preview allowed ten minutes of CPU is
/// a wedged decoder nobody kills, and an unpack allowed ten seconds is a
/// four-gigabyte archive that dies two thirds of the way through.
///
/// **No `RLIMIT_NPROC`.** It counts every task of the real uid, not this child's:
/// 1,807 of them for this user on the development machine during a build. A cap
/// low enough to stop a fork bomb refuses `pdftoppm` on a busy desktop, and one
/// high enough not to is no cap — so the thumbnailer, which starts nothing, is
/// denied process creation by its filter instead. See ADR 0046.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// How much address space the child may map, in bytes.
    pub address_space: u64,
    /// How many CPU seconds it may burn before the kernel ends it.
    pub cpu_seconds: u64,
    /// The largest file it may write, in bytes — `RLIMIT_FSIZE`.
    ///
    /// A pipe is not a file to this limit, so a reply on `stdout` and a
    /// renderer's PNG on its own `stdout` are not counted.
    pub file_size: u64,
}

impl Limits {
    /// What a decoder answering one request gets, where it writes no file.
    ///
    /// A gigabyte, because a 4096×4096 RGBA image is 64 MiB and the decoders hold
    /// a few multiples of their output — far above anything honest and far below
    /// what makes a machine swap. Ten CPU seconds, which is the case the host's
    /// own deadline cannot see: a child killed for missing a deadline is killed
    /// by the host, and a child in a loop inside a decoder is killed by this.
    /// **No file at all**, because the document helper and an archive listing
    /// answer through a pipe.
    pub const PREVIEW: Self =
        Self { address_space: 1024 * 1024 * 1024, cpu_seconds: 10, file_size: 0 };

    /// What the thumbnailer gets: [`Self::PREVIEW`], and files of up to a MiB.
    ///
    /// Its one honest write is a 128-pixel thumbnail: 64 KiB of pixels stored
    /// uncompressed at worst, plus a URI chunk of at most three bytes for each
    /// byte of a 4096-byte path — under 80 KB, so a MiB is thirteen times the
    /// worst case and still a ceiling on what a decoder talked into writing can
    /// put into the shared cache per file.
    pub const THUMBNAIL: Self = Self { file_size: 1024 * 1024, ..Self::PREVIEW };

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
    ///
    /// **No ceiling on a file**, because an entry is as large as the archive
    /// says and a disk image inside a tar is an honest one. The destination's
    /// free-space check is what stops an unpack filling the disk.
    pub const UNPACK: Self = Self {
        address_space: 2 * 1024 * 1024 * 1024,
        cpu_seconds: 600,
        file_size: libc::RLIM_INFINITY,
    };
}

/// Which helper is confining itself, which decides what it may do besides read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// `skerry-thumbnailer`: decodes in-process, writes the thumbnail cache, and
    /// starts nothing.
    Thumbnails,
    /// `skerry-preview-doc`: starts a renderer for each page and writes nothing.
    Pages,
    /// `skerry-archive` listing: starts a decompressor and writes nothing.
    Listing,
    /// `skerry-archive` unpacking: starts a decompressor, writes its destination,
    /// and stamps what it wrote with the archive's dates.
    Unpacking,
}

impl Role {
    /// What it may spend.
    #[must_use]
    pub const fn limits(self) -> Limits {
        match self {
            Self::Thumbnails => Limits::THUMBNAIL,
            Self::Pages | Self::Listing => Limits::PREVIEW,
            Self::Unpacking => Limits::UNPACK,
        }
    }

    /// The syscall policy, given the Landlock ABI that took.
    ///
    /// **The archive helper reaps by killing**: a listing that stops at its
    /// ceiling kills the decompressor still feeding it. The other two wait for
    /// what they start, so they may signal nothing but themselves.
    fn policy(self, landlock: Option<i32>) -> crate::seccomp::Policy {
        crate::seccomp::Policy {
            spawns: self != Self::Thumbnails,
            stamps: self == Self::Unpacking,
            reaps: matches!(self, Self::Listing | Self::Unpacking),
            truncation: landlock.is_none_or(|abi| abi < 3),
            pid: std::process::id(),
        }
    }
}

/// What ended up in force, for the line a child prints to `stderr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Report {
    /// Whether the child left the session it was started in.
    pub session: bool,
    /// Whether `PR_SET_NO_NEW_PRIVS` took.
    pub no_new_privs: bool,
    /// Whether the address-space, CPU and file-size caps took.
    pub limits: bool,
    /// The Landlock ABI in force, or `None` where it did not take.
    pub landlock: Option<i32>,
    /// Whether decoding without Landlock was allowed by the setting.
    pub unconfined: bool,
    /// Whether the inherited syscall restrictions took.
    pub seccomp: bool,
}

impl Report {
    /// The line a child prints, which names what is *not* in force as well.
    #[must_use]
    pub fn line(&self) -> String {
        let landlock = match self.landlock {
            Some(abi) => format!(
                "landlock abi {abi}, truncation {}, signals {}",
                if abi >= 3 { "by landlock" } else { "by seccomp" },
                if abi >= 6 { "scoped" } else { "by seccomp" }
            ),
            None if self.unconfined => "landlock ABSENT, decoding anyway by setting".to_owned(),
            None => "landlock ABSENT".to_owned(),
        };
        let session = if self.session { "own session" } else { "session SHARED" };
        let privs = if self.no_new_privs { "no-new-privs" } else { "no-new-privs FAILED" };
        let limits = if self.limits { "rlimits" } else { "rlimits FAILED" };
        let seccomp = if self.seccomp { "seccomp" } else { "seccomp FAILED" };
        format!("{session}, {privs}, {limits}, {landlock}, {seccomp}")
    }
}

/// Locks this process down to reading `readable`, running `executable` and writing
/// `writable`, with whatever else `role` may do.
///
/// Called before decoding. Mandatory restrictions fail closed with status 1, and a
/// Landlock that did not take fails closed with [`REFUSED_WITHOUT_LANDLOCK`]
/// unless [`WITHOUT_LANDLOCK`] says `allow` — ADR 0046. Execute grants name files,
/// never directories; callers resolve tools before confinement.
///
/// **A session that could not be left is reported, not fatal.** `setsid` refuses
/// only a process that already leads a group, which is what a shell's job control
/// makes of a `--probe` run by hand — and never what the host makes of a helper.
#[must_use]
pub fn confine(role: Role, readable: &[&Path], executable: &[&Path], writable: &[&Path]) -> Report {
    let mut report = Report {
        session: own_session(),
        no_new_privs: no_new_privs(),
        limits: caps(role.limits()),
        landlock: None,
        unconfined: std::env::var_os(WITHOUT_LANDLOCK).is_some_and(|value| value == "allow"),
        seccomp: false,
    };
    let outputs: Vec<_> = writable.iter().filter_map(|path| path.canonicalize().ok()).collect();
    let executable: Vec<_> = executable
        .iter()
        .copied()
        .filter(|path| {
            path.is_file()
                && outputs.len() == writable.len()
                && !outputs.iter().any(|output| path.starts_with(output))
        })
        .collect();
    report.landlock = landlock(readable, &executable, writable);
    report.seccomp = crate::seccomp::install(role.policy(report.landlock));
    if !report.no_new_privs || !report.limits || !report.seccomp {
        eprintln!("skerry: mandatory helper confinement failed: {}", report.line());
        std::process::exit(1);
    }
    if report.landlock.is_none() && !report.unconfined {
        eprintln!(
            "skerry: refusing to decode without Landlock ({}); the decode_without_landlock \
             setting allows it",
            report.line()
        );
        std::process::exit(REFUSED_WITHOUT_LANDLOCK);
    }
    report
}

/// The Landlock ABI this kernel offers, or nothing — asked once per process.
///
/// **Public for the host**, which asks before starting a helper that would only
/// refuse. It is the documented version query and reads no file, so it is not a
/// filesystem syscall for rule 1's purposes.
#[must_use]
pub fn landlock_abi() -> Option<i32> {
    static ABI: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    *ABI.get_or_init(abi)
}

/// Known PATH tools and their ELF interpreters, resolved before restricting exec.
#[must_use]
pub fn executables(names: &[&str]) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut paths = Vec::new();
    let search = std::env::var_os("PATH").unwrap_or_default();
    for name in names {
        let found = std::env::split_paths(&search).map(|dir| dir.join(name)).find(|path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        });
        let Some(path) = found.and_then(|path| path.canonicalize().ok()) else { continue };
        let Ok(loader) = interpreter(&path) else { continue };
        paths.push(path);
        if let Some(loader) = loader.and_then(|path| path.canonicalize().ok()) {
            paths.push(loader);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn interpreter(path: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStringExt as _;
    let invalid = || std::io::Error::other("unsupported external executable");
    let mut file = std::fs::File::open(path)?;
    let mut header = [0; 64];
    file.read_exact(&mut header)?;
    if &header[..6] != b"\x7fELF\x02\x01" {
        return Err(invalid());
    }
    let word = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let offset = u64::from_le_bytes(header[32..40].try_into().map_err(|_| invalid())?);
    if word(54) != 56 || word(56) > 1024 {
        return Err(invalid());
    }
    for index in 0..u64::from(word(56)) {
        file.seek(std::io::SeekFrom::Start(offset.checked_add(index * 56).ok_or_else(invalid)?))?;
        let mut program = [0; 56];
        file.read_exact(&mut program)?;
        if program[..4] != [3, 0, 0, 0] {
            continue;
        }
        let at = u64::from_le_bytes(program[8..16].try_into().map_err(|_| invalid())?);
        let size = u64::from_le_bytes(program[32..40].try_into().map_err(|_| invalid())?);
        if !(2..=4096).contains(&size) {
            return Err(invalid());
        }
        file.seek(std::io::SeekFrom::Start(at))?;
        let mut bytes = vec![0; size as usize];
        file.read_exact(&mut bytes)?;
        if bytes.pop() != Some(0) || bytes.contains(&0) {
            return Err(invalid());
        }
        let loader = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        if !loader.is_absolute() {
            return Err(invalid());
        }
        return Ok(Some(loader));
    }
    Ok(None)
}

/// `prctl(PR_SET_NO_NEW_PRIVS, 1)`.
fn no_new_privs() -> bool {
    // SAFETY: `prctl` with this option reads no pointers and writes nothing; the
    // four trailing arguments are ignored and passed as zero, as the manual page
    // requires.
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 }
}

/// `setsid()`: a session with no controlling terminal, led by this process.
fn own_session() -> bool {
    // SAFETY: `setsid` takes nothing and changes only this process's session.
    unsafe { libc::setsid() != -1 }
}

/// The address-space, CPU and file-size ceilings.
///
/// **`SIGXFSZ` is ignored**, and the disposition survives `exec`: a write past the
/// file ceiling is then `EFBIG` rather than a renderer killed mid-page by a
/// fontconfig cache write it would have shrugged off.
fn caps(limits: Limits) -> bool {
    let set = |resource, value: u64| {
        let limit = libc::rlimit { rlim_cur: value, rlim_max: value };
        // SAFETY: `setrlimit` reads the struct and writes nothing.
        unsafe { libc::setrlimit(resource, &raw const limit) == 0 }
    };
    // SAFETY: `SIG_IGN` installs no handler, so nothing runs on this signal.
    let ignored = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) } != libc::SIG_ERR;
    set(libc::RLIMIT_AS, limits.address_space)
        & set(libc::RLIMIT_CPU, limits.cpu_seconds)
        & set(libc::RLIMIT_FSIZE, limits.file_size)
        & ignored
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
/// ABI 3 adds truncation independently of writes; below it, seccomp denies
/// truncation instead. Executable paths receive a separate grant from writable
/// output directories.
///
/// **From ABI 6 the domain is scoped**: a signal or an abstract Unix socket
/// reaches only processes inside it, which is the helper and what it started. A
/// kernel older than the 24-byte attribute reads the first eight and checks the
/// rest are zero, which they are wherever `scoped` is not set.
fn landlock(readable: &[&Path], executable: &[&Path], writable: &[&Path]) -> Option<i32> {
    let abi = landlock_abi()?;
    let rights =
        ABI_1_RIGHTS | if abi >= 2 { 1 << 13 } else { 0 } | if abi >= 3 { 1 << 14 } else { 0 };
    let attribute = RulesetAttribute {
        handled_access_fs: rights,
        handled_access_net: 0,
        scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
    };
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
    let rights = if path.is_file() {
        rights & (LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_READ_FILE | (1 << 1) | (1 << 14))
    } else {
        rights
    };
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
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttribute {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{REFUSED_WITHOUT_LANDLOCK, Report, Role, WITHOUT_LANDLOCK};
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    /// Runs `test` again in a child with `envs` set.
    ///
    /// **Confinement cannot be taken back**, so it is applied in a disposable
    /// process and never to the test runner. Through pipes rather than the
    /// runner's own descriptors, because a confined child may have a file ceiling
    /// of zero and a runner's `stdout` can be a file.
    fn again(test: &str, envs: &[(&str, &OsStr)]) -> Output {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--nocapture"]).env_remove(WITHOUT_LANDLOCK);
        for (name, value) in envs {
            command.env(name, value);
        }
        command.output().unwrap()
    }

    /// Whether the child ran its one test and passed it. A name that matched
    /// nothing runs zero tests and exits zero, which is not a pass.
    fn passed(output: &Output) -> bool {
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains(" 1 passed")
    }

    fn said(output: &Output) -> String {
        let out = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        format!("{out}{err}")
    }

    /// The errno a raw syscall failed with, or `None` where it succeeded.
    fn failed(result: libc::c_long) -> Option<i32> {
        (result == -1).then(|| std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    /// A directory of this test's own, empty.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn name_of(path: &Path) -> std::ffi::CString {
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap()
    }

    /// The line names what is missing as loudly as what is present, which is the
    /// difference between a log and a reassurance.
    #[test]
    fn the_report_says_what_is_not_in_force() {
        let full = Report {
            session: true,
            no_new_privs: true,
            limits: true,
            landlock: Some(8),
            unconfined: false,
            seccomp: true,
        };
        let line = full.line();
        assert!(line.contains("landlock abi 8"), "{line}");
        assert!(line.contains("signals scoped"), "{line}");
        assert!(line.contains("own session"), "{line}");
        assert!(!line.contains("FAILED") && !line.contains("SHARED"), "{line}");

        let old = Report { landlock: Some(2), ..full }.line();
        assert!(old.contains("truncation by seccomp, signals by seccomp"), "{old}");

        let none = Report::default();
        let line = none.line();
        assert!(line.contains("landlock ABSENT"), "{line}");
        assert!(line.contains("session SHARED"), "{line}");
        assert!(line.contains("no-new-privs FAILED"), "{line}");
        assert!(line.contains("rlimits FAILED"), "{line}");
        assert!(!line.contains("by setting"), "{line}");
        let allowed = Report { unconfined: true, ..none }.line();
        assert!(allowed.contains("decoding anyway by setting"), "{allowed}");
    }

    /// Each role gets the policy ADR 0046 gives it, and truncation is seccomp's
    /// exactly where Landlock has no right for it.
    #[test]
    fn each_helper_gets_its_own_policy() {
        let thumbnails = Role::Thumbnails.policy(Some(8));
        assert!(!thumbnails.spawns && !thumbnails.stamps && !thumbnails.reaps);
        let pages = Role::Pages.policy(Some(8));
        assert!(pages.spawns && !pages.stamps && !pages.reaps);
        assert!(Role::Listing.policy(Some(8)).reaps);
        let unpacking = Role::Unpacking.policy(Some(8));
        assert!(unpacking.spawns && unpacking.stamps && unpacking.reaps);
        assert!(Role::Pages.policy(None).truncation);
        assert!(Role::Pages.policy(Some(2)).truncation);
        assert!(!Role::Pages.policy(Some(3)).truncation);
        assert_eq!(Role::Unpacking.limits().file_size, libc::RLIM_INFINITY);
        assert_eq!(Role::Listing.limits().file_size, 0);
        assert_eq!(Role::Thumbnails.limits().file_size, 1024 * 1024);
    }

    /// **The finding ADR 0046 exists for, reproduced the way the review did it**:
    /// Landlock's syscalls answer `ENOSYS`, and the helper used to carry on with
    /// every write right the user has. It refuses now, with a status the host can
    /// tell from a crash, and decodes only where the setting said `allow`.
    #[test]
    fn a_helper_without_landlock_refuses_unless_the_setting_allows_it() {
        const MARKER: &str = "SKERRY_REFUSAL_REGRESSION";
        const NAME: &str =
            "sandbox::tests::a_helper_without_landlock_refuses_unless_the_setting_allows_it";
        if std::env::var_os(MARKER).is_some() {
            assert!(super::no_new_privs());
            assert!(crate::seccomp::hide_landlock());
            let report = super::confine(Role::Pages, &[Path::new("/")], &[], &[]);
            assert_eq!(report.landlock, None, "{}", report.line());
            assert!(report.unconfined, "{}", report.line());
            return;
        }
        let marked = (MARKER, OsStr::new("1"));
        let refused = again(NAME, &[marked]);
        assert_eq!(refused.status.code(), Some(REFUSED_WITHOUT_LANDLOCK), "{}", said(&refused));
        assert!(
            said(&refused).contains("refusing to decode without Landlock"),
            "{}",
            said(&refused)
        );

        let allowed = again(NAME, &[marked, (WITHOUT_LANDLOCK, OsStr::new("allow"))]);
        assert!(passed(&allowed), "{}", said(&allowed));

        let misspelt = again(NAME, &[marked, (WITHOUT_LANDLOCK, OsStr::new("yes"))]);
        assert_eq!(misspelt.status.code(), Some(REFUSED_WITHOUT_LANDLOCK), "{}", said(&misspelt));
    }

    /// A confined helper leads a session with no terminal, and a write past its
    /// file ceiling is an error it survives rather than a signal that kills it.
    #[test]
    fn a_confined_helper_leads_its_own_session_and_stops_at_its_file_ceiling() {
        const MARKER: &str = "SKERRY_SESSION_REGRESSION";
        const NAME: &str =
            "sandbox::tests::a_confined_helper_leads_its_own_session_and_stops_at_its_file_ceiling";
        if let Some(root) = std::env::var_os(MARKER) {
            let root = PathBuf::from(root);
            let report = super::confine(Role::Thumbnails, &[Path::new("/")], &[], &[&root]);
            assert!(report.session, "{}", report.line());
            // SAFETY: neither call takes a pointer.
            assert_eq!(unsafe { libc::getsid(0) }, unsafe { libc::getpid() });
            assert!(std::fs::File::open("/dev/tty").is_err(), "a terminal is still reachable");
            let big = std::fs::write(root.join("big"), vec![0u8; 2 * 1024 * 1024]);
            assert_eq!(big.err().and_then(|it| it.raw_os_error()), Some(libc::EFBIG));
            assert!(std::fs::write(root.join("small"), [0u8; 4096]).is_ok());
            return;
        }
        let root = scratch("session");
        let output =
            again(NAME, &[(MARKER, root.as_os_str()), (WITHOUT_LANDLOCK, OsStr::new("allow"))]);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(passed(&output), "{}", said(&output));
    }

    /// **What Landlock does not govern**, because none of it is opening a path:
    /// a file's mode, owner, attributes and times, another process, and the
    /// keyrings. Each was reproduced against a helper at ABI 8 before this filter
    /// named it. Signal 0 and null arguments, so a filter that let one through
    /// would change nothing on the machine running the test.
    #[test]
    fn seccomp_keeps_a_helper_off_metadata_other_processes_and_keys() {
        const MARKER: &str = "SKERRY_SECCOMP_REACH";
        const NAME: &str =
            "sandbox::tests::seccomp_keeps_a_helper_off_metadata_other_processes_and_keys";
        if let Some(root) = std::env::var_os(MARKER) {
            let file = PathBuf::from(root).join("private");
            let opened = std::fs::File::open(&file).unwrap();
            // **Each priority call asks for what this process already has**, so
            // one a broken filter let through is a no-op rather than a desktop
            // reniced — `PRIO_USER` would be every process this user runs.
            // SAFETY: neither query takes a pointer.
            let priorities = unsafe {
                (
                    libc::getpriority(libc::PRIO_PROCESS, 0),
                    libc::syscall(libc::SYS_ioprio_get, 1, 0),
                )
            };
            assert!(super::no_new_privs());
            assert!(crate::seccomp::install(Role::Pages.policy(Some(8))));
            let mut attempts = files_reached(&file, &opened);
            attempts.extend(processes_reached(priorities));
            for (call, errno) in attempts {
                assert_eq!(errno, Some(libc::EPERM), "{call} was not denied");
            }
            // **And what a helper needs of the same calls still works**: a
            // signal to itself, which is what `abort` is, and its own limits.
            let own = libc::c_long::from(std::process::id());
            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            let (null, at) = (std::ptr::null::<u8>(), &raw mut limit);
            // SAFETY: signal 0 to this process; a live out-pointer for the limit.
            unsafe {
                assert_eq!(failed(libc::syscall(libc::SYS_kill, own, 0)), None);
                assert_eq!(failed(libc::syscall(libc::SYS_tgkill, own, libc::gettid(), 0)), None);
                let mine = libc::syscall(libc::SYS_prlimit64, 0, libc::RLIMIT_CPU, null, at);
                assert_eq!(failed(mine), None);
            }
            return;
        }
        let root = scratch("reach");
        std::fs::write(root.join("private"), "key").unwrap();
        std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let output = again(NAME, &[(MARKER, root.as_os_str())]);
        let mode = std::fs::metadata(root.join("private")).unwrap().permissions().mode();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(passed(&output), "{}", said(&output));
        assert_eq!(mode & 0o777, 0o600, "the mode changed under a denied chmod");
    }

    /// Each call that would change `file`'s metadata, and how it failed.
    fn files_reached(file: &Path, opened: &std::fs::File) -> Vec<(&'static str, Option<i32>)> {
        let name = name_of(file);
        let fd = std::os::fd::AsRawFd::as_raw_fd(opened);
        let (null, attribute) = (std::ptr::null::<u8>(), c"user.skerry".as_ptr());
        let at = libc::AT_FDCWD;
        // SAFETY: each call passes a descriptor, a live NUL-terminated name, or
        // null where the kernel refuses before reading.
        let attempts = unsafe {
            vec![
                (
                    "fchmodat",
                    failed(libc::syscall(libc::SYS_fchmodat, at, name.as_ptr(), 0o644, 0)),
                ),
                ("fchmodat2", failed(libc::syscall(452, at, name.as_ptr(), 0o644, 0))),
                ("fchmod", failed(libc::syscall(libc::SYS_fchmod, fd, 0o644))),
                (
                    "fchownat",
                    failed(libc::syscall(libc::SYS_fchownat, at, name.as_ptr(), -1, -1, 0)),
                ),
                ("fchown", failed(libc::syscall(libc::SYS_fchown, fd, -1, -1))),
                (
                    "setxattr",
                    failed(libc::syscall(libc::SYS_setxattr, name.as_ptr(), attribute, null, 0, 0)),
                ),
                ("fremovexattr", failed(libc::syscall(libc::SYS_fremovexattr, fd, attribute))),
                (
                    "utimensat",
                    failed(libc::syscall(libc::SYS_utimensat, at, name.as_ptr(), null, 0)),
                ),
            ]
        };
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above.
        let attempts = attempts.into_iter().chain(unsafe {
            [
                ("chmod", failed(libc::syscall(libc::SYS_chmod, name.as_ptr(), 0o644))),
                ("chown", failed(libc::syscall(libc::SYS_chown, name.as_ptr(), -1, -1))),
                ("lchown", failed(libc::syscall(libc::SYS_lchown, name.as_ptr(), -1, -1))),
                ("utimes", failed(libc::syscall(libc::SYS_utimes, name.as_ptr(), null))),
            ]
        });
        attempts.into_iter().collect()
    }

    /// Each call that would reach another process or the keyrings, and how it
    /// failed. `priorities` is what this process already has, so a priority call
    /// that got through would change nothing.
    fn processes_reached(priorities: (i32, libc::c_long)) -> Vec<(&'static str, Option<i32>)> {
        let (nice, io) = priorities;
        let parent = libc::c_long::from(std::os::unix::process::parent_id());
        let null = std::ptr::null::<u8>();
        // SAFETY: signal 0, which delivers nothing, and null where the kernel
        // refuses before reading.
        unsafe {
            vec![
                ("keyctl", failed(libc::syscall(libc::SYS_keyctl, 0, -3, 0))),
                ("add_key", failed(libc::syscall(libc::SYS_add_key, null, null, null, 0, 0))),
                ("request_key", failed(libc::syscall(libc::SYS_request_key, null, null, null, 0))),
                ("kill parent", failed(libc::syscall(libc::SYS_kill, parent, 0))),
                ("kill everyone", failed(libc::syscall(libc::SYS_kill, -1, 0))),
                ("tgkill parent", failed(libc::syscall(libc::SYS_tgkill, parent, parent, 0))),
                ("tkill", failed(libc::syscall(libc::SYS_tkill, parent, 0))),
                (
                    "rt_sigqueueinfo",
                    failed(libc::syscall(libc::SYS_rt_sigqueueinfo, parent, 0, null)),
                ),
                (
                    "pidfd_send_signal",
                    failed(libc::syscall(libc::SYS_pidfd_send_signal, -1, 0, null, 0)),
                ),
                (
                    "prlimit64",
                    failed(libc::syscall(
                        libc::SYS_prlimit64,
                        parent,
                        libc::RLIMIT_CPU,
                        null,
                        null,
                    )),
                ),
                (
                    "sched_setaffinity",
                    failed(libc::syscall(libc::SYS_sched_setaffinity, parent, 0, null)),
                ),
                (
                    "setpriority",
                    failed(libc::syscall(libc::SYS_setpriority, libc::PRIO_PROCESS, 0, nice)),
                ),
                ("ioprio_set", failed(libc::syscall(libc::SYS_ioprio_set, 1, 0, io))),
                (
                    "process_madvise",
                    failed(libc::syscall(libc::SYS_process_madvise, -1, null, 0, 0, 0)),
                ),
            ]
        }
    }

    /// The archive helper's two exceptions, and nothing past them: it stamps
    /// what it wrote and kills the decompressor it started, and it still signals
    /// no group and changes no mode.
    #[test]
    fn an_unpack_stamps_and_kills_its_decompressor_but_never_a_group() {
        const MARKER: &str = "SKERRY_SECCOMP_UNPACK";
        const NAME: &str =
            "sandbox::tests::an_unpack_stamps_and_kills_its_decompressor_but_never_a_group";
        if let Some(root) = std::env::var_os(MARKER) {
            let file = PathBuf::from(root).join("entry");
            let name = name_of(&file);
            // SAFETY: a pointer-free query of the parent's group.
            let group = libc::c_long::from(unsafe { libc::getpgid(0) });
            assert!(super::no_new_privs());
            assert!(crate::seccomp::install(Role::Unpacking.policy(Some(8))));
            let null = std::ptr::null::<u8>();
            // SAFETY: a live NUL-terminated name, null times, signal 0.
            unsafe {
                assert_eq!(
                    failed(libc::syscall(
                        libc::SYS_utimensat,
                        libc::AT_FDCWD,
                        name.as_ptr(),
                        null,
                        0
                    )),
                    None
                );
                assert_eq!(failed(libc::syscall(libc::SYS_kill, -1, 0)), Some(libc::EPERM));
                assert_eq!(failed(libc::syscall(libc::SYS_kill, 0, 0)), Some(libc::EPERM));
                assert_eq!(failed(libc::syscall(libc::SYS_kill, -group, 0)), Some(libc::EPERM));
                assert_eq!(
                    failed(libc::syscall(
                        libc::SYS_fchmodat,
                        libc::AT_FDCWD,
                        name.as_ptr(),
                        0o644,
                        0
                    )),
                    Some(libc::EPERM)
                );
            }
            let mut decompressor = Command::new("sleep").arg("30").spawn().unwrap();
            decompressor.kill().unwrap();
            assert!(!decompressor.wait().unwrap().success());
            return;
        }
        let root = scratch("unpack");
        std::fs::write(root.join("entry"), "x").unwrap();
        let output = again(NAME, &[(MARKER, root.as_os_str())]);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(passed(&output), "{}", said(&output));
    }

    /// **Below Landlock ABI 3 truncation is not a Landlock right**, so on kernels
    /// 5.13 to 6.1 any file the user could write could be emptied. The filter
    /// takes it over there, and a file created whole is still a file created.
    #[test]
    fn below_abi_3_seccomp_denies_truncation() {
        const MARKER: &str = "SKERRY_SECCOMP_TRUNCATION";
        const NAME: &str = "sandbox::tests::below_abi_3_seccomp_denies_truncation";
        if let Some(root) = std::env::var_os(MARKER) {
            let root = PathBuf::from(root);
            let file = root.join("input");
            let name = name_of(&file);
            let writing = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
            let fd = std::os::fd::AsRawFd::as_raw_fd(&writing);
            assert!(super::no_new_privs());
            assert!(crate::seccomp::install(Role::Pages.policy(Some(2))));
            let how = [libc::O_WRONLY as u64 | libc::O_TRUNC as u64, 0, 0];
            // SAFETY: a live NUL-terminated name and a live `open_how`.
            unsafe {
                assert_eq!(
                    failed(libc::syscall(libc::SYS_truncate, name.as_ptr(), 0)),
                    Some(libc::EPERM)
                );
                assert_eq!(failed(libc::syscall(libc::SYS_ftruncate, fd, 0)), Some(libc::EPERM));
                assert_eq!(
                    failed(libc::syscall(
                        libc::SYS_openat2,
                        libc::AT_FDCWD,
                        name.as_ptr(),
                        how.as_ptr(),
                        24
                    )),
                    Some(libc::ENOSYS)
                );
            }
            let emptied = std::fs::OpenOptions::new().write(true).truncate(true).open(&file);
            assert_eq!(emptied.err().and_then(|it| it.raw_os_error()), Some(libc::EPERM));
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(root.join("new"))
                    .is_ok()
            );
            return;
        }
        let root = scratch("truncation");
        std::fs::write(root.join("input"), "valuable").unwrap();
        let output = again(NAME, &[(MARKER, root.as_os_str())]);
        let kept = std::fs::read(root.join("input")).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(passed(&output), "{}", said(&output));
        assert_eq!(kept, b"valuable");
    }

    /// **The thumbnailer starts nothing**, which is what stands in for an
    /// `RLIMIT_NPROC` — see [`super::Limits`] for why that is not a cap — while a
    /// thread, which a decoder may want, is still a thread.
    #[test]
    fn the_thumbnailer_starts_no_process_but_still_starts_threads() {
        const MARKER: &str = "SKERRY_SECCOMP_SPAWN";
        const NAME: &str =
            "sandbox::tests::the_thumbnailer_starts_no_process_but_still_starts_threads";
        if std::env::var_os(MARKER).is_some() {
            assert!(super::no_new_privs());
            assert!(crate::seccomp::install(Role::Thumbnails.policy(Some(8))));
            assert!(Command::new("true").status().is_err(), "a process started");
            assert_eq!(std::thread::spawn(|| 42).join().unwrap(), 42);
            let program = c"/bin/true";
            let arguments = [program.as_ptr(), std::ptr::null()];
            let environment = [std::ptr::null::<libc::c_char>()];
            // SAFETY: a NUL-terminated path and two null-terminated arrays. A
            // filter that let this through would end the test, which is a fail.
            let executed = unsafe {
                libc::syscall(
                    libc::SYS_execve,
                    program.as_ptr(),
                    arguments.as_ptr(),
                    environment.as_ptr(),
                )
            };
            assert_eq!(failed(executed), Some(libc::EPERM));
            #[cfg(target_arch = "x86_64")]
            {
                // SAFETY: `fork` takes nothing; a child that a broken filter let
                // through leaves at once.
                let forked = unsafe { libc::syscall(libc::SYS_fork) };
                if forked == 0 {
                    // SAFETY: `_exit` in the child, as `fork` requires.
                    unsafe { libc::_exit(0) };
                }
                assert_eq!(failed(forked), Some(libc::EPERM));
            }
            return;
        }
        let output = again(NAME, &[(MARKER, OsStr::new("1"))]);
        assert!(passed(&output), "{}", said(&output));
    }

    #[test]
    fn landlock_denies_input_truncation_and_output_execution() {
        const MARKER: &str = "SKERRY_LANDLOCK_REGRESSION_ROOT";
        if let Some(root) = std::env::var_os(MARKER) {
            let root = PathBuf::from(root);
            let out = root.join("out");
            assert!(super::no_new_privs());
            let tools = super::executables(&["true"]);
            let executable: Vec<_> = tools.iter().map(std::path::PathBuf::as_path).collect();
            let Some(abi) = super::landlock(&[Path::new("/")], &executable, &[&out]) else {
                assert!(
                    std::env::var_os("SKERRY_REQUIRE_LANDLOCK").is_none(),
                    "Landlock is required by this test environment"
                );
                eprintln!("skerry: skip: Landlock unavailable in this environment");
                return;
            };
            assert!(Command::new("true").status().unwrap().success());
            assert_eq!(std::fs::read(root.join("input")).unwrap(), b"valuable");
            std::fs::write(out.join("allowed"), "output").unwrap();
            if abi >= 3 {
                assert!(std::fs::File::create(root.join("input")).is_err());
                assert_eq!(std::fs::read(root.join("input")).unwrap(), b"valuable");
            }
            assert!(Command::new(out.join("program")).status().is_err());
            return;
        }
        let root = std::env::temp_dir().join(format!("skerry-landlock-{}", std::process::id()));
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("input"), "valuable").unwrap();
        std::fs::copy("/bin/true", root.join("out/program")).unwrap();
        std::fs::set_permissions(root.join("out/program"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
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

    /// **From ABI 6 a signal stays inside the helper's domain**: the window,
    /// outside it, cannot be stopped, and the decompressor the helper started,
    /// inside it, can still be killed. Below ABI 6 this says it skipped.
    #[test]
    fn landlock_scopes_signals_to_the_helper_and_what_it_starts() {
        const MARKER: &str = "SKERRY_LANDLOCK_SCOPE";
        const NAME: &str =
            "sandbox::tests::landlock_scopes_signals_to_the_helper_and_what_it_starts";
        if std::env::var_os(MARKER).is_some() {
            assert!(super::no_new_privs());
            let tools = super::executables(&["sleep"]);
            let executable: Vec<_> = tools.iter().map(PathBuf::as_path).collect();
            let abi = super::landlock(&[Path::new("/")], &executable, &[]);
            let Some(abi) = abi.filter(|abi| *abi >= 6) else {
                eprintln!("skerry: skip: Landlock ABI {abi:?} has no signal scope");
                return;
            };
            let parent = libc::c_long::from(std::os::unix::process::parent_id());
            // SAFETY: signal 0 delivers nothing.
            let reached = unsafe { libc::syscall(libc::SYS_kill, parent, 0) };
            assert_eq!(failed(reached), Some(libc::EPERM), "ABI {abi} let a signal out");
            let mut decompressor = Command::new("sleep").arg("30").spawn().unwrap();
            decompressor.kill().unwrap();
            assert!(!decompressor.wait().unwrap().success());
            return;
        }
        let output = again(NAME, &[(MARKER, OsStr::new("1"))]);
        assert!(passed(&output), "{}", said(&output));
    }

    #[test]
    fn seccomp_denies_networking_but_preserves_threads_and_tools() {
        const MARKER: &str = "SKERRY_SECCOMP_REGRESSION";
        if std::env::var_os(MARKER).is_some() {
            assert!(super::no_new_privs());
            assert!(crate::seccomp::install(Role::Pages.policy(Some(8))));
            for family in [libc::AF_INET, libc::AF_INET6, libc::AF_UNIX] {
                // SAFETY: socket has no pointer arguments. On regression, close
                // the unexpected descriptor before reporting the failed denial.
                let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
                let error = std::io::Error::last_os_error().raw_os_error();
                if fd >= 0 {
                    unsafe {
                        libc::close(fd);
                    }
                }
                assert_eq!((fd, error), (-1, Some(libc::EPERM)));
            }
            // SAFETY: null clone3 arguments are rejected before dereferencing;
            // the filter must return ENOSYS for the libc fallback.
            assert_eq!(unsafe { libc::syscall(libc::SYS_clone3, 0usize, 0usize) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENOSYS));
            assert_eq!(std::thread::spawn(|| 42).join().unwrap(), 42);
            assert!(Command::new("true").status().unwrap().success());
            return;
        }
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sandbox::tests::seccomp_denies_networking_but_preserves_threads_and_tools",
                    "--nocapture"
                ])
                .env(MARKER, "1")
                .status()
                .unwrap()
                .success()
        );
    }
}
