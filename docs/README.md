# Skerry design documents

## Reading order

1. **[01-research-file-pilot.md](01-research-file-pilot.md)** — the prior art, studied at beta v0.8.3 (August 2026).
2. **[02-positioning.md](02-positioning.md)** — why the Linux options don't feel like it, and the gap that leaves.
3. **[03-principles.md](03-principles.md)** — the ten rules. Everything downstream is derived from these.
4. **[04-architecture.md](04-architecture.md)** — how the code is organized so it stays that way.
5. **[05-platform-integration.md](05-platform-integration.md)** — the desktop integration surface.
6. **[06-portability.md](06-portability.md)** — distro-agnostic policy.
7. **[07-dependencies.md](07-dependencies.md)** — dependency policy.
8. **[08-roadmap.md](08-roadmap.md)** — spikes and milestones.
9. **[09-performance-budgets.md](09-performance-budgets.md)**, **[10-risks.md](10-risks.md)**, **[11-non-goals.md](11-non-goals.md)** — the guardrails.

## Conventions

- **Numbered documents are living.** They describe the current intent and are edited in place.
- **ADRs are immutable.** An [ADR](adr/) records one decision, its context, and its
  consequences at the moment it was made. Decisions are not edited; they are superseded by
  a later ADR that references them. `adr/0002` already supersedes an assumption made in
  the first draft of `04-architecture.md`, and that is the intended mechanism.
- **Every claim about performance is a number with a measurement method.** "Fast" is not a
  design document word. If it cannot be measured it belongs in `03-principles.md` as a
  rule with a failure condition, or nowhere.
- **Every dependency mentioned by name carries a justification** in
  [07-dependencies.md](07-dependencies.md), or it is not in the graph.
- **Estimates are ratios, not promises.** They assume one experienced developer working
  steadily and exist to order the work, not to schedule it.
