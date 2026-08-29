# 0010 — A binary split tree whose leaves are tab strips

**Status:** Accepted

## Context

M2 owes "nested horizontal and vertical panes, tabs, drag-to-rearrange with snap
overlays, middle-click to split", and [08-roadmap.md](../08-roadmap.md) says the
milestone "ships the layout model, which is the hardest thing to retrofit later."
That claim now has a number against it: `App` holds one `Pane` and refers to
`self.pane` **129 times**, and every feature added before the layout model adds
more of them.

Three things are decided here rather than in code, because each is expensive to
reverse once a session file has been written in terms of it.

### The shape of the tree

A **list of panes** — what a two-pane commander is — cannot express a column
beside two stacked rows, which is the layout a person builds within an hour of
having splits at all.

An **n-ary grid**, where a split holds a `Vec` of children and a row of weights,
is what VS Code uses. Its advantage is real and narrow: three equal columns are
three equal weights, and "equalize" is a division. Its cost is that every
operation is a vector edit with an index — inserting between two children,
removing one and merging its weight into a neighbour, deciding which neighbour a
divider belongs to — and each of those is a place an off-by-one puts a pane at
zero width with no way back.

A **binary split** — an axis, a ratio, and two children — makes every operation
local and total. Splitting replaces a leaf with a node holding the old leaf and a
new one. Closing a leaf replaces its parent with its sibling. A divider is a node,
so a drag is one `f32` between two subtrees and cannot involve a third. The
recursion is four lines and the serialization is the same four lines.

### What a leaf holds

Tabs could be a property of the window (one strip, switching what every pane
shows) or of a leaf (a strip per pane). The window-wide strip is what a
single-pane file manager has, and it is what makes tabs and splits fight: with
one strip, a split is a mode the whole window is in, and switching tabs
rearranges the window. Per leaf, the two features are orthogonal — a leaf is a
tab strip that happens to be alone.

### Routing a read to the pane that asked for it

`Pane::generation` counts reads per pane and `Pane::apply` discards any message
that does not match. With one pane that is a complete protocol. With four, two
panes reach generation 3 within a second of each other and each takes delivery of
the other's listing — a directory of one pane's entries under the other's path,
with no error anywhere.

## Decision

**A binary split tree whose leaves are tab strips**, in `sk-app::panes`:

```rust
enum Node {
    Leaf(Tabs),
    Split { axis: Axis, ratio: f32, first: Box<Node>, second: Box<Node> },
}
struct Tabs { panes: Vec<Pane>, active: usize }
```

- **Every leaf holds at least one tab**, and the tree holds at least one leaf.
  Closing the last tab of a leaf closes the leaf; closing the last leaf of the
  tree closes the window, which is what every tabbed application does.
- **A leaf is named by a `PaneId`**, a counter that is never reused, so focus, a
  command and a hit test name a leaf without carrying a path through the tree
  that the next split invalidates.
- **Read generations are process-wide**, from one atomic counter, so a message
  belongs to exactly one pane and a message from a pane that has been closed
  matches nothing and is dropped. This replaces a per-pane counter that was
  correct only because there was one pane.
- **The watcher watches a set**, not a directory: one inotify instance, one watch
  per *visible* pane — the active tab of each leaf — and `Change` names the
  directory it happened in. A background tab is re-read when it is activated,
  which bounds the watch count by the number of splits rather than by the number
  of tabs. Spike C measured the watch budget as per-user and shared, so bounded
  matters.
- **Layout produces per-leaf geometry.** `Layout` keeps the window chrome — path
  bar, sidebar, status, window buttons — and gains a `content` rectangle. The
  header, the body and the four columns move to a `PaneLayout` computed per leaf
  by the same function, into a `Vec` the frame reuses.

## Consequences

- Three equal columns are `0.333` over `0.5`, not `[1, 1, 1]`. An "equalize"
  command has to count the leaves under each subtree instead of dividing by their
  number. Written once, and it is the only operation the n-ary form would have
  made simpler.
- A divider belongs to exactly one node, so hit-testing dividers is a walk that
  yields a rectangle and a `&mut f32`. No divider can be ambiguous between two
  parents, which is the bug class the vector form has and this one cannot.
- `self.pane` becomes `self.panes.focused()` and `self.panes.focused_mut()` at
  every call site. The diff is large and mechanical, and it makes "which pane"
  explicit at each of the 129 places where it was previously the only pane there
  was.
- A `Pane` is `Box`ed inside `Tabs` only if it grows; today it is moved in and out
  of a `Vec` on tab close, which is a memcpy of one listing handle.
- Sessions are not in this decision. The tree serializes to the state store M2
  owes, and its shape is chosen here to make that a recursive four-line form
  rather than to make it possible.

## Revisit trigger

If drag-to-rearrange with snap overlays turns out to need a pane's *neighbours*
in a direction more often than it needs its parent — a "drop to the right of
these three stacked panes" gesture that has to insert into a row that does not
exist as a node — the tree becomes n-ary and this decision is superseded rather
than patched. The signal is a `neighbours_of` that has to walk to the root to
answer.
