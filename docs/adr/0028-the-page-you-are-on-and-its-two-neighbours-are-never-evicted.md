<!-- SPDX-License-Identifier: Apache-2.0 -->
# 0028 — The page you are on and its two neighbours are never evicted

**Status:** Accepted

## Context

[ADR 0020](0020-a-preview-is-a-few-textures-bounded-by-bytes.md) bounds the held
previews by bytes: twenty-four mebibytes, evicted least-recently-drawn first,
because a count is sixteen megabytes on one machine and forty-six on another. It
says an evicted page "is rendered again, which is the right degradation".

Reading a document says otherwise. Three things came out of using it:

**A prefetched page has never been drawn.** Eviction sorts on the frame a picture
was last on screen, and a page fetched ahead has been on screen zero times — so it
is always the *first* thing dropped, which is exactly backwards. The comment in
`QuickLook::retain_held` already recorded the shape of this: "at a large panel a
page is 11.9 MB, so opening a document evicts a prefetch almost at once".

**The prefetch only looked forward.** `next_ahead` walked `page + 1` and
`page + 2`, so `↑` was a render every time and `↓` was free. A preview that is
quick in the direction nobody notices and slow in the one they do.

**And the pages were far bigger than they needed to be**, which is what made the
budget bite: the child was asked for a box and rendered the page's *long edge* to
the box's long edge, so a portrait page in a landscape panel came back 709×1004
where 417×590 was wanted — 2.9× the pixels. That is fixed in the child rather
than here, and it is why three pages now usually fit inside 0020's budget
anyway.

The question this record settles is what happens when they do not.

## Decision

**The page on screen and the pages either side of it are pinned: the byte budget
may not evict them.**

- `Preview::keep` takes the pinned keys and `over_budget` skips them, alongside
  the rule it already had about pictures drawn this frame. The frame loop passes
  the current page and whichever neighbours are held, once per settle.
- **The budget still governs everything else**, and it is still the number in
  0020. What changes is that it is a bound on the *unpinned* set rather than on
  the whole store. Three pages of a large document on a 4K panel is about 17 MB
  after the render-size fix, so the pin is normally free; where it is not, the
  store goes over rather than dropping the page somebody is about to turn to.
- **The prefetch goes both ways**: `page + 1`, then `page - 1`, then `page + 2`.
  The first two are what this record promises; the third is what keeps reading
  forwards ahead of the reader.

## Consequences

**The bound is no longer absolute, and that is the cost.** A pathological
document — a poster-sized page on a 5K panel — could pin three textures of
30 MB each and hold 90 MB. Nothing in the code prevents it, and the honest
mitigation is that the pinned set is *three*, so the worst case is bounded by
three times one page rather than by how long somebody browses.

**A page nobody has drawn is now the thing that survives**, which inverts the
policy the store was written with. That is deliberate and it is the whole point:
what a person is about to look at matters more than what they last looked at.

**`over_budget` can now return nothing while over budget.** A caller that
expected it to always bring the store back inside the bound would loop for ever;
there is no such caller, and the loop it does have exits on "nothing droppable"
rather than on "inside the bound".

## Revisit trigger

**A pinned set over 64 MB**, measured by the diagnostic report's preview bytes
against a document somebody actually reads. At that point the answer is not a
bigger budget but a smaller pin — one neighbour rather than two, or a
half-resolution neighbour promoted on the turn — and either is a different
decision from this one.

**Or a second thing wanting a pin.** Today the frame loop is the only caller and
it replaces the whole set each settle. A second caller means the set needs
ownership, which this record deliberately does not give it.
