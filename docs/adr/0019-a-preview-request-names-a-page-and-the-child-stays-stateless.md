# 0019 — A preview request names a page, and the child stays stateless

**Status:** Accepted
**Supersedes:** [0017](0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)

## Context

[ADR 0017](0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)
settled the preview protocol when the only document preview was a first page: a
request named a path and a size, and `skerry-preview-doc` rendered page one. Its
revisit trigger named this moment exactly —

> A preview that is not a single image — a scrollable document, a video with a
> scrubber, anything where the child owns state across requests. In-order replies
> with no ids cannot express a subscription, and the honest move at that point is
> a new protocol rather than a flag on this one.

— and the honest move turns out to be narrower than the trigger feared, because
of a measurement the trigger was written without.

**The fear was that a page deep in a document costs the whole document.** If
rendering page 400 meant parsing 400 pages, then a stateless request per page
would make page-turning quadratic in the obvious way, and the only fix would be
a child that holds a parsed document open across requests — which is the state
0017 refuses to have, and which brings a correlation table, a handle lifecycle,
and the question of what an open handle means when the file changes underneath
it.

Measured instead, with `pdftoppm` on this machine, five runs each, wall clock:

| Document | Pages | Page 1 | A middle page | A late page |
| --- | --- | --- | --- | --- |
| `latexmk.pdf`, 239 KB | 66 | 0.18–0.34 s | 0.13–0.18 s (20) | 0.14–0.17 s (60) |
| `UserManual.pdf`, 9.6 MB | 1200 | 0.09–0.12 s | 0.18–0.30 s (100) | 0.13–0.16 s (400) |

**Depth costs nothing.** Page 400 of a 1200-page manual is no dearer than page 1,
and the variation that does exist tracks how much is drawn on the page rather
than how far into the file it is — PDF has a cross-reference table and poppler
uses it. The premise the revisit trigger was resting on is false for this format.

Three alternatives were live:

- **A new protocol with request ids and a held document.** What 0017 pointed at.
  It buys page turns that skip the spawn — 20 ms rather than 150 ms, by the
  numbers above — at the cost of a correlation table, a child that can be wrong
  about a file that changed, and a second lifecycle to get right. It is the right
  design for a video scrubber, where seeking genuinely is stateful, and it is
  paying for that design a milestone early.
- **Render every page when the document opens.** A 1200-page manual is 1200
  renders and about three minutes. Not seriously considered; recorded because it
  is the shape a cache-everything instinct suggests.
- **A page number on the request.** Each request stays self-contained, ordering
  stays the only correlation mechanism, and the child holds nothing at all
  between one request and the next.

## Decision

**A request names a page, a reply says how many pages there are, and the child
holds no state across requests.** Everything else in 0017 stands as written and
is restated here so this record can be read alone:

- **The wire is `sk_preview::wire`, fixed 24-byte little-endian headers and a
  payload.** A request carries the magic, the version, the ask, the box to fit
  inside, **the page**, and the path. A reply carries the refusal, the size of the
  image, **the number of pages the document has**, and the pixels.
- **A page is one-based, and page 0 means "whatever the first one is".** A caller
  that does not care about pages — every image request, every thumbnail — sends
  zero and never thinks about it. `skerry-thumbnailer` ignores the field.
- **A page count of zero means "unknown", not "none".** `skerry-preview-doc` gets
  the count from `pdfinfo` or `mutool info`, and a machine with the renderer but
  not the tool that counts pages still previews the document — it just cannot say
  how long it is, and the panel says the page number without a total rather than
  inventing one. Rule 7.
- **One child per kind, spawned on the first request that needs it**, kept for the
  life of the process. `skerry-thumbnailer` decodes images and SVG in pure Rust;
  `skerry-preview-doc` drives a renderer it finds on `PATH`. A window that opens
  no image spawns nothing.
- **The child reads and writes the cache, not the host**, and whether a reply came
  from a cached PNG or from decoding the original is the child's business. A
  document page is never cached — ADR 0018 — so this concerns thumbnails only.
- **One request in flight, with a deadline.** In-order replies mean no request ids
  and no correlation table; a child that misses its deadline is killed and
  respawned on the next request, which costs one preview.
- **Errors are a status byte, not a dead child.** Only a crash, a hang or a
  protocol violation costs the child itself.
- **The sandbox is applied by the child to itself** immediately after start-up and
  before it reads a single byte of a request, and reduced isolation is logged
  rather than silently accepted — ADR 0006.

## Consequences

**A page turn costs a process spawn and a render: 0.09–0.30 s by the table
above.** That is not instant and this ADR does not pretend it is. It is fast
enough that a person pressing `↓` sees the next page arrive rather than waiting
for it, and slow enough that holding `↓` down is not a flip-book. The panel keeps
showing the page it has until the new one lands, so the cost reads as latency and
never as a blank.

**The count needs a second program.** `pdfinfo` costs 0.01 s and ships in the same
package as `pdftoppm`, so in practice it is there whenever the renderer is — but
"in practice" is not "always", which is why zero means unknown and why the panel
has a shape for that case. It is one more thing to test deliberately.

**The host now has a page to keep, and a page is not part of a preview's
identity the way a size is.** `sk_preview::Key` gains the page, so a reply for
page 3 cannot be drawn as page 4 after a fast `↓ ↓`; that is the same
staleness rule the box already had, applied to one more field.

**Twenty-four-byte headers, not twenty.** Both ends are the same build spawned
from the same directory, so this is a recompile rather than a migration — but
`VERSION` moves to 2 so that a stale helper says which side is old instead of
misparsing a header.

**What this forecloses is nothing, and that is the point.** A child that holds a
document open is still available later; it would supersede this the way this
supersedes 0017, and the trigger below says when.

## Revisit trigger

**A page turn whose p99 exceeds 400 ms**, or a preview format where the
measurement above comes out the other way — where reaching frame *n* costs
something proportional to *n*, which is true of a video without an index and
false of PDF. Either one means the spawn-per-page bargain has stopped paying,
and the answer then is the held-handle protocol with request ids that 0017
described: a correlation table, a document opened once, and an explicit
invalidation when the file changes underneath it.
