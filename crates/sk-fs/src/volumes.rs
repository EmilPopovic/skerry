// SPDX-License-Identifier: Apache-2.0
//! Every filesystem on this machine, mounted or not.
//!
//! **[ADR 0030](../../../docs/adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).**
//! [`crate::mounts`] reads `/proc/self/mountinfo`, which lists *mounts* — so a
//! USB stick nothing has mounted is not in it, and was not in the sidebar. This
//! module adds the other half: the block devices udev has described as holding a
//! filesystem, whether or not anything mounted them.
//!
//! # Two sources, and the mounted one is untouched
//!
//! What is mounted comes from the mount table exactly as before, so no row that
//! used to appear can stop appearing. What is *not* mounted comes from
//! `/sys/class/block`, described by the file udev wrote for it. A machine with no
//! udev gets the first list and not the second, which is the behaviour this
//! module replaced and the documented fallback rather than an error.
//!
//! # Why udev's own database and not `libblkid`
//!
//! The type and the label of an unmounted filesystem are in its superblock, and
//! reading that means opening the block device — which this process, not being
//! root, cannot do. udev opened it at boot and wrote the answer down in
//! `/run/udev/data/b<major>:<minor>`, which is a world-readable text file. That
//! is the whole lookup, and it is the same udev whose `/dev/disk/by-label`
//! symlinks this crate already reads.

use std::path::{Path, PathBuf};

use crate::mounts::{INTERESTING, Mount};

/// A filesystem a person would call a place, mounted or waiting to be.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Volume {
    /// The block device — `/dev/sda1` — or nothing for a filesystem that has
    /// none, which is every network mount.
    pub device: Option<PathBuf>,
    /// The device's number as `major:minor`, which is what joins the two
    /// sources. See [`Mount::numbers`].
    pub numbers: String,
    /// What it is: `ext4`, `vfat`, `fuse.sshfs`.
    pub kind: String,
    /// The name somebody gave the disk, where it has one.
    pub label: Option<String>,
    /// Where it is mounted, if anything has mounted it.
    pub at: Option<PathBuf>,
    /// Bytes free and bytes in total, for a mounted local one.
    ///
    /// **Nothing at all until it is mounted**, because there is nothing to
    /// measure — and [rule 7](../../../docs/03-principles.md) has no room for a
    /// figure that was guessed at.
    pub free: Option<(u64, u64)>,
    /// Whether reaching it may block for a network timeout.
    pub remote: bool,
    /// Whether the drive it sits on is one a person unplugs.
    ///
    /// From `removable` in sysfs, which is the kernel's own answer, falling back
    /// to udev's `ID_BUS=usb`. Used for the order and for whether Eject means
    /// anything, and never for whether to *show* the row: an internal partition
    /// nobody has mounted is still a filesystem somebody can open.
    pub removable: bool,
}

impl Volume {
    /// Whether this is the root filesystem.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.at.as_deref() == Some(Path::new("/"))
    }

    /// Whether anything has mounted it.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        self.at.is_some()
    }

    /// The name to show: the filesystem's label, else the mount point's own
    /// name, else the device's.
    #[must_use]
    pub fn name(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        let of = |path: &Path| {
            path.file_name().map(|it| it.to_string_lossy().into_owned()).filter(|it| !it.is_empty())
        };
        self.at
            .as_deref()
            .and_then(of)
            .or_else(|| self.device.as_deref().and_then(of))
            .unwrap_or_else(|| self.kind.clone())
    }
}

/// Every filesystem worth showing: the mounted ones first, then the rest.
///
/// **Runs on a job.** One read of `/proc`, one directory of symlinks, and one
/// small file per block device — sixty-one of them on the reference machine,
/// which is why it is not on the frame loop.
#[must_use]
pub fn volumes() -> Vec<Volume> {
    let mounted = crate::mounts::mounts();
    let mut found: Vec<Volume> = mounted.iter().map(of_mount).collect();
    // **Against every mounted number, not against the rows above.** A
    // filesystem the sidebar deliberately hides — `/boot` and `/boot/efi` — is
    // mounted and is not in `mounted`, so a join against these rows alone put
    // both of them back as unmounted disks. See `mounts::mounted_numbers`.
    let held = crate::mounts::mounted_numbers();
    for spare in unmounted() {
        if held.contains(&spare.numbers) {
            continue;
        }
        found.push(spare);
    }
    // Root first, then everything else mounted, then the rest — because a row a
    // press opens is worth more than a row a press mounts, and the order has to
    // be stable or the sidebar reshuffles under a pointer.
    found.sort_by(|a, b| {
        b.is_root()
            .cmp(&a.is_root())
            .then_with(|| b.is_mounted().cmp(&a.is_mounted()))
            .then_with(|| a.at.cmp(&b.at))
            .then_with(|| a.name().cmp(&b.name()))
            .then_with(|| a.numbers.cmp(&b.numbers))
    });
    found
}

fn of_mount(mount: &Mount) -> Volume {
    Volume {
        device: (mount.source.starts_with('/')).then(|| PathBuf::from(&mount.source)),
        numbers: mount.numbers.clone(),
        kind: mount.kind.clone(),
        label: mount.label.clone(),
        at: Some(mount.at.clone()),
        free: mount.free,
        remote: mount.remote,
        removable: mount.source.starts_with('/') && is_removable(Path::new(&mount.source)),
    }
}

/// Where the kernel lists block devices, and where udev writes about them.
const BLOCK: &str = "/sys/class/block";
const UDEV_DATA: &str = "/run/udev/data";

/// Every block device udev calls a filesystem, whether mounted or not.
fn unmounted() -> Vec<Volume> {
    let Ok(names) = crate::scan::names(Path::new(BLOCK), &sk_util::Cancel::new()) else {
        return Vec::new();
    };
    let listing = &names.listing;
    let mut found = Vec::new();
    for row in 0..listing.rows() {
        let Some(id) = listing.at(row) else { continue };
        if let Some(volume) = described(listing.name(id)) {
            found.push(volume);
        }
    }
    found
}

/// One block device, if udev says it holds a filesystem this program can show.
fn described(name: &str) -> Option<Volume> {
    let at = Path::new(BLOCK).join(name);
    let numbers = std::fs::read_to_string(at.join("dev")).ok()?.trim().to_owned();
    let told = udev_said(&numbers)?;
    // **`ID_FS_USAGE` before the type.** A LUKS container, an LVM member and a
    // swap partition all have an `ID_FS_TYPE`, and none of them is somewhere a
    // person goes. udev's own word for "this holds a filesystem" is the filter
    // the allow-list then narrows.
    if value(&told, "ID_FS_USAGE") != Some("filesystem") {
        return None;
    }
    let kind = value(&told, "ID_FS_TYPE")?;
    let &(_, remote) = INTERESTING.iter().find(|(it, _)| *it == kind)?;
    Some(Volume {
        device: Some(PathBuf::from("/dev").join(name)),
        numbers,
        kind: kind.to_owned(),
        label: value(&told, "ID_FS_LABEL").map(crate::mounts::unescape_label),
        at: None,
        free: None,
        remote,
        removable: is_removable(&PathBuf::from("/dev").join(name))
            || value(&told, "ID_BUS") == Some("usb"),
    })
}

/// What udev wrote about the device numbered `numbers`.
///
/// The file is `KEY:value` a line at a time, and the ones worth having are
/// under `E:` — udev's own name for a device property. Absent on a machine with
/// no udev, which is a machine with no unmounted volumes rather than an error.
fn udev_said(numbers: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(UDEV_DATA).join(format!("b{numbers}"))).ok()
}

/// One `E:` property out of that file.
fn value<'a>(told: &'a str, key: &str) -> Option<&'a str> {
    told.lines().find_map(|line| line.strip_prefix("E:")?.strip_prefix(key)?.strip_prefix('='))
}

/// Whether the drive `device` sits on is one a person unplugs.
///
/// **The parent's flag, not the partition's.** `removable` exists on a whole
/// disk and is always `0` on a partition of one, so asking the partition says
/// every USB stick is fixed. The parent is found by resolving the sysfs symlink
/// and stepping up one, because `/sys/class/block/sda1/..` is
/// `/sys/class/block` and not `/sys/.../sda`.
fn is_removable(device: &Path) -> bool {
    let Some(name) = device.file_name() else { return false };
    let Ok(real) = std::fs::canonicalize(Path::new(BLOCK).join(name)) else { return false };
    let flag =
        |dir: &Path| std::fs::read_to_string(dir.join("removable")).ok().map(|it| it.trim() == "1");
    flag(&real).or_else(|| real.parent().and_then(flag)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Volume, value, volumes};

    fn made(numbers: &str, at: Option<&str>, label: Option<&str>) -> Volume {
        Volume {
            device: Some(std::path::PathBuf::from("/dev/sda1")),
            numbers: numbers.to_owned(),
            kind: "ext4".to_owned(),
            label: label.map(str::to_owned),
            at: at.map(std::path::PathBuf::from),
            free: None,
            remote: false,
            removable: false,
        }
    }

    /// The `E:` lines are the ones worth having, and a key that is a prefix of
    /// another must not answer for it.
    #[test]
    fn a_property_comes_out_of_the_e_lines_and_matches_a_whole_key() {
        let told = "S:disk/by-label/SHARED\n\
                    I:86346225\n\
                    E:ID_FS_LABEL=SHARED\n\
                    E:ID_FS_LABEL_ENC=SHARED\n\
                    E:ID_FS_TYPE=exfat\n\
                    E:ID_FS_USAGE=filesystem\n";
        assert_eq!(value(told, "ID_FS_TYPE"), Some("exfat"));
        assert_eq!(value(told, "ID_FS_USAGE"), Some("filesystem"));
        assert_eq!(value(told, "ID_FS_LABEL"), Some("SHARED"));
        assert_eq!(value(told, "ID_BUS"), None);
        // Not an `E:` line, so not a property — `S:` is a symlink udev made.
        assert_eq!(value(told, "disk/by-label/SHARED"), None);
    }

    /// **`ID_FS_LABEL_ENC` must not answer for `ID_FS_LABEL`.** They differ by a
    /// suffix and the encoded one carries udev's escapes, so a lookup that
    /// matched the longer key would put `My\x20disk` in the sidebar.
    #[test]
    fn a_key_is_not_matched_by_its_own_prefix() {
        let told = "E:ID_FS_LABEL_ENC=My\\x20disk\nE:ID_FS_LABEL=My disk\n";
        assert_eq!(value(told, "ID_FS_LABEL"), Some("My disk"));
    }

    /// The name is the label, then the mount point, then the device — because a
    /// row reading `nvme1n1p3` is a row nobody can identify.
    #[test]
    fn a_volume_is_named_by_the_best_thing_it_has() {
        assert_eq!(made("8:1", Some("/run/media/emil/STICK"), Some("Photos")).name(), "Photos");
        assert_eq!(made("8:1", Some("/run/media/emil/STICK"), None).name(), "STICK");
        assert_eq!(made("8:1", None, None).name(), "sda1");
        let nameless =
            Volume { device: None, kind: "fuse.sshfs".to_owned(), ..made("0:60", None, None) };
        assert_eq!(nameless.name(), "fuse.sshfs", "a mount with neither is still a row");
    }

    #[test]
    fn a_mounted_volume_says_so_and_an_unmounted_one_does_not() {
        assert!(made("8:1", Some("/"), None).is_root());
        assert!(made("8:1", Some("/"), None).is_mounted());
        assert!(!made("8:1", Some("/mnt/x"), None).is_root());
        assert!(!made("8:1", None, None).is_mounted());
    }

    /// **The one assertion about this machine**, and it is the regression this
    /// module must not cause: every mount that was a row is still a row, with
    /// the same mount point, and nothing appears twice.
    #[test]
    fn every_mount_is_still_a_volume_and_no_device_appears_twice() {
        let found = volumes();
        for mount in crate::mounts::mounts() {
            assert!(
                found.iter().any(|it| it.at.as_deref() == Some(mount.at.as_path())),
                "{:?} was a row and is not one now",
                mount.at
            );
        }
        let mut seen: Vec<&str> = Vec::new();
        for volume in &found {
            assert!(!seen.contains(&volume.numbers.as_str()), "{volume:?} twice");
            seen.push(&volume.numbers);
        }
        // **And nothing that is already mounted is offered as unmounted.** The
        // sidebar hides `/boot` and `/boot/efi` on purpose, and the first
        // version of the join put both of them back with a Mount command
        // attached — which would have unmounted somebody's boot partition on a
        // second press.
        let held = crate::mounts::mounted_numbers();
        for volume in found.iter().filter(|it| !it.is_mounted()) {
            assert!(
                !held.contains(&volume.numbers),
                "{volume:?} is mounted and is offered as though it were not"
            );
        }
        assert!(found.first().is_some_and(Volume::is_root), "root is not first: {found:?}");
    }

    /// A volume udev described is a filesystem, and one that is not mounted has
    /// no room to report.
    #[test]
    fn a_volume_is_described_by_what_udev_wrote_down() {
        for volume in volumes() {
            assert!(!volume.kind.is_empty(), "{volume:?}");
            assert!(!volume.numbers.is_empty(), "{volume:?}");
            if !volume.is_mounted() {
                assert!(volume.free.is_none(), "an unmounted volume reported room: {volume:?}");
                assert!(volume.device.is_some(), "an unmounted volume with no device: {volume:?}");
            }
            if let Some(label) = &volume.label {
                assert!(!label.is_empty(), "{volume:?}");
            }
        }
    }
}
