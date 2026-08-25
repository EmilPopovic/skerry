---
name: check
description: Run every repository gate — links, layering, dependency budget, invariants, and the Rust toolchain checks once a workspace exists — and explain what failed and why it is a rule.
---

# /check

```
./tools/check
```

Each gate abstains rather than guesses. "nothing to check — no workspace members
yet" is a pass, not a skip to apologize for.

## Reporting a failure

Do not paste the output back. For each failure, give three lines: **what the
rule is, why it exists, and the smallest change that satisfies it.** The rule
always has a home — cite it.

| Gate | The rule behind it |
| --- | --- |
| `links` | The design documents are the product right now; a dead cross-reference rots the reasoning trail |
| `layering` | [04-architecture.md](../../../docs/04-architecture.md#layering-is-enforced-not-encouraged) — without it, level 4 grows a dependency on `sk-gpu` in month four |
| `dep-budget` | Rule 10 — a dependency without a row in [07](../../../docs/07-dependencies.md) is a review blocker |
| `invariants` | Rules 9 and 10, [ADR 0003](../../../docs/adr/0003-wgpu-first-backend.md), [0005](../../../docs/adr/0005-no-async-runtime.md), [0007](../../../docs/adr/0007-runtime-capability-detection.md) |
| `clippy` | `unwrap` and `expect` are denied outside tests and named invariants |

## Fixing, in order of preference

1. **Change the code.** Almost always the right answer.
2. **Change the document**, when the code is right and the document has drifted.
   Numbered documents are living. Say what drifted.
3. **Suppress the line**, with the invariant named on the same line:
   `// skerry-allow: no-unwrap — set in new(), never cleared`. A suppression
   without a stated invariant is itself a finding.
4. **Move the rule.** Only through an ADR, and never in the same commit as the
   change that the rule blocked.

Never satisfy a gate by weakening it. A budget that moves whenever it is
inconvenient is a comment, not a gate.
