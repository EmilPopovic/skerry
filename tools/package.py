#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Create the binary distribution, without rebuilding or downloading anything."""
import argparse
import gzip
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import struct
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
BINARIES = ("skerry", "skerry-thumbnailer", "skerry-preview-doc", "skerry-archive")


def package(binaries: Path, output: Path, arch: str, tag: str | None = None) -> Path:
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"tag {tag!r} does not match Cargo version v{version}")
    if arch not in ("x86_64", "aarch64"):
        raise ValueError(f"unsupported architecture: {arch}")
    for name in BINARIES:
        if not (binaries / name).is_file() or not os.access(binaries / name, os.X_OK):
            raise ValueError(f"missing executable: {binaries / name}")
        with (binaries / name).open("rb") as executable:
            header = executable.read(20)
        if len(header) < 20 or header[:6] != b"\x7fELF\x02\x01":
            raise ValueError(f"not a little-endian 64-bit ELF executable: {name}")
        machine = struct.unpack_from("<H", header, 18)[0]
        if machine != {"x86_64": 62, "aarch64": 183}[arch]:
            raise ValueError(f"{name} architecture disagrees with {arch}")
    identity = subprocess.check_output([str((binaries / "skerry").resolve()), "--version"], text=True).strip()
    if not identity.startswith(f"skerry {version} ("):
        raise ValueError(f"binary version disagrees with manifest: {identity}")
    for name in BINARIES[1:]:
        helper = subprocess.check_output([str((binaries / name).resolve()), "--version"], text=True).strip()
        if helper != identity.replace("skerry ", f"{name} ", 1):
            raise ValueError(f"helper identity disagrees with application: {helper}")
    versions = set()
    for name in BINARIES:
        symbols = subprocess.check_output(["readelf", "--version-info", str(binaries / name)], text=True)
        versions.update(re.findall(r"\bGLIBC_(\d+(?:\.\d+)+)\b", symbols))
    floor = max(versions, key=lambda value: tuple(map(int, value.split("."))), default="none")
    if floor != "none" and tuple(map(int, floor.split("."))) > (2, 39):
        raise ValueError(f"GLIBC_{floor} exceeds the alpha baseline GLIBC_2.39")
    toolchain = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    rust_notices = toolchain / "share/doc/rust/COPYRIGHT-library.html"
    if not rust_notices.is_file():
        raise ValueError("missing Rust standard-library distribution notices")
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    if epoch < 0:
        raise ValueError("SOURCE_DATE_EPOCH must be nonnegative")
    name = f"skerry-{version}-{arch}-linux"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"{name}.tar.gz"
    with tempfile.TemporaryDirectory(prefix="skerry-package-") as scratch:
        root = Path(scratch) / name
        (root / "usr/bin").mkdir(parents=True)
        (root / "usr/share/applications").mkdir(parents=True)
        for binary in BINARIES:
            target = root / "usr/bin" / binary
            shutil.copyfile(binaries / binary, target)
            target.chmod(0o755)
        for source, target in (
            ("packaging/Makefile.binary", "Makefile"),
            ("packaging/README.binary.md", "README.md"),
            ("packaging/dev.skerry.Skerry.desktop", "usr/share/applications/dev.skerry.Skerry.desktop"),
            ("LICENSE", "LICENSE"), ("NOTICE", "NOTICE"),
            ("RELEASE-NOTES.md", "RELEASE-NOTES.md"), ("SECURITY.md", "SECURITY.md"),
            ("packaging/icons/dev.skerry.Skerry.svg", "usr/share/icons/hicolor/scalable/apps/dev.skerry.Skerry.svg"),
            ("packaging/THIRD-PARTY-NOTICES.txt", "THIRD-PARTY-NOTICES.txt"),
        ):
            (root / target).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / source, root / target)
        shutil.copyfile(rust_notices, root / "RUST-COPYRIGHT.html")
        (root / "BUILD-INFO").write_text(f"{identity}\narchitecture: {arch}\nglibc-symbol-floor: {floor}\n")
        with archive.open("wb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in [root, *sorted(root.rglob("*"))]:
                    info = tar.gettarinfo(str(path), arcname=str(path.relative_to(root.parent)))
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = epoch
                    info.mode = 0o755 if path.is_dir() or path.parent == root / "usr/bin" else 0o644
                    if path.is_file():
                        with path.open("rb") as data:
                            tar.addfile(info, data)
                    else:
                        tar.addfile(info)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{checksum}  {archive.name}\n")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, default=ROOT / "target/release")
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--arch", required=True, choices=("x86_64", "aarch64"))
    parser.add_argument("--tag")
    args = parser.parse_args()
    try:
        print(package(args.binaries, args.output, args.arch, args.tag))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"skerry package: {error}\n")


if __name__ == "__main__":
    main()
