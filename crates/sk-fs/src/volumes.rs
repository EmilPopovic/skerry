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
    // **And the partition table has the last word** — [ADR
    // 0031](../../../docs/adr/0031-a-partition-the-table-says-not-to-automount-is-not-a-volume.md).
    // Applied to both halves and not only to the unmounted candidates: the
    // Windows Recovery partition that produced that record was mounted at the
    // time, by a press on the very row it exists to remove.
    found.retain(|volume| !ignored(&volume.numbers));
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

/// Whether udev has been told to ignore this device.
///
/// **[ADR 0031](../../../docs/adr/0031-a-partition-the-table-says-not-to-automount-is-not-a-volume.md).**
/// `UDISKS_IGNORE=1` is written into the udev database by udisks2's own rules at
/// boot, so reading it is how this sidebar agrees with every other desktop's
/// about the same disk — `HintIgnore` by construction rather than by agreement,
/// and with no D-Bus, which is the whole shape ADR 0030 chose.
///
/// **Not `UDISKS_AUTO`**, which is `0` on `/` and on `/boot` on the reference
/// machine: it means *do not mount this at login*, which the root filesystem
/// plainly does not need and which says nothing about whether anybody goes
/// there.
///
/// Nothing to read is not a statement: a machine with no udev answers `false`,
/// which is the behaviour that existed before any of this.
fn ignored(numbers: &str) -> bool {
    let Some(told) = udev_said(numbers) else { return false };
    if value(&told, "UDISKS_IGNORE") == Some("1") {
        return true;
    }
    hidden_by_the_partition_table(&told)
}

/// The fallback for a machine with udev and no udisks2 rules to have written
/// that property.
///
/// Bit 63 of a GPT partition's attributes is the UEFI specification's *no drive
/// letter* — the table's own way of saying this partition is not to be
/// automounted, and what Windows sets on the ones it does not want anybody to
/// see. Bit 62 is *hidden*. Either one hides the row.
///
/// **Only under GPT.** In the `dos` scheme the field is one byte and `0x80`
/// means *bootable*, which the USB stick on the reference machine has. Read as a
/// GPT attribute that is bit 7 and means nothing — but a rule that did not check
/// the scheme would be a rule that is right by luck.
///
/// **It is a fallback and not the rule**, because it is one of udisks2's several
/// inputs: the EFI System Partition has no attributes at all and udisks2 ignores
/// it anyway. Measured, by asking udisks2 about every device on this machine and
/// finding the disagreement on the first one.
fn hidden_by_the_partition_table(told: &str) -> bool {
    if value(told, "ID_PART_ENTRY_SCHEME") != Some("gpt") {
        return false;
    }
    let Some(text) = value(told, "ID_PART_ENTRY_FLAGS") else { return false };
    let Ok(flags) = u64::from_str_radix(text.trim().trim_start_matches("0x"), 16) else {
        return false;
    };
    flags & (NO_DRIVE_LETTER | HIDDEN) != 0
}

/// GPT attribute bit 63: no drive letter, which is *do not automount*.
const NO_DRIVE_LETTER: u64 = 1 << 63;
/// GPT attribute bit 62: hidden.
const HIDDEN: u64 = 1 << 62;

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

    /// **The four cases from [ADR
    /// 0031](../../../docs/adr/0031-a-partition-the-table-says-not-to-automount-is-not-a-volume.md)'s
    /// own table**, as udev wrote them on the reference machine — against the
    /// *fallback*, which is what a machine with no udisks2 rules gets. Two are
    /// hidden, two are not, and the USB stick is the one that says why the scheme
    /// has to be checked: `0x80` under `dos` is *bootable*, and read as a GPT
    /// attribute it is bit 7 and means nothing.
    #[test]
    fn the_partition_table_hides_what_it_says_not_to_automount() {
        let told = |scheme: &str, flags: &str| {
            let entry = if flags.is_empty() {
                String::new()
            } else {
                format!("E:ID_PART_ENTRY_FLAGS={flags}\n")
            };
            format!("E:ID_FS_TYPE=ntfs\nE:ID_PART_ENTRY_SCHEME={scheme}\n{entry}")
        };
        let hidden =
            |scheme: &str, flags: &str| super::hidden_by_the_partition_table(&told(scheme, flags));

        // A Microsoft reserved partition: bit 63 alone.
        assert!(hidden("gpt", "0x8000000000000000"));
        // Windows Recovery: bit 63 and bit 0, which is *required partition*.
        assert!(hidden("gpt", "0x8000000000000001"));
        // Hidden, which is bit 62.
        assert!(hidden("gpt", "0x4000000000000000"));
        // A basic data partition with no attributes at all.
        assert!(!hidden("gpt", ""));
        // **The USB stick.** `0x80` in the MBR scheme is the active flag, and a
        // rule that did not check the scheme would read bit 7 and still say no —
        // right by luck. The same value with bit 63 set under `dos` is the case
        // that separates the two, and it must be ignored.
        assert!(!hidden("dos", "0x80"));
        assert!(!hidden("dos", "0x8000000000000000"), "a dos flags byte read as GPT");
        // Bit 0 on its own is *required partition*, which is true of the ESP and
        // says nothing about whether a person goes there.
        assert!(!hidden("gpt", "0x1"));
        // Something unreadable is not a statement either.
        assert!(!hidden("gpt", "wat"));
    }

    /// **`UDISKS_IGNORE` decides where it is there, and never `UDISKS_AUTO`.**
    /// The second is `0` on the root filesystem on the reference machine, so a
    /// rule that read it would hide `/`.
    #[test]
    fn udisks_ignore_decides_and_udisks_auto_is_not_it() {
        let flag = |told: &str| {
            if super::value(told, "UDISKS_IGNORE") == Some("1") {
                return true;
            }
            super::hidden_by_the_partition_table(told)
        };
        assert!(flag("E:UDISKS_AUTO=0\nE:UDISKS_IGNORE=1\n"));
        assert!(flag("E:UDISKS_IGNORE=1\n"));
        assert!(!flag("E:UDISKS_AUTO=0\n"), "UDISKS_AUTO=0 is set on the root filesystem");
        assert!(!flag("E:UDISKS_IGNORE=0\n"));
        assert!(!flag(""));
        // And the fallback still applies underneath it, for a machine whose udev
        // has no udisks2 rules to have written the property at all.
        assert!(flag("E:ID_PART_ENTRY_SCHEME=gpt\nE:ID_PART_ENTRY_FLAGS=0x8000000000000000\n"));
    }

    /// **This machine, against udisks2's own answer** — ADR 0031's revisit
    /// trigger. The property is read the way the reference implementation
    /// computes it, or this sidebar disagrees with every other desktop's about
    /// the same disk — which is how the flag-only version was caught, on the
    /// first device it asked about.
    /// Skipped where there is no `udisksctl` to ask, which is most CI.
    #[test]
    fn what_this_rule_hides_is_what_udisks2_ignores() {
        let Ok(names) =
            crate::scan::names(std::path::Path::new(super::BLOCK), &sk_util::Cancel::new())
        else {
            return;
        };
        let listing = &names.listing;
        let mut asked = 0;
        for row in 0..listing.rows() {
            let Some(id) = listing.at(row) else { continue };
            let name = listing.name(id);
            let Ok(numbers) =
                std::fs::read_to_string(std::path::Path::new(super::BLOCK).join(name).join("dev"))
            else {
                continue;
            };
            let ours = super::ignored(numbers.trim());
            let Some(theirs) = udisks_ignores(name) else { continue };
            asked += 1;
            assert_eq!(
                ours, theirs,
                "{name}: this rule says ignored={ours} and udisks2 says HintIgnore={theirs}"
            );
        }
        // Nothing asked is a machine with no udisks2, which is a machine this
        // says nothing about rather than a machine that passed.
        let _ = asked;
    }

    /// udisks2's own `HintIgnore` for a block device, or nothing where there is
    /// no udisks2 to ask. Read through `busctl` rather than through this
    /// workspace's own client, which is a level above this crate.
    fn udisks_ignores(name: &str) -> Option<bool> {
        let out = std::process::Command::new("busctl")
            .args([
                "--system",
                "get-property",
                "org.freedesktop.UDisks2",
                &format!("/org/freedesktop/UDisks2/block_devices/{name}"),
                "org.freedesktop.UDisks2.Block",
                "HintIgnore",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let said = String::from_utf8_lossy(&out.stdout);
        Some(said.trim() == "b true")
    }

    /// **The one assertion about this machine**, and it is the regression this
    /// module must not cause: every mount that was a row is still a row, with
    /// the same mount point, and nothing appears twice.
    #[test]
    fn every_mount_is_still_a_volume_and_no_device_appears_twice() {
        let found = volumes();
        for mount in crate::mounts::mounts() {
            // **Except one udev says to ignore** — [ADR
            // 0031](../../../docs/adr/0031-a-partition-the-table-says-not-to-automount-is-not-a-volume.md).
            // A Windows Recovery partition somebody mounted is still a partition
            // nobody wants listed, and the filter is deliberately not restricted
            // to unmounted candidates.
            if super::ignored(&mount.numbers) {
                continue;
            }
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
