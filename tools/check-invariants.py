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
        text = source.read_text()
        lines = text.splitlines()
        if source.name not in REPORTS:
            for match in PRINT.finditer(text):
                # The format string can be on the line after the macro, so the
                # match is found in the whole file and the line worked out from
                # where it started.
                number = text.count("\n", 0, match.start()) + 1
                if suppressed(lines[number - 1], "log-prefix"):
                    continue
                report.fail(
                    f"{rel(source)}:{number}",
                    "a print outside a report writes `skerry`, so the tools that read "
                    "this program's output can tell a log line from a debug line",
                )
        for number, line in enumerate(lines, 1):
            for rule, pattern, message in SOURCE_RULES:
                if pattern.search(line) and not suppressed(line, rule):
                    report.fail(f"{rel(source)}:{number}", message)
            # **A comment naming `wgpu` is not a leak.** ADR 0003's rule is about
            # *types* crossing the seam, and the first thing above `sk-gpu` to
            # mention the crate at all was a doc comment saying it does not use it
            # — which this failed, correctly by the letter and wrongly by the
            # point. Code only, so the rule matches what the ADR is about.
            code = line.split("//")[0]
            if "wgpu" in code and "sk-gpu" not in source.parts and not suppressed(line, "gpu-seam"):
                report.fail(f"{rel(source)}:{number}", "wgpu must not leak above sk-gpu — ADR 0003")
            # Which display server this is must not reach the code above the
            # platform layer. `sk-gpu` is the one exception: it converts the raw
            # handles, and a Wayland surface and an X11 window are different
            # shapes. Everywhere else, a backend named above `sk-plat` is a
            # behaviour that differs between two servers without a test that
            # would notice — ADR 0009.
            if (
                BACKEND_NAME.search(line)
                and "sk-plat" not in source.parts
                and "sk-gpu" not in source.parts
                and not suppressed(line, "backend-seam")
            ):
                report.fail(
                    f"{rel(source)}:{number}",
                    "no crate above sk-plat may name a display server — ADR 0009",
                )


# A print that is not a line of the program's own log. Every diagnostic this
# program writes is prefixed `skerry`, so the tools that read its output can tell
# them apart — and a print that is not one is a debug line somebody left in. One
# survived nineteen commits and fired on every scroll event.
PRINT = re.compile(r'\bdbg!\(|\be?println!\(\s*"(?!skerry)')

# The three files whose *output* is the point: a benchmark's table, a capture's
# summary, and the state a scenario reads.
REPORTS = {"bench.rs", "headless.rs"}

# The names of the two backends, as they appear in code rather than in prose: a
# path, a variant, or the crate itself. Deliberately not the bare words — a
# comment explaining why something is the way it is on Wayland is documentation,
# and this is about code that branches on it.
BACKEND_NAME = re.compile(
    r"\b(?:sk_plat::(?:wayland|x11)|Backend::(?:Wayland|X11)|RawHandles::(?:Wayland|X11)"
    r"|wayland_client|x11rb)\b"
)

# Every string a person can see goes through the catalogue, and a key that is not
# in it resolves to itself — so a typo ships as `notice.trashed` on screen.
CATALOGUE = ROOT / "crates/sk-model/catalogue/en.txt"
# **`plural` and `tp!` are here too, and the key may be on the next line.** A
# plural key has no bare line in the catalogue — ADR 0016 — so asking for one
# through `format` renders `properties.items` as a title, which is exactly what
# this gate exists to stop. And `\s*` now crosses newlines, because the call it
# missed was `i18n::format(\n    "properties.items",` — a line-by-line scan sees
# the call on one line and the key on another and matches neither.
STRING_KEY = re.compile(
    r'(?:i18n::)?(lookup|format|plural|t!|tp!)'
    r'\(\s*"([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)"',
    re.DOTALL,
)

# Which callers want a plural key and which want a bare one.
#
# **Both directions are a defect.** A plural key has no bare line — ADR 0016 — so
# `format("properties.items", …)` draws the key as a title; and a bare key asked
# for through `plural` falls back to itself, which is the same thing one call away.
PLURAL_CALLERS = {"plural", "tp!"}


def catalogue_check(report: Report) -> None:
    """Every catalogue key a source file asks for exists.

    A missing key is not an error at run time: `lookup` returns the key, so the
    window shows `notice.trashed` where it meant to show a sentence. Command
    names are covered by `every_command_is_reachable_and_named` instead, which
    asserts a name did not resolve to its own id.
    """
    keys = set()
    for number, line in enumerate(CATALOGUE.read_text().splitlines(), start=1):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key = line.split("=", 1)[0].strip()
        # **A key twice is a key a translator maintains twice**, and the loser is
        # silent: the loader keeps one and nothing says which. Both of the ones
        # this caught were a scripted edit that appended a block it had already
        # appended, which is invisible in a diff that is read as an addition.
        if key in keys:
            report.fail(f"{rel(CATALOGUE)}:{number}", f"`{key}` is in the catalogue twice")
        keys.add(key)
    for source in rust_sources("crates", "bin"):
        text = source.read_text()
        lines = text.splitlines()
        for found in STRING_KEY.finditer(text):
            caller, key = found.group(1), found.group(2)
            plural = caller in PLURAL_CALLERS
            wanted = f"{key}[other]" if plural else key
            if wanted in keys:
                continue
            # The line the *key* is on, which is where a reader will look for it
            # and where a suppression has to be written.
            number = text.count("\n", 0, found.start(2)) + 1
            if suppressed(lines[number - 1], "missing-string"):
                continue
            because = (
                f"`{key}` has no `[other]` form, so `{caller}` would draw the key"
                if plural
                else f"`{key}` is not in the catalogue, so it would show as itself"
            )
            report.fail(f"{rel(source)}:{number}", because)


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

    **A second shape, and this one is exact.** An attribute followed immediately by
    a doc comment — `#[derive(Default)]` and then `/// Something else entirely` —
    is an item's `derive` left stranded above the comment of the item that was
    inserted under it. Rust accepts the ordering, which is why nothing else
    complains; this project never writes it, and a sweep of `crates` and `bin` when
    this was added found zero. The two hits in `spikes/` are a `#[cfg]` above a
    doc comment, and spikes are not scanned.

    **What neither catches** is a comment spliced onto the *end* of a paragraph
    rather than after a summary — the block then reads as one comment with an odd
    last paragraph, and the item below is left with none. The detectable half of
    that is the orphan, and the lint for it,
    `clippy::missing_docs_in_private_items`, reports 300 items across the
    workspace today. See [12-measurement.md](../docs/12-measurement.md).
    """
    for source in rust_sources("crates", "bin"):
        lines = source.read_text().splitlines()
        for number, (here, following) in enumerate(zip(lines, lines[1:]), start=1):
            if (
                here.strip().startswith("#[")
                and following.strip().startswith("///")
                and not suppressed(here, "attribute-then-doc")
            ):
                report.fail(
                    f"{rel(source)}:{number}",
                    "an attribute above a doc comment — the item it belongs to is "
                    "below the comment, which documents something else",
                )
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
            "banned crates, capability flags, crate lints, GPU and backend seams, log "
            "prefixes, doc comments, "
            "catalogue keys, ADR immutability"
        )
    return report.finish()


main(run)
