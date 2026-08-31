# 0013 — One cell geometry behind every uniform view, and Tree is not one of them

**Status:** Accepted

## Context

[08-roadmap.md](../08-roadmap.md#m2--4-wk--panes-tabs-views) asks M2 for four views:
"Grid with thumbnails, inline Tree, List columns, Group By". One exists. The
Details view's geometry is not a seam — it is arithmetic, repeated:

```
top = body.y + row as f32 * metrics.row_height - scroll.offset()
```

That expression, or its inverse, appears in `PaneLayout::row_at`, in the body's
paint pass, in the hit test, in `reveal_cursor`, in the page size for `PageUp`, in
the scrollbar's content height, and in the rubber band — fifteen places in
`sk-app`, each of which assumes *one column, one fixed height, one entry per row*.
A second view added against that shape means fifteen `match` arms, and a third
means thirty.

The layout model landed early in M0 for exactly this reason, and
[ADR 0010](0010-a-split-tree-of-tab-strips.md) records why: `App` held one `Pane`
and named it 129 times, and every feature added before the tree added more. This
is the same pressure one level down, and it is cheaper to answer now than after
Grid has been written against the arithmetic.

## Decision

One value, `crate::cells::Cells`, answers every geometric question a view has.
It is computed per pane per frame from the body rectangle, the metrics, the entry
count and the mode, and it owns:

- the rectangle an entry is drawn in, given the scroll offset
- the entry at a point, or none
- the content height the scrollbar measures
- how far `PageUp` and `PageDown` move
- the entry a direction key reaches from another, for all four directions

Nothing else computes an entry's position. `metrics.row_height` stops being read
outside `Cells` and the theme.

**Details is a one-column grid**, not a special case. That is what makes the seam
worth having rather than a wrapper over two implementations: `columns` is 1, the
cell is the body's full width, and every one of the answers above falls out of the
same arithmetic. It also settles the direction keys — Left and Right are previous
and next entry, which in a one-column view is what Up and Down already do, so
they need no mode-dependent binding.

### What this covers, and what it does not

**Covered: uniform-cell views.** Details, Grid, and the List columns M2 asks for
are all "every entry occupies an identical box, laid out in reading order". The
seam is a grid of identical cells and nothing more.

**Not covered: Tree.** An inline tree is not a listing laid out differently, it is
a *different listing* — one whose rows come from several directories at once, at
varying depth, with a per-row expansion state. `Listing` can already hold rows
from several directories (`home`, added for flattened search), which is the half
that exists; the depth and the expansion state are not modelled anywhere. Tree
needs its own decision and will not be reached by adding a `Mode` variant here.

**Not covered: Group By.** Grouping is orthogonal to cell geometry — it inserts
headings into the sequence of cells, which changes what index maps to what
position but not how a cell is shaped. It belongs on top of this seam rather than
inside it, and doing it inside would make `Cells` answer two questions.

### Persistence

The mode is per tab, beside the sort and the scroll offset, in [ADR
0011](0011-one-plain-text-state-file-written-atomically.md)'s file. The tab line
gains a token and `read_tab` accepts both arities: a mode word is not a number, so
the old three-field form is unambiguous and still parses. **No version bump**,
because a bump discards the file and the file is somebody's window arrangement —
which is the whole thing ADR 0011 exists to preserve.

## Consequences

- Fifteen call sites move behind one value, and a sixteenth cannot be written
  without noticing: `row_height` outside `Cells` is now the smell.
- `Cells` is pure arithmetic over five numbers, so it is unit-testable without a
  window, a GPU or a directory. That is where the coverage for every future view
  goes, and it is cheaper coverage than a golden frame.
- Two modes now means every geometric bug has two shapes to appear in. The
  mitigation is that Details is not a special case, so a bug in the shared
  arithmetic shows up in both rather than hiding in one.
- Grid without thumbnails is a large coloured mark and a name. It is genuinely
  less useful than Grid with them, and the thumbnails are M5's. Shipping the seam
  and the mode first is deliberate: the seam is what is expensive to retrofit, and
  the thumbnail is a cell's content rather than its shape.
- A third and fourth mode become small. Tree does not, and this ADR says so
  rather than letting a future reader discover it.

## Revisit trigger

Any of: a view is wanted whose cells are not uniform — a waterfall of images at
their own aspect ratios would be one — which would make the grid assumption the
thing to spend against; `Cells` grows a `match` on the mode in more than two of
its methods, which would mean the modes are not variants of one geometry after
all; or Group By turns out to need the index-to-position mapping inside `Cells`
rather than above it.
