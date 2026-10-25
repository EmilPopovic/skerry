# 0045 — Bundled symbols use the glyph mask cache

**Status:** Accepted. Supersedes the built-in geometric drawing and fresh-install
icon-source choices in [0042](0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).

## Context

The custom icon study supplies one family for files and controls. Its curves,
round joins and small emblems must survive integration without being redrawn as
overlapping GPU rectangles. The existing glyph rasterizer already handles paths
and caches coverage masks at device size. Desktop icons remain an explicit choice.

## Decision

Bundle the reviewed SVG geometry as generated, immutable path records. `sk-text`
rasterizes only these trusted records with the `zeno` rasterizer already exposed
by `swash`. Reserve `u16::MAX` as the glyph face for symbols; symbol index and layer
occupy its glyph ID. This face never enters text shaping or font discovery.

The normal glyph atlas caches each mask by symbol, layer, quarter-pixel size and
horizontal subpixel offset. A symbol has up to three masks: opaque backing,
ten-percent tint, and ink. The caller supplies colours. No filesystem access,
theme lookup, subprocess, new dependency or asynchronous arrival is involved.
The compiled paths are bounded; external SVGs still use the sandboxed decoder.

`sk-ui::Mark` names both content and controls. `sk-app` maps file types and
command identifiers to marks. Menus, the palette and chrome draw those same
marks. New configurations select `marks`; an explicitly saved desktop or named
theme continues to work.

## Verification and tradeoffs

Render every mask at small, fractional and large sizes; reject clipped or empty
ink. Capture files, menus, controls and the specimen in both palettes. Exercise
the normal cache path and compare the frame benchmark before and after. Cold
symbol rasterization shares the glyph cache's miss cost and eviction behaviour;
the path data itself is bounded by the bundled set. Revisit this choice if cold
frames or glyph-atlas pressure exceed the existing frame budget.

The editable source and generator are checked in together. The standalone HTML
review gallery remains untracked.
