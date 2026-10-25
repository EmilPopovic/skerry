# Bundled symbols

`designs.json` is the editable source for 184 original icons. Each record has a
stable identifier, semantic group, colour role and SVG geometry on a 24-unit
grid. `commands.json` maps command identifiers to that family; related actions
share a symbol. Neither file is read at runtime.

Run `python3 tools/generate-icons.py` from the repository to regenerate
`sk-ui::Mark`, `sk-text`'s trusted path records, and the app's command mapping.
The generator uses only Python's standard library and the existing Rust
formatter. It does not add a build dependency.

The runtime uses the glyph mask cache at the requested device size, including
fractional scales. Ink, a ten-percent tint and optional opaque backing are
separate masks. A badge's caller supplies its backing colour. The symbols never
enter text shaping or the font fallback chain. See [ADR
0045](../../../docs/adr/0045-bundled-symbols-use-the-glyph-mask-cache.md).

Folders have one continuous outer contour and a horizontal inner seam. Small
file emblems share their geometry with controls. Clear symbols keep the full
control stroke weight; their host contours leave space around them. Pin and
unpin use an upright silhouette, and column reset uses inward arrows between
the column boundaries.

`skerry --capture /tmp/marks.png --marks --size 1230x2320 --scale 150` draws the
whole family at 48, 32, 20 and 13 logical pixels. Review both palettes after
geometry changes. The standalone HTML design gallery stays outside version
control.
