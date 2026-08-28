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
#
# The non-GPU figure is a **ratchet at the measured floor**, not a guess. 25 was
# written before any crate existed and is two below what the four tier-0
# dependencies plus the two Wayland protocol crates actually cost; the derivation
# is in docs/09-performance-budgets.md. Raising it requires a row in
# docs/07-dependencies.md and a re-measure in the same commit, which is the only
# thing that makes the number a gate rather than a preference.
#
# 27 to 32 is the second display server, priced in ADR 0009: `x11rb`,
# `x11rb-protocol`, `as-raw-xcb-connection`, `gethostname` and `xcursor`.
BUDGET_NON_GPU = 32
BUDGET_TOTAL = 120

# The crate whose graph is the shipping binary's graph.
PRODUCT = "skerry"

# The crates that are the GPU stack's entry points. What the stack drags in is
# discovered from the graph, not guessed from names.
GPU_ENTRY_POINTS = ("wgpu", "naga")


def documented() -> set[str]:
    if not POLICY.exists():
        return set()
    return set(re.findall(r"`([a-z0-9][a-z0-9_-]*)`", POLICY.read_text()))


def internal_names() -> set[str]:
    """Every workspace member. These are not dependencies and are not budgeted."""
    found = set()
    for manifest in manifests(spikes=False):
        name = load(manifest).get("package", {}).get("name")
        if name:
            found.add(name)
    return found


def declared() -> dict[str, list[str]]:
    """External dependency name → the manifests that declare it."""
    internal = internal_names()
    raw: dict[str, list[str]] = {}
    for manifest in manifests(spikes=False):
        parsed = load(manifest)
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

    everything, non_gpu, internal = split
    total, outside = len(everything), len(non_gpu)
    report.note(
        f"{total} external transitive crates, {outside} outside the GPU stack "
        f"({total - outside} reachable only through it); "
        f"{internal} workspace crates, which are not dependencies"
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


def _partition() -> tuple[set[str], set[str], int] | None:
    """External crates in the product's graph, those outside the GPU stack, and
    the number of workspace crates.

    Three things this gets right that the obvious version does not, each learned
    by the obvious version being wrong:

    1. **The GPU stack is found by reachability, not by name.** Measured on Spike
       A's tree, the name-prefix version left `ash`, `glow`, `gpu-alloc`,
       `gpu-descriptor`, `khronos-egl`, `spirv`, `renderdoc-sys`,
       `codespan-reporting`, `hexf-parse` and about fifteen more on the wrong side
       of the line — 6 crates counted as GPU where the honest figure is around 50.
    2. **An entry point is looked for anywhere in the graph, not only among the
       product's direct dependencies.** `skerry` depends on `sk-app`, which
       depends on `sk-gpu`, which depends on `wgpu`. Filtering the *root's* direct
       edges therefore excluded nothing at all, and the first run against a real
       workspace reported 86 non-GPU crates against a budget of 25.
    3. **Workspace members are not dependencies.** Rule 10 budgets what we take
       on from outside. Counting our own twelve crates against it would mean the
       crate table in docs/04-architecture.md consumes half the budget before a
       single external crate is chosen.

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

    compiler_only = _proc_macros(meta)
    edges = {
        i: [d for d in _normal_deps(node) if d not in compiler_only]
        for i, node in nodes.items()
    }
    internal = internal_names()

    everything = _reachable(edges.get(root, []), edges)
    # The same walk with every edge *into* a GPU entry point cut. What is left is
    # what we would carry with no renderer at all.
    trimmed = {
        source: [d for d in targets if not names.get(d, "").startswith(GPU_ENTRY_POINTS)]
        for source, targets in edges.items()
    }
    non_gpu = _reachable(trimmed.get(root, []), trimmed)

    external = {names[i] for i in everything} - internal
    return (external, {names[i] for i in non_gpu} - internal, len(internal))


def _proc_macros(meta: dict) -> set[str]:
    """Packages that run in the compiler rather than in the binary.

    Excluded for the same reason dev and build edges are: they are not in the
    shipped graph. Measured here, that is `bytemuck_derive` and the `syn`,
    `quote`, `proc-macro2`, `unicode-ident` chain beneath it — five crates, zero
    bytes of `skerry`, and the source of a `syn` duplicate that is neither ours
    to resolve nor present at run time.
    """
    found = set()
    for package in meta.get("packages", []):
        kinds = {kind for target in package.get("targets", []) for kind in target.get("kind", [])}
        if "proc-macro" in kinds:
            found.add(package["id"])
    return found


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
