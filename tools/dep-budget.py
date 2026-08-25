#!/usr/bin/env python3
"""Rule 10: every dependency has a row in docs/07-dependencies.md, and the
transitive graph stays inside the budgets in docs/09-performance-budgets.md.

The manifest half runs anywhere. The graph half needs cargo and reports nothing
when it is absent, because a check that guesses is worse than a check that
abstains.

The GPU stack is budgeted separately from everything else, which makes "which
crates are the GPU stack" the load-bearing question in this file. It is answered
by reachability from the `wgpu` edge, never by matching names — see `_partition`.
"""

from __future__ import annotations

import json
import re
import subprocess

from _common import ROOT, Report, load, main, manifests, nothing_to_check, rel

POLICY = ROOT / "docs" / "07-dependencies.md"

# From the budget table in docs/09-performance-budgets.md.
BUDGET_NON_GPU = 25
BUDGET_TOTAL = 120

# The crate whose graph is the shipping binary's graph.
PRODUCT = "skerry"

# Direct dependencies that are the GPU stack. Only the entry points belong here:
# what the stack drags in is discovered from the graph, not guessed from names.
GPU_ENTRY_POINTS = ("wgpu", "naga")


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


def run() -> int:
    report = Report("dep-budget")
    external = declared()
    if not external:
        return nothing_to_check(report, "no dependencies declared yet")

    known = documented()
    for dep, sites in sorted(external.items()):
        if dep not in known:
            report.fail(sites[0], f"`{dep}` has no row in docs/07-dependencies.md")

    split = _partition()
    if split is None:
        report.note("cargo metadata unavailable — counts not checked")
        return report.finish()

    everything, non_gpu = split
    total, outside = len(everything), len(non_gpu)
    report.note(
        f"{total} transitive crates, {outside} outside the GPU stack "
        f"({total - outside} reachable only through it)"
    )
    if outside > BUDGET_NON_GPU:
        report.fail("Cargo.lock", f"{outside} non-GPU crates exceeds the budget of {BUDGET_NON_GPU}")
    if total > BUDGET_TOTAL:
        report.fail("Cargo.lock", f"{total} crates exceeds the budget of {BUDGET_TOTAL} — ADR 0003's revisit trigger fires")

    ours, theirs = _duplicates(non_gpu)
    if ours:
        report.fail("Cargo.lock", f"duplicate versions: {', '.join(ours)}")
    if theirs:
        # Inside the GPU stack we do not choose the versions, and the count is
        # already charged against the separate GPU budget above.
        report.note(f"duplicate versions inside the GPU stack, not ours to resolve: {', '.join(theirs)}")
    return report.finish()


def _partition() -> tuple[set[str], set[str]] | None:
    """All crate names in the product's graph, and those outside the GPU stack.

    The GPU stack is the set reachable *only* through a GPU entry point, not the
    set whose names begin with one. Measured on Spike A's tree, the name-prefix
    version left `ash`, `glow`, `gpu-alloc`, `gpu-descriptor`, `khronos-egl`,
    `spirv`, `renderdoc-sys`, `codespan-reporting`, `hexf-parse` and about fifteen
    more on the wrong side of the line — 6 crates counted as GPU where the honest
    figure is around 50, which would have failed the 25-crate non-GPU budget on
    the first commit that added `wgpu`.

    A crate reachable from both sides counts as non-GPU, because it is one we
    would carry anyway. That direction is deliberate: it stops the GPU stack from
    concealing our own dependencies.
    """
    meta = _metadata()
    if meta is None:
        return None

    names = {p["id"]: p["name"] for p in meta.get("packages", [])}
    nodes = {n["id"]: n for n in meta.get("resolve", {}).get("nodes", [])}
    root = next((i for i, name in names.items() if name == PRODUCT), None)
    if root is None or root not in nodes:
        return None

    edges = {i: _normal_deps(node) for i, node in nodes.items()}
    direct = edges.get(root, [])
    outside_entries = [d for d in direct if not names.get(d, "").startswith(GPU_ENTRY_POINTS)]

    everything = _reachable(direct, edges)
    non_gpu = _reachable(outside_entries, edges)
    return ({names[i] for i in everything}, {names[i] for i in non_gpu})


def _normal_deps(node: dict) -> list[str]:
    """Normal dependencies only — dev and build edges are not shipped."""
    out = []
    for dep in node.get("deps", []):
        kinds = dep.get("dep_kinds") or [{"kind": None}]
        if any(kind.get("kind") is None for kind in kinds):
            out.append(dep["pkg"])
    return out


def _reachable(seeds: list[str], edges: dict[str, list[str]]) -> set[str]:
    seen: set[str] = set()
    stack = list(seeds)
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        stack.extend(edges.get(current, []))
    return seen


def _duplicates(non_gpu: set[str]) -> tuple[list[str], list[str]]:
    """Crates resolved at more than one version, split into ours and the GPU
    stack's.

    `cargo tree --duplicates` prints a reverse-dependency tree beneath each
    duplicated crate, so only the unindented lines name a duplicate — everything
    under them is a path back to the root. Reading the whole block reported
    `skerry` itself as duplicated.
    """
    args = ["tree", "--duplicates", "--edges", "normal"]
    triple = _host_triple()
    if triple:
        args += ["--target", triple]
    output = _cargo(*args) or ""

    found = set()
    for line in output.splitlines():
        match = re.match(r"^([a-zA-Z0-9_-]+) v\d", line)
        if match:
            found.add(match.group(1))
    ours = sorted(n for n in found if n in non_gpu)
    theirs = sorted(n for n in found if n not in non_gpu)
    return ours, theirs


def _metadata() -> dict | None:
    if not (ROOT / "Cargo.toml").exists():
        return None
    args = ["metadata", "--format-version", "1"]
    # Without a platform filter the graph carries every target's dependencies,
    # which counts Windows and macOS crates against a Linux budget.
    triple = _host_triple()
    if triple:
        args += ["--filter-platform", triple]
    raw = _cargo(*args)
    if raw is None:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return None


def _host_triple() -> str | None:
    try:
        done = subprocess.run(("rustc", "-vV"), capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    if done.returncode != 0:
        return None
    for line in done.stdout.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    return None


def _cargo(*args: str) -> str | None:
    try:
        done = subprocess.run(("cargo", *args), cwd=ROOT, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout if done.returncode == 0 else None


main(run)
