#!/bin/sh
# Runs the gate that the edited file can actually break, and nothing else.
# Silence is a pass; only violations reach the transcript.
set -u
root="$(cd "$(dirname "$0")/../.." && pwd)"
file="${1:-}"

case "$file" in
    *Cargo.toml)
        python3 "$root/tools/check-layering.py" | grep -E '^\s*FAIL' && exit 2
        python3 "$root/tools/dep-budget.py" | grep -E '^\s*FAIL' && exit 2
        ;;
    *.rs)
        python3 "$root/tools/check-invariants.py" | grep -E '^\s*FAIL' && exit 2
        [ -f "$root/Cargo.toml" ] && cargo fmt -- "$file" 2>/dev/null
        ;;
    *.md)
        python3 "$root/tools/check-links.py" | grep -E '^\s*FAIL' && exit 2
        ;;
esac
exit 0
