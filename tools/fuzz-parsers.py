#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run bounded, reproducible mutation campaigns against every parser target.

ADR 0040. One test binary per package that holds a target, and every test in it
whose name says `parser_campaign`: tar and ZIP (skerry-archive), the D-Bus wire and
`index.theme` (sk-desktop), GIF and EXIF (skerry-thumbnailer), the PDF words
layer (skerry-preview-doc), markdown (sk-app), and file URIs and the X11
selection collector (sk-plat).
"""
import argparse
import json
import os
from pathlib import Path
import resource
import subprocess

ROOT = Path(__file__).resolve().parent.parent

# Package, and the name of the test target its campaigns are compiled into.
TARGETS = {
    "skerry-archive": "skerry-archive",
    "sk-desktop": "sk_desktop",
    "skerry-thumbnailer": "skerry-thumbnailer",
    "skerry-preview-doc": "skerry-preview-doc",
    "sk-app": "sk_app",
    "sk-plat": "sk_plat",
}


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (2 * 1024**3, 2 * 1024**3))
    resource.setrlimit(resource.RLIMIT_CPU, (120, 120))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=int, default=50000)
    parser.add_argument("--seed", type=int, default=380535)
    args = parser.parse_args()
    if args.cases < 1 or args.cases > 1000000:
        parser.error("cases must be 1..1000000")
    packages = [argument for name in TARGETS for argument in ("-p", name)]
    built = subprocess.check_output([
        "cargo", "test", "--locked", *packages, "--no-run", "--message-format=json",
    ], cwd=ROOT, text=True)
    binaries = set()
    for line in built.splitlines():
        item = json.loads(line)
        if item.get("profile", {}).get("test") and item.get("executable") and item.get("target", {}).get("name") in TARGETS.values():
            binaries.add(item["executable"])
    if len(binaries) != len(TARGETS):
        raise RuntimeError(f"expected {len(TARGETS)} parser test executables, found {len(binaries)}")
    env = dict(os.environ, SKERRY_FUZZ_CASES=str(args.cases), SKERRY_FUZZ_SEED=str(args.seed))
    for binary in sorted(binaries):
        subprocess.run([binary, "parser_campaign", "--nocapture", "--test-threads=1"],
                       cwd=ROOT, env=env, check=True, timeout=150, preexec_fn=limits)


if __name__ == "__main__":
    main()
