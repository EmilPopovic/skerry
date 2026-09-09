# Parser mutation corpus

`valid.tar` contains a POSIX/PAX archive with ordinary and Unicode names.
`valid.zip` contains stored and DEFLATE members. Both contain synthetic public
test bytes, generated with Python's standard-library archivers. D-Bus seeds are
small wire structures embedded beside its target.

Run `python3 tools/fuzz-parsers.py --cases 50000 --seed 380535`. The driver limits
each test process to 2 GiB of address space, 120 CPU seconds and 150 wall seconds.
The default workspace tests run 256 mutations per target. A panic saves its input
under `/tmp/skerry-fuzz-TARGET-SEED-ITERATION.bin`; rerun with the same seed and at
least that many cases. Add every finding as a targeted regression before fixing
it. These are deterministic mutation campaigns, not coverage-guided fuzzers.
