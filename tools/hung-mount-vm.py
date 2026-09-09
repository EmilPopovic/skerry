#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run the FUSE rig and actual GUI liveness scenario without changing host devices."""
import argparse
import gzip
import importlib.util
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("sandbox_vm", ROOT / "tools/sandbox-vm.py")
vm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vm)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--busybox", type=Path, required=True)
    args = parser.parse_args()
    files = {}

    def put(name, data, mode=stat.S_IFREG | 0o755):
        name = str(name).lstrip("/")
        for parent in reversed(Path(name).parents):
            if str(parent) != ".":
                files.setdefault(str(parent), (stat.S_IFDIR | 0o755, b""))
        files[name] = (mode, data)

    def dynamic(source, destination=None):
        source = Path(source)
        put(destination or source, source.read_bytes())
        linked = subprocess.run(["ldd", str(source)], capture_output=True, text=True)
        for name in set(re.findall(r"/[^\s]+", linked.stdout)):
            path = Path(name)
            if path.is_file():
                put(path, path.read_bytes())

    def tree(source):
        for path in Path(source).rglob("*"):
            if path.is_file() and "__pycache__" not in path.parts:
                put(path, path.read_bytes(), stat.S_IFREG | 0o644)
                if path.suffix == ".so":
                    dynamic(path)

    for name in ("python3", "Xvfb", "xdpyinfo", "xwininfo", "fusermount3", "xkbcomp"):
        program = shutil.which(name)
        if not program:
            parser.error(f"missing guest prerequisite: {name}")
        dynamic(program, f"/usr/bin/{name}")
    version = subprocess.check_output(["/usr/bin/python3", "-c", "import sys;print(f'{sys.version_info.major}.{sys.version_info.minor}')"], text=True).strip()
    tree(f"/usr/lib/python{version}")
    tree("/usr/share/X11")
    for name in ("libvulkan.so.1", "libvulkan_lvp.so", "libX11.so.6", "libX11-xcb.so.1", "libxcb.so.1"):
        dynamic(Path("/usr/lib/x86_64-linux-gnu") / name)
    for path in Path("/usr/share/vulkan/icd.d").glob("lvp*json"):
        put(path, path.read_bytes(), stat.S_IFREG | 0o644)
    for path in Path("/usr/share/fonts/truetype/noto").glob("NotoSans-Regular.ttf"):
        put(path, path.read_bytes(), stat.S_IFREG | 0o644)
    for name in ("skerry", "skerry-drive", "skerry-thumbnailer", "skerry-preview-doc", "skerry-archive"):
        dynamic(ROOT / "target/release" / name, f"/repo/target/release/{name}")
    for name in ("gui-test", "hung-mount"):
        put(f"/repo/tools/{name}", (ROOT / "tools" / name).read_bytes())
    put("/bin/busybox", args.busybox.read_bytes())
    for name in ("sh", "mount", "mkdir", "chmod", "chown", "su", "cat", "echo", "umount", "stat", "ls"):
        put(f"/bin/{name}", b"busybox", stat.S_IFLNK | 0o777)
    put("/etc/passwd", b"root:x:0:0:root:/:/bin/sh\ntest:x:1000:1000:test:/home/test:/bin/sh\n")
    put("/etc/group", b"root:x:0:\ntest:x:1000:\n")
    put("/etc/mtab", b"/proc/mounts", stat.S_IFLNK | 0o777)
    put("/guest-test", b'''#!/bin/sh
export PATH=/usr/bin:/bin HOME=/home/test XDG_RUNTIME_DIR=/home/test/runtime
export SKERRY_REQUIRE_HUNG_MOUNT=1 SKERRY_HUNG_LIVENESS_ONLY=1
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
cd /repo
python3 tools/hung-mount --self-test &&
python3 tools/gui-test --prebuilt --x11 --keep --scenario-timeout=180 a-hung-mount-never-blocks-the-frame-loop &&
echo SKERRY_HUNG_GUI_VM_PASS
''')
    put("/init", b'''#!/bin/sh
export PATH=/usr/bin:/bin
mkdir -p /proc /sys /dev /tmp /home/test /tmp/.X11-unix
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/shm
mount -t tmpfs tmpfs /dev/shm
chmod 1777 /tmp /tmp/.X11-unix
chmod 666 /dev/fuse
chmod 4755 /usr/bin/fusermount3
chown -R 1000:1000 /home/test /repo
su test -c /guest-test
for file in /tmp/skerry-gui-*/app.err /tmp/skerry-gui-*/app.log; do cat "$file"; done
/bin/busybox poweroff -f
''')
    entries = [(name, mode, body) for name, (mode, body) in files.items()]
    entries.append(("TRAILER!!!", 0, b""))
    with tempfile.TemporaryDirectory(prefix="skerry-hung-gui-vm-") as scratch:
        archive = Path(scratch) / "initramfs.gz"
        archive.write_bytes(gzip.compress(vm.initramfs(entries), compresslevel=1))
        result = subprocess.run(["qemu-system-x86_64", "-accel", "tcg", "-m", "1536", "-smp", "2",
            "-no-reboot", "-display", "none", "-serial", "stdio", "-monitor", "none", "-nic", "none",
            "-kernel", str(args.kernel.resolve()), "-initrd", str(archive), "-append",
            "console=ttyS0 panic=-1 rdinit=/init lsm=landlock"], capture_output=True, text=True, timeout=300)
    print(result.stdout)
    if result.returncode or "SKERRY_HUNG_GUI_VM_PASS" not in result.stdout:
        parser.exit(1, result.stderr + "\nHung GUI VM did not pass\n")
    print("VM liveness passed; emulated timing is not evidence of the native 8 ms frame budget.")


if __name__ == "__main__":
    main()
