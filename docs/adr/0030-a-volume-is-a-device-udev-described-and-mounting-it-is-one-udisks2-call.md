# 0030 — A volume is a device udev described, and mounting it is one udisks2 call

**Status:** Accepted

## Context

The sidebar's Volumes section is built from `/proc/self/mountinfo`, and that file
lists **mounts**. A USB stick that nothing has mounted is not in it, so it is not
in the sidebar — and nothing in this program will mount it. Reported as *"a
plugged in usb stick is not detected and placed into the sidebar, not even with
it plugged in before `cargo run`"*, which is exactly right and is not a defect in
the mount reader: there was nothing to read.

The gap is wider than one stick. `lsblk` on the reference machine lists three
partitions with real filesystems that are not mounted — an `ext4` labelled *DO
NOT USE*, an `ntfs`, and a BitLocker volume — and Skerry shows none of them. A
file manager that can only reach filesystems somebody else mounted is a directory
browser.

Three questions have to be answered, and they are independent.

**What is there.** `mountinfo` cannot say. The alternatives are udisks2's
`GetManagedObjects`, `libblkid`, and reading what udev already wrote down.

**That something has appeared.** The mount table is polled for `POLLPRI`, which
fires when a mount changes and says nothing when a device is plugged in.

**How to mount it.** This process is not root and
[rule 9](../03-principles.md) forbids GVfs, so `mount(2)` and `gio mount` are
both out.

### What each option costs

**udisks2 for all three.** It is one dependency-free D-Bus service that answers
every question — but it answers the third one by *signal*.
[ADR 0023](0023-d-bus-is-a-socket-we-speak-ourselves.md)'s revisit trigger names
this case exactly: "a feature that needs to receive signals — udisks2's
`InterfacesAdded` for a disk plugged in, say — needs a subscribing connection,
and that is where this shape stops fitting." Discovery through
`GetManagedObjects` is also a nest of `a{oa{sa{sv}}}` that has to be walked to
find two strings, and it makes a machine with no udisks2 a machine with no
volumes at all — which [06-portability.md](../06-portability.md) does not allow.

**`libblkid`.** A tier-2 C dependency to read a superblock, which needs a
privileged open of the block device this process does not have. Both halves
refuse it.

**What udev already wrote down.** `/run/udev/data/b<major>:<minor>` is a
world-readable text file holding `E:ID_FS_TYPE=exfat`, `E:ID_FS_LABEL=SHARED` and
`E:ID_FS_USAGE=filesystem` — for mounted and unmounted devices alike. Measured on
the reference machine: the unmounted `ntfs` partition reports its type from that
file, and the labels are already read out of `/dev/disk/by-label`, which is the
same udev, one directory over.

**A `NETLINK_KOBJECT_UEVENT` socket** answers the second question with a
descriptor. `socket()`, `bind()`, and it joins the `poll` the frame loop already
sleeps on. Measured: it binds as uid 1000 on both the kernel and the udev
multicast groups on the reference machine, with no capability and no library.
Zero CPU at idle, no timer, and no subscribing D-Bus connection — so ADR 0023's
revisit trigger is **not** fired, because the signal it names is not the only way
to hear about a disk.

That leaves udisks2 for mounting alone, where the call is a *method* call and the
short-lived connection per call ADR 0023 chose is exactly the right shape.
Measured: `org.freedesktop.UDisks2` is on the system bus and answers this user,
and `org.freedesktop.UDisks2.policy` is the polkit action that decides.

## Decision

**Three sources, one for each question, and each degrades on its own.**

- **`sk_fs::volumes` enumerates `/sys/class/block` and describes each entry from
  `/run/udev/data/b<major>:<minor>`.** A device is a volume when udev says
  `ID_FS_USAGE=filesystem` and its `ID_FS_TYPE` is on the allow-list
  [`mounts::INTERESTING`](../../crates/sk-fs/src/mounts.rs) already keeps — one
  list, so a filesystem type is added in one place. The label is
  `ID_FS_LABEL`, falling back to what `/dev/disk/by-label` says, falling back to
  the mount point's own name. **No udev is no volumes beyond the mounted ones**,
  which is today's behaviour and the documented fallback rather than an error.
- **A volume knows whether it is mounted**, by joining on the device against
  [`sk_fs::mounts`](../../crates/sk-fs/src/mounts.rs). An unmounted volume is a
  row in the sidebar with no free-space bar, because there is nothing to measure
  until it is mounted, and rule 7 has no room for a guess.
- **Hotplug is a `NETLINK_KOBJECT_UEVENT` socket on the watcher's thread**, beside
  the inotify descriptor and the `mountinfo` poll that are already there. It is
  read for `SUBSYSTEM=block` and coalesced the same way the mount table's
  `POLLPRI` is: what reaches the window is *the list changed*, never a device.
  **A socket that cannot be bound is a capability reported absent** — ADR 0007 —
  and the list is then only as fresh as a mount change makes it.
- **Mounting is one `org.freedesktop.UDisks2.Filesystem.Mount` call** on the
  system bus, through `sk_desktop::bus`, on a job. Unmount and eject are its two
  siblings. **A machine with no udisks2 draws the row and dims the command**, with
  the reason in the status line: a person can still reach the volume the moment
  something else mounts it.
- **Mount and Unmount are not on the undo stack**, for the reason
  [ADR 0021](0021-a-pin-is-a-line-in-gtks-bookmarks-file.md) gives about a pin:
  what makes each reversible is that its inverse is one press away and is the row
  right there. Rule 3 is about a person's files, and neither of these touches one.

## Consequences

**A second unprivileged failure mode to design for.** polkit may refuse the
mount, and it refuses with a D-Bus error naming an action id. That error has to
reach the status line as a sentence about *this disk*, not as the chain — which
is what rule 8's separation of message from log already asks, and is one more
place to get it right.

**udev's database is not a specification.** `/run/udev/data` is systemd's
implementation detail: the `E:` prefix and the file name are stable in practice
and are not promised. The cost of it changing is a machine that shows only its
mounted volumes, which is where this record starts — so the failure is a
regression to the old behaviour rather than a broken window, and that is the
reason it is acceptable. `libudev` would buy the promise for a tier-2 C
dependency and a privileged read, which is the trade
[07-dependencies.md](../07-dependencies.md) exists to refuse.

**Three partitions appear on the reference machine that were not there before**,
including one labelled *DO NOT USE*. That is correct — they are filesystems a
person can open — and it is noisier than the four rows the sidebar had. A setting
to hide internal volumes is a seventh setting and
[ADR 0027](0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)
says what that costs; it is not taken here.

**The netlink socket is a third descriptor on the watcher's thread**, and a
uevent burst — plugging in a hub with four sticks on it — is dozens of messages
in a few milliseconds. It is coalesced on the same floor `REFRESH_FLOOR` already
imposes on the mount table, or a plugged-in hub is a dozen re-enumerations.

**What is deliberately not here.** Encrypted volumes (`Encrypted.Unlock` and a
passphrase field), a drive's power-off after an eject, and network shares. Each
is its own surface and each would be smuggled in under a record about local
disks.

## Revisit trigger

**A feature that genuinely needs a D-Bus signal**, which this one turned out not
to. Following the settings portal's `SettingChanged` is the live candidate, and
it will fire ADR 0023's trigger properly rather than borrowing this record's
socket.

**Or `/run/udev/data` changing shape**, measured as
`a_volume_is_described_by_what_udev_wrote_down` failing on a machine that has
udev — at which point the choice is `libudev` against a privileged superblock
read, and this record's Consequences section is the argument to re-run.

**Or more than ten mounts a second**, which is a hub being plugged and unplugged
by a script rather than by a person, and would make the coalescing floor the
subject rather than a detail.
