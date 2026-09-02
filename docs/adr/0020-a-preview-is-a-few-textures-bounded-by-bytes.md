# 0020 — A preview is a few textures, bounded by bytes, and the pages ahead are rendered before they are asked for

**Status:** Accepted
**Supersedes:** [0018](0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)

## Context

[ADR 0018](0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md) settled the
atlas and, in its last bullet, said this about the other half:

> **A preview is one texture, sized to the panel, replaced when the file
> changes.** It is not in the atlas: it is one image at a time, it is not 128×128,
> and putting it in a slot grid would mean either a second grid of a different
> size or the heterogeneous packing this decision exists to avoid.

"One image at a time" was true when a document preview was its first page. It
stopped being true when `↑`/`↓` began walking pages
([ADR 0019](0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md)),
and the consequence is measurable: **a page turn costs a full render, 135–200 ms**,
which is the number 0019 recorded and accepted. Used, it is the wrong side of the
line — a person looking for something in a document presses `↓` repeatedly, and
150 ms per press is a document you cannot skim.

The draft-first pass 0019's successor added takes the *first* pixel to 61–76 ms,
which helps the wait feel shorter and does not remove it.

What would remove it is having the next page already on the GPU when the key is
pressed. Three ways to get there:

- **A child that holds the document open**, which 0019's revisit trigger points at
  and which this does not need: the pages can be rendered by the same stateless
  requests, just earlier. Keeping that option unspent is worth something.
- **One texture, re-uploaded per turn.** The render is the cost, not the upload —
  4 MB through `write_texture` is under a millisecond against 150 ms of poppler —
  so a single texture means re-rendering, which is the thing being removed.
- **A few textures, and requests sent before they are needed.** What this is.

The reason 0018 said one was fragmentation and packing, and neither applies: these
are whole textures with their own sizes, allocated and dropped individually, not
regions of a shared one. What does apply is memory, and that is what the bound is
for. A page at 848×1200 RGBA is **4.07 MB**; the same page on a 4K panel is
1440×2000, **11.5 MB**. A count would mean 16 MB on one machine and 46 MB on
another against a 90 MB RSS budget that
[09-performance-budgets.md](../09-performance-budgets.md) already records as
missed.

## Decision

**Previews are a small set of whole textures keyed by what they are of, bounded by
total bytes and evicted by what was last drawn; and the pages after the one on
screen are requested before anybody asks for them.** The atlas half of 0018 is
unchanged and restated below so this record can be read alone.

- **Bounded by bytes, not by count: 24 MiB.** Six pages of A4 at a 1200-pixel
  panel, two at 4K. A count would be a memory figure that depends on the monitor;
  a byte budget is the same promise everywhere and degrades to holding fewer pages
  on the machine where each one is dearer. It is never less than one, because the
  page on screen is not evictable.
- **Evicted by what the last frame drew**, which is the atlas's rule applied to a
  second store, and it gives "the page behind" for free: turning forward keeps the
  page just left, because it was drawn a moment ago.
- **One preview is bound per frame.** A frame draws one page, so the renderer binds
  the texture that frame's draw list names and rebuilds the bind group only when it
  changes — on a turn, not per frame. A frame that named two different previews
  would draw one of them; nothing does, and a debug assertion says so.
- **Two pages ahead, one request at a time.** After the page on screen is complete
  the host asks for the next, and on its arrival the one after. Serially, so a
  turn to a page that is *not* held waits behind at most one prefetch rather than
  behind a batch of them.
- **A prefetch is not urgent.** Previews jump ahead of thumbnails in the queue
  because somebody is waiting for them; a page nobody has asked for yet is not
  that, so it queues behind anything that is. What a person pressed always
  overtakes what the program guessed.
- **Dropped when the look closes.** The set belongs to the document being looked
  at. Closing the panel frees all of it, which is what makes 24 MiB a peak rather
  than a floor.
- **The atlas is unchanged**: one RGBA texture of fixed 128×128 slots, 2048×2048
  for 256 of them, allocated on the first thumbnail and never resized, keyed by
  path and modification time and size, evicted by the frame last drawn and never
  the current one, uploads bounded by the same 256 KiB per frame glyphs are.
- **Above `sk-gpu`, both are a key and a rectangle** — `Painter::image`,
  `Painter::standby`, `Painter::preview` — and no `wgpu` type crosses the seam,
  which is [ADR 0003](0003-wgpu-first-backend.md).

## Consequences

**Up to 24 MiB of GPU memory while a document is open**, on top of the atlas's 16
MiB, against a budget already missed. That is the price and it is a real one; what
makes it acceptable is that it is a peak tied to a panel somebody has open, freed
when they close it, and that the alternative is a document nobody can skim.

**Work is done that is thrown away.** Somebody who opens a page and closes the
panel has paid for two renders they never saw — 300 ms of a helper's CPU, on a
core the frame loop is not using. This is the first place in the program that
speculates, and speculation is a thing to keep rare and to keep bounded: two pages,
never on the frame's thread, and never ahead of something asked for.

**A turn that outruns the prefetch is still 135–200 ms**, and holding `↓` down does
exactly that. Prefetching further ahead would help and would cost linearly; two is
the figure that fits the byte budget on a 4K panel with the current page, and it
moves with a measurement rather than a preference.

**The renderer now has two stores with different rules**, and the temptation is to
unify them. They are not the same problem — equal slots with index placement
against whole textures with their own sizes — and 0018's argument for the grid is
the argument against merging them.

**And a page turn has to be tested for what it does *not* do.** A held page must
not be redrawn from a stale texture after the file changes underneath it, which is
what the modification time in the key is for; the test is a document rewritten
while a look is open.

## Revisit trigger

**A prefetch that is not enough**: a page turn whose p99 exceeds 200 ms while
walking a document at a key repeat rate, measured with the prefetch working. That
means the guess is too shallow rather than wrong, and the answer is a deeper
queue, not a different design.

**Or 24 MiB proving to be the wrong shape of bound** — a panel where one page
exceeds it, which is a 3400×4800 preview and not a thing today. Then the page on
screen is held at the panel's size and the prefetch is held at the draft's, which
is a different decision from this one.
