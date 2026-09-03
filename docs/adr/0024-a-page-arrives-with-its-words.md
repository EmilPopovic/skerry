# 0024 — A page arrives with its words: the renderer's own text layer, in word boxes, beside the pixels

**Status:** Accepted

## Context

The quick look selects and copies the text of a *text file* — a drag, `Ctrl+A`,
`Ctrl+C`, all three through [`Scope::Look`](../../crates/sk-cmd/src/registry.rs)
— and over a PDF page those same three commands are drawn dimmed. That is an
inconsistency rather than a missing feature: the panel makes a promise about text
and then declines it for the one kind of document people most often want a line
out of.

What stands in the way is that a page is pixels. `skerry-preview-doc` drives
`pdftoppm` or `mutool` and answers with an image
([ADR 0017](0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)),
and an image has no idea where its words are.

Three ways to get them, and two of them are the reason this record exists.

- **Parse the PDF ourselves.** A content-stream interpreter, font programs, CMaps
  and `ToUnicode` tables — [07-dependencies.md](../07-dependencies.md) already
  prices this: "every pure-Rust PDF *renderer* is a research project". Extraction
  is a smaller project than rendering and still the wrong one; the failure mode
  is mojibake on exactly the files a person cares about, and the tests would be a
  corpus we do not have.
- **A crate.** `pdf-extract` and friends are that same project taken as a
  dependency, in a tree that is already the largest in the workspace, for a
  feature that only exists on machines which have a renderer anyway.
- **Ask the tool that is already rendering the page.** `pdftotext -bbox-layout`
  answers with word boxes grouped into lines and blocks in reading order, with
  the page's size in points. `mutool draw -F stext` answers with the same thing
  at glyph level. Both ship in the same package as the renderer beside them, both
  are already permitted by ADR 0006's "drive a renderer the machine already has",
  and both cost nothing we are not already paying: **10 ms for one page against
  50–60 ms for the render**, measured on this machine against the fixture.

The argument against the third is that it makes a *second* external tool
load-bearing for a feature. It is the weaker argument, and this is why: the tool
is the same tool. A machine with poppler has both binaries or neither, and the
machine with neither already has no page to select text on.

## Decision

**A page reply carries a text layer beside its pixels, and the layer is word
boxes produced by the same toolchain that rendered the page.**

- **One request, two payloads.** The wire's reply grows a second length: pixels,
  then the layer. A separate `Ask` would be a second round trip for something
  that costs a fifth of what the caller is already waiting for, and it would let
  the two disagree about which page they are of.
- **The child parses, the host decodes.** `skerry-preview-doc` turns the tool's
  XHTML into a flat binary layer — a page size, then a word per record: four
  floats, a line number, and its UTF-8 bytes. The XML never crosses the pipe.
  That keeps the parsing where ADR 0006 puts every other parse of a document's
  own bytes, and it means the host's decoder is a `for` loop over fixed-size
  records rather than an XML reader.
- **Words, not glyphs.** A word box is what selection needs and what both tools
  agree on; a character offset inside a word is interpolated across its box,
  which is exact for the monospaced case and within half a character for
  proportional text. Glyph quads — which `mutool` has and `pdftotext` does not —
  would double the layer's size to move a caret by a fraction of a character.
- **Reading order is the tool's answer, not ours.** The layer keeps the order the
  words arrived in and numbers the lines; a selection across two columns copies
  what poppler thinks the order is. Deciding that ourselves means a page
  segmentation algorithm, which is the research project again.
- **Poppler first, and `mutool`'s format is a later arm.** A machine with only
  mupdf renders pages and cannot select text on them, which is a capability with
  a documented degradation — [06-portability.md](../06-portability.md) — and not
  an error.
- **A page with no text layer is a page with no selection.** Scanned documents
  are images of text; the three commands stay dimmed and nothing pretends
  otherwise. No OCR, at 1.0 or after it.

## Consequences

**The panel's selection model becomes two models.** A text file's selection is a
line and a byte; a page's is a word and a character inside it, because the second
one is geometric and the first is not. They share `Chosen`'s anchor-and-caret
shape and the commands above them, and nothing else — a single model over both
would be a line-and-byte model with a coordinate system bolted to it.

**The wire's version moves to 4.** Both ends are the same build spawned from the
same directory, so that is a recompile rather than a migration; the number exists
so a stale helper says which side is old.

**A second process per page fetch.** `pdftotext` is spawned after the renderer,
in the same child, under the same sandbox, and the page is not answered until it
returns — so a machine where it is slow makes the whole page slow. Measured at 10
ms against the render's 50–60; the revisit trigger below is the number that says
this stopped being true.

**What is copied is words with single spaces, and a hyphen stays a hyphen.**
The layer has word boxes and a line grouping; the gaps *inside* a line are not in
it, so a selected paragraph pastes as its words separated by single spaces with a
newline where the page broke the line. Measured against a real document — a
714-word manual page — the reconstruction is identical to `pdftotext`'s own plain
text except at hyphenated line breaks, where this copies `spe-` and `cialized`
and poppler's plain mode joins them. That difference is deliberate: joining them
needs a heuristic about whether a trailing hyphen belongs to the word, and the
heuristic mangles `well-` and `1990-` to fix `spe-`. What is on the page is what
comes off it.

**Coordinates are a place to be wrong.** The layer is in PDF points with the
origin at the page's top left, and the page is drawn as a texture fitted, zoomed
and panned in logical pixels. One transform, through
[`view::fitted_box`](../../crates/sk-app/src/view.rs) — the function the pinch
zoom already needs — and a golden frame of a highlighted selection is what says
it is the right one. Rotated pages are the case where the tools disagree with
themselves, and they are tested rather than reasoned about.

## Revisit trigger

**A page fetch where the text layer is more than a quarter of the time.** It is a
fifth of a render today; if a tool or a document class makes it comparable, the
layer becomes a second request fetched on the first press instead of with the
page — which is a different decision and a worse one to make speculatively.

**Or a selection that needs to be finer than a word.** Word-box interpolation
puts a caret within half a character of where it was clicked; if that turns out
to be visible in ordinary use, `mutool`'s glyph quads are already in the format
this reads and the record for that is a successor to this one.
