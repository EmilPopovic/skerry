#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the real archive helper with Python-generated archives and wire requests."""
import argparse
import io
from pathlib import Path
import struct
import subprocess
import tarfile
import tempfile
import zipfile

MAGIC = 0x534B4152


def unpack(binary, archive, destination):
    destination.mkdir()
    source, target = bytes(archive), bytes(destination)
    request = struct.pack("<IHHIII", MAGIC, 1, 2, len(source), len(target), 0) + source + target
    child = subprocess.run([str(binary)], input=request, capture_output=True, timeout=15, check=True)
    data = child.stdout
    done = None
    while data:
        if len(data) < 32:
            raise AssertionError("truncated helper record")
        magic, version, kind, refusal, _, entries, size, length = struct.unpack("<IHHHHQQI", data[:32])
        assert (magic, version) == (MAGIC, 1)
        assert len(data) >= 32 + length
        if kind == 2:
            assert done is None
            done = (refusal, entries, size)
        else:
            assert kind == 1 and done is None
        data = data[32 + length:]
    assert done is not None, child.stderr.decode(errors="replace")
    return done


def tar_bytes(format, files):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=format) as tar:
        for name, contents in files.items():
            entry = tarfile.TarInfo(name)
            entry.size = len(contents)
            tar.addfile(entry, io.BytesIO(contents))
    return stream.getvalue()


def check(binary):
    with tempfile.TemporaryDirectory(prefix="skerry-archive-corpus-") as scratch:
        root = Path(scratch)
        cases = 0
        for label, format, files in (
            ("ustar", tarfile.USTAR_FORMAT, {"ordinary": b"contents", "folder/other": b"next"}),
            ("gnu", tarfile.GNU_FORMAT, {"long/" + "x" * 140: b"long name"}),
            ("pax", tarfile.PAX_FORMAT, {"café\nnotes": b"x" * 513, "following": b"next"}),
        ):
            path = root / f"{label}.tar"
            path.write_bytes(tar_bytes(format, files))
            with tarfile.open(path) as reference:
                assert {item.name: reference.extractfile(item).read() for item in reference} == files
            destination = root / label
            refusal, _, size = unpack(binary, path, destination)
            assert refusal == 0, (label, refusal)
            assert size == sum(map(len, files.values())), (label, size)
            for name, contents in files.items():
                assert (destination / name).read_bytes() == contents
            cases += 1
        # A legal PAX override deliberately disagrees with the legacy size field.
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w", format=tarfile.PAX_FORMAT) as tar:
            entry = tarfile.TarInfo("override")
            entry.size = 513
            entry.pax_headers = {"size": "513"}
            tar.addfile(entry, io.BytesIO(b"x" * 513))
        data = bytearray(stream.getvalue())
        with tarfile.open(fileobj=io.BytesIO(data)) as reference:
            offset = reference.next().offset_data - 512
        data[offset + 124:offset + 136] = b"00000000001\0"
        data[offset + 148:offset + 156] = b" " * 8
        checksum = sum(data[offset:offset + 512])
        data[offset + 148:offset + 156] = f"{checksum:06o}\0 ".encode()
        path = root / "override.tar"
        path.write_bytes(data)
        with tarfile.open(path) as reference:
            assert reference.extractfile(reference.next()).read() == b"x" * 513
        assert unpack(binary, path, root / "override")[0] == 0
        assert (root / "override/override").read_bytes() == b"x" * 513
        cases += 1
        original = tar_bytes(tarfile.USTAR_FORMAT, {"first": b"x", "second": b"y"})
        corrupt = bytearray(original)
        corrupt[1024] ^= 1
        for label, data in (("bad-header", corrupt), ("short-payload", original[:512]), ("short-padding", original[:513]), ("short-header", original[:1024] + b"broken")):
            path = root / f"{label}.tar"
            path.write_bytes(data)
            assert unpack(binary, path, root / label)[0] != 0, label
            cases += 1
        for label, compression in (("stored", zipfile.ZIP_STORED), ("deflated", zipfile.ZIP_DEFLATED)):
            path = root / f"{label}.zip"
            with zipfile.ZipFile(path, "w", compression=compression) as zip:
                zip.writestr("folder/file", b"payload" * 100)
            assert unpack(binary, path, root / label)[0] == 0
            assert (root / label / "folder/file").read_bytes() == b"payload" * 100
            cases += 1
        outside = root / "sentinel"
        outside.write_text("valuable")
        path = root / "traversal.tar"
        with tarfile.open(path, "w") as tar:
            link = tarfile.TarInfo("link")
            link.type = tarfile.SYMTYPE
            link.linkname = str(root)
            tar.addfile(link)
            for name in ("../sentinel", "link/sentinel"):
                entry = tarfile.TarInfo(name)
                entry.size = 7
                tar.addfile(entry, io.BytesIO(b"changed"))
        assert unpack(binary, path, root / "traversal")[0] == 9
        assert outside.read_text() == "valuable"
        cases += 1
        print(f"archive interoperability: {cases} cases pass")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path(__file__).resolve().parent.parent / "target/release/skerry-archive")
    check(parser.parse_args().binary.resolve())
