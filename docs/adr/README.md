# Architecture decision records

One decision per file. **ADRs are immutable**: they record the context and reasoning at the
moment a choice was made. A decision that changes is not edited — a new ADR supersedes it
and links back, so the reasoning trail survives.

Format: Context · Decision · Consequences · Revisit trigger · Status.

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-custom-gpu-ui.md) | A custom GPU UI layer instead of GTK or Qt | Accepted |
| [0002](0002-own-platform-layer.md) | Own the Wayland/X11 platform layer instead of using `winit` | Accepted, pending Spike D |
| [0003](0003-wgpu-first-backend.md) | `wgpu` as the first GPU backend, behind a `Renderer` seam | Accepted |
| [0004](0004-swash-for-text.md) | `swash` for shaping and rasterization; our own line layout | Accepted |
| [0005](0005-no-async-runtime.md) | One owned job pool; no async runtime in the core | Accepted |
| [0006](0006-out-of-process-decoders.md) | Decoders in sandboxed helper processes | Accepted |
| [0007](0007-runtime-capability-detection.md) | Runtime capability probing, never build-time features | Accepted |
