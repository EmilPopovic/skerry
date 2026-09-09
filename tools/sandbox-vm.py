#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run actual sandbox denials in a disposable x86-64 kernel, without host mounts."""
import argparse
import json
import re
from pathlib import Path
import stat
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def initramfs(files):
    data = bytearray()
    for inode, (name, mode, body) in enumerate(files, 1):
        encoded = name.encode() + b'\0'
        fields = (inode, mode, 0, 0, 1, 0, len(body), 0, 0, 0, 0, len(encoded), 0)
        data.extend(b'070701' + ''.join(f'{value:08x}' for value in fields).encode())
        data.extend(encoded)
        data.extend(b'\0' * (-len(data) % 4))
        data.extend(body)
        data.extend(b'\0' * (-len(data) % 4))
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--busybox', type=Path, required=True, help='statically linked BusyBox')
    args = parser.parse_args()
    build = subprocess.check_output(['cargo', 'test', '--locked', '--target',
        'x86_64-unknown-linux-musl', '-p', 'sk-preview', '--lib', '--no-run',
        '--message-format=json'], cwd=ROOT, text=True)
    executable = next(Path(record['executable']) for line in build.splitlines()
                      if (record := json.loads(line)).get('executable'))
    init = b'''#!/bin/busybox sh
export PATH=/bin
/bin/busybox mount -t proc proc /proc
export SKERRY_REQUIRE_LANDLOCK=1
/test --exact sandbox::tests::landlock_denies_input_truncation_and_output_execution --nocapture &&
/test --exact sandbox::tests::seccomp_denies_networking_but_preserves_threads_and_tools --nocapture &&
echo SKERRY_SANDBOX_VM_PASS
/bin/busybox poweroff -f
'''
    files = [(name, stat.S_IFDIR | 0o755, b'') for name in ('bin', 'tmp', 'proc', 'dev')]
    files += [('bin/busybox', stat.S_IFREG | 0o755, args.busybox.read_bytes()),
              ('bin/sh', stat.S_IFLNK | 0o777, b'busybox'),
              ('bin/true', stat.S_IFREG | 0o755, Path('/usr/bin/true').read_bytes()),
              ('init', stat.S_IFREG | 0o755, init),
              ('test', stat.S_IFREG | 0o755, executable.read_bytes()),
              ]
    # A dynamic executable exercises the exact ELF-loader grant as well as exec.
    libraries = subprocess.check_output(['ldd', '/usr/bin/true'], text=True)
    directories = {'bin', 'tmp', 'proc', 'dev'}
    for name in sorted(set(re.findall(r'/[^\s]+', libraries))):
        path = Path(name)
        for parent in reversed(path.parents):
            relative = str(parent).lstrip('/')
            if relative and relative not in directories:
                files.append((relative, stat.S_IFDIR | 0o755, b''))
                directories.add(relative)
        files.append((name.lstrip('/'), stat.S_IFREG | 0o755, path.read_bytes()))
    files.append(('TRAILER!!!', 0, b''))
    with tempfile.TemporaryDirectory(prefix='skerry-sandbox-vm-') as scratch:
        archive = Path(scratch) / 'initramfs'
        archive.write_bytes(initramfs(files))
        result = subprocess.run(['qemu-system-x86_64', '-accel', 'tcg', '-m', '512',
            '-smp', '2', '-no-reboot', '-display', 'none', '-serial', 'stdio',
            '-monitor', 'none', '-nic', 'none', '-kernel', str(args.kernel.resolve()),
            '-initrd', str(archive), '-append',
            'console=ttyS0 panic=-1 rdinit=/init lsm=landlock'],
            capture_output=True, timeout=120, text=True)
    print(result.stdout)
    if result.returncode or 'SKERRY_SANDBOX_VM_PASS' not in result.stdout:
        parser.exit(1, result.stderr + '\nSandbox VM did not pass\n')


if __name__ == '__main__':
    main()
