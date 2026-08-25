# spikes

Throwaway binaries, each answering one question that could invalidate the plan,
each with a number attached. They live here after they are finished because
their measurements are the evidence behind the ADRs.

Each spike is a standalone Cargo package — not a workspace member, exempt from
the layering, lint and dependency rules, and free to measure alternatives we
would never ship.

| Spike | Question | Bar | Result |
| --- | --- | --- | --- |
| A | Can a Rust process paint a million rows of shaped text at a hard 8 ms p99? | p99 < 8 ms scrolling · crisp glyphs at fractional scale · < 300 MB RSS · zero CPU at idle | **Go.** p99 2.618 ms worst of 18 runs, 0 of 29.6 M glyph quads not 1:1 (though on-screen crispness needed a scale-negotiation fix a golden frame could not see), 0 unprompted redraws at idle; RSS missed on the NVIDIA proprietary driver alone, whose floor is 332 MB before our data exists — [a-million-rows](a-million-rows/README.md) |
| B | What does `getdents64` + batched `statx` actually buy over `read_dir`? | warm walk of 1M files < 1 s · 200 rows on screen within 30 ms · a hung mount never blocks | *not yet run* |
| C | Does inotify hold up across thousands of directories? | external `rm -rf` reflected within 100 ms · no lost entries after a forced overflow | *not yet run* |
| D | Can we be a drag-and-drop source and target against GTK and Qt apps on both display servers? | every pair works, or the failure is documented as a compositor limitation with a fallback · IME composes CJK | *not yet run* |
| E | Do the copy paths preserve everything, across every filesystem? | zero data loss across the matrix · `ENOSPC` leaves no partial file · reflink instant on btrfs | *not yet run* |

Full specifications, including what each one decides, are in
[08-roadmap.md](../docs/08-roadmap.md#phase-0--five-spikes-34-weeks). Use
`/spike` to create or resolve one.

Spike A is built twice, so the renderer comparison is measured rather than
argued: [a-million-rows](a-million-rows/README.md) is the `wgpu` + `swash` arm
and carries the bar, the method and both verdicts;
[a-million-rows-gpui](a-million-rows-gpui/README.md) is the GPUI arm.

**A bar is written before the measurement and never adjusted after seeing a
result.** A spike that moves its own bar has measured nothing.
