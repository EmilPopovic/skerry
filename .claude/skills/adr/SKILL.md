---
name: adr
description: Write the next architecture decision record and update the index. Use before implementing anything expensive to reverse — a dependency, a seam, a wire protocol, a persistence format, a threading decision.
---

# /adr

ADRs record the context and reasoning at the moment a choice was made. **They are
immutable.** A decision that changes is not edited: a new ADR supersedes it and
links back, so the reasoning trail survives. `tools/check-invariants.py` fails
the build on a modified committed ADR.

## Write one when

The decision is expensive to reverse. Concretely: a tier 0 or tier 2 dependency,
a crate seam, a threading or scheduling decision, a wire protocol, a persistence
format, or anything that moves a row in the budget table.

Not for: choosing a data structure inside one function, naming, or anything a
future contributor could change in an afternoon.

## Steps

1. `ls docs/adr/` — the next number is the highest plus one, four digits.
2. Filename: `NNNN-short-kebab-summary.md`. The summary is the decision, not the
   topic: `0005-no-async-runtime.md`, not `0005-threading.md`.
3. Write it in the format below.
4. Add the row to the table in `docs/adr/README.md`.
5. `./tools/check`.

## Format

```markdown
# NNNN — <the decision, as a sentence fragment>

**Status:** Accepted
<!-- optionally: **Supersedes:** [NNNN](NNNN-....md) -->

## Context

What forced the decision. The constraints that were live at the time, the
alternatives that were genuinely considered, and what each would have cost.
Written so that someone in two years understands the pressure without having
been there. Cite the rules and documents; the reasoning is the value.

## Decision

What was chosen, precisely enough that a reader can tell whether a given piece
of code honours it. Name the crate, the trait, the boundary.

## Consequences

What this costs, including the parts that are unpleasant. An ADR that lists only
benefits is a sales document and will not be trusted later. Say what work this
creates, what it forecloses, and what now has to be tested deliberately.

## Revisit trigger

The condition under which this should be reconsidered — as a number wherever a
number is possible, so that revisiting is a lookup rather than a debate.
```

## Voice

Match the existing records. Declarative, specific, numbers over adjectives, one
idea per paragraph. The strongest existing ADRs — [0002](../../../docs/adr/0002-own-platform-layer.md)
and [0005](../../../docs/adr/0005-no-async-runtime.md) — argue against their own
conclusion first and then say why it lost. Do that.

A revisit trigger of "if it turns out to be a problem" is not a trigger. Every
accepted ADR in this repository has a falsifiable one; match that bar.
