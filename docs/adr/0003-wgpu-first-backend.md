# 0003 — `wgpu` as the first GPU backend, behind a `Renderer` seam

**Status:** Accepted — Spike A resolved, see [its result](../../spikes/a-million-rows/README.md#result)

## Context

Rule 10 wants a small dependency graph, and `wgpu` is the heaviest single entry we are
contemplating — it alone dominates the transitive crate count. The alternative is `ash`
plus hand-written Vulkan, with a GL fallback: perhaps three transitive crates instead of
several dozen, at the cost of writing swapchain management, memory allocation, present-mode
selection and per-vendor driver workarounds ourselves.

The workload is genuinely simple — instanced quads and two texture atlases. That argues for
`ash`. What argues against it is that the *hard* part of a GPU backend on Linux is not the
drawing, it is the matrix of drivers, present modes and llvmpipe behaviour, and `wgpu`
embodies years of that knowledge.

## Decision

Ship 1.0 on `wgpu`, Vulkan where available and GL as the fallback, behind a `Renderer` trait
in `sk-gpu` narrow enough that an `ash` backend is a contained swap rather than a rewrite.
The trait handles: surface configuration, atlas upload, instanced batch submission, damage
presentation, timestamp queries.

## Consequences

- The dependency budget carries a second, larger number for the GPU stack, stated openly in
  [09-performance-budgets.md](../09-performance-budgets.md), rather than pretending the
  25-crate figure covers everything.
- Binary size grows by several megabytes. The 20 MB budget accounts for it.
- We inherit `wgpu`'s driver workarounds, which is the entire point.
- The seam must be honoured from the first commit. A `wgpu` type leaking above `sk-gpu` is a
  review blocker, because it silently converts a swap into a rewrite.

## Revisit trigger

Any of: the total transitive count exceeds 120; the stripped binary exceeds 40 MB; a
driver-specific bug in `wgpu` blocks a budget in
[09-performance-budgets.md](../09-performance-budgets.md) for more than one release cycle;
or Spike A shows the `ash` path measurably ahead on frame time at equal effort.
