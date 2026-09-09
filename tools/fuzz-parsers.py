#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run bounded, reproducible tar/ZIP/D-Bus mutation campaigns."""
import argparse
import json
import os
from pathlib import Path
import resource
import subprocess

ROOT = Path(__file__).resolve().parent.parent


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
    built = subprocess.check_output([
        "cargo", "test", "--locked", "-p", "skerry-archive", "-p", "sk-desktop",
        "--no-run", "--message-format=json",
    ], cwd=ROOT, text=True)
    binaries = set()
    for line in built.splitlines():
        item = json.loads(line)
        if item.get("profile", {}).get("test") and item.get("executable") and item.get("target", {}).get("name") in ("sk_desktop", "skerry-archive"):
            binaries.add(item["executable"])
    if len(binaries) != 2:
        raise RuntimeError(f"expected two parser test executables, found {len(binaries)}")
    env = dict(os.environ, SKERRY_FUZZ_CASES=str(args.cases), SKERRY_FUZZ_SEED=str(args.seed))
    for binary in sorted(binaries):
        subprocess.run([binary, "parser_campaign", "--nocapture", "--test-threads=1"],
                       cwd=ROOT, env=env, check=True, timeout=150, preexec_fn=limits)


if __name__ == "__main__":
    main()
