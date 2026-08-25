#!/usr/bin/env python3
"""Dependency edges between workspace crates must point strictly downward.

Each crate declares its level in its own manifest, so this check and the level
diagram in docs/04-architecture.md are both derived from the manifests rather
than from each other.

    [package.metadata.skerry]
    level = 1

Run with --diagram to print the level diagram.
"""

from __future__ import annotations

import sys

from _common import Report, load, main, manifests, nothing_to_check, rel

LEVEL_NAMES = {
    0: "core types and primitives",
    1: "the hostile outside world",
    2: "policy over primitives",
    3: "pixels and input",
    4: "product behaviour",
    5: "binaries",
}


def crates() -> dict[str, dict]:
    found = {}
    for manifest in manifests(spikes=False):
        parsed = load(manifest)
        package = parsed.get("package", {})
        name = package.get("name")
        if not name:
            continue
        found[name] = {
            "manifest": manifest,
            "level": package.get("metadata", {}).get("skerry", {}).get("level"),
            "deps": sorted(set(parsed.get("dependencies", {}))),
        }
    return found


def diagram(found: dict[str, dict]) -> str:
    by_level: dict[int, list[str]] = {}
    for name, crate in sorted(found.items()):
        if crate["level"] is not None:
            by_level.setdefault(crate["level"], []).append(name)
    lines = []
    for level in sorted(by_level, reverse=True):
        members = "   ".join(by_level[level])
        lines.append(f"level {level}   {members:<52}{LEVEL_NAMES.get(level, '')}")
        if level:
            lines.append(" " * 16 + "│")
    return "\n".join(lines)


def run() -> int:
    report = Report("layering")
    found = crates()
    if not found:
        return nothing_to_check(report, "no workspace members yet")

    if "--diagram" in sys.argv:
        print(diagram(found))
        return 0

    for name, crate in sorted(found.items()):
        if crate["level"] is None:
            report.fail(rel(crate["manifest"]), "missing [package.metadata.skerry] level")
            continue
        for dep in crate["deps"]:
            other = found.get(dep)
            if other is None or other["level"] is None:
                continue
            if other["level"] >= crate["level"]:
                direction = "sideways" if other["level"] == crate["level"] else "upward"
                report.fail(
                    rel(crate["manifest"]),
                    f"{direction} edge {name} (level {crate['level']}) → {dep} (level {other['level']})",
                )

    if not report.failures:
        report.note(f"{len(found)} crates, all edges downward")
    return report.finish()


main(run)
