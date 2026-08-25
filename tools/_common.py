"""Shared helpers for the repository checks. Standard library only, by policy."""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SUPPRESS = re.compile(r"skerry-allow:\s*([a-z0-9-]+)")


@dataclass
class Report:
    """Collects findings for one check and renders them the same way every time."""

    name: str
    failures: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    def fail(self, where: str, message: str) -> None:
        self.failures.append(f"{where}: {message}")

    def note(self, message: str) -> None:
        self.notes.append(message)

    def finish(self) -> int:
        for note in self.notes:
            print(f"  {note}")
        for failure in self.failures:
            print(f"  FAIL {failure}")
        verdict = "FAIL" if self.failures else "ok"
        print(f"{verdict:>4}  {self.name}" + (f" ({len(self.failures)} violations)" if self.failures else ""))
        return 1 if self.failures else 0


def manifests(*, spikes: bool = True) -> list[Path]:
    """Every crate manifest, whether or not a workspace root exists yet.

    Spikes are throwaway measurement binaries and are exempt from the layering
    and dependency rules (spikes/README.md, docs/08-roadmap.md) — they exist to
    measure alternatives we would never ship, including their dependency trees.
    Pass spikes=False from a check that honours that exemption. The
    banned-toolkit rule is never exempt, so check-invariants keeps them.
    """
    bases = ("crates", "bin", "tools", "spikes") if spikes else ("crates", "bin", "tools")
    return sorted(p for base in bases for p in (ROOT / base).glob("*/Cargo.toml"))


def load(manifest: Path) -> dict:
    with manifest.open("rb") as handle:
        return tomllib.load(handle)


def rust_sources(*bases: str) -> list[Path]:
    return sorted(p for base in bases for p in (ROOT / base).rglob("*.rs") if "target" not in p.parts)


def suppressed(line: str, rule: str) -> bool:
    match = SUPPRESS.search(line)
    return bool(match and match.group(1) == rule)


def rel(path: Path) -> str:
    return str(path.relative_to(ROOT))


def git(*args: str) -> str | None:
    """Run a git command, or return None where git cannot answer (shallow CI, no repo)."""
    try:
        done = subprocess.run(("git", *args), cwd=ROOT, capture_output=True, text=True, timeout=15)
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout if done.returncode == 0 else None


def nothing_to_check(report: Report, reason: str) -> int:
    report.note(f"nothing to check — {reason}")
    return report.finish()


def main(run) -> None:
    sys.exit(run())
