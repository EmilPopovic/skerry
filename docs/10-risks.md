# Risks

Ordered by expected cost. Each carries a mitigation that is scheduled, not hoped for.

| Risk | Severity | Mitigation |
| --- | --- | --- |
| **Owning the Wayland/X11 platform layer is a large, unglamorous surface** — six weeks before anything is visible, and a long tail of compositor quirks | High | Spike D de-risks the hardest parts first (DnD source, IME, fractional scale). M0 is scheduled explicitly rather than absorbed into M1 so the cost is visible. [ADR 0002](adr/0002-own-platform-layer.md) states the trigger for reversing the decision: if Spike D shows a general-purpose windowing crate can do DnD source and `text-input-v3` adequately, take it. |
| **Wayland drag-and-drop and clipboard gaps across compositors** | High | Spike D covers GNOME, KDE, Sway and wlroots. Keep the X11 path working. Document per-compositor failures rather than pretending they do not exist. |
| **A custom renderer means no toolkit accessibility** | High | `accesskit` wired in at M1, not M7, and produced by the same immediate-mode pass as the draw commands so it cannot drift. A screen-reader pass is part of every milestone's exit. |
| **GPU driver variance, and virtual machines with software rendering** | High | The GL fallback is tested on llvmpipe on *every* CI run — llvmpipe is the CI baseline, not an afterthought. A CPU raster path only if Spike A shows it is needed. |
| **Scope creep from Total Commander expectations** | Medium | [11-non-goals.md](11-non-goals.md) is enforced in review. A feature that does not serve one of the ten rules waits for 1.1. |
| **Fractional-scaling blur — the classic custom-renderer tell** | Medium | `wp_fractional_scale_v1` handled in Spike A. Golden-frame diffs at 100/125/150/200% in CI. |
| **Text edge cases: RTL, CJK, combining marks, 255-byte names, invalid UTF-8** | Medium | `swash` from the first commit, and an adversarial-filename fixture directory that every renderer change runs against. This is the defect we are explicitly not inheriting from the prior art. |
| **One-developer bus factor against a large surface** | Medium | Crate seams chosen so `sk-preview`, `sk-desktop` and `sk-index` are contributable without understanding the frame loop. The layering check keeps that true under pressure. |
| **`wgpu`'s dependency weight versus rule 10** | Medium | A hard number in the budget table and a `Renderer` seam that makes an `ash` backend a contained swap. [ADR 0003](adr/0003-wgpu-first-backend.md) names the triggers instead of leaving it to taste. |
| **Distro-agnostic claims decaying into "works on the maintainer's distro"** | Medium | The forced-degradation matrix (`SKERRY_DISABLE=…`) runs in CI, and both architectures are built from the first commit. Portability is a test, not a promise. |
| **The operation engine losing data** | Low probability, unbounded cost | M3 is gated on Spike E's matrix being green. Every destructive path routes through trash by default. Integrity is verified after every matrix run. |
| **Theming expectations — users will ask for their GTK theme** | Low | Ship a strong opinionated default, read the system light/dark preference and accent colour through the settings portal, offer a colour-scheme file. Say no to toolkit theme matching, in the README, once. |
