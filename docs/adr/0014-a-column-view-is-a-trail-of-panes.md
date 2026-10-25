# 0014 — A column view is a trail of panes, and only the last one is the pane

**Status:** Accepted

## Context

[ADR 0013](0013-one-cell-geometry-behind-every-uniform-view.md) put one cell
geometry under every *uniform* view and said which views that covers:

> A third and fourth mode become small. Tree does not, and this ADR says so
> rather than letting a future reader discover it.

A column view — the NeXTSTEP browser, Finder's third mode — is the case that
ADR 0013 neither covers nor rules out, and it is worth being precise about why.
Each column *is* a uniform list: one entry per row, one fixed row height, one
column of cells. `Cells` answers every geometric question inside a column
unchanged. What is new is that there are **N of them, over N different
directories**, and a `Pane` holds one `Listing` for one path.

So the shape of the decision is not geometry. It is: what is a column made of,
and which one of them is the pane that the rest of the program talks to?

Everything above the listing assumes there is one. `Pane::path` is where the pane
is; `pane.selection` is what an operation acts on; `pane.generation` is what a
finished read is routed by; the watcher watches one directory per visible pane;
the session stores one path per tab. Four of those are load-bearing at a distance
— an operation, a read, a watch, a restore — and a column view that answered any
of them differently would be a second set of answers for the same questions.

## Decision

**A column is a `Pane`.** A pane in column mode holds a `trail: Vec<Pane>` of its
ancestors, deepest last, and *is* the last column itself. Nothing else changes
shape: the trail's panes have paths, listings, selections, scrolls and
generations, because they are panes.

Four consequences, one for each of the things that assumed a single listing:

- **Reads route themselves.** A generation is unique across the process and
  `Panes::by_generation_mut` is the only thing that maps a reply to a pane. It
  gains the trail to search. No new message, no depth field, no second read path.
- **Operations act on the pane, which is the last column.** `Pane::path` and
  `pane.selection` keep their meanings: the deepest column is where you are.
  Selecting a folder in an ancestor column *navigates* — it truncates the trail
  at that depth and pushes the chosen folder — which is what a column browser
  does and is the same operation as clicking a breadcrumb.
- **The watcher watches the trail.** M0 already moved it from one directory to
  every visible pane; the trail is more visible panes. The per-user inotify
  budget in [09](../09-performance-budgets.md#the-watch-budget-is-per-user-shared-and-not-a-constant)
  is the constraint, and a trail is bounded by path depth, which is small.
- **The session stores nothing new.** A trail is derivable from a path: the
  ancestors of `/home/tester/repos/skerry` are its parents. `Mode::Columns` in the
  tab line is the whole of it, which is what [ADR
  0011](0011-one-plain-text-state-file-written-atomically.md)'s no-bump rule
  wants.

**A trail pane carries fields it must not use independently.** A `Pane` has a
mode, a grouping and a filter; a column has no business having its own. The mode
is read from the leaf's own pane and never from a trail entry, and a test asserts
that a trail pane's mode is never read — the alternative, a lighter `Column`
struct, buys that guarantee for the price of a second read path, a second apply,
a second selection type and a second thing to keep in step with `Pane`. That
trade is the one ADR 0013 already made once and it went the same way.

## Consequences

- `Cells` is untouched. A column's geometry is Details' geometry over a narrower
  body, which is the point of ADR 0013 and the reason this is not a fourth
  arithmetic.
- `Pane` becomes recursive through a `Vec`, which means `Pane: !Copy` stays true
  and `describe`/`restore` must not walk the trail. Both are already the case.
- The rubber band, the drag source and the drop target all work inside a column
  because they work on a pane. Between columns they do not, and that is correct:
  dragging from one column to another is dragging between two directories, which
  is the same gesture the split already serves.
- **Horizontal scrolling is new.** A trail deeper than the pane is wide has to
  scroll, and `sk_ui::Scroll` is one axis. A second instance of it on x is the
  cheap answer and the fling comes free.
- Two panes side by side in column mode is two trails, which is two sets of
  reads. Bounded by depth, so the cost is a handful of directories rather than a
  multiplier on anything.

## Revisit trigger

Any of: a trail pane's mode, grouping or filter turns out to need a value of its
own, which would mean a column is not a pane after all; the number of directories
a trail holds open makes the watch budget the binding constraint on how deep a
path may be; or a column needs to show anything a Details row does not, which
would make the "a column is Details over a narrower body" claim the thing to
spend against.
