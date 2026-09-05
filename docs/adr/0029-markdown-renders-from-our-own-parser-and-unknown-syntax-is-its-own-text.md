<!-- SPDX-License-Identifier: Apache-2.0 -->
# 0029 — Markdown renders from our own parser, and unknown syntax is its own text

**Status:** Accepted

## Context

A markdown file previews today as what it is on disk: mono, numbered, with the
four-role highlighter over it. That is the right answer for a config file and the
wrong one for a README, which is the single most common markdown file a person
opens in a file manager and the one whose whole point is that it was written to
be *read*.

Rendering it raises three questions this project has to answer deliberately.

**Whose parser.** CommonMark is a specification with six hundred examples and no
small correct implementation; `pulldown-cmark` is the good crate and it is a
dependency whose surface we would use a fifth of. Against that,
[07-dependencies.md](../07-dependencies.md)'s rule is whether a dependency earns
its row, and every other format this program reads — GTK's `bookmarks`,
`mountinfo`, desktop entries, `mimeapps.list`, GIF, EXIF, D-Bus — is parsed by
hand. The question is not "can we write CommonMark" (we cannot, and should not
try) but "what does a preview need".

**What a preview needs.** Not conformance: legibility. A reader wants headings
that look like headings, code that looks like code, lists that line up, and the
words in between. A construct the parser does not know must not become a hole in
the page or a stack of asterisks — and what to do about it is the decision that
keeps this small.

**Where the second layout engine goes.** The text preview lays out fixed-pitch
rows against a character grid. Rendered markdown is proportional text at four
sizes in three faces, wrapped by measurement — a different pass, and a second
thing that can disagree with the first about where a click landed.

## Decision

**A parser of ours, a strict subset, and anything outside it renders as its own
text.**

- **`sk_app::markdown` parses to blocks and runs**, and knows nothing about
  pixels: ATX and setext headings, paragraphs, fenced and indented code, ordered
  and unordered lists with nesting, block quotes, thematic breaks, GFM pipe
  tables, and the inline set — emphasis, strong, code spans, links, images by
  their alt text, autolinks, strikethrough, hard breaks and backslash escapes.
- **Unknown syntax is literal text, never a hole.** An HTML block, a footnote, a
  definition list, a directive: each renders as the characters it is made of, in
  the paragraph it was found in. That is what makes a small parser honest — a
  reader sees everything the file says, and the worst case is that a construct is
  not *prettier* than its source.
- **Rendered is the default for markdown and the mode is not remembered.** Every
  open of a `.md` starts rendered; the toggle applies to the look in front of you
  and dies with it. A remembered mode is a setting, and settings have a
  precedence rule to obey — [ADR
  0027](0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)
  — which is a decision this one deliberately does not make.
- **Three states, not two.** Rendered is view-only; source is the existing mono
  view with the highlighter; editing is the existing editor, which implies
  source. `Ctrl+E` from rendered goes to editing, and leaving edit mode comes
  back to rendered, because the thing a person was reading is the thing they
  should get back.
- **Text can be selected in the rendered view**, over what is *drawn* rather than
  over the source: a selection is a range in the visual lines, and `Ctrl+C` gives
  the words as they are laid out. Selecting the markup is what the source view is
  for, one press away.
- **Emphasis needs a real italic face**, so `sk_text::Weight` splits — which its
  own comment named as the trigger: *"when a third face arrives — an italic —
  that is the moment"*. A synthesised oblique is not on the table; the face is
  discovered like the others and its absence degrades to regular.

## Consequences

**We own a markdown parser, and somebody will find a file it renders oddly.** The
mitigation is the literal-text rule: odd means "less pretty than it could be",
never "missing". A corpus of real READMEs is the test, not the specification's
examples.

**A second layout pass inside one panel.** The source view measures a character
grid and the rendered view measures runs, and a click has to land in the same
place the draw put it — the same trap `Hits::name_field` records. Both go through
one function per view and the selection is tested against what the frame recorded.

**Nested blockquotes and lists have a depth limit**, because a parser that
recurses on input is a parser that can be handed input. Sixteen, refused by
flattening rather than by failing.

**HTML is deliberately absent and deliberately planned for.** A `<div>` renders as
its characters today. When it renders as a box, that is a decision about a second
parser and a sanitiser and a security boundary, and it gets its own record rather
than growing out of this one.

## Revisit trigger

**A README from a project people have heard of that renders wrongly** — not
un-prettily, but with text missing, reordered, or a construct swallowing the rest
of the document. That is the bar this record is written against, and one failure
of it is a bug rather than a revisit.

**Or a second consumer of the parse.** Search over rendered text, an outline, a
table of contents: each makes the block tree an interface rather than an
implementation detail, and the shape it should have then is not the shape that is
convenient now.
