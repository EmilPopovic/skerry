#!/usr/bin/env python3
"""The rules from docs/03-principles.md and the ADRs that a grep can decide.

Everything here is a rule someone already wrote down as "fails if". A line may
opt out with a reason:

    let atlas = self.atlas.borrow_mut();  // skerry-allow: no-unwrap — invariant: …

Spikes are throwaway measurement binaries (docs/08-roadmap.md) and are exempt
from everything except the banned-toolkit rule, which is never negotiable.
"""

from __future__ import annotations

import re

from _common import ROOT, Report, git, load, main, manifests, rel, rust_sources, suppressed

# Rule 9 / docs/06-portability.md: adopting any of these adopts its main loop.
BANNED_CRATES = {
    "glib": "rule 9", "glib-sys": "rule 9", "gtk": "rule 9", "gtk4": "rule 9",
    "gio": "rule 9", "gdk": "rule 9", "gdk4": "rule 9", "qt": "rule 9",
    "qttypes": "rule 9", "cpp": "rule 9", "gstreamer": "rule 9",
    "dbus": "rule 9 — speak D-Bus as a protocol, not through libdbus",
    # ADR 0005: one job pool for the process.
    "tokio": "ADR 0005", "async-std": "ADR 0005", "smol": "ADR 0005",
    "rayon": "ADR 0005", "threadpool": "ADR 0005",
    # docs/04-architecture.md, error policy.
    "anyhow": "no anyhow below level 4",
}

# Capabilities are probed, never compiled (ADR 0007).
PROBED = ("systemd", "udisks", "portal", "vulkan", "wayland", "x11", "landlock", "seccomp")

REQUIRED_LINTS = ("clippy::unwrap_used", "clippy::expect_used")

SOURCE_RULES = (
    ("no-read-dir", re.compile(r"\bstd::fs::read_dir\b"),
     "use getdents64 through sk-fs — read_dir allocates per entry and stats implicitly"),
    ("no-blocking-ui", re.compile(r"\bstd::thread::sleep\b"),
     "rule 1 — the UI thread sleeps on the event queue, never on a timer"),
)


def banned_crate_check(report: Report) -> None:
    for manifest in manifests():
        if manifest.parts[-3] == "tools":
            continue
        parsed = load(manifest)
        level = parsed.get("package", {}).get("metadata", {}).get("skerry", {}).get("level")
        for table in ("dependencies", "build-dependencies"):
            for dep in parsed.get(table, {}):
                reason = BANNED_CRATES.get(dep)
                if reason is None:
                    continue
                if dep == "anyhow" and level is not None and level >= 4:
                    continue
                report.fail(rel(manifest), f"`{dep}` is banned — {reason}")


def feature_flag_check(report: Report) -> None:
    for manifest in manifests():
        for feature in load(manifest).get("features", {}):
            if any(name in feature.lower() for name in PROBED):
                report.fail(rel(manifest), f"feature `{feature}` should be a runtime probe — ADR 0007")


def lint_header_check(report: Report) -> None:
    for entry in sorted((ROOT / "crates").glob("*/src/lib.rs")):
        text = entry.read_text()
        missing = [lint for lint in REQUIRED_LINTS if lint not in text]
        if missing:
            report.fail(rel(entry), f"missing crate lints: {', '.join(missing)}")


def source_check(report: Report) -> None:
    for source in rust_sources("crates", "bin"):
        for number, line in enumerate(source.read_text().splitlines(), 1):
            for rule, pattern, message in SOURCE_RULES:
                if pattern.search(line) and not suppressed(line, rule):
                    report.fail(f"{rel(source)}:{number}", message)
            if "wgpu" in line and "sk-gpu" not in source.parts and not suppressed(line, "gpu-seam"):
                report.fail(f"{rel(source)}:{number}", "wgpu must not leak above sk-gpu — ADR 0003")


# Every string a person can see goes through the catalogue, and a key that is not
# in it resolves to itself — so a typo ships as `notice.trashed` on screen.
CATALOGUE = ROOT / "crates/sk-model/catalogue/en.txt"
STRING_KEY = re.compile(
    r'(?:i18n::lookup|i18n::format|\blookup|\bformat|t!)\(\s*"([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)"'
)


def catalogue_check(report: Report) -> None:
    """Every catalogue key a source file asks for exists.

    A missing key is not an error at run time: `lookup` returns the key, so the
    window shows `notice.trashed` where it meant to show a sentence. Command
    names are covered by `every_command_is_reachable_and_named` instead, which
    asserts a name did not resolve to its own id.
    """
    keys = set()
    for line in CATALOGUE.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            keys.add(line.split("=", 1)[0].strip())
    for source in rust_sources("crates", "bin"):
        for number, line in enumerate(source.read_text().splitlines(), 1):
            if suppressed(line, "missing-string"):
                continue
            for key in STRING_KEY.findall(line):
                if key not in keys:
                    report.fail(
                        f"{rel(source)}:{number}",
                        f"`{key}` is not in the catalogue, so it would show as itself",
                    )


def spliced_comment_check(report: Report) -> None:
    """A doc comment that has been left on the wrong item.

    Inserting a function above an existing one, with no blank line between the
    two comments, gives the new item the old one's comment and leaves the old one
    with none. It has happened six times in this repository, twice unnoticed for
    a commit, and `missing_docs` only catches the cases where the orphaned item
    is public.

    The shape is precise: this project's doc comments are a one-line summary, a
    blank `///`, then prose — so a summary line followed immediately by the start
    of another sentence is two comments that have been run together. Measured
    against the whole tree when this was written: two hits, both real, no false
    positives.
    """
    for source in rust_sources("crates", "bin"):
        lines = source.read_text().splitlines()
        index = 0
        while index < len(lines):
            if not lines[index].strip().startswith("///"):
                index += 1
                continue
            start = index
            while index < len(lines) and lines[index].strip().startswith("///"):
                index += 1
            run = [line.strip().removeprefix("///").strip() for line in lines[start:index]]
            if len(run) < 2 or not run[0].endswith(".") or not run[1]:
                continue
            words = run[1].lstrip("*`[").split()
            if words and words[0][:1].isupper() and not suppressed(lines[start + 1], "two-summaries"):
                report.fail(
                    f"{rel(source)}:{start + 1}",
                    "a summary line followed by another sentence — two doc comments run "
                    "together, and the item below the first one has none",
                )


def adr_immutability_check(report: Report) -> None:
    """ADRs record what was decided when. A changed decision is a new ADR."""
    changed = git("diff", "--name-only", "HEAD", "--", "docs/adr")
    if changed is None:
        return
    committed = git("ls-tree", "-r", "--name-only", "HEAD", "docs/adr") or ""
    tracked = set(committed.split())
    for path in changed.split():
        if path in tracked and re.search(r"/\d{4}-", path):
            report.fail(path, "ADRs are immutable — supersede it with a new ADR instead")


def run() -> int:
    report = Report("invariants")
    banned_crate_check(report)
    feature_flag_check(report)
    lint_header_check(report)
    source_check(report)
    spliced_comment_check(report)
    catalogue_check(report)
    adr_immutability_check(report)
    if not report.failures:
        report.note(
            "banned crates, capability flags, crate lints, GPU seam, doc comments, "
            "catalogue keys, ADR immutability"
        )
    return report.finish()


main(run)
