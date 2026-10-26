# Parser mutation corpus

`valid.tar` contains a POSIX/PAX archive with ordinary and Unicode names.
`valid.zip` contains stored and DEFLATE members. Both contain synthetic public
test bytes, generated with Python's standard-library archivers. Every other
target's seeds are built beside it from the fixtures its own tests use: D-Bus
wire structures, `index.theme` groups, GIFs from the decoder's test encoder,
JPEG `APP1` segments, poppler's and mupdf's word layers, markdown documents,
copied-file lists and URIs, and scripts of X11 `INCR` chunks.

The other files are regressions, each kept with the focused test that replays
it: `dbus-nonascii-signature.bin`, and `gif-zero-width-interlaced.bin`, an
interlaced frame zero pixels wide that panicked the thumbnailer.

Run `python3 tools/fuzz-parsers.py --cases 50000 --seed 380535`. The driver limits
each test process to 2 GiB of address space, 120 CPU seconds and 150 wall seconds.
The default workspace tests run 256 mutations per target. A panic saves its input
under `/tmp/skerry-fuzz-TARGET-SEED-ITERATION.bin`; rerun with the same seed and at
least that many cases. Add every finding as a targeted regression before fixing
it. These are deterministic mutation campaigns, not coverage-guided fuzzers.
