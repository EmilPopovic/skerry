#!/usr/bin/env bash
# Spike E — the filesystems that need root, which is all of them except ext4,
# tmpfs and NTFS.
#
# `mount(2)` needs CAP_SYS_ADMIN and there is no way around it: a loop device
# cannot be set up from a user namespace. Everything else here is unprivileged,
# and this script exists so the privileged part is one command with one password.
#
#   sudo ./with-root.sh                 # btrfs, xfs, exfat, vfat, and NFS if present
#   sudo ./with-root.sh /var/tmp/spike  # somewhere with room for the images
#
# The matrix itself runs as the *invoking* user, not as root. That is deliberate:
# root can set any extended attribute and write any file, so the "report what
# could not be preserved" paths never fire and the run would be a weaker test
# than the unprivileged one.
set -u
cd "$(dirname "$0")"
BIN=$PWD/target/release/spike-e-operations
WORK=${1:-/var/tmp/skerry-spike-e-fs}

if [ "$(id -u)" != 0 ]; then
  echo "this script needs root for mount(2); re-run with sudo" >&2
  exit 1
fi
if [ ! -x "$BIN" ]; then
  echo "build first: cargo build --release" >&2
  exit 1
fi
RUN_UID=${SUDO_UID:-$(id -u)}
RUN_GID=${SUDO_GID:-$(id -g)}
# `setpriv` changes the uid and not the environment, so `$HOME` still points at
# root's home. The matrix looks for a directory under `$HOME` to find the root
# filesystem, so without this the ext4 target disappears from a `sudo` run.
RUN_HOME=$(getent passwd "$RUN_UID" | cut -d: -f6)
RUN_HOME=${RUN_HOME:-/tmp}

mounted=()
loops=()
cleanup() {
  for m in "${mounted[@]:-}"; do
    [ -n "$m" ] || continue
    umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null
  done
  for l in "${loops[@]:-}"; do
    [ -n "$l" ] && losetup -d "$l" 2>/dev/null
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$WORK"
targets=()

# btrfs needs about 110 MiB, xfs about 300 MiB. The images are sparse, so the
# size costs nothing until it is used.
#
# FAT-family filesystems have no concept of ownership: every file belongs to
# whoever the mount says, and `chown` on them fails with EPERM even as root. So
# the owner is a *mount option* there, and getting that wrong is how the first
# version of this script produced two filesystems that the matrix then could not
# write to — and silently skipped.
mount_opts() {
  case $1 in
    exfat|vfat) echo "loop,uid=$RUN_UID,gid=$RUN_GID,umask=022" ;;
    *)          echo "loop" ;;
  esac
}

needs_chown() {
  case $1 in exfat|vfat) return 1 ;; *) return 0 ;; esac
}

package_for() {
  case $1 in
    btrfs) echo "btrfs-progs" ;;
    xfs)   echo "xfsprogs" ;;
    exfat) echo "exfatprogs" ;;
    vfat)  echo "dosfstools" ;;
    *)     echo "the package providing mkfs.$1" ;;
  esac
}

make_fs() {
  local fs=$1 size=$2; shift 2
  if ! command -v "mkfs.$fs" >/dev/null; then
    echo "  skip $fs: no mkfs.$fs — install $(package_for "$fs")"
    return
  fi
  local img="$WORK/$fs.img" mnt="$WORK/$fs"
  truncate -s "$size" "$img" || return
  if ! "mkfs.$fs" "$@" "$img" >/dev/null 2>&1; then
    echo "  skip $fs: mkfs refused the image"
    return
  fi
  mkdir -p "$mnt"
  if ! mount -o "$(mount_opts "$fs")" "$img" "$mnt" 2>/dev/null; then
    echo "  skip $fs: mount refused"
    return
  fi
  mounted+=("$mnt")
  local dir="$mnt/skerry"
  if ! mkdir -p "$dir"; then
    echo "  skip $fs: could not create a directory on the mount"
    return
  fi
  if needs_chown "$fs"; then
    chown "$RUN_UID:$RUN_GID" "$dir" || { echo "  skip $fs: chown failed"; return; }
  fi
  # The matrix runs unprivileged, so a directory it cannot write to is a target
  # that will be reported as a failure rather than tested. Check here, where the
  # reason is still known.
  if ! setpriv --reuid "$RUN_UID" --regid "$RUN_GID" --clear-groups \
       test -w "$dir"; then
    echo "  skip $fs: mounted, but uid $RUN_UID cannot write to it"
    return
  fi
  targets+=("$dir")
  echo "  ready $fs at $dir"
}

echo "creating filesystems under $WORK"
make_fs btrfs 2G -q -f
make_fs xfs   2G -q -f
make_fs exfat 1G
make_fs vfat  1G -F 32

# NFS over loopback, if the machine can export. This is the sixth filesystem in
# the roadmap's list and the one whose failure modes are least like the others:
# no hardlink guarantees worth trusting, no xattrs, and a server that can go away
# mid-copy.
if command -v exportfs >/dev/null && [ -d "$WORK/btrfs" ]; then
  export_dir="$WORK/nfs-export"
  mkdir -p "$export_dir" "$WORK/nfs"
  chown "$RUN_UID:$RUN_GID" "$export_dir"
  if exportfs -o rw,no_subtree_check,insecure "127.0.0.1:$export_dir" 2>/dev/null &&
     mount -t nfs -o vers=4 "127.0.0.1:$export_dir" "$WORK/nfs" 2>/dev/null; then
    mounted+=("$WORK/nfs")
    mkdir -p "$WORK/nfs/skerry"
    chown "$RUN_UID:$RUN_GID" "$WORK/nfs/skerry"
    targets+=("$WORK/nfs/skerry")
    echo "  ready nfs at $WORK/nfs/skerry"
  else
    echo "  skip nfs: could not export or mount"
    exportfs -u "127.0.0.1:$export_dir" 2>/dev/null
  fi
else
  echo "  skip nfs: exportfs is not installed"
fi

if [ ${#targets[@]} -eq 0 ]; then
  echo >&2
  echo "no additional filesystem could be created." >&2
  echo "the reflink condition needs btrfs or xfs:  apt install btrfs-progs xfsprogs" >&2
  exit 1
fi

echo
echo "running the matrix as uid $RUN_UID (HOME=$RUN_HOME) over ${#targets[@]} extra filesystem(s)"
echo
setpriv --reuid "$RUN_UID" --regid "$RUN_GID" --clear-groups \
  env HOME="$RUN_HOME" "$BIN" run "${targets[@]}"
rc=$?

# The NFS export outlives the mount, so it is removed explicitly.
if [ -d "$WORK/nfs-export" ]; then
  umount "$WORK/nfs" 2>/dev/null
  exportfs -u "127.0.0.1:$WORK/nfs-export" 2>/dev/null
fi
exit $rc
