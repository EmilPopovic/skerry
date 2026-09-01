# 0018 — Thumbnails are slots in one atlas; a preview is not

**Status:** Accepted.

## Context

`sk-gpu`'s atlas holds glyphs in two textures — an R8 coverage mask and a smaller
RGBA surface for colour glyphs — packed by a shelf allocator whose own doc comment
says what it is and is not:

> A shelf allocator: rows of increasing height, each packed left to right.
> **Wrong for long-lived heterogeneous images**, right for glyphs, which arrive in
> a handful of heights and never move.

Thumbnails are long-lived and they are images, so that sentence is a warning
addressed at this decision. But the premise it rests on — heterogeneous — is false
here, and that is the whole of what makes this easy.

**A thumbnail is one size.** The XDG specification's `normal` is 128×128 and
nothing in this program wants a different figure: a details row's mark is sixteen
logical pixels and a grid cell's is forty to sixty, so 128 device pixels covers
every zoom and every scale factor this window reaches. A *preview* is a screenful
and is not that.

The frame loop's own contract already names the pressure. Step 5 in
[04-architecture.md](../04-architecture.md#the-frame-loop-precisely) bounds glyph
upload per frame "so a thousand new thumbnails cannot blow the budget", and
`UPLOAD_BUDGET_BYTES` is 256 KiB — which is four thumbnails.

## Decision

**One RGBA atlas of fixed 128×128 slots, allocated on the first thumbnail and
never resized; a preview is its own texture, outside it.**

- **A grid, not shelves.** Every entry is the same size, so placement is an index
  and there is no fragmentation, no repacking and no growth policy. Eviction is a
  slot going free, which is the property shelves cannot offer and the reason the
  glyph allocator's warning does not apply.
- **2048×2048 — 256 slots, 16 MiB — allocated lazily.** A window that opens no
  image pays nothing, which is what makes the size affordable against a 90 MB RSS
  budget that is already missed. 256 is more thumbnails than a 4K window shows in a
  grid; if that stops being true the number moves with a measurement beside it.
- **Keyed by path, modification time and size.** The mtime is what makes a
  re-encoded photograph a different thumbnail rather than a stale one, and it is
  already in the listing, so the key costs no `statx`.
- **Evicted by what the last frame asked for.** Each entry carries the frame
  number it was last drawn in; when the atlas is full, the oldest such entry is
  the one replaced. A slot in use this frame is never taken, so a full atlas
  degrades to thumbnails that flicker rather than to a wrong picture in a cell.
- **Upload is bounded by the same budget glyphs are.** A thumbnail is 64 KiB
  against a 256 KiB frame budget, so a screenful arrives over several frames and
  each of those frames is inside 8 ms. That is the bound step 5 already promised.
- **Above `sk-gpu`, a thumbnail is a key and a rectangle.** `Painter::image(key,
  rect)` — no `wgpu` type crosses the seam, which is
  [ADR 0003](0003-wgpu-first-backend.md).
- **A preview is one texture, sized to the panel, replaced when the file
  changes.** It is not in the atlas: it is one image at a time, it is not 128×128,
  and putting it in a slot grid would mean either a second grid of a different size
  or the heterogeneous packing this decision exists to avoid.

## Consequences

- Two allocators in `sk-gpu` — shelves for glyphs, slots for thumbnails — and they
  share nothing but the upload budget. That is a duplication worth having: each is
  a few dozen lines and each is right for exactly one thing, where one allocator
  general enough for both would be right for neither.
- A thumbnail cannot be drawn at more than 128 device pixels without going soft.
  The grid mark tops out well below that today; a view that wants larger has to
  ask the child for `large` (256×256) and that is a second slot size, which is a
  second grid and a change to this decision.
- The atlas is per-window and dies with it. A restored session re-reads its
  thumbnails from the shared cache, which is a decode-free round trip to the child
  rather than a re-encode.

## Revisit trigger

A view that draws a thumbnail larger than 128 device pixels — a cover-flow, a
single-item inspector that uses the atlas rather than a preview texture, or a
grid whose cells grow past that at 200% and a large zoom. At that point either the
slot size changes for everything or the atlas holds two sizes, and the second is a
different decision from this one.
