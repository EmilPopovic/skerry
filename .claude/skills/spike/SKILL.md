---
name: spike
description: Create or resolve a Phase 0 spike — a throwaway binary that answers one question with a number against a bar written in advance. Use this for the next piece of work on this repository, before any crate exists.
---

# /spike

A spike answers **one question that could invalidate the plan**, with a number,
against a bar written down *before* the measurement. Five are specified in
[08-roadmap.md](../../../docs/08-roadmap.md). If a spike misses its bar, the
architecture changes while there is still nothing to throw away — that is the
entire point, and the reason Phase 0 comes before any crate.

Spike code is throwaway. Spike *measurements* are permanent: they are the
evidence behind the ADRs, so they are recorded even when the spike is deleted.

## Creating one

```
spikes/<letter>-<slug>/
├── README.md      the question, the bar, the method — written first
├── Cargo.toml     standalone; not a workspace member
└── src/main.rs
```

`README.md` before `main.rs`. In order:

- **Question** — one sentence, answerable with a number.
- **Bar** — the go/no-go condition, from the roadmap, quoted exactly. Never
  written or adjusted after seeing a result.
- **Method** — machine, cache state, workload, sample count, what is warmed.
  Ask `perf-analyst` to review this before running anything; a method designed
  after the fact is how a spike lies.
- **Result** — filled in after. Numbers with variance, p50 and p99 for latencies.
- **Verdict** — go or no-go, and *what changes* if no-go.

Spikes are exempt from the layering, lint and dependency rules — they exist to
measure, including to measure alternatives we would never ship. They are not
exempt from the banned-toolkit rule, because a measurement of GTK tells us
nothing we are allowed to act on.

## Resolving one

1. Run it on the reference machine and, for frame measurements, on llvmpipe.
2. Record the result in the spike's own README.
3. Compare to the bar. State the verdict in one sentence. Do not soften a miss.
4. If it decides an ADR — Spike A decides [0003](../../../docs/adr/0003-wgpu-first-backend.md),
   Spike D decides [0002](../../../docs/adr/0002-own-platform-layer.md) — update
   that ADR's status from "Accepted, pending Spike X" to "Accepted", or write the
   superseding ADR. Use `/adr`.
5. Add the number to [09-performance-budgets.md](../../../docs/09-performance-budgets.md)
   where it becomes a permanent gate.

## Where the project actually is

**Spike A, and nothing else.** A million-row virtualized list with real shaped
text at a hard 8 ms p99, built twice — once on `wgpu` + `swash`, once on GPUI —
so the renderer comparison is measured rather than argued. Wayland and X11;
Intel, AMD, NVIDIA proprietary and llvmpipe; 100% and 150% scaling.

> **Bar:** p99 frame under 8 ms while scrolling · crisp glyphs at fractional
> scale · under 300 MB RSS · zero CPU at idle.

If the answer is yes, the plan is sound and the rest is ordinary engineering. If
no, the differentiator is unreachable on this stack and the stack changes. Either
way it costs days, and finding out later costs months.
