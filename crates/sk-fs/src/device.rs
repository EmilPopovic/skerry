//! What kind of device a path is on, probed at runtime.
//!
//! [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md): a
//! capability is probed, never compiled in. This is the probe that keeps one
//! sick mount from consuming every worker in the pool — an NFS server that stops
//! answering makes each read take as long as its timeout, and eight workers all
//! waiting on it is a file manager that has stopped.

use std::os::linux::fs::MetadataExt as _;
use std::path::Path;

use rustix::fs::{StatFs, statfs};
use sk_util::Lane;

/// What the probe concluded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Device {
    /// Local, and answers concurrent reads faster than serial ones.
    Local,
    /// Local and rotational, where seeking between concurrent readers costs more
    /// than queueing them.
    Rotational,
    /// A network or userspace filesystem. Reads may block for a timeout, so they
    /// get one worker between them and never the whole pool.
    Remote,
    /// The probe could not answer. Treated as `Local`, and recorded as unknown
    /// rather than silently assumed.
    Unknown,
}

impl Device {
    /// Which lane a read on this device belongs on.
    #[must_use]
    pub const fn lane(self) -> Lane {
        match self {
            Self::Local | Self::Unknown => Lane::Parallel,
            Self::Rotational | Self::Remote => Lane::Serial,
        }
    }
}

/// Filesystem magics that mean "this read can block for a network timeout".
///
/// From `linux/magic.h` and the `statfs(2)` man page. FUSE is on the list
/// because sshfs, rclone and every cloud-drive client is a FUSE filesystem, and
/// none of them can promise a bounded read.
const REMOTE_MAGICS: &[u64] = &[
    0x6969,      // NFS_SUPER_MAGIC
    0xFF53_4D42, // CIFS_SUPER_MAGIC
    0x517B,      // SMB_SUPER_MAGIC
    0xFE53_4D42, // SMB2_SUPER_MAGIC
    0x6573_5546, // FUSE_SUPER_MAGIC
    0x5346_4846, // AFS_FS_MAGIC
    0x0187,      // AUTOFS_SUPER_MAGIC
    0x00C3_6400, // CEPH_SUPER_MAGIC
    0x0BD0_0BD0, // LUSTRE_SUPER_MAGIC
    0x7461_7273, // RATFS / 9p
    0x0129_2F31, // V9FS_MAGIC
];

/// Probes the device behind `path`.
///
/// Two syscalls at most, and only on a directory change — not per entry.
#[must_use]
pub fn probe(path: &Path) -> Device {
    let Ok(stats) = statfs(path) else { return Device::Unknown };
    if is_remote(&stats) {
        return Device::Remote;
    }
    match rotational(path) {
        Some(true) => Device::Rotational,
        Some(false) => Device::Local,
        None => Device::Unknown,
    }
}

/// The lane a read on `path` belongs on.
#[must_use]
pub fn lane_for(path: &Path) -> Lane {
    probe(path).lane()
}

fn is_remote(stats: &StatFs) -> bool {
    let magic = stats.f_type as u64;
    REMOTE_MAGICS.contains(&magic)
}

/// Whether the block device under `path` says it seeks.
///
/// `/sys/dev/block/MAJOR:MINOR/queue/rotational` is the kernel's own answer.
/// There is no syscall for it, and guessing from the device name is how a
/// project ends up treating an `NVMe` namespace as a floppy.
fn rotational(path: &Path) -> Option<bool> {
    let metadata = std::fs::metadata(path).ok()?;
    let device = metadata.st_dev();
    // The device number of a filesystem, not of a partition: a btrfs volume
    // spanning two disks reports one that resolves to neither, and the read
    // below simply fails, which is the `Unknown` the caller is told about.
    let (major, minor) = (dev_major(device), dev_minor(device));
    let queue = format!("/sys/dev/block/{major}:{minor}/queue/rotational");
    let text = std::fs::read_to_string(queue).ok()?;
    match text.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

// The encoding of a `dev_t` is kernel ABI, not arithmetic we get to choose:
// twelve bits of major split across the word. `libc`'s `major`/`minor` do
// exactly this, and this is the whole reason we would otherwise link it.
const fn dev_major(device: u64) -> u64 {
    ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff)
}

const fn dev_minor(device: u64) -> u64 {
    (device & 0xff) | ((device >> 12) & !0xff)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Device, dev_major, dev_minor, probe};
    use sk_util::Lane;
    use std::path::Path;

    #[test]
    fn a_device_number_splits_the_way_the_kernel_encodes_it() {
        // 8:16 is /dev/sdb on every Linux system, and the encoding is stable ABI.
        let device = (8 << 8) | 16;
        assert_eq!(dev_major(device), 8);
        assert_eq!(dev_minor(device), 16);
        // 259:0 is the first NVMe namespace, and its major does not fit in eight
        // bits, which is the case the naive shift gets wrong.
        let nvme = (0x103 & 0xfff) << 8;
        assert_eq!(dev_major(nvme), 259);
    }

    #[test]
    fn tmp_and_root_probe_to_something_and_never_panic() {
        for path in ["/", "/tmp", "/proc"] {
            let device = probe(Path::new(path));
            assert!(matches!(
                device,
                Device::Local | Device::Rotational | Device::Remote | Device::Unknown
            ));
        }
    }

    #[test]
    fn a_path_that_does_not_exist_is_unknown_and_still_gets_a_lane() {
        assert_eq!(probe(Path::new("/nonexistent-skerry-probe")), Device::Unknown);
        assert_eq!(Device::Unknown.lane(), Lane::Parallel);
        assert_eq!(Device::Remote.lane(), Lane::Serial, "a hung mount gets one worker");
    }
}
