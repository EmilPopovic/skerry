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
| B | What does `getdents64` + batched `statx` actually buy over `read_dir`? | warm walk of 1M files < 1 s · 200 rows on screen within 30 ms · a hung mount never blocks | **Go, cold cache owed.** 0.530 s against `read_dir+meta`'s 1.195 s, which misses the bar; 0.966 s on a Raspberry Pi 5 at six workers where `read_dir+meta` takes 3.882 s, and 0.170 s on a 24-thread laptop; 0.143 ms to 200 rows; 4 ms on the pool against 28,000 ms for the same call on the frame loop — [b-enumeration](b-enumeration/README.md) |
| C | Does inotify hold up across thousands of directories? | external `rm -rf` reflected within 100 ms · no lost entries after a forced overflow | *not yet run* |
| D | Can we be a drag-and-drop source and target against GTK and Qt apps on both display servers? | every pair works, or the failure is documented as a compositor limitation with a fallback · IME composes CJK | *not yet run* |
| E | Do the copy paths preserve everything, across every filesystem? | zero data loss across the matrix · `ENOSPC` leaves no partial file · reflink instant on btrfs | *not yet run* |
| F | Can flattened search hit a 50 ms first result on 1M files, and what does an index cost? | first result < 50 ms from a keystroke on a warm 1M-file tree | *not yet run* |

Full specifications for A to E, including what each one decides, are in
[08-roadmap.md](../docs/08-roadmap.md#phase-0--five-spikes-34-weeks). Use
`/spike` to create or resolve one.

**F is not in the roadmap's original five.** It exists because
[ADR 0008](../docs/adr/0008-persistent-index-is-required.md) decided that the
persistent index is required rather than optional, and then declined to choose a
format without numbers. Spike F produces those numbers. It also covers the tree
shapes Spike B's realistic workload cannot reach — a single directory with a
million entries in it.

Spike A is built twice, so the renderer comparison is measured rather than
argued: [a-million-rows](a-million-rows/README.md) is the `wgpu` + `swash` arm
and carries the bar, the method and both verdicts;
[a-million-rows-gpui](a-million-rows-gpui/README.md) is the GPUI arm.

**A bar is written before the measurement and never adjusted after seeing a
result.** A spike that moves its own bar has measured nothing.
