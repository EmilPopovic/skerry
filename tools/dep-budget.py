#!/usr/bin/env python3
"""Rule 10: every dependency has a row in docs/07-dependencies.md, and the
transitive graph stays inside the budgets in docs/09-performance-budgets.md.

The manifest half runs anywhere. The tree half needs cargo and a lockfile and
reports nothing when they are absent, because a check that guesses is worse
than a check that abstains.
"""

from __future__ import annotations

import re
import subprocess

from _common import ROOT, Report, load, main, manifests, nothing_to_check, rel

POLICY = ROOT / "docs" / "07-dependencies.md"

# From the budget table in docs/09-performance-budgets.md.
BUDGET_NON_GPU = 25
BUDGET_TOTAL = 120

# wgpu's tree is budgeted separately; these roots identify it in cargo tree output.
GPU_ROOTS = ("wgpu", "wgpu-core", "wgpu-hal", "wgpu-types", "naga")


def documented() -> set[str]:
    if not POLICY.exists():
        return set()
    return set(re.findall(r"`([a-z0-9][a-z0-9_-]*)`", POLICY.read_text()))


def declared() -> dict[str, list[str]]:
    """External dependency name → the manifests that declare it."""
    internal = set()
    raw: dict[str, list[str]] = {}
    for manifest in manifests(spikes=False):
        parsed = load(manifest)
        name = parsed.get("package", {}).get("name")
        if name:
            internal.add(name)
        for table in ("dependencies", "build-dependencies"):
            for dep in parsed.get(table, {}):
                raw.setdefault(dep, []).append(rel(manifest))
    for dep in internal:
        raw.pop(dep, None)
    return raw


def crate_names(tree: str) -> set[str]:
    return set(re.findall(r"^[\s│├└─]*([a-zA-Z0-9_-]+) v\d", tree, re.MULTILINE))


def run() -> int:
    report = Report("dep-budget")
    external = declared()
    if not external:
        return nothing_to_check(report, "no dependencies declared yet")

    known = documented()
    for dep, sites in sorted(external.items()):
        if dep not in known:
            report.fail(sites[0], f"`{dep}` has no row in docs/07-dependencies.md")

    tree = _cargo_tree()
    if tree is None:
        report.note("cargo tree unavailable — counts not checked")
        return report.finish()

    names = crate_names(tree)
    gpu = {n for n in names if n.startswith(GPU_ROOTS)}
    total, non_gpu = len(names), len(names - gpu)
    report.note(f"{total} transitive crates, {non_gpu} outside the GPU stack")
    if non_gpu > BUDGET_NON_GPU:
        report.fail("Cargo.lock", f"{non_gpu} non-GPU crates exceeds the budget of {BUDGET_NON_GPU}")
    if total > BUDGET_TOTAL:
        report.fail("Cargo.lock", f"{total} crates exceeds the budget of {BUDGET_TOTAL} — ADR 0003's revisit trigger fires")

    duplicates = _cargo_duplicates()
    if duplicates:
        report.fail("Cargo.lock", f"duplicate versions: {', '.join(duplicates)}")
    return report.finish()


def _cargo(*args: str) -> str | None:
    try:
        done = subprocess.run(("cargo", *args), cwd=ROOT, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout if done.returncode == 0 else None


def _cargo_tree() -> str | None:
    if not (ROOT / "Cargo.toml").exists():
        return None
    return _cargo("tree", "--edges", "normal", "--package", "skerry")


def _cargo_duplicates() -> list[str]:
    output = _cargo("tree", "--duplicates", "--edges", "normal") or ""
    return sorted(crate_names(output))


main(run)
