// SPDX-License-Identifier: Apache-2.0
//! What is mounted, from `/proc/self/mountinfo`.
//!
//! For a sidebar, which needs the handful of mounts a person thinks of as a
//! place: the filesystem, a USB stick, a network share. This machine has
//! **sixty-four** mounts and four of them qualify — the rest are `cgroup2`,
//! `tracefs`, one `tmpfs` per service credential, and one `squashfs` per snap.
//!
//! **An allow-list of filesystem types, not a deny-list.** A deny-list is a
//! promise to have heard of every pseudo-filesystem the kernel will ever add, and
//! the failure mode is a sidebar with `bpf` in it. The allow-list's failure mode
//! is a real disk nobody sees, which is one line to fix and visible the moment
//! somebody plugs one in.

use std::path::{Path, PathBuf};

/// A filesystem a person would call a place.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mount {
    /// The label the filesystem was formatted with, where it has one.
    ///
    /// **From `/dev/disk/by-label`, not from udisks2.** The names in that
    /// directory are the labels and the symlinks point at the devices, which is
    /// the whole lookup — so a session with no D-Bus, which is the bare Sway case
    /// [06-portability.md](../../../docs/06-portability.md) is written for, gets
    /// the same names a GNOME session does. The escaping is `\x20` for a space
    /// and so on, because a label is a filename here.
    pub label: Option<String>,
    /// Bytes a person could still write there, and the size of the filesystem.
    ///
    /// **`statvfs`, on the job that reads the mount table.** Free space is one
    /// syscall and udisks2 is a round trip; the only thing D-Bus knows that this
    /// does not is a device that is *not* mounted, which has no free space to
    /// report anyway. `None` for a remote mount, which is where the syscall can
    /// take a network timeout — the one thing rule 1 will not have.
    pub free: Option<(u64, u64)>,
    /// Where it is mounted.
    pub at: PathBuf,
    /// What it is: `ext4`, `vfat`, `fuse.sshfs`.
    pub kind: String,
    /// What backs it, for the label when the mount point has no useful name.
    pub source: String,
    /// Whether it may block for a network timeout — the same question
    /// [`crate::device`] asks, answered from the type rather than by `statfs`,
    /// because this list is read before anything is opened.
    pub remote: bool,
    /// The device's number, as `major:minor`.
    ///
    /// **The only field that joins reliably against `/sys/class/block`** — see
    /// [`crate::volumes`]. A source string does not: this filesystem is
    /// `/dev/mapper/ubuntu--vg-ubuntu--lv` in the mount table and `dm-1` in
    /// sysfs, and the same disk is `/dev/sda1` in one and `sda1` in the other.
    /// The kernel writes the number in both places and means the same thing by
    /// it.
    pub numbers: String,
}

impl Mount {
    /// Whether this is the root filesystem.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.at == std::path::Path::new("/")
    }
}

/// Filesystem types worth showing, and whether each can block.
///
/// Read by [`crate::volumes`] too, which is the point of it being one list: a
/// filesystem type this program can show is added here and nowhere else.
///
/// Ordered by how likely a person is to have one, which is only a comment: the
/// lookup is linear over twenty entries and runs once per mount.
pub(crate) const INTERESTING: &[(&str, bool)] = &[
    ("ext4", false),
    ("ext3", false),
    ("ext2", false),
    ("btrfs", false),
    ("xfs", false),
    ("f2fs", false),
    ("bcachefs", false),
    ("zfs", false),
    ("vfat", false),
    ("exfat", false),
    ("ntfs3", false),
    ("ntfs", false),
    // `fuseblk` is what ntfs-3g and exfat-fuse mount as, so it is a local disk
    // whatever its name suggests.
    ("fuseblk", false),
    ("iso9660", false),
    ("udf", false),
    ("nfs", true),
    ("nfs4", true),
    ("cifs", true),
    ("smb3", true),
    ("9p", true),
    ("fuse.sshfs", true),
    ("fuse.rclone", true),
    ("fuse.s3fs", true),
];

/// Every device number in the mount table, whatever kind of filesystem it is.
///
/// **Every line, not the interesting ones.** `/boot` and `/boot/efi` are real
/// filesystems that [`is_system`] deliberately keeps out of the sidebar — so
/// they are absent from [`mounts`] while being very much mounted, and
/// [`crate::volumes`] listing them as *unmounted* disks somebody could mount is
/// worse than not listing them at all. Measured: both appeared, as two rows
/// nobody wants, the first time the two lists were joined.
#[must_use]
pub fn mounted_numbers() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    text.lines().filter_map(|line| line.split(' ').nth(2).map(str::to_owned)).collect()
}

/// Every mount worth showing, root first and then by mount point.
///
/// **Runs on a job.** It is one read of a `/proc` file, which is a page or two
/// and never blocks — but rule 1 has no exemption for small files, and the
/// sidebar this feeds is built off the frame loop anyway.
#[must_use]
pub fn mounts() -> Vec<Mount> {
    let mut found = read("/proc/self/mountinfo");
    let labels = labels();
    for mount in &mut found {
        mount.label = labels
            .iter()
            .find(|(device, _)| *device == mount.source)
            .map(|(_, label)| label.clone());
        // **Not for a remote one.** `statvfs` over NFS is a round trip to a
        // server that may not answer, and [Spike
        // B](../../../spikes/b-enumeration/README.md) measured what one
        // unresponsive mount costs: 28,000 ms.
        if !mount.remote {
            mount.free = room_on(&mount.at);
        }
    }
    found
}

/// Every filesystem label this machine can see, with the device it names.
///
/// `/dev/disk/by-label` is a directory of symlinks: the *name* is the label and
/// the target is the device. Absent on a machine with no udev, which is a machine
/// with no labels rather than an error.
fn labels() -> Vec<(String, String)> {
    let Ok(names) = crate::scan::names(Path::new(BY_LABEL), &sk_util::Cancel::new()) else {
        return Vec::new();
    };
    let listing = &names.listing;
    let mut found = Vec::new();
    for row in 0..listing.rows() {
        let Some(id) = listing.at(row) else { continue };
        let name = listing.name(id);
        let link = Path::new(BY_LABEL).join(name);
        let Ok(target) = std::fs::canonicalize(&link) else { continue };
        found.push((target.to_string_lossy().into_owned(), unescape_label(name)));
    }
    found
}

/// Where udev puts the labels.
const BY_LABEL: &str = "/dev/disk/by-label";

/// `\x20` back to a space, and so on.
///
/// udev escapes anything that cannot be a filename — a space, a slash — as
/// `\xNN`, which is a *different* escape from the octal one `/proc` uses for a
/// mount point: two directories, two conventions, and a label that arrives with
/// the escapes still in it reads as `My\x20disk`.
pub(crate) fn unescape_label(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\'
            && bytes.get(at + 1) == Some(&b'x')
            && let Some(text) = name.get(at + 2..at + 4)
            && let Ok(byte) = u8::from_str_radix(text, 16)
        {
            out.push(byte);
            at += 4;
            continue;
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bytes free and bytes in total, from one `statvfs`.
///
/// **`f_bavail` and not `f_bfree`**: the difference is the reserve only root can
/// write into, and a number that counts space this person cannot use is a number
/// that says the disk is emptier than it is.
///
/// **Runs on a job.**
fn room_on(at: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(at).ok()?;
    let block = if stat.f_frsize > 0 { stat.f_frsize } else { stat.f_bsize };
    Some((stat.f_bavail.saturating_mul(block), stat.f_blocks.saturating_mul(block)))
}

fn read(from: &str) -> Vec<Mount> {
    let Ok(text) = std::fs::read_to_string(from) else { return Vec::new() };
    let mut found: Vec<Mount> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(mount) = parse(line, &mut seen) else { continue };
        found.push(mount);
    }
    found.sort_by(|a, b| b.is_root().cmp(&a.is_root()).then_with(|| a.at.cmp(&b.at)));
    found
}

/// One `mountinfo` line, or `None` if it is not a place.
///
/// The format is `id parent major:minor root point options [tags…] - type
/// source superopts`, per `proc(5)`. The separator is the reason the optional
/// tags can be skipped without counting them.
fn parse(line: &str, seen: &mut Vec<String>) -> Option<Mount> {
    let (left, right) = line.split_once(" - ")?;
    let mut fields = left.split(' ');
    let device = fields.nth(2)?.to_owned();
    let root = fields.next()?;
    let at = fields.next()?;
    let mut rest = right.split(' ');
    let kind = rest.next()?;
    let source = rest.next().unwrap_or("");

    let &(_, remote) = INTERESTING.iter().find(|(name, _)| *name == kind)?;
    if is_system(at) {
        return None;
    }
    // A bind mount of a subdirectory is the same filesystem seen twice, and so
    // is the same device mounted at two points. Either would put one disk in the
    // sidebar several times.
    if root != "/" || seen.contains(&device) {
        return None;
    }
    seen.push(device.clone());
    Some(Mount {
        label: None,
        free: None,
        at: PathBuf::from(unescape(at)),
        kind: kind.to_owned(),
        source: unescape(source),
        remote,
        numbers: device,
    })
}

/// Whether a mount point is part of the operating system rather than a place.
///
/// `/boot` is `ext4` and `/boot/efi` is `vfat`, so both pass the type filter and
/// neither is somewhere a person goes — and on this machine they were two of the
/// four things the sidebar showed. A deny-list works here where it did not for
/// filesystem types, because these directory names are the FHS and have not
/// changed in thirty years, while the kernel adds a pseudo-filesystem a year.
fn is_system(at: &str) -> bool {
    const SYSTEM: &[&str] =
        &["/boot", "/usr", "/var", "/etc", "/opt", "/srv", "/snap", "/nix", "/sys", "/proc"];
    SYSTEM.iter().any(|prefix| at == *prefix || at.starts_with(&format!("{prefix}/")))
}

/// `/proc` escapes space, tab, newline and backslash in octal.
///
/// A mount point called `My Drive` arrives as `My\040Drive`, and a sidebar that
/// shows the escape is a sidebar that has not read `proc(5)`.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &field[index + 1..index + 4];
            if let Ok(byte) = u8::from_str_radix(digits, 8) {
                out.push(char::from(byte));
                index += 4;
                continue;
            }
        }
        // Not an escape: copy the character, which may be several bytes.
        let next = field[index..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&field[index..index + next]);
        index += next;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{mounts, parse, unescape};

    #[test]
    fn a_real_disk_is_a_place_and_a_pseudo_filesystem_is_not() {
        let mut seen = Vec::new();
        let disk =
            parse("46 1 252:1 / / rw,relatime shared:1 - ext4 /dev/mapper/vg-lv rw", &mut seen)
                .expect("ext4 is a place");
        assert_eq!(disk.at.to_string_lossy(), "/");
        assert_eq!(disk.kind, "ext4");
        assert!(disk.is_root() && !disk.remote);

        for line in [
            // The operating system, not a place. Both of these are on the type
            // allow-list and both were in the sidebar.
            "80 46 252:2 / /boot rw - ext4 /dev/nvme0n1p2 rw",
            "81 80 259:1 / /boot/efi rw - vfat /dev/nvme0n1p1 rw",
            "34 46 0:29 / /run rw,nosuid - tmpfs tmpfs rw,mode=755",
            "25 30 0:24 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw",
            "99 46 7:12 / /snap/firefox/8803 ro - squashfs /dev/loop12 ro",
            "88 46 0:52 / /run/user/1000/doc rw - fuse.portal portal rw",
        ] {
            assert!(parse(line, &mut Vec::new()).is_none(), "{line}");
        }
    }

    /// The same device at two mount points, and a bind mount of a subdirectory,
    /// are one disk each — and a sidebar that lists a disk three times is a
    /// sidebar nobody reads.
    #[test]
    fn one_device_is_one_place_however_many_times_it_is_mounted() {
        let mut seen = Vec::new();
        let first = "46 1 252:1 / / rw - ext4 /dev/sda1 rw";
        let again = "70 46 252:1 / /mnt/copy rw - ext4 /dev/sda1 rw";
        let bind = "71 46 252:2 /home/emil /mnt/bound rw - ext4 /dev/sda2 rw";
        assert!(parse(first, &mut seen).is_some());
        assert!(parse(again, &mut seen).is_none(), "the same device twice");
        assert!(parse(bind, &mut seen).is_none(), "a bind mount of a subdirectory");
    }

    #[test]
    fn a_network_mount_says_it_can_block() {
        let sshfs = parse(
            "90 46 0:60 / /mnt/remote rw - fuse.sshfs emil@host:/ rw,user_id=1000",
            &mut Vec::new(),
        )
        .expect("sshfs is a place");
        assert!(sshfs.remote);
        assert_eq!(sshfs.source, "emil@host:/");
    }

    #[test]
    fn an_escaped_mount_point_is_unescaped() {
        assert_eq!(unescape("/media/emil/My\\040Drive"), "/media/emil/My Drive");
        assert_eq!(unescape("/plain/path"), "/plain/path");
        assert_eq!(unescape("a\\011b"), "a\tb");
        // A backslash that is not an escape survives, and so does a multi-byte
        // character next to one.
        assert_eq!(unescape("/a\\b"), "/a\\b");
        assert_eq!(unescape("/mnt/héllo"), "/mnt/héllo");
    }

    /// A place that merely begins with a system name is still a place.
    #[test]
    fn a_mount_point_is_matched_by_component_and_not_by_prefix() {
        assert!(super::is_system("/boot") && super::is_system("/boot/efi"));
        assert!(!super::is_system("/bootcamp"), "a disk called bootcamp is a disk");
        assert!(!super::is_system("/media/emil/stick"));
        assert!(!super::is_system("/"), "the root filesystem is the first place");
    }

    /// The one assertion about this machine: whatever it has, `/` is on the list
    /// and nothing pseudo is.
    #[test]
    fn this_machine_has_a_root_filesystem_and_no_pseudo_ones() {
        let found = mounts();
        assert!(found.first().is_some_and(super::Mount::is_root), "root is not first: {found:?}");
        for mount in &found {
            assert!(
                !matches!(mount.kind.as_str(), "tmpfs" | "squashfs" | "cgroup2" | "proc"),
                "{mount:?}"
            );
        }
    }

    /// udev's escaping is not `/proc`'s, and a label with a space in it is the
    /// case that shows the difference.
    #[test]
    fn a_labels_escapes_are_udevs_and_not_procs() {
        assert_eq!(super::unescape_label("My\\x20disk"), "My disk");
        assert_eq!(super::unescape_label("plain"), "plain");
        assert_eq!(super::unescape_label("caf\\xc3\\xa9"), "café");
        // `/proc`'s octal is not udev's hex, and neither reads the other's.
        assert_eq!(super::unescape_label("My\\040disk"), "My\\040disk");
    }

    /// This machine's own mounts, with whatever labels and free space it has.
    ///
    /// The numbers are the machine's, so what is asserted is the shape: a
    /// filesystem that is mounted has a size, a free figure inside it, and a
    /// label that is either absent or not empty.
    #[test]
    fn what_is_mounted_here_reports_room_that_makes_sense() {
        for mount in super::mounts() {
            if let Some((free, total)) = mount.free {
                assert!(total > 0, "{mount:?} has no size at all");
                assert!(free <= total, "{mount:?} has more free than it holds");
            } else {
                assert!(mount.remote, "a local mount reported no room: {mount:?}");
            }
            if let Some(label) = &mount.label {
                assert!(!label.is_empty(), "{mount:?}");
            }
        }
    }
}
