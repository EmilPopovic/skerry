# 0002 — Own the Wayland/X11 platform layer instead of using `winit`

**Status:** Accepted
**Supersedes:** the `winit` assumption in the first draft of the architecture plan

## Context

The first draft assumed `winit` for windowing. Two later constraints changed the
calculation.

Rule 5 makes the pointer a peer of the keyboard, and [05-platform-integration.md](../05-platform-integration.md)
makes clipboard and drag-and-drop interop with Nautilus, Dolphin and browsers a
requirement rather than a nicety. A file manager is a drag-and-drop **source** at least as
often as a target — and drag *source* support, primary selection, `text-input-v3` and
`wp_fractional_scale_v1` are precisely where general-purpose windowing crates are thinnest.
Reaching them typically means dropping to the raw protocol anyway, at which point the
abstraction is a layer we fight rather than a layer we use.

Rules 9 and 10 push the same direction: the pieces we would bolt on around a windowing
crate (a clipboard crate, a DnD crate, an IME shim) are individually small and collectively
a second, worse platform layer.

## Decision

`sk-plat` owns Wayland and X11 directly: `wayland-client` and `x11rb` for protocol plumbing
and code generation, `xkbcommon` bindings for keyboard mapping, and our own policy above
them for input, clipboard, drag-and-drop as source and target, IME, monitors, fractional
scaling and window lifecycle.

## Consequences

- M0 exists: roughly six weeks before anything is visible on screen. This is the single most
  expensive decision in the plan and it is scheduled explicitly rather than hidden inside
  M1.
- A long tail of compositor quirks becomes ours. Mitigated by testing against GNOME, KDE,
  Sway and wlroots from Spike D onward, and by keeping the X11 path first-class rather than
  vestigial.
- In exchange: DnD source support, primary selection, correct fractional scaling and IME are
  all reachable, and none of them are a fight against someone else's abstraction.
- `sk-plat` is publishable standalone if it proves useful to others. Not a goal; a good test
  of whether the seam is real.

## Revisit trigger

If Spike D demonstrates that a general-purpose windowing crate can act as a DnD source
against GTK and Qt applications on both display servers **and** handle `text-input-v3`
adequately, take it and delete this decision — six weeks is a large price for a problem
someone else has solved.
