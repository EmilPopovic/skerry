#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise archive contents and installation independently of a source build."""
from pathlib import Path
import hashlib
import subprocess
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import package


class Distribution(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="skerry distribution ")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.binaries = self.root / "binaries"
        self.binaries.mkdir()
        for name in package.BINARIES:
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18] = 62
            (self.binaries / name).write_bytes(header)
            (self.binaries / name).chmod(0o755)
        self.version = tomllib.loads((package.ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        self.output = self.root / "out"
        rust = self.root / "toolchain/share/doc/rust"
        rust.mkdir(parents=True)
        (rust / "COPYRIGHT-library.html").write_text("fixture Rust library notices")

    def build(self, **kwargs):
        def command(args, **_):
            if args[0] == "rustc":
                return str(self.root / "toolchain")
            return "GLIBC_2.28 GLIBC_2.34" if args[0] == "readelf" else f"skerry {self.version} (fixture)"
        with patch.object(package.subprocess, "check_output", side_effect=command):
            return package.package(self.binaries, self.output, kwargs.get("arch", "x86_64"), kwargs.get("tag"))

    def test_binary_distribution_installs_without_cargo_and_preserves_other_files(self):
        archive = self.build(tag=f"v{self.version}")
        first = archive.read_bytes()
        self.assertEqual(first, self.build().read_bytes(), "identical inputs must produce identical tarballs")
        self.assertEqual(archive.with_suffix(".gz.sha256").read_text().split()[0], hashlib.sha256(first).hexdigest())
        unpacked = self.root / "unpacked"
        with tarfile.open(archive) as tar:
            tar.extractall(unpacked, filter="data")
        source = next(unpacked.iterdir())
        self.assertFalse((source / "Cargo.toml").exists())
        self.assertIn("glibc-symbol-floor: 2.34", (source / "BUILD-INFO").read_text())
        destination = self.root / "staging area"
        args = [f"DESTDIR={destination}", "PREFIX=/custom prefix"]
        subprocess.run(["make", "install", *args], cwd=source, check=True, capture_output=True)
        installed = destination / "custom prefix"
        for name in package.BINARIES:
            self.assertEqual((installed / "bin" / name).read_bytes(), (self.binaries / name).read_bytes())
        for name in ("THIRD-PARTY-NOTICES.txt", "RUST-COPYRIGHT.html"):
            self.assertEqual((installed / "share/licenses/skerry" / name).read_bytes(), (source / name).read_bytes())
        sentinel = installed / "share/licenses/skerry/retained"
        sentinel.write_text("unrelated")
        subprocess.run(["make", "uninstall", *args], cwd=source, check=True, capture_output=True)
        self.assertEqual(sentinel.read_text(), "unrelated")
        self.assertFalse((installed / "bin/skerry").exists())

    def test_wrong_tag_missing_helper_and_wrong_architecture_are_refused(self):
        with self.assertRaises(ValueError):
            self.build(tag="v999.0.0")
        with self.assertRaises(ValueError):
            self.build(arch="aarch64")
        (self.binaries / "skerry-archive").unlink()
        with self.assertRaises(ValueError):
            self.build()

    def test_stale_binary_is_refused(self):
        with patch.object(package.subprocess, "check_output", return_value="skerry 0.0.0 (stale)"):
            with self.assertRaises(ValueError):
                package.package(self.binaries, self.output, "x86_64")


if __name__ == "__main__":
    unittest.main()
