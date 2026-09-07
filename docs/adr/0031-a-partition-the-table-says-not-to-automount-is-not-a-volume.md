# 0031 — A partition udev has flagged to be ignored is not a volume

**Status:** Accepted. Supersedes
[0030](0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md)
on **which devices are volumes**. That record's other four decisions — the udev
database as the description, the `NETLINK_KOBJECT_UEVENT` socket as the
notification, udisks2's three method calls as the action, and mount and unmount
being off the undo stack — stand unchanged and are not restated here.

## Context

[ADR 0030](0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md)
made a device a volume when udev says `ID_FS_USAGE=filesystem` and its
`ID_FS_TYPE` is on the allow-list, and then defended the result:

> **Three partitions appear on the reference machine that were not there
> before**, including one labelled *DO NOT USE*. That is correct — they are
> filesystems a person can open — and it is noisier than the four rows the
> sidebar had.

**That was wrong, and the person whose machine it is said so**: *"I think
DO_NOT_USE and the other one should not be shown here, they are not really
actual volumes someone would want to access from a file manager."* The other one
was a row reading `1E584DFD584DD465`, which is a filesystem UUID standing in for
a name nobody gave it.

"A filesystem a person can open" turned out to be the wrong test. Every
partition on a disk is one. What a sidebar answers is narrower: which of them is
somewhere a person *goes*.

### Three answers were measured, and the first two were wrong

**Read the GPT attribute flag.** Bit 63 of a GPT partition's attributes is the
UEFI specification's *no drive letter*, which is what Windows sets on partitions
it does not want anybody to see. On the reference machine it is set on exactly
the two rows that are not wanted and absent from the two that are:

| partition | what it is | `ID_PART_ENTRY_FLAGS` | wanted |
| --- | --- | --- | --- |
| `nvme1n1p1` | *DO_NOT_USE*, `ext4`, named `Microsoft reserved partition` | `0x8000000000000000` | no |
| `nvme1n1p3` | `ntfs`, no label, Windows Recovery type | `0x8000000000000001` | no |
| `nvme0n1p4` | *SHARED*, `exfat`, basic data | *absent* | **yes** |
| `sda1` | *ESD-USB*, `vfat`, a USB stick | `0x80`, scheme `dos` | **yes** |

It was implemented, and then checked against udisks2's own `HintIgnore` — the
answer every desktop's sidebar already follows — on every block device this
machine has. **They disagreed on the first one asked.** The EFI System
Partition's attributes are empty and udisks2 ignores it anyway, because the flag
is one of several inputs and not the rule.

**Ask udisks2 for `HintIgnore`.** It is the reference implementation and it is
one property read per device — but ADR 0030's whole reason for reading udev's
database rather than `GetManagedObjects` is that a machine with no D-Bus still
has volumes. A filter that needs D-Bus would make the bare-Sway case list
partitions the GNOME case hides, and **a rule that produces a different sidebar
depending on whether a service is running is worse than either sidebar**.

**Read what udisks2 already wrote into udev.** udisks2 ships `80-udisks2.rules`,
which computes the answer at boot and writes it where the type and the label
already are:

```
b259:1  UDISKS_AUTO=0  UDISKS_IGNORE=1     DO_NOT_USE, Microsoft reserved
b259:3                 UDISKS_IGNORE=1     the Windows Recovery ntfs
b259:5                 UDISKS_IGNORE=1     the EFI System Partition
b259:6  UDISKS_AUTO=0                      /boot
b259:8                                     SHARED
b8:1                                       the USB stick
b252:1  UDISKS_AUTO=0                      /
```

`UDISKS_IGNORE=1` is on exactly the three that should not be listed and on none
of the three that should. It is `HintIgnore` **by construction** rather than by
agreement, it costs no round trip, and it is in a file already being read.

**`UDISKS_AUTO` is the wrong one**, and the table above is why: it is `0` on `/`
and on `/boot`. It means *do not mount this automatically at login*, which the
root filesystem obviously does not need and is not a statement about whether
anybody goes there.

## Decision

**A device is a volume when udev says `ID_FS_USAGE=filesystem`, its `ID_FS_TYPE`
is on [`mounts::INTERESTING`](../../crates/sk-fs/src/mounts.rs), and udev has not
been told to ignore it.**

- **`UDISKS_IGNORE=1` in `/run/udev/data/b<major>:<minor>` hides the row.** It is
  udisks2's own answer, written by udisks2's own rules, and reading it is how
  this sidebar agrees with every other desktop's about the same disk without
  depending on a running service.
- **Where that property is absent, the GPT attribute is the fallback**: bit 63
  (*no drive letter*) or bit 62 (*hidden*), and **only under the `gpt` scheme**.
  In the `dos` scheme the field is one byte where `0x80` means *bootable* —
  which the USB stick above has — and reading it as a GPT attribute is reading a
  number in the wrong language. This is the machine with udev and no udisks2,
  which is a real machine and the one ADR 0030's whole shape exists for.
- **It applies whether or not the device is mounted.** The Windows Recovery
  partition above was mounted at the time, by a press on the very row this record
  exists to remove, so a filter over unmounted candidates alone would have left
  it.
- **The absence of a statement is not a statement.** An LVM logical volume, an
  MBR partition and a disk with no partition table have nothing to read, and `/`
  is one of these.
- **No udev is no filter**, the same degradation ADR 0030 documents for the
  description: a machine with no udev database shows what the mount table shows,
  which is where all of this started.

## Consequences

**A hidden volume that something else mounted cannot be unmounted from here.**
The row is gone, so its Unmount and Eject are gone with it. That is the cost of
taking the person at their word, and the remedy is `udisksctl` or the desktop's
own disks application — the tools that mounted it. A row that appeared when
mounted and vanished when not would be a sidebar that changes shape for a reason
nobody can see, which is worse.

**A udev property is now load-bearing, and it is a third party's.**
`UDISKS_IGNORE` is udisks2's, not a specification's, and a udisks2 that renamed
it would take the filter with it — falling back to the GPT flag, which catches
two of the three rows on this machine and not the EFI partition. The failure mode
is a row that should not be there, which is this record's own subject and
therefore the thing most likely to be reported.

**The two agree, and a test says so.** `what_this_rule_hides_is_what_udisks2_ignores`
walks every block device on the machine and compares this rule against
`HintIgnore` read over the bus, skipping where there is no udisks2 to ask. It
found the disagreement that produced this record's second option, on the first
device it asked about.

**The sidebar is now a shorter list than the disk holds**, deliberately. Somebody
who wants a hidden partition has `Ctrl+L` and its mount point, which is the
answer the non-goals already give for every place the sidebar does not list.

## Revisit trigger

**A partition somebody wants that is hidden**, which is the mirror of the report
that produced this record and would arrive the same way. The answer then is not
to weaken the filter but to give the sidebar a *Show every partition* setting,
which is
[ADR 0027](0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)'s
seventh setting and costs what that record says it costs.

**Or the test above failing on any machine**, which is either udisks2 changing
what it writes or this rule reading it wrongly — and the difference is one
`busctl` call away.
