# How this project measures

Every rule below was learned by getting a number wrong first, in Phase 0, and
each one names the measurement that corrected it. They are here because the cost
of relearning them is days, and because
[03-principles.md](03-principles.md) demands numbers without saying how to take
one honestly.

---

## The machine must be idle, and the load must be recorded

The same llvmpipe configuration measured **2.428 ms p99 on an idle machine and
16.008 ms beside a `cargo build`** — a factor of 6.6, and the difference between
passing an 8 ms budget and missing it twice over.

Every sweep waits for a one-minute load average under 1.3 before it starts and
records the load beside the result. A number without its load is not a number.

> **Fails if:** a benchmark result is quoted without the machine state it was
> taken on.

## A single per-core multiplier is not a portability model

Phase 0 projected reference hardware from a fast laptop with "roughly 3× slower
per core", taken from one comparison. Measured across the same pair of machines,
the real factor ranges from **2.6× to 12×** depending on the operation.

The model that works: **an operation ports at the factor of whichever memory
access pattern dominates it.** Over the same 28.5 MB working set —

| | laptop | Pi 5 | ratio |
| --- | --- | --- | --- |
| sequential read | 16.88 GB/s | 5.43 GB/s | **3.1×** |
| independent random 8-byte reads | 793.9 M/s | 26.1 M/s | **30×** |

— a linear scan ported at 2.6× against a 3.1× primitive, and a radix scatter at
12× against a 30× one. No constant could have covered both.

> **Fails if:** a budget is declared met on reference hardware without reference
> hardware having run it.

## Working-set size is a cliff, not a slope

Two machines with **the same four cores** measured **22× apart** on random reads
and **4.5× apart** on the same sort. The variable was neither core count nor
instruction set: one holds a 28 MB working set in its last-level cache and the
other does not.

A million filenames at 28 bytes each sits directly on that boundary. So
**prefer sequential passes over a compact representation to clever indirection** —
the indirection is precisely what the slow machine cannot afford, and a design
tested only on a machine with 64 MB of L3 will not reveal it.

## Predictions about memory behaviour are worse than experiments

Three predictions in Phase 0, each reasoned from a real measurement, each
plausible, each wrong:

| Prediction | Reality |
| --- | --- |
| a single 3× portability multiplier | 2.6× to 12× |
| "76% of the sort is its serial phase" | parallelising it bought 11 ms |
| "permuting the arena will help the slow machine" | 5% slower, on that machine |

The rule that follows is uncomfortable and cheap: **when the question is "why is
this slow", spend five minutes measuring rather than ten minutes reasoning.** The
reasoning is not worthless — it generates the experiment — but it does not
substitute for one.

## A test that can only pass has measured nothing

Rule 1's hung-mount test passed on its first run, and had to: a frame loop with
no path to a filesystem syscall cannot be blocked by one, so "no frame exceeded
8 ms" restated the design rather than testing it.

Adding the **control** — the same listing issued from the frame loop instead of
the job pool — produced **4 ms against 28,000 ms**, same mount, same syscall, the
only variable being which thread called it. That contrast is the result; the
first number alone was a tautology.

> **Fails if:** a gate has never been observed to fail. Break it on purpose and
> watch it fire — a banned crate added to a manifest, a bug reintroduced behind a
> flag — or it is decoration.

## Removing a misleading metric removes the dimension

Peak RSS was dropped from a results table for a good reason: `VmHWM` only rises,
so with several arms in one process the first arm's peak is every arm's peak. The
replacement — one run per arm — existed, was correct, and was never run against a
newly added arm.

The result was a **522 MB peak against an 83 MB baseline**, over a 300 MB bar,
introduced by the change that made the code fast and unnoticed for two commits.
It surfaced only because a contributed cold-cache run happened to print the line.

> **Fails if:** a metric is removed from the default output without its
> replacement being on the default path.

## Some defects are only visible to a person looking

Two text bugs — right-to-left runs rendering as overlapping glyphs, and the
`.webp` in `קובץ.webp` reordered to the far left — passed every number the
harness printed, including an assertion that every glyph was drawn 1:1 with its
raster. They were found by opening a golden frame and reading it.

A third was found the same way and by someone else: text that was crisp by every
internal measure and visibly fuzzy on screen, because the renderer never
negotiated the compositor's fractional scale. **The assertion covered
atlas-to-framebuffer; nothing covered framebuffer-to-screen.**

Golden frames on llvmpipe are therefore mandatory infrastructure, and so is
looking at them. An assertion proves the thing it asserts and is silent about the
step after it.

## One median is not a result

A Raspberry Pi's sort was reported at "0.966 s, 3.4% of margin" from a single
five-repeat median. The width curve — five repeats at each of six widths — gave
medians from **0.912 s to 1.072 s with every configuration producing runs over a
second**. The honest description was "sits on the bar", not "meets it".

Report min, median and max, and quote the spread when the decision is close. A
single median that lands low is how a project talks itself into believing a budget
is met.

## On a memory-constrained machine, "warm cache" is not warm

Five repeats of the same million-file walk on an 8 GB machine gave a **p50 of
1,136 ms and a p99 of 6,332 ms** — the signature of runs going back to the device
because the dentry cache did not survive between them.

Every other "warm" figure in these spikes was taken on a machine with 60 GB. A
warm-cache benchmark is a claim about the machine as much as the code.

## Shell plumbing quietly eats results

Three separate incidents in one Phase 0 session:

- `./tools/check | tail -3` — a pipeline's exit status is the *last* command's, so
  a red gate returned success and a commit landed with a broken link.
- `grep -E "^  ($arm|...)"` where the arm was `read_dir+meta` — `+` is an ERE
  metacharacter, so the pattern matched `read_di` followed by one-or-more `r`,
  which is nothing. **Two arms of a cold-cache run were measured and silently
  discarded**, and the run needed sudo to repeat.
- A remote job printed its `done` marker while running a **stale binary**,
  because an interrupted `scp` had failed before the `ssh` that followed it
  succeeded. Caught by comparing checksums, not by trusting the marker.

> **Fails if:** a gate's output is piped when the gate's exit code is what
> matters; or a remote result is trusted without verifying what produced it.

## A harness that drives someone else's UI is the first suspect

Spike D's drag matrix has to press a button inside another application's window,
and there is no protocol request for "pick up that peer's first item". Every pair
that fails is therefore two hypotheses at once: the peer refused, or the harness
missed. Firefox produced **three** clean, reproducible, consistent refusals, and
all three were the harness:

- The peer was matched by window class, which found the browser that was *already
  open*. "We dragged onto Firefox" meant "we dragged onto whatever was there".
- A `--profile "$(mktemp -d)"` instance could not load a profile at all under
  confinement, so the window under test had no page in it. Fixing the path fixed
  the loading and still failed, because a first-run profile fills the window with
  onboarding UI.
- A press 250 ms after a click at the same position is a **double click**, which
  opens the file instead of dragging it. Six candidate grab positions opened six
  text editors and started no drags.

The tell in all three cases was the same: a refusal that was *too clean*. A peer
that declines a type it does not want says so and completes the conversation —
Qt Designer sends `accepted false` and an `XdndFinished` that agrees. A peer that
was never really there declines everything, identically, every time.

> **Fails if:** a peer-interaction failure is recorded as a finding before the
> harness has been eliminated, or before a peer that is known to work has been run
> through the same path as a control.

## A byte-for-byte comparison is not a correctness test

[Spike E](../spikes/e-operations/README.md) found four defects. **Two of them pass
a byte-for-byte comparison of source and destination.**

- `copy_file_range` over a whole file turned a 1 GiB sparse file with 12 KiB of
  data into a gigabyte of zeros. Every byte matched. What did not match was the
  extent map, and the allocated size, and the free space on the disk.
- The `link`+`unlink` placement left a temporary name linked to the copy. Every
  byte matched. What did not match was the link count.

The pattern is that identity of *content* is the cheapest property a file has, and
the one least likely to be the thing that broke. A copy has a hole map, an
allocated size, a link count, a mode, a timestamp to the nanosecond, a set of
extended attributes and a name — and each of them is a separate comparison.

> **Fails if:** a data-preserving operation is verified by comparing contents and
> nothing else.

## A matrix whose cases all say yes has not been tested

Spike E ran green on ext4 and tmpfs. Both support reflink probing, holes,
hardlinks, symlinks, `user.*` attributes, POSIX modes and nanosecond timestamps —
so the code paths that *report what could not be preserved* were never reached,
and the distinction between "not preserved" and "not preservable" had never been
exercised.

Adding NTFS — which is not in the roadmap's list, and is what an external drive is
actually formatted as — produced six limitations and **two defects in an
afternoon**, including one where every single file failed to copy because ntfs-3g
refuses `RENAME_NOREPLACE`.

The general form: a test suite needs a case that says no, and the case that says
no is usually the one nobody thought to install.

> **Fails if:** every filesystem, compositor, driver or peer in a matrix supports
> everything under test. That is a matrix of one case wearing several hats.

## An assertion between two derived numbers can pass while both are wrong

`sk-text`'s metrics test asserted `ascent > 0` and
`line_height >= ascent + descent`. Both held. Both were **thirteen times too
large**, because `swash`'s `linear_scale` takes a ratio and had been handed a
pixel size — and the two sides of the comparison were wrong by the same factor,
so the relationship between them survived. Every baseline in the program landed
about four thousand pixels below the window, the suite was green, and the window
rendered no text at all.

The rule: **assert against the input, not against a sibling of the output.** The
test now checks the ascent against the size it asked for — `ascent > size * 0.5`
and `ascent < size * 1.5` — which cannot be satisfied by a units error.

> **Fails if:** every assertion in a test compares two values the code under test
> produced.

## A bound has to be on the quantity that runs out

The glyph atlas bounded uploads at **128 glyphs per frame**, to keep a cold cache
from monopolising a frame. Opening the statistics overlay needs about 350 new
glyphs, so two frames uploaded 256 of them and drew the remaining hundred as
blanks — text with holes in it, because an atlas *entry* exists as soon as a glyph
is rasterized and only the texels were late.

The count was never the scarce resource. A glyph mask at 13 px is about sixty
bytes; the case the bound exists for is a thousand colour emoji at 4 KiB each. The
budget is now **256 KiB per frame**, which is four thousand text glyphs and still
refuses 4 MiB of emoji.

> **Fails if:** a per-frame budget counts items whose cost varies by two orders of
> magnitude.

## An accelerator has to agree with the thing it accelerates

[Spike F](../spikes/f-index-and-search/README.md) proposed an eight-byte prefix
key to make a name sort read sequentially, with the full comparison repairing
ties. Implemented against natural-order collation, it reordered the listing: the
key compares `img10.png` against `img2.png` as `1` against `2` and returns, and
the tiebreak is never consulted. Natural order is not lexical order, so no lexical
key can be its first pass.

This is not a bug that a benchmark finds. It is a bug that a correctness test
finds, and only if the fixture contains two names that differ in a digit run.
`the_prefix_key_cannot_accelerate_a_natural_order_sort` is kept as a test rather
than a comment so that reintroducing the idea fails the build.

> **Fails if:** a fast path is measured against the slow path's speed and not
> against its output.

## The bar is quoted before the code and never adjusted after

Written into [08-roadmap.md](08-roadmap.md) for the spikes and
[09-performance-budgets.md](09-performance-budgets.md) for the product. A method
may be corrected before a result is recorded — a warmup phase that warms the
wrong thing is a bug, not a result — but **a bar that moves after seeing a number
has measured nothing.**

Where a bar turns out to be unmeetable, say so and leave it standing. Phase 0 has
three: the 90 MB "ordinary session" RSS target is below the GPU driver floor on
every device tested; `Sort 1M entries < 120 ms` is 191 ms at best on reference
hardware; and "an external `rm -rf` reflected within 100 ms" measures the
deletion rather than the notification, so it passes at 2,000 files and fails at
30,000. Each is recorded as unmet. None was quietly widened.
