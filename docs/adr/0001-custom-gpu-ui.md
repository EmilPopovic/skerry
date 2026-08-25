# 0001 — A custom GPU UI layer instead of GTK or Qt

**Status:** Accepted

## Context

The differentiator is latency ([02-positioning.md](../02-positioning.md)), and the budget is
a p99 keystroke-to-frame under 8 ms while a million-entry directory scrolls. The obvious
path is a mature toolkit; the obvious path is also how every existing Linux file manager
arrived at its ceiling.

Three properties of retained-widget toolkits are incompatible with the budget rather than
merely inconvenient: per-row widget instantiation in list views, scroll implementations we
cannot control, and a main loop whose conventions assume filesystem calls are cheap and
synchronous. The last is the deepest — rule 1 forbids exactly what GIO and KIO's ergonomics
encourage. Adopting a toolkit also means adopting its object system, which imports the
dependency weight rule 10 exists to prevent and ends the distro-agnostic story of rule 9.

The prior art is evidence, not proof: File Pilot reached this class of latency by writing
its own renderer and UI framework in C, and reviewers lead with the result every time.

## Decision

Build a small immediate-mode UI layer over the GPU: `sk-gpu` for device, atlases and
batching; `sk-ui` for the immediate-mode pass, layout, virtualized lists, focus and the
accessibility tree. No GTK, Qt, GLib or GIO anywhere in the graph.

## Consequences

- We own text rendering, input handling, accessibility, IME and the entire widget
  vocabulary. That is months of work that a toolkit would have given us.
- Accessibility must be built deliberately (`accesskit`, from M1) because there is no
  toolkit layer providing it. Same for IME.
- Golden-frame testing on llvmpipe becomes mandatory infrastructure, not a nice-to-have,
  because a custom renderer is otherwise unreviewable.
- The visual identity is ours, which is a benefit for a usability-led project and a cost in
  desktop-theme integration — see the theming policy in
  [06-portability.md](../06-portability.md).
- We are not competing with a toolkit on breadth. A file manager needs perhaps twenty widget
  kinds, and they are all in the list-and-panel family.

## Revisit trigger

Spike A failing its bar on all evaluated stacks would mean the differentiator is
unreachable in this ecosystem, which invalidates the project rather than this ADR.
