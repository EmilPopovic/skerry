#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Relative links and heading anchors across the markdown resolve.

The design documents are the product right now, and they cross-reference each
other heavily. A dead link is the cheapest possible way for the reasoning trail
to rot.
"""

from __future__ import annotations

import re
import unicodedata

from _common import ROOT, Report, main, rel

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
HEADING = re.compile(r"^#{1,6}\s+(.*?)\s*#*$", re.MULTILINE)
SKIP = ("http://", "https://", "mailto:", "#")


def slug(heading: str) -> str:
    """GitHub's heading slug: strip inline markup and punctuation, one hyphen per space."""
    text = heading.replace("`", "").replace("*", "")
    text = unicodedata.normalize("NFKD", text).lower().strip()
    text = re.sub(r"[^\w\s-]", "", text)
    return text.replace(" ", "-")


def anchors(path):
    return {slug(h) for h in HEADING.findall(path.read_text())}


def documents():
    return sorted(p for p in ROOT.rglob("*.md") if ".git" not in p.parts and "target" not in p.parts)


def run() -> int:
    report = Report("links")
    files = documents()
    anchor_cache = {}
    count = 0

    for source in files:
        fenced = False
        for number, line in enumerate(source.read_text().splitlines(), 1):
            if line.lstrip().startswith("```"):
                fenced = not fenced
            if fenced:
                continue
            for target in LINK.findall(line):
                if target.startswith(SKIP):
                    continue
                count += 1
                path, _, fragment = target.partition("#")
                resolved = (source.parent / path).resolve() if path else source
                if not resolved.exists():
                    report.fail(f"{rel(source)}:{number}", f"broken link → {target}")
                    continue
                if fragment and resolved.suffix == ".md":
                    if resolved not in anchor_cache:
                        anchor_cache[resolved] = anchors(resolved)
                    if fragment not in anchor_cache[resolved]:
                        report.fail(f"{rel(source)}:{number}", f"no such heading → {target}")

    report.note(f"{count} relative links across {len(files)} documents")
    return report.finish()


main(run)
