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
| C | Does inotify hold up across thousands of directories? | external `rm -rf` reflected within 100 ms · no lost entries after a forced overflow · the watch budget as a number | **Go, with three things to design.** Notification is 0.063 ms on a Pi; overflow recovery is 411 ms there by pruning on directory mtime with zero false negatives, against 1,234 ms unpruned; the watch budget is per-user, shared, and not a constant (60,793–65,536) with one tree costing 34%; and the `rm -rf` bar measures the deletion, so it fails at `node_modules` scale — [c-watching](c-watching/README.md) |
| D | Can we be a drag-and-drop source and target against GTK and Qt apps on both display servers? | every pair works, or the failure is documented as a compositor limitation with a fallback · IME composes CJK | **Go, with three things to design.** Files land on disk from a drag we start; every Wayland pair passes — GTK 4, GTK 3 and Qt 6 — and Nautilus and Konsole pass on X11 too; our target reads what Nautilus gives it. Fractional scale negotiates as 150/120 and a one-physical-pixel checkerboard survives to the framebuffer; CJK composes in 2.2 ms; a hung peer's clipboard transfer costs the loop 0.394 ms against rule 1's 8 ms. Four compositor limitations are documented, and three of them report **no error at all** — [d-platform-and-dnd](d-platform-and-dnd/README.md) |
| E | Do the copy paths preserve everything, across every filesystem? | zero data loss across the matrix · `ENOSPC` leaves no partial file · reflink instant on btrfs | **Go.** All three conditions met on seven filesystems: 81 pass, 0 fail, 16 limitations, 5 owed, of 102 checks. Reflink is instant where it exists — a 192 MiB clone consumes **0 KiB** of free space against 196,608 KiB for a byte copy, in 2.0 ms against 123.4 ms; `ENOSPC` fails at exactly byte 307,200 with no destination and nothing left behind, and at the tree level 8 of 20 files land byte-exact. Four defects found by a check failing — `copy_file_range` turning a 1 GiB sparse file into a gigabyte of zeros, `RENAME_NOREPLACE` refused by ntfs-3g so *nothing* copied to an external drive, an ignored `unlink`, and a file-type test that walked a directory as a file — [e-operations](e-operations/README.md) |
| F | Can flattened search hit a 50 ms first result on 1M files, and what does an index cost? | first result < 50 ms from a keystroke on a warm 1M-file tree | **Go on the index.** 195 ms cold load on a Pi 5 against a 2.3–3.7 s cold walk; search 13.3 ms with a parallel scan; sorting 191 ms against a 120 ms target, inside the 300 ms fail — [f-index-and-search](f-index-and-search/README.md) |

Full specifications for A to E, including what each one decides, are in
[08-roadmap.md](../docs/08-roadmap.md#phase-0--spikes-34-weeks). Use
`/spike` to create or resolve one.

**Spike E is the one exception to "spike code is thrown away."** The roadmap makes
it the permanent torture matrix and gates M3 on it, so it is written to be re-run
against a filesystem it has never seen: the filesystem list is probed at runtime,
and it ships a FUSE filesystem that fails at an exact offset on demand, because a
real full disk is not a repeatable instrument.

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

**Spike D is built twice for the same reason**, because
[ADR 0002](../docs/adr/0002-own-platform-layer.md) carries a revisit trigger and
six weeks of M0 is too large a price to settle by argument.
[d-platform-and-dnd](d-platform-and-dnd/README.md) is the `wayland-client` +
`x11rb` arm and carries the bar; [d-platform-winit](d-platform-winit/README.md)
audits the same capability list against `winit`'s public API and finds four of
nine unreachable.

**A bar is written before the measurement and never adjusted after seeing a
result.** A spike that moves its own bar has measured nothing.
