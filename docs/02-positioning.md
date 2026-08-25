# Positioning: the gap on Linux

## The field

| What exists | Good at | Where it breaks |
| --- | --- | --- |
| **Nautilus** | Clean, coherent, well integrated with GNOME | Minimal by policy: no dual pane, no inline tree, weak type-ahead, few rebindable keys. Struggles once a directory holds a few thousand entries. |
| **Dolphin** | Genuinely powerful — split view, terminal panel, deep settings | Configuration sprawl with no discovery surface; slow startup; KIO stalls the UI on slow network mounts. Power without polish. |
| **Thunar / Nemo / PCManFM** | Light, predictable, quick to start | Widget-toolkit scrolling, dated interaction model, thin previews, no command surface. |
| **Yazi / ranger / lf / nnn** | Keyboard-first, fast, scriptable — closest in spirit | Terminal-bound: cell-grid previews, no real thumbnails, no drag-and-drop, the mouse is an afterthought. Excludes everyone who wants a pointer. |
| **Krusader / Double Commander** | Every power feature you can name | Interface frozen in 1998. Discoverability zero. |

These failures are structural, not accidental, which is exactly why there is room. Each
project made a defensible early decision — a toolkit, a terminal, a minimalist product
philosophy — and the ceiling followed from it.

## The gap, stated once

> Nothing on Linux is simultaneously GPU-composited with a real frame budget,
> non-blocking on every filesystem call, keyboard-first **and** excellent with a mouse,
> and discoverable rather than merely configurable.

That combination is the product. Not any single feature in
[01-research-file-pilot.md](01-research-file-pilot.md). If Skerry ships feature parity on a
stack that drops frames, or a beautiful frame budget with a configuration file as its only
discovery surface, it has failed on its own terms while passing a feature checklist.

## Who it is for

In priority order, because when these conflict the earlier one wins:

1. **People who move a lot of files and know their keyboard.** Developers, photographers,
   sysadmins, anyone with a downloads directory measured in tens of thousands of entries.
   They will notice the frame budget within ten seconds and the command palette within a
   minute.
2. **People arriving from macOS or Windows** who expect quick look, split panes, and a
   context menu that does not hide half of itself. Interoperating correctly with the rest
   of the desktop matters more to this group than to the first.
3. **People on unusual setups** — a bare Sway session, a musl distro, an aarch64 laptop, a
   machine with no systemd. They are badly served today and cheap to serve well if
   portability is a constraint from the start rather than a port later. See
   [06-portability.md](06-portability.md).

Explicitly *not* the primary audience: someone who wants their file manager to look
identical to the rest of GNOME or KDE. Skerry reads the system light/dark preference and
accent colour and ships a strong default; it does not chase a toolkit theme, because
doing so would require adopting the toolkit.

## What "usability-led" commits us to

The distinguishing work is interaction design, so it gets the same rigour usually reserved
for the renderer:

- Latency is a usability property, not a performance one. Every budget in
  [09-performance-budgets.md](09-performance-budgets.md) exists because exceeding it makes
  the app feel like a different, worse product.
- Discoverability outranks configurability. A feature nobody can find has no users, no
  matter how well it is implemented.
- Reversibility outranks confirmation dialogs. Undo is a better answer than "are you
  sure?" for almost every destructive action.
- Consistency outranks local cleverness. One selection model everywhere beats a smarter
  selection model in one view.
