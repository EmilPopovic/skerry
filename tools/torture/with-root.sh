#!/usr/bin/env bash
# The torture matrix — the filesystems that need root, which is all of them except ext4,
# tmpfs and NTFS.
#
# `mount(2)` needs CAP_SYS_ADMIN and there is no way around it: a loop device
# cannot be set up from a user namespace. Everything else here is unprivileged,
# and this script exists so the privileged part is one command with one password.
#
#   sudo ./with-root.sh                 # btrfs, xfs, exfat, vfat, and NFS if present
#   sudo ./with-root.sh /var/tmp/spike  # a new directory, somewhere with room for the images
#
# The matrix itself runs as the *invoking* user, not as root. That is deliberate:
# root can set any extended attribute and write any file, so the "report what
# could not be preserved" paths never fire and the run would be a weaker test
# than the unprivileged one.
#
# **Root acts only on paths nobody else can change, and deletes only what it
# made.** Root follows symlinks, so a directory another user prepared — the old
# default was the predictable `/var/tmp/skerry-spike-e-fs` — let them aim its
# `truncate`, `mkfs` and `rm -rf` at any file on the machine; and the cleanup
# was `rm -rf` of whatever directory was named, `/var/tmp` included. So the
# default is `mktemp -d`, and a named directory must be new — or empty, owned
# by root and writable by nobody else — under ancestors owned by root or the
# invoking user and writable by nobody else unless sticky.
set -u
RUN_UID=${SUDO_UID:-$(id -u)}
RUN_GID=${SUDO_GID:-$(id -g)}

refuse() {
  echo "with-root.sh: $*" >&2
  exit 1
}

# Trusted: root, and the invoking user, whose `sudo` this already is. Not a
# group, not even theirs: nothing here can tell a private group from a shared
# one. A sticky directory such as `/var/tmp` is safe to stand under whoever can
# write to it, because nobody can rename or remove an entry they do not own.
refuse_unsafe_ancestors() {
  local dir=$1 info owner mode
  while :; do
    info=$(stat -c '%u %a' -- "$dir") || refuse "cannot stat $dir"
    read -r owner mode <<<"$info"
    if [ "$owner" != 0 ] && [ "$owner" != "$RUN_UID" ]; then
      refuse "$dir belongs to uid $owner, who could replace what is under it"
    fi
    if (( 8#$mode & 8#022 && !(8#$mode & 8#1000) )); then
      refuse "$dir is writable by its group or by every user, and is not sticky"
    fi
    [ "$dir" = / ] && return
    dir=$(dirname -- "$dir")
  done
}

# `stat` without `-L` describes a symlink, not where it points.
refuse_unusable_work() {
  local info owner mode entries
  [ -L "$WORK" ] && refuse "$WORK is a symlink"
  if [ ! -e "$WORK" ]; then
    WORK_IS_NEW=1
    return 0
  fi
  [ -d "$WORK" ] || refuse "$WORK exists and is not a directory"
  info=$(stat -c '%u %a' -- "$WORK") || refuse "cannot stat $WORK"
  read -r owner mode <<<"$info"
  [ "$owner" = 0 ] || refuse "$WORK exists and is not owned by root"
  (( 8#$mode & 8#022 )) && refuse "$WORK is writable by users other than root"
  entries=$(ls -A -- "$WORK") || refuse "cannot list $WORK"
  [ -z "$entries" ] || refuse "$WORK exists and is not empty"
}

# Resolved before the `cd` below, so a relative path means what the caller meant.
WORK=
WORK_IS_NEW=
if [ $# -gt 0 ]; then
  name=$(basename -- "$1")
  case $name in .|..|/) refuse "name a new directory for the images, not $1" ;; esac
  parent=$(realpath -e -- "$(dirname -- "$1")") || refuse "$(dirname -- "$1") does not exist"
  WORK=${parent%/}/$name
  refuse_unsafe_ancestors "$parent"
  refuse_unusable_work
fi

cd "$(dirname "$0")"
BIN=$PWD/target/release/skerry-torture

if [ "$(id -u)" != 0 ]; then
  echo "this script needs root for mount(2); re-run with sudo" >&2
  exit 1
fi
if [ ! -x "$BIN" ]; then
  echo "build first: cargo build --release" >&2
  exit 1
fi
# `setpriv` changes the uid and not the environment, so `$HOME` still points at
# root's home. The matrix looks for a directory under `$HOME` to find the root
# filesystem, so without this the ext4 target disappears from a `sudo` run.
RUN_HOME=$(getent passwd "$RUN_UID" | cut -d: -f6)
RUN_HOME=${RUN_HOME:-/tmp}

mounted=()
loops=()
exported=
# What this run made, removed in reverse. Directories go with `rmdir`, so one
# that is still a mount point, or holds anything this script did not put there,
# is named and left rather than recursed into.
made_files=()
made_dirs=()
nfs_export=
cleanup() {
  local i
  for (( i = ${#mounted[@]} - 1; i >= 0; i-- )); do
    umount "${mounted[i]}" 2>/dev/null || umount -l "${mounted[i]}" 2>/dev/null
  done
  # The NFS export outlives the mount, so it is removed explicitly.
  [ -n "$exported" ] && exportfs -u "$exported" 2>/dev/null
  for l in "${loops[@]:-}"; do
    [ -n "$l" ] && losetup -d "$l" 2>/dev/null
  done
  # The one directory whose contents are the matrix's rather than this
  # script's: what it wrote through NFS. `--one-file-system`, so a mount that
  # would not come off is not descended into.
  [ -n "$nfs_export" ] && rm -rf --one-file-system -- "$nfs_export"
  for (( i = ${#made_files[@]} - 1; i >= 0; i-- )); do
    rm -f -- "${made_files[i]}"
  done
  for (( i = ${#made_dirs[@]} - 1; i >= 0; i-- )); do
    rmdir -- "${made_dirs[i]}" 2>/dev/null || echo "left behind: ${made_dirs[i]}" >&2
  done
}
trap cleanup EXIT

if [ -z "$WORK" ]; then
  WORK=$(mktemp -d /var/tmp/skerry-torture.XXXXXXXXXX) || exit 1
  made_dirs+=("$WORK")
  # `mktemp` makes it 0700, and the matrix, running as the invoking user, has
  # to reach the mounts inside it.
  chmod 0755 "$WORK"
elif [ -n "$WORK_IS_NEW" ]; then
  # On what the check saw, not on a second look: the path is in our command
  # line for anyone to read, so something may have appeared there since, and
  # then this fails instead of adopting it.
  mkdir -m 0755 -- "$WORK" || exit 1
  made_dirs+=("$WORK")
fi
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
  made_files+=("$img")
  if ! "mkfs.$fs" "$@" "$img" >/dev/null 2>&1; then
    echo "  skip $fs: mkfs refused the image"
    return
  fi
  mkdir -- "$mnt" || return
  made_dirs+=("$mnt")
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
#
# Everything root does inside the export happens before `exportfs`. The export
# is `insecure` and AUTH_SYS, so once it is live any local user can speak NFS
# to it claiming to be uid $RUN_UID, and a `mkdir` or `chown` by root through
# the mount would follow whatever they had planted there.
if command -v exportfs >/dev/null && [ -d "$WORK/btrfs" ]; then
  export_dir="$WORK/nfs-export"
  if mkdir -- "$export_dir" && nfs_export=$export_dir &&
     mkdir -- "$export_dir/skerry" && chown "$RUN_UID:$RUN_GID" "$export_dir/skerry" &&
     mkdir -- "$WORK/nfs" && made_dirs+=("$WORK/nfs") &&
     exportfs -o rw,no_subtree_check,insecure "127.0.0.1:$export_dir" 2>/dev/null &&
     exported=127.0.0.1:$export_dir &&
     mount -t nfs -o vers=4 "127.0.0.1:$export_dir" "$WORK/nfs" 2>/dev/null; then
    mounted+=("$WORK/nfs")
    targets+=("$WORK/nfs/skerry")
    echo "  ready nfs at $WORK/nfs/skerry"
  else
    echo "  skip nfs: could not export or mount"
    [ -n "$exported" ] && exportfs -u "$exported" 2>/dev/null
    exported=
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
exit $?
