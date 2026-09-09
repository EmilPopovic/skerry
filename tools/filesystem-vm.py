#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run Spike E on disposable guest filesystems; never mounts a host filesystem."""
import argparse
import gzip
import importlib.util
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('sandbox_vm', ROOT / 'tools/sandbox-vm.py')
vm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vm)


def run(args, scratch):
    files = {}

    def put(name, data, mode=stat.S_IFREG | 0o755):
        for parent in reversed(Path(name).parents):
            if str(parent) != '.':
                files.setdefault(str(parent), (stat.S_IFDIR | 0o755, b''))
        files[name] = (mode, data)

    def dynamic(source, destination, mode=stat.S_IFREG | 0o755):
        put(destination, Path(source).read_bytes(), mode)
        listing = subprocess.check_output(['ldd', str(source)], text=True)
        for library in set(re.findall(r'/[^\s]+', listing)):
            put(library.lstrip('/'), Path(library).read_bytes())

    packages = args.packages.resolve()
    put('bin/busybox', (packages / 'usr/bin/busybox').read_bytes())
    for name in ('sh', 'mount', 'mkdir', 'chmod', 'chown', 'su', 'cat', 'echo'):
        put('bin/' + name, b'busybox', stat.S_IFLNK | 0o777)
    dynamic('/bin/umount', 'bin/umount', stat.S_IFREG | 0o4755)
    dynamic(packages / 'bin/fusermount3', 'bin/fusermount3', stat.S_IFREG | 0o4755)
    put('etc/mtab', b'/proc/mounts', stat.S_IFLNK | 0o777)
    put('bin/test', args.executable.resolve().read_bytes())
    put('etc/passwd', b'root:x:0:0:root:/:/bin/sh\ntest:x:1000:1000:test:/home/test:/bin/sh\n')
    put('etc/group', b'root:x:0:\ntest:x:1000:\n')
    modules = {path.name.removesuffix('.ko.zst'): path for path in packages.rglob('*.ko.zst')}
    order = []

    def module(name):
        if name in order or name not in modules:
            return
        data = subprocess.check_output([packages / 'usr/bin/zstd', '-dc', modules[name]])
        for dependencies in re.findall(rb'depends=([^\0]+)', data):
            for dependency in dependencies.decode().split(','):
                module(dependency)
        order.append(name)
        put('modules/' + name + '.ko', data)

    for name in ('btrfs', 'xfs', 'nls_cp437', 'nls_iso8859-1'):
        module(name)
    disks = []
    environment = dict(os.environ, LD_LIBRARY_PATH=str(packages / 'usr/lib/x86_64-linux-gnu'))
    for filesystem, formatter in (('ext4', Path('/usr/sbin/mkfs.ext4')),
            ('btrfs', packages / 'sbin/mkfs.btrfs'), ('xfs', packages / 'sbin/mkfs.xfs'),
            ('vfat', packages / 'usr/sbin/mkfs.vfat')):
        disk = scratch / (filesystem + '.img')
        with disk.open('wb') as output:
            output.truncate(1024 * 1024 * 1024)
        subprocess.run([str(formatter), str(disk)], env=environment, check=True)
        disks.append((filesystem, disk))
    init = '''#!/bin/busybox sh
export PATH=/bin HOME=/home/test
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /dev/shm /home/test
mount -t tmpfs tmpfs /tmp
mount -t tmpfs tmpfs /dev/shm
chmod 1777 /tmp /dev/shm
chmod 666 /dev/fuse
chown 1000:1000 /home/test
'''
    for name in order:
        init += f'/bin/busybox insmod /modules/{name}.ko || /bin/busybox poweroff -f\n'
    for index, (filesystem, _) in enumerate(disks):
        options = '-o uid=1000,gid=1000' if filesystem == 'vfat' else ''
        init += (f'mkdir -p /mnt/{filesystem}\n'
                 f'mount -t {filesystem} {options} /dev/vd{chr(97 + index)} /mnt/{filesystem}'
                 ' || /bin/busybox poweroff -f\n'
                 f'mkdir -p /mnt/{filesystem}/test\n')
        if filesystem != 'vfat':
            init += f'chown 1000:1000 /mnt/{filesystem}/test\n'
    targets = ' '.join('/mnt/' + filesystem + '/test' for filesystem, _ in disks)
    init += f'su test -c "/bin/test run {targets}" && echo SKERRY_FILESYSTEM_VM_PASS\n'
    init += '/bin/busybox poweroff -f\n'
    put('init', init.encode())
    for name in ('proc', 'sys', 'dev'):
        put(name, b'', stat.S_IFDIR | 0o755)
    archive = vm.initramfs([(name, mode, data) for name, (mode, data) in files.items()]
                          + [('TRAILER!!!', 0, b'')])
    initrd = scratch / 'initramfs.gz'
    initrd.write_bytes(gzip.compress(archive, mtime=0))
    command = ['qemu-system-x86_64', '-accel', 'tcg', '-m', '2048', '-smp', '2',
               '-no-reboot', '-display', 'none', '-serial', 'stdio', '-monitor', 'none',
               '-nic', 'none', '-kernel', str(args.kernel.resolve()), '-initrd', str(initrd),
               '-append', 'console=ttyS0 panic=-1 rdinit=/init lsm=landlock']
    for _, disk in disks:
        command += ['-drive', f'file={disk},format=raw,if=virtio']
    result = subprocess.run(command, capture_output=True, text=True, timeout=600)
    print(result.stdout)
    required = ('enospc', 'eio', 'no-xattr', 'short-write', 'enospc-tree',
                'reflink/btrfs→btrfs', 'reflink/xfs→xfs')
    if (result.returncode or 'SKERRY_FILESYSTEM_VM_PASS' not in result.stdout
            or any(not re.search(r'PASS\s+' + name + r'\s', result.stdout) for name in required)):
        raise SystemExit(result.stderr + '\nFilesystem VM did not pass every required fault and reflink check')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--packages', type=Path, required=True, help='extracted Ubuntu amd64 packages')
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--executable', type=Path, default=ROOT / 'spikes/e-operations/target/'
                        'x86_64-unknown-linux-musl/release/spike-e-operations')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='skerry-filesystem-vm-') as directory:
        run(args, Path(directory))


if __name__ == '__main__':
    main()
