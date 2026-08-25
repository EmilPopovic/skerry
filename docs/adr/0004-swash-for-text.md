# 0004 — `swash` for shaping and rasterization; our own line layout

**Status:** Accepted

## Context

Text is the one genuinely hard problem in a file list, and the prior art fails at it: File
Pilot's roadmap still lists Unicode support as unimplemented. Filenames are adversarial by
nature — RTL scripts, combining marks, emoji, 255-byte names, invalid UTF-8 that must still
render as *something*.

The options were `cosmic-text` (shaping, fallback, layout and editing in one, at the cost of
a sizeable tree), `swash` (shaping, font introspection and rasterization in one crate with
a small tree), or `rustybuzz` plus a rasterizer plus fallback logic assembled by hand.

Our layout needs are narrow: single-line, ellipsized, cached per (string, size, weight),
laid out for a row rather than a paragraph. `cosmic-text`'s editing and multi-line
capabilities are mostly weight we would not use, and its layout cache is not shaped like our
columnar store.

## Decision

`swash` for shaping, font introspection and glyph rasterization. Our own line layout,
ellipsis logic, font fallback chain and layout cache in `sk-text`, keyed to the arena so a
row's shaped run is looked up by `EntryId` rather than re-derived. Font discovery by parsing
fontconfig's configuration ourselves rather than linking it.

Real shaping, bidi and fallback exist from the first commit, and an adversarial-filename
fixture directory runs against every renderer change.

## Consequences

- Font fallback is ours, including the CJK and emoji chains. This is the fiddliest part and
  the reason `cosmic-text` remains a fallback option if it proves worse than expected.
- The layout cache can be designed around the columnar store rather than adapted to a
  general-purpose one, which is where the frame budget is won.
- Text editing widgets (rename field, search box) need cursor and selection logic we write.
  Small, because they are single-line.

## Revisit trigger

If our fallback chain produces worse results than `cosmic-text` on the adversarial fixture
set after a focused week, switch — correctness beats the dependency budget every time.
