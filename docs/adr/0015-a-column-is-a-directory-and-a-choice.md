# 0015 — A column is a directory and a choice, not a pane

**Status:** Accepted. Supersedes [0014](0014-a-column-view-is-a-trail-of-panes.md).

## Context

[ADR 0014](0014-a-column-view-is-a-trail-of-panes.md) decided that a column *is*
a `Pane`, on the grounds that a pane already has everything a column needs — a
path, a listing, a selection, a scroll, and a generation a finished read is routed
by — and that the fields it would carry and not use were a smell rather than a
defect. That reasoning was wrong, and the objection that corrected it is
structural rather than aesthetic:

> a column being a pane seems wrong, that would imply that columns are independent
> from each other which is not the case

A pane *is* an independent view. That is what the type means everywhere else in
this program: two panes in a split have their own histories, their own modes,
their own sorts, and neither can say anything about the other. Columns are the
opposite of independent. They are **one path decomposed**, and the relation
between two neighbours is the whole feature.

Three things follow, and each is a defect rather than a smell.

**The coupling is the load-bearing invariant, and a `Vec<Pane>` cannot hold it.**
Column *i+1*'s directory *is* the entry chosen in column *i*. Nothing in a vector
of panes says so, so the relation would be maintained by convention at every site
that navigates, reloads, takes delivery of a read, or has a directory vanish
underneath it. An invariant held by discipline across many call sites is exactly
what [ADR 0013](0013-one-cell-geometry-behind-every-uniform-view.md) exists to
prevent one level down — fifteen copies of `body.y + row * row_height` were the
same mistake about geometry.

**An ancestor's choice is not a `Selection`.** It is one entry: the folder that is
open to the right. A `Selection` is a bitset with a cursor and an anchor, and
multi-select, `Ctrl+click` and the rubber band all act on it. Every ancestor
would carry one that must hold exactly one bit, and any input that ever reached
an ancestor would quietly work — a rubber band in column two selecting nine things
that mean nothing, with no type to have refused it.

**Sort, mode, grouping and filter must be equal across columns.** One sort applies
to the whole view; the mode is definitionally one thing. A trail of panes holds
one copy per column and a re-sort has to walk them and keep them equal. N copies
that must be equal is a generator of bugs, not a smell.

What 0014 got right was the other half, and it survives: the deepest column *is*
the pane, so `Pane::path` and `pane.selection` keep their meanings and no
operation, no rule 8 obligation and no session field changes.

## Decision

**Extract the thing a read is about**, which is neither a pane nor a column:

```rust
/// One directory, read and held. What a generation names and what a reply lands on.
pub struct Directory {
    path: PathBuf,
    listing: Listing,
    generation: u64,
    loading: bool,
    problem: Option<Problem>,
}
```

- `Pane` holds a `Directory` plus everything that is a *decision about a view*:
  selection, scroll, sort, mode, grouping, filter, bands, kinds.
- A column is `Level { directory: Directory, chosen: Option<EntryId>, scroll: Scroll }`
  — a directory, which of its entries opened the next column, and how far down it
  is scrolled. Nothing else. There is no mode, no sort and no selection to get out
  of step, because there is no field for one.
- A pane in column mode holds `trail: Vec<Level>` of its **ancestors**, deepest
  last, and is itself the last column.

`by_generation_mut` returns `&mut Directory` and finds it in a pane or in any
level of any trail. **One `apply`, in one place**, which is the read-routing
argument 0014 made — and it never needed a column to be a pane, only to have a
generation.

**The relation between neighbours is one method.** `descend(depth, child)`
truncates the trail below `depth`, records `chosen`, and starts the read for the
new deepest level. Nothing else may push or pop a level, so the invariant that
`trail[i].chosen` names `trail[i+1]`'s directory is a property of the type's only
mutator rather than of every caller's care.

**A trail level is ordered by name, folders first, and fetches no metadata.** A
column shows a name and whether it is a folder, and `getdents64` answers the
second for free — so a trail of eight directories costs eight enumerations and
zero `statx` fan-outs. The consequence is that ancestor columns are not in the
pane's sort order when that order is by size or date, and that is the right way
round: a column is how you got here, the pane is where you are working, and the
one you are working in has the full order.

## Consequences

- The `Directory` extraction is a wide mechanical change to `Pane` — `self.path`
  becomes `self.dir.path` at every reader — and it is the whole cost of this
  decision. It buys one implementation of the read, the apply, the cancel and the
  vanished-directory path for both kinds of thing that hold a listing.
- `Cells` is untouched, which is ADR 0013 working: a column's geometry is Details'
  geometry over a narrower body.
- A drag or a band inside a column is impossible rather than prevented, because a
  level has no selection to put a result in. Dragging *between* columns is
  dragging between two directories, which the split already serves.
- Horizontal scrolling is new: a trail deeper than the pane is wide has to scroll,
  and `sk_ui::Scroll` is one axis, so a second instance of it on x — and the fling
  comes free with it.
- A level keeps whatever listing it has. A directory that was the pane before you
  descended keeps its metadata; one read as an ancestor never had any. Nothing in
  a column displays it either way, so the two are indistinguishable on screen.

## Revisit trigger

Any of: a level turns out to need a sort, a mode or a multi-selection of its own,
which would mean a column is a pane after all and this decision should go back to
[0014](0014-a-column-view-is-a-trail-of-panes.md)'s answer; somebody wants the
ancestor columns in the pane's size or date order, which would make the "no
metadata for a trail" trade the thing to spend against; or `Directory` grows a
field that only one of its two holders uses, which would mean the seam is in the
wrong place.
