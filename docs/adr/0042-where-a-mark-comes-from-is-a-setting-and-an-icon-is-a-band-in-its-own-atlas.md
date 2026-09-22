# 0042 — Where a mark comes from is a setting, and an icon is a band in its own atlas

**Status:** Accepted

## Context

A folder drawn as a coloured page beside a desktop full of themed icons reads as
unfinished whatever the frame time is. That is the one surface where this product
is visibly poorer than what it replaces, and it is the reason icon themes are in
the first release at all.

The obvious answer — draw the desktop's icons and delete ours — is the one this
record rejects first, because it is wrong about who is being served. A file
manager that draws the desktop's icons everywhere looks like every other window
on that desktop. The nine kind hues over a geometric mark cost 0.012 ms a frame,
measured at 200,000 entries in
[09-performance-budgets.md](../09-performance-budgets.md#what-m0-measured), and
somebody who prefers them should not have to uninstall an icon theme to keep
them. So the marks are a value somebody chooses, not the branch that runs when
nothing else worked — and a branch nobody chooses is a branch that rots.

That has a consequence for the lookup that is easy to miss in review: *the person
asked for no theme* and *no theme was found* must be two different states, said
differently, reachable from different code. If they collapse into one `Option`
that is `None`, a machine with a broken `index.theme` and a machine whose owner
chose our marks are indistinguishable, and the honest sentence rule 7 wants
cannot be written.

**Three seams that cannot see each other.** The lookup is `index.theme` parsing
and the freedesktop search algorithm, and
[04-architecture.md](../04-architecture.md#layers) already puts XDG icon themes
in `sk-desktop`. The pixels come from a decoder, which
[ADR 0006](0006-out-of-process-decoders.md) puts in a sandboxed child that
`sk-preview` hosts. The atlas is `sk-gpu`. All three are level 2, and the layering
forbids a sideways edge — the same reason kind detection lives in `sk-app` rather
than in `sk-preview`, which the architecture document already states.

**The atlas is not the one thumbnails use.**
[ADR 0018](0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md) allocates
128×128 slots keyed by path, modification time and size, and its whole argument
rests on one sentence: a thumbnail is one size. An icon is not. It is keyed by
theme, name, size and scale; it is drawn from 16 device pixels in a details row
to something near 92 in a grid cell at 200%; and the freedesktop per-size
directories exist precisely because 16-pixel art is not 48-pixel art downscaled.
Putting a 16-pixel icon in a 64 KiB slot spends 1/64th of the pixels it paid for,
and 256 slots is fewer 16-pixel icons than a 4K details list shows at once.

**The cache is where the interesting constraint is.**
[ADR 0034](0034-helper-execution-and-network-rights-are-explicit.md) grants the
image helper the XDG thumbnail cache and nothing else, and the value of that
record is that the list of writable paths is short enough to read. Icons would
need either a second granted directory or no cache at all. They cannot go into
the thumbnail cache: it is keyed by the source file's URI, it is read by every
other file manager on the machine, and an entry for
`/usr/share/icons/Adwaita/32x32/places/folder.png` at 32 pixels would be a record
those programs would misread and we would have to skip.

The alternative is re-rasterising the working set at every start, and
[rule 1](../03-principles.md) is explicit that its start-up list is a list nothing
joins without a measurement of what it costs. So it was measured.

**Method.** The release `skerry-thumbnailer` driven over its own pipe, one
request at a time, with `Ask::Preview` rather than `Ask::Thumbnail` so that
nothing is served from the XDG cache and every raster is real work. The names are
the forty a file manager actually draws — the places, the volumes, and the generic
marks a mixed directory resolves to — at 16, 24, 32 and 48 pixels, each resolved to
the theme's nearest per-size directory. Best of five runs of the whole set, warm
page cache, on an AMD Ryzen AI 9 HX 370 at a load average of 1.35 to 2.75 over 24
threads, with the helper's own single thread doing the work. Per
[12-measurement.md](../12-measurement.md), the run is reported best-of-five and
the cold case is named separately rather than averaged into it.

| | Adwaita | breeze |
| --- | --- | --- |
| Names found, of 40 | 32 | 38 |
| Rasters, four sizes each | 128 | 152 |
| Whole set, best of 5 | **19.0 ms** | **6.4 ms** |
| Whole set, median run | 19.2 ms | 6.4 ms |
| Per raster, median | 0.051 ms | 0.029 ms |
| Per raster, p99 | 2.023 ms | 0.154 ms |
| Per raster, worst | 4.411 ms | 0.173 ms |

Adwaita's worst four rasters are the same file: `user-trash-full`, 55,934 bytes of
SVG, at 4.411, 2.599, 2.023 and 1.447 ms for the four sizes. Two figures sit
beside the table. Spawning the helper and confining it, measured through a first
request for a PNG so that no font work is charged to it, is **1.9 ms**. The
`fontdb` build inside the child, paid once on its first SVG, is **63 to 86 ms** —
and on the first run of all, against a cold page cache over the font
directories, that one request took **512 ms**.

The theme's own directory listing was measured too, because it decides where the
lookup runs rather than what it costs to draw. Adwaita is 832 files and breeze is
19,853; walking them warm takes 0.4 ms and 23.1 ms as an upper bound, since the
walk was Python's and a Rust one is faster. 23.1 ms is three frame budgets, which
settles the question.

## Decision

**The source of a mark is a setting with three values; an icon is rasterised by
the existing image helper at `sk-app`'s request; and it lands in a second
`sk-gpu` atlas of four fixed bands, with no disk cache at all.**

### The setting

- **`icons` is a line in the config**, in
  [ADR 0027](0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)'s
  grammar and under its precedence rule: the file says what a fresh window does,
  the session says what this one was left doing and wins, but only because the
  window itself changed it. It is the seventh entry in
  [`settings::ALL`](../../crates/sk-app/src/settings.rs) and it is a row in the
  `Ctrl ,` panel with its provenance beside it, like the other six.
- **Three values: `marks`, `desktop`, or the name of an installed theme.**
  `marks` is ours. `desktop` asks what the desktop is using and follows it. A
  name pins one theme regardless of what the desktop does. A fresh install takes
  `desktop`, so a machine with a theme installed looks like the rest of that
  desktop without anybody being asked.
- **`desktop` is a runtime probe, never a build-time choice** — [ADR
  0007](0007-runtime-capability-detection.md). The name comes from the settings
  portal `sk-desktop` already reads for the colour scheme, with the GTK settings
  files as the fallback where no portal answers, and `hicolor` as the terminal
  fallback the specification requires.
- **Asked for no theme and no theme found are different states.** The resolved
  source is `Ours`, `Ready(Theme)` or `Failed(reason)`, and the three are drawn
  differently in the settings panel. Under `Ours` no `index.theme` is parsed, no
  theme is held in memory and the lookup is never called; under `Failed` the panel
  names the theme that could not be loaded; under `Ready`, a name the theme does
  not have is a *miss* that falls through inheritance to `hicolor` and then to the
  mark, and the next name may still hit. A test asserts that the three produce
  three different sentences.
- **`icons` is the first setting that is neither a truth nor a number**, so
  `settings::Kind` gains a variant for a choice and `settings::Values` gains a
  field that is not a `bool`. `Marks` is untouched: it is indexed per setting and
  carries no value.

### Where an icon is rasterised

- **`sk-desktop` answers with a path and nothing else.** It parses
  `index.theme`, walks the inheritance chain, runs the freedesktop search over the
  `data_dirs()` it already computes, and returns the file it found. It opens no
  image, spawns no process and knows nothing about an atlas. `index.theme` is
  hand-written, as [07-dependencies.md](../07-dependencies.md) already committed
  it to be; the non-GPU crate budget is at 32 of 32 and this adds nothing to it.
- **The theme loads on the job pool, once.** 23.1 ms of directory listing for
  breeze is three frame budgets, so the walk is a job like every other and the
  lookup after it is in memory. Nothing joins rule 1's start-up list.
- **`sk-app` is the caller, because nothing else can be.** `sk-desktop` and
  `sk-preview` are both level 2 and may not see each other, so the crate that
  asks `sk-desktop` for a path, hands it to
  [`Previews::want`](../../crates/sk-preview/src/host.rs) and names the answer an
  `ImageKey` is `sk-app` — the same shape kind detection has, for the same reason.
  `sk-preview` never learns what a theme is.
- **An icon is `Ask::Preview`, not `Ask::Thumbnail`.** `Ask::Thumbnail` forces the
  cache's 128 pixels and writes the shared XDG cache, which is exactly the two
  things an icon must not do. `Ask::Preview` already names a box in device pixels
  and already returns straight RGBA8. **No wire change: the protocol version stays
  4**, and [ADR 0019](0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md)'s
  stateless child stays stateless.
- **Above `sk-gpu` an icon is a key and a rectangle**, through the
  [`Painter::icon`](../../crates/sk-ui/src/painter.rs) call every mark in the
  window already goes through — a fifth `Icon` variant beside `Folder`, `Page`,
  `Disk` and `Picture`. No `wgpu` type crosses the seam, which is
  [ADR 0003](0003-wgpu-first-backend.md).

### The atlas

**One 1024×1024 RGBA8 texture — 4 MiB — one texel until the first icon exists,
cut into four equal bands of 256 texture rows.**

| Band | Slot | Slots | Bytes a slot | Bytes a band |
| --- | --- | --- | --- | --- |
| 128 device px | 128×128 | 16 | 64 KiB | 1 MiB |
| 64 device px | 64×64 | 64 | 16 KiB | 1 MiB |
| 32 device px | 32×32 | 256 | 4 KiB | 1 MiB |
| 16 device px | 16×16 | 1,024 | 1 KiB | 1 MiB |

- **A band's slot count quadruples as its side halves**, which is the right way
  round: a details list shows the most rows and draws the smallest mark, a grid
  draws the largest mark in the fewest cells. 1,360 slots in 4 MiB, against
  [ADR 0018](0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)'s 256 in
  16 MiB.
- **A grid within each band, not shelves**, so placement is an index and eviction
  is a slot going free — 0018's argument unchanged, and the reason the glyph
  allocator's warning about heterogeneous images does not apply here either. Four
  uniform grids are four indices; one allocator general enough for all four sizes
  would be right for none of them.
- **Keyed by theme generation, name, nominal size and scale.** Scale is in the key
  rather than folded into the pixel count, because a theme's `16x16@2x` directory
  is different art from its `32x32` one; the band is the product of the two. The
  generation changes when the theme setting changes or the theme's own directory
  does, which is what makes a theme swap a re-key rather than a stale picture.
- **Drawn from the band at or above the device size, never upscaled.** A 24-pixel
  mark comes from the 32 band; a 92-pixel grid mark at 200% comes from the 128
  band.
- **Eviction and the upload budget are 0018's, unchanged**: least recently drawn,
  never a slot drawn this frame, and the same 256 KiB per frame. At the 16 band a
  slot is 1 KiB, so 256 icons arrive inside the budget that carries four
  thumbnails.

### The cache: none

- **Icons are re-rasterised at every start and nothing is written to disk.** The
  whole working set is 19.0 ms of another process's time for Adwaita and 6.4 ms
  for breeze. A cache would save that and cost a second writable path in a sandbox
  whose value is that the list is short enough to read, so
  [ADR 0034](0034-helper-execution-and-network-rights-are-explicit.md)'s grant
  stands exactly as written and the helper gains no new right.
- **The XDG thumbnail cache is refused for a second, independent reason.** Its
  entries are keyed by a source file's URI and read by every other file manager on
  the machine; an entry naming a theme's SVG at 32 pixels is a record they would
  misread and we would have to learn to skip.
- **Nothing joins rule 1's start-up list, because the window does not wait.** The
  rasterisation is in a child process; the geometric mark is drawn at the final
  position, to the logical pixel, until the real icon lands. That is
  [ADR 0018](0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)'s *one
  texel until the first thumbnail exists* in its other form, and it is why an
  icon set that takes tens of milliseconds to arrive is a gap nobody sees rather
  than a frame anybody misses.
- **This makes [05-platform-integration.md](../05-platform-integration.md)'s
  sketch precise and contradicts it in one place**: that document says everything
  is cached into a GPU atlas keyed by name, size, scale and theme generation,
  which this record adopts, and says nothing about a disk cache, which this record
  rules out with a number.

## Consequences

**The `marks` path is tested forever, which is the whole point.** The release gate
is a golden frame of each of the three sources over the same directory, so the
built-in marks have a golden frame that fails the build when they rot. It is also
what makes the row shippable before the theme half is finished: a build where the
lookup does not work yet still has a source that does.

**Two atlases in `sk-gpu`, three allocators in total.** Shelves for glyphs, one
128-pixel grid for thumbnails, four banded grids for icons. That is the
duplication 0018 already argued for, one step further along, and the honest cost
is that a fourth image kind will be tempted to add a fifth.

**4 MiB of resident memory once a themed window draws its first icon**, against
an RSS budget of 90 MB that is already missed at 117.6 MB. A window under `marks`
allocates none of it, and a window under a theme allocates all four bands at once
because they are one texture — lazy per band would be four textures and four bind
groups, and that is a worse trade than 3 MiB.

**The 128 band holds sixteen icons**, which is enough for a grid at 200% where
most cells are thumbnails and few are icons, and not enough for a hypothetical
grid of two hundred distinct file kinds at that scale. It degrades to icons that
flicker rather than to a wrong picture in a cell, which is the property the
eviction rule already guarantees; the revisit trigger below is how we would find
out.

**Icons and thumbnails share one image child and one queue.**
[ADR 0017](0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)
gives one child per kind answering one request at a time, so a fresh icon set and
a folder of two hundred photographs are in the same line. The icons are tens of
requests and they are wanted at window open, before anything asks for a
thumbnail, so the interleaving is bounded — but a directory opened during the
icon load will see its first thumbnails a few milliseconds later than it does
today, and that is a real cost of not having a cache.

**The helper builds a font database it almost never needs.** 63 to 86 ms on the
first SVG, and an icon with text in it is rare. Deferring it until a document
actually references a font is a change to
[`decode::svg`](../../bin/skerry-thumbnailer/src/decode.rs) rather than to this
record, and it is worth doing before it is worth caching pixels.

**The first run on a cold page cache is 512 ms against 67 ms warm**, which is the
font directories being read, not the icons being drawn. Any measurement taken
here reports warm and says so, and any bug report that says icons are slow on a
first boot is this number and not a defect.

**A seventh setting means touching three places**, as ADR 0027 promised: the
config's parser, the session's writer and the panel. This one costs more than the
six before it, because it is the first that is neither a truth nor a number, and
`Kind`, `Values::take` and `Values::word` all grow a case that the existing six
did not need.

**`index.theme` is a new parser on untrusted-ish input** — a file from a theme
package rather than from a download directory, but a file we did not write. It
belongs with the other hand-written parsers on the fuzzing list rather than
outside it.

## Revisit trigger

**The 128 band thrashes.** An icon evicted from the 128-pixel band and asked for
again within the next 60 frames, in a grid view on a 3840×2160 screen at 200%.
Sixteen slots is then the wrong split and the four bands stop being equal
quarters — which is a change to the table above and to nothing else in this
record.

**Re-rasterising the working set passes 250 ms** on the slowest machine in
[06-portability.md](../06-portability.md)'s target set, or a theme's working set
passes 400 icons in one band. 19.0 ms here leaves a factor of thirteen before
that bar; at 250 ms the icons stop arriving inside the time it takes somebody to
look at the window, and a cache becomes worth a second granted directory — its
own, under `$XDG_CACHE_HOME/skerry`, never the shared thumbnail one, and a
successor to [ADR 0034](0034-helper-execution-and-network-rights-are-explicit.md)
rather than an edit to it.

**A view draws a mark larger than 128 device pixels.** That is 0018's own trigger
in this atlas's terms, and the answer is the same: either the largest band grows
for everything or the atlas holds a fifth size, and the second is a different
decision from this one.
