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

## State keyed by an index into a rebuilt collection is a bug waiting for a rebuild

Twice now, and the second time was a user report.

An `EntryId` is an index into one listing's parallel arrays. Entry 3 of a
directory and entry 3 of the next one are different files. Anything that caches
by that index — and caching by it is the *point* of an index — serves the first
when asked for the second, and does it silently, on the frame after a navigation.

* The **text layout cache** drew the parent directory's names in the child's
  rows.
* The **folder totals** were stored in the `Listing` and their in-flight walks
  tracked by `EntryId`, so every new listing threw all of them away. A change
  anywhere in the watched directory, or a trip to the parent folder and back,
  re-walked every subtree from nothing.

Both fixes were the same shape and neither was "remember to invalidate": key the
state by something that outlives the rebuild. The layout cache gained the
listing's version; the totals moved out of the listing entirely and are keyed by
**path**, which is what a folder's size actually belongs to. The second fix also
made coming back to a directory instant, because the right key was the one that
made the cache useful rather than merely correct.

> **Fails if:** anything outside a collection remembers a position in it.

## The first suspect is rarely the culprit; instrument before fixing

The folder-total report came with a plausible cause attached — the directory was
being watched, a watch causes a refresh, a refresh looked likely to cancel the
walks. Twenty seconds of `inotify` on the same directory recorded **zero events**.
The walks were not being cancelled by the watcher at all; a trace of the running
program showed **all twenty folders reporting and nothing being dropped**, which
moved the question from "why is nothing arriving" to "why is nothing kept" and
found the real defect in a minute.

The measurement cost less than the first fix would have, and the first fix would
have been for a cause that was not occurring.

> **Fails if:** a defect is explained before it is observed.

## Every unit test can pass while the frame is wrong

Entering a subdirectory drew four rows: two of the child's files, and two of the
*parent's* — with the parent's names and the parent's sizes. Reported by the one
person using it, on the first directory they opened.

Nothing was wrong with the code the tests covered. The pane read the right
directory, published the right listing, sorted it correctly, and discarded the
abandoned read's updates; four tests said so, including one driving the reporter's
own directories through the real thread pool. The defect was one level up: the
text layout cache is keyed by *entry index*, an `EntryId` is an index, and entry 3
of one directory is a different file from entry 3 of the next. Rows that hit the
warm cache drew the previous listing's text. The cache had an `invalidate` method
whose doc comment said exactly when to call it, and no caller.

Two things came out of it, and the second is the transferable one.

**The fix was to delete the choice, not to make it.** The listing's identity is
now part of the cache key, so a new listing cannot hit an old entry and there is
nothing to remember. `invalidate` is gone. This is the same shape as the eventfd
that was never drained: both were APIs where correctness depended on a caller
remembering one line, and both were fixed by making the line unnecessary.

**The assertion that catches it is about frames, not units:** *a warm cache draws
the same frame as a cold one.* Render, navigate, render again with the same cache
and again with a fresh one, and compare the glyph streams. It needs no GPU and no
display server — `DrawList::glyphs()` exists for it — and it catches any staleness
anywhere in the layout pipeline, including the next one nobody has thought of.

> **Fails if:** the only tests of a drawing path are tests of the things it draws
> *from*.

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

## A test that documents the defect is worse than no test

`Chord::matches` compared a bound character to a pressed one exactly, and xkb
applies Shift before the event arrives — so `Ctrl+Shift+N` came through as `N`,
the binding was written `n`, and **three commands were bound to chords no
keyboard can produce**: New folder, Copy the path as text, and Redo. Two had no
other key. Nothing in the interface hinted at it; the keyboard overlay read the
chord out of the registry and displayed `Ctrl Shift N` correctly.

The reason it lasted was a test named
`shift_counts_for_a_named_key_and_not_for_a_character`, which asserted that
`Ctrl+Shift+A` runs Select All. That is true of the code and false of the
product, and a green test with a confident name is the most expensive comment in
a repository: it is read as a decision that has already been examined.

The rule: **a test asserts what the product promises, not what the code does.**
The replacement states the rule in both directions — two chords for a letter, one
for punctuation — and a second test presses the three chords as xkb actually
sends them.

> **Fails if:** a test's name asserts a design decision that nobody outside the
> function it tests would recognise.

## A comment can be left on the wrong item, six times

Inserting a function above an existing one, with no blank line between the two
comments, gives the new item the old one's doc comment and leaves the old one
with none. It happened **six times** while the operations and the context menu
were written — the mechanical consequence of editing files with textual
replacement — and two of them survived a commit. `missing_docs` catches only the
cases where the orphaned item is public, which was one of the six.

The shape turned out to be precise enough to check: this project's doc comments
are a one-line summary, a blank `///`, then prose, so **a summary line followed
immediately by the start of another sentence is two comments run together**.
Measured against the whole tree before it was added: two hits, both real, no
false positives. `tools/check-invariants.py` fails the build on it.

> **Fails if:** a doc comment's second line begins a new sentence rather than
> continuing the first.

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

## A clock that starts after the expensive part measures the cheap part

The frame number was `build_us + cpu_us`, and the clock started at the top of the
immediate-mode pass. Everything before it was excluded: taking delivery of a
10 MB `Listing`, writing 2,048 metadata rows into it, scheduling folder totals —
the two most expensive things the loop does, both of them between two frames each
reported as sub-millisecond. The benchmark then made the exclusion permanent by
reading the directory to completion *before* the trace, so those steps could not
happen during it at all.

Nothing about either choice was dishonest at the time; each was a local decision
about where a function begins. Together they reported 0.445 ms for a row named
"Keystroke to frame" and the true worst frame was **eleven times that**. Moving
the clock to where the loop stops waiting, and starting the read underneath the
trace, produced 0.34 ms p99 for the scroll and a second number that had never
existed: 2.5–5.0 ms while the listing lands.

The rule: **the clock starts where the user's wait starts.** For a frame loop that
is the instant the event arrives, not the instant drawing begins. And a trace that
never overlaps the loading it claims to cover is not a trace of that work.

> **Fails if:** a frame timer starts inside the render function, or a benchmark's
> setup does work that the measured loop does in production.

## A cache key that says where, not what, serves the old string forever

The layout cache keyed a shaped run by listing, row, column, weight and size —
deliberately, because hashing twelve bytes beats hashing a filename, and the
listing in the key is what stops row 3 of one directory from drawing row 3 of the
previous one. What the key does not carry is the string.

That is fine for a filename, which cannot change under a fixed row of a fixed
listing. It is wrong for the two cells in this product that can: a folder total
climbing while the walk runs, and a `Modified` cell reading `21:40` that becomes
`26 Aug` when the clock passes midnight. Both drew the first string ever shaped
there and never anything else. The debug assertion meant to catch exactly this
compared `source_len`, so `1.2 GB` becoming `1.3 GB` passed it.

The fix is a 64-bit fingerprint of the content stored beside the run and checked
on every hit, inside `get_or_shape` rather than at the call site — a caller cannot
forget a check it is not offered. Measured over a 900-frame scroll at 200,000
entries: p50 0.191 ms before and after, p99 within run-to-run variance.

> **Fails if:** a cache is keyed by position and any string at that position is
> derived from something other than the collection the key names.

## A component's own log is the first place to look, not the last

The window died on GNOME with `error 7: failed to import supplied dmabufs`, and
it was written down as "may be the nested software path rather than anything
about GNOME" — a guess, recorded honestly as a guess, and wrong. mutter's own log
had already answered it, four lines above the failure:

```
Added device '/dev/dri/renderD128' (nvidia-drm)
Added device '/dev/dri/renderD129' (amdgpu)
GPU /dev/dri/renderD128 selected as primary
```

Two GPUs, and the compositor had picked the other one. Nothing about nesting,
nothing about software rendering, and nothing that a real GNOME session would
have made go away — it is every hybrid laptop. The diagnosis cost one `grep` of a
log file that had been written to disk the whole time.

> **Fails if:** an unexplained failure in somebody else's component is recorded
> as unexplained without reading what that component said about it.

## Contention and waiting look the same and are not the same

The worst frame in the product is the one where a large directory lands, 2.5–6.2
ms of an 8 ms budget, and the pool is as wide as the machine — so every core is
inside `statx` while the loop is trying to draw. Lowering the workers to nice +5
is the obvious fix and it is worse: eight runs each, 2.6–6.2 ms before and
3.8–9.5 ms after, with the only over-budget frame ever recorded on this machine
among the niced ones.

It is worse because the loop is not only competing with the pool for a core; it
is *waiting* for the pool to hand it something to draw. Making the workers slower
lengthens the window in which there is anything to contend over at all. A
priority change helps a loop that would have work to do anyway, and this one
would not.

The rule: **before lowering something's priority, establish that the thing being
protected is not blocked on it.** The two are indistinguishable in a profile that
shows only CPU time.

> **Fails if:** a scheduling change is made on the strength of "these threads are
> busy" without a measurement of the thing they are supposedly starving.

## A harness that leaks is a harness that eventually reports somebody else's bug

`tools/gui-test` started a nested compositor per run and signalled its process
group at the end. Hyprland re-executes itself into a session of its own, so the
process the harness started was a launcher that exited and the compositor was
re-parented to init, in a group `killpg` could not reach. Nothing was slow,
nothing failed, and the count went up by one per run.

What broke, after about eighty runs, was `sk-fs`'s **unit tests** — three of
them, with `Too many open files`, in code that had not been touched. Each
compositor holds inotify instances and the per-user limit is 128. The failure
appeared as far from its cause as it is possible to get: a crate that does not
know the harness exists, in a suite that runs without one.

The cleanup now signals the group, waits to see the process go, and then kills by
`--config` path — which names this run's compositor and no other, and is what
makes killing by pattern safe at all. The harness also counts running Hyprland
*processes* on the way in and says so if there is a crowd. Counting instance
*directories* would not have worked: those outlive the compositor that made them,
so the count is a crowd of ghosts.

> **Fails if:** a test harness starts a process it does not confirm the death of.

## A reference is only valid against inputs that are not in the repository

`tools/golden` compares fourteen rendered frames to committed PNGs at a tolerance of
**two channel levels of 255**, which is what makes it able to catch a glyph that
moved half a pixel. That sensitivity means every input has to be pinned, and two
of them were not:

- **The timezone.** The Modified column is `localtime_r` of a fixed epoch second.
  Fixed as `2001-02-03T00:00:00Z`, which is 3 February anywhere east of UTC and
  2 February anywhere west of it.
- **The fonts.** The fixture holds Hebrew, Arabic and an emoji on purpose, so the
  whole fallback chain is in the image. The references here were rendered with
  Noto; a machine that has Inter installed renders a different frame from the same
  commit, and the comparison reports several hundred thousand differing samples
  and implicates the renderer.

The chain is now recorded in `tests/golden/faces.txt` **by the hash of each file**
and checked once, before the first comparison. The hash and not the name, because
the name is identical on machines whose font packages are not: Ubuntu 24.04 ships
`NotoColorEmoji.ttf` at 2.047 where 26.04 ships 2.051, and the beach umbrella in
the fixture is a different bitmap in each. This is what makes the frames runnable
in CI at all — and what turned "the golden frames only pass on one laptop" from an
unknown into a message that names the file.

> **Fails if:** a comparison against a stored artifact cannot say what it was
> stored against.

## An assertion has to be watched matching real output, not plausible output

The CI job for Spike E's matrix asserts on the one machine-readable line the
matrix prints:

```
  counts pass=37 fail=0 limitation=6 owed=3 total=46
```

The assertion was written as `grep -q '^counts .*fail=0'` from the line as it
appears in the spike's own README — where it is shown unindented. The matrix
prints it indented by two spaces, so the pattern matches nothing, and the gate
would have failed every run for a reason having nothing to do with any
filesystem. It was caught by running the matrix and grepping the real output,
which took a minute.

A gate that can only fail is the same defect as a test that can only pass: both
are decoration, and both look like diligence in a diff.

> **Fails if:** a pattern in a gate has never been run against the output it
> claims to match.

## A defect at frame 1 needs a harness that can see frame 1

[08-roadmap.md](08-roadmap.md) owed a 1×1 pre-map probe so that the first frame
would be at the right scale. Nothing here could show that it was wrong. A capture
is *told* its scale, so a golden frame proves nothing about it; and the GUI
harness ran its nested compositor at scale 1, where 120 is both the right answer
and the answer a program that never read `wp_fractional_scale_v1` would give.

Two things made it measurable, and neither was the fix:

- **The harness takes `--scale`**, so the compositor can be nested at 1.25. At 1
  the assertion cannot fail, which is most of what it is there to say.
- **The program reports the scale it drew its first frame at**, kept for no other
  purpose. Without it the scenario reads the scale 700 ms after start-up, by which
  time it is right — the defect is one frame wide and the state line is the only
  place a test can see one frame.

With both, the failure was one line: *the compositor is at 1.25, the first frame
was drawn at 1.0*. The cause is in the protocol rather than in the code —
`preferred_scale` arrives when the surface enters an output, a surface enters an
output when it is mapped, and a surface maps on the first commit that carries a
buffer. So a client that waits for the scale waits forever and one that draws
first draws at 100%.

> **Fails if:** a defect one frame wide is chased with a harness whose finest
> resolution is a second.

## A test calibrated on a loaded machine fails on an idle one

The scenario for cancelling a copy was written while this machine was at a load
average of **5**, and it passed. Once the machine was idle it failed **three for
three**, and the message was about the wrong thing: *after stopping, the status
line says 'Done'* — the copy had finished before the keystroke arrived.

Thirty thousand small files on tmpfs take about **350 ms** idle and about three
times that loaded. The cancel was sent 400 ms after the paste, which was
comfortably inside the window when the number was chosen and outside it once
nothing else was running. Every rule in this document about load applies to a
*test's* timing as much as to a measurement's, and this is the direction nobody
watches for: the machine getting **faster** is what broke it.

Three things fixed it, and only the third is about timing:

- **The gap has to be inside one driver script.** Each `drive` call is a fresh
  process, so a keystroke in its own call lands whenever that process starts.
- **Ask more than once.** `edit.cancel` is disabled when nothing is running, so an
  attempt outside the window does nothing — six of them 60 ms apart cover a window
  whose length is not known.
- **Assert what the feature claims, not a proxy for it.** Two of the failures were
  the assertion's fault: "fewer than ten folders arrived" is false when the stop
  lands inside the tenth, and "the undo removed files" is false when the entry it
  undoes had made its folder and not yet filled it.

> **Fails if:** a scenario's `sleep` was chosen against a machine that was busy.

And rewriting that scenario deleted two others. The edit replaced everything
between one `@scenario` and the next, and two scenarios added since had landed in
between — the same shape as the [spliced doc
comments](#a-comment-can-be-left-on-the-wrong-item-six-times), and silent for the
same reason: the only sign was the summary count going from 54 to 52, which is
noticed only by somebody who remembers what it was. `tools/gui-test` now refuses
to run when it registers fewer scenarios than a recorded floor.

> **Fails if:** a count in a summary is the only thing standing between a deleted
> test and nobody knowing.

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

## A selector that matches nothing has to fail, not select nothing

`tools/gui-test` took scenario names as substrings and silently ran none when a
name matched nothing. Asking it for a scenario that had been renamed printed
`0 passed, 0 failed` and exited 0. It cost ten minutes here — long enough to
believe a filter test had passed and move on — and in CI it would cost however
long it took somebody to notice a job that had asserted nothing since the rename.

The general shape is the one this document keeps returning to: **an invocation
that cannot fail has measured nothing.** A filter over a set of tests is an
invocation, the same as the tests themselves. It now exits 2 and names what it
could not find.

## A helper can be correct only because of the fixture it grew up in

`tools/gui-test`'s `point_at` computed a row's position as a percentage of the
**window** and handed it to a driver that reads percentages of the **output**.
Those are the same number under Hyprland, because the nested compositor makes the
window fill the screen — so for the whole life of the harness the arithmetic was
wrong and could not be observed.

The second display server observed it immediately. A 1,100×720 window in a
1,600×1,000 root put every click six rows below where it was aimed, and five
scenarios failed at once. The nudge loop that exists to absorb small offsets
could never have recovered it: the error was **proportional**, not an offset, and
a loop that adds half a row at a time cannot fix a factor.

The general form is worth stating because it is not about coordinates. **A
fixture with a coincidence in it hides every bug that coincidence covers**, and
no amount of running the same suite finds them — only a second environment where
the coincidence does not hold. That is most of the argument for the X11 backend
being tested by the same 61 scenarios rather than by a new set written against
it: a scenario that has passed on one server for a year is evidence about that
server, and the first thing it says on a second one is what it was never
checking.

## `flush` is not delivery, and a process that exits never hears the error

`skerry-drive --close` sent a `WM_DELETE_WINDOW` client message, called `flush`,
and exited. The window never closed. The message was well-formed, the window id
was right, and the receiving code was correct — and nothing arrived, six times in
a row.

Two X11 properties combined to make this invisible. Requests are asynchronous, so
`flush` queues a write rather than completing a transaction; and **errors are
asynchronous too**, so a `SendEvent` the server refused reports back on a
connection that has already been closed. A process that issues one request and
exits has no point at which either could be observed.

The fix is one round trip — any request with a reply, `GetInputFocus` by
convention — after the last thing that matters. It cost twenty minutes of
suspecting the receiving end, which had been right all along, and it is the same
shape as **the first suspect is rarely the culprit**: the new code was assumed
guilty because it was new, and the bug was in the four-line helper that talked to
it.

## An assertion between two readings of the same absent value cannot fail

`the-frame-is-at-the-compositors-scale` compares what the program scaled by
against what the display server says its scale is. On Wayland it has teeth. On
X11 it passed for three hours while proving nothing, because **both sides were
reading a resource database that was empty**: the harness's `xrdb -merge` exited
0 and wrote nothing, so the server's scale read as the default 96 DPI and the
program's read as the same default, and 120 equalled 120.

Two separate mechanisms hid it, and each is worth knowing on its own.

`xrdb -merge` and `xrdb -load` both **exit 0 against an `Xvfb` and create no
`RESOURCE_MANAGER` property at all**, so the harness's write was a no-op with a
success code. Writing the property directly with `x11rb` fixed the write and did
not fix the test — because `xprop -root RESOURCE_MANAGER` then reported `not
found` for a property that the same connection could read back. The read was a
no-op too, and the two no-ops agreed.

Underneath both: **an X server frees a client's resources when its connection
closes**, and a property set on the root is one of them. A process that writes it
and exits leaves nothing behind. `XSetCloseDownMode(RetainPermanent)` is the
request that says otherwise, and it is most of what `xrdb` is for.

The general form is the one this document keeps arriving at from new directions.
**An assertion whose two sides are derived from the same missing input is an
assertion that cannot fail**, and it looks exactly like one that is passing. The
way it was caught is the way these are always caught: a *deliberately broken*
implementation was put underneath it — `logical` set equal to the device size —
and the test still passed. That is the only question worth asking of a green
test, and it takes two minutes.

## A driver that writes and exits has written nothing

The same shape twice in one afternoon, in two different tools.
`skerry-drive --close` sent a `WM_DELETE_WINDOW`, flushed, and exited; the window
never closed. `--xft-dpi` wrote a property, read it back on the same connection
to prove it was there, and exited; the property was gone.

X11 requests are asynchronous and so are X11 errors, so a short-lived process has
no point at which either could be observed — and both failures reported success.
The two fixes are different (`GetInputFocus` as a round trip; `RetainPermanent`
before the write) and the rule behind them is one: **a tool that fires a request
and exits must prove the request landed**, either by waiting for a reply or by
reading it back from a *different* process. Reading it back from the same one
proves only that the server has it right now.

## A process is more threads than the program has written

`no-cpu-when-idle` reads `utime + stime` out of `/proc/<pid>/stat` and fails on
any tick at all. On X11 it failed at 1 and 2 ticks per three seconds, twice, and
the obvious suspect was the new backend's `pump` — a frame loop that woke on
something it should have slept through.

It had not. Traced across a three-second idle window the loop made **zero
syscalls**: it sat in one `ppoll` on the display descriptor and the pool's
eventfd and never returned. The ticks were charged to a thread inside the same
process that this repository did not write — the proprietary GPU driver's, doing
presentation work by hand because **`Xvfb` has no DRI3** and there is no other way
for it to get a frame onto that server.

The discriminator took one command and settled it in thirty seconds, where the
reasoning had already taken twenty minutes:

| Renderer under `Xvfb` | Ticks per 3 s, idle |
| --- | --- |
| Proprietary driver | 1, 1, 2 |
| `lavapipe` | 0, 0, 0 |

Two things follow. **A per-process CPU measurement is a measurement of every
thread in it**, including a driver's, and attributing it to the code under test
is the same error as attributing a frame time to the code when the machine was
loaded. And **a fixture that cannot do the thing being measured is not a
fixture**: `Xvfb` cannot present through a hardware driver, so what was being
measured there was the fallback and not the product. The X11 scenarios now run on
software Vulkan, which is what a runner has, what the golden frames already
render with, and — measured — 20% faster here than the fallback path they were
using.

## A one-in-three failure looks like flakiness and was a protocol violation

The first drag from a GTK application into this one worked. The second did not,
the third did, and the fourth did not. Every trace on *our* side was byte for
byte identical: the same `enter`, the same accept for every motion, the same
`drop`, the same `receive` on a live offer. The peer simply never served the
bytes, about a third of the time.

The temptation with a ratio like that is to call it a race in somebody else's
code and pace the test around it. Slowing the gesture from eight steps to
fourteen and from 150 ms to 220 ms changed nothing, which is what said it was not
timing. What said what it *was* is the protocol text: `wl_data_offer.finish` is
to be sent **after reading the data**, and this sent it immediately after asking
for it — so the source was told the drag was over while the transfer it was still
being asked for was in flight. Whether the bytes arrived first was a race, and it
went our way twice in three.

XDND has the same requirement and this had the same bug: `XdndFinished` was sent
with the conversion still outstanding. It had not been observed to fail, which is
worth stating rather than glossing — the X11 half was *also* wrong and passed
anyway, so "it works" was evidence about a timing window and not about the code.

Two things follow. **An identical trace on your own side is not evidence that
your side is right**, only that it is consistent; the thing to compare against is
the specification, not the previous run. And **a failure rate between zero and
one is not automatically flakiness** — the word smuggles in the assumption that
the code is correct and the environment is not, which is the assumption most
worth checking first.

## Instrumenting a race can hide it, so instrument the *next* one instead

`cut-and-paste-moves-it` failed about one run in four on X11. Two real bugs came
out of chasing it — a refused selection conversion whose lookup could never match
its own pending entry, so the pipe leaked and the job reading it waited forever
on an end-of-file that could not come; and a paste that returned silently when a
conversion was refused. The rate fell to roughly one in twenty.

Then it stopped being findable. Every attempt to watch it changed it: an
`eprintln!` per key — one write syscall — made fourteen consecutive runs pass. A
hypothesis about the driver decomposing `Ctrl+V` into a bare `v` was tested by
construction, with a round trip proving the modifier had been processed before
the key was sent, and **the rate did not move**. The hypothesis was wrong, and
the measurement said so, which is the only reason it did not become a story.

Two things this cost, and one that paid.

It cost a reading that proved nothing and was nearly taken as evidence: the
state dump showed `typed=""` after the failure, which looked like "the chord did
not decompose" and in fact meant nothing at all — the type-ahead buffer expires
after a second and the dump was taken 1.1 s later. **A field that is empty
because it was cleared and a field that is empty because it was never written
look identical**, and a dump taken after the window has closed can only produce
the first.

It cost three hypotheses, each of which pointed at a different file, and none of
which survived. That is the ordinary price of the rule this document keeps
returning to; it is not always refunded.

What paid was giving up on catching *this* occurrence and making the next one
explain itself. A chord that resolves to a command whose `enabled` says no
consumes the key and produces **nothing** — no notice, no job, no frame worth
looking at — which from outside the process is indistinguishable from a key that
never arrived. That ambiguity was the whole difficulty, and it is now one
`&'static str` in the diagnostic dump, written on a path that already ran.
`offered` is there for the same reason: "why is Paste dimmed" had no answer from
outside.

**The residual failure is unexplained and recorded as such.** One in roughly
forty runs at the last measurement, against one in four before the fixes — which
is not enough to tell "rarer" from "gone", and saying so is the point. The next
occurrence will name the command it refused.

## An assertion about what does *not* happen has to cover something

A menu opened by the path bar hangs over the list, and the row beneath it was
lighting up — a hover that promises a click which the panel will swallow. The
fix was one early return. The test written for it hovered the menu's first row
and asserted the list showed nothing.

It passed. It also passed with the fix removed. It passed with a second guard
removed, and with **both** removed — because that first menu row sits *above* the
list's first row, so nothing was under it either way. A test written for a
specific reported defect could not detect that defect.

Aiming at a row that genuinely overlaps the list fails with `hover is '1'`, which
is the bug exactly. The scenario now checks that the row it is covering is really
there before asserting that it stays dark, because **an assertion about what does
not light up is worth nothing if nothing was going to**. That check is three
lines and it is the difference between a test and a decoration.

The same session had already found this shape twice — an assertion between two
readings of an absent value, and a fixture whose coincidence hid a bug — and it
still went in. Watching a test fail is not a step to be skipped when the fix is
"obviously" right; the two minutes it costs is the only thing that separates a
green test from a green test.

## Unreachable defensive code is untested code

The same fix arrived with a companion: a `clear_hovers` that put out anything lit
at the moment the menu opened. It looked prudent and it was dead. The pointer has
to cross to the chevron to open the menu, and crossing there clears whatever it
was lighting; after that the early return means no branch that sets a hover is
reached at all.

It was removed rather than kept "just in case". A guard for a state that cannot
happen is a guard nothing exercises, and it tells the next reader that the state
*can* happen — which is worse than the two lines it saves. What replaced it is a
comment saying why nothing needs clearing.

## A frame of the full case is not a frame of the empty one

The sibling menu has had a golden frame since the day it was written: three rows,
the mark on the one the path goes through, the panel anchored under its chevron.
It looked right because it was. The *empty* case — a chevron opened on a folder
with no folders in it — had no frame at all, and it shipped as eight pixels of
background with "No folders in here" hanging out of the top and the bottom.

The arithmetic was `rows * STEP + PAD * 2`, which is correct for every menu that
has rows. With none, a panel sized by its contents is nothing but its own
padding. Any count that can be zero has that case, and the populated frame cannot
see it: the two states share no pixels to compare.

Rendering it cost one capture flag and one line in the frame list. Then the new
reference was **watched failing** against the old code — 30578 samples off,
worst by 40 levels — because a reference blessed from the fix is a reference that
has never disagreed with anything.

The first draft of the fix centred the message in a one-row box inside the panel
rather than in the panel. Both expressions round to the same baseline, so the
frame could not tell them apart, and the simpler one stayed. A second way of
saying the same number is not a safeguard; it is a thing to keep in step.

## The state a test reads has to be the state that can be wrong

The watcher was changed from one directory to a set so that two panes could both
be live. The scenario written for it split the window, put one pane in a
subdirectory, created a file in each directory, and asserted the listing changed
for one and not the other.

It passed. It would also have passed against the old watcher, because every
assertion in it was about the pane that *had the keyboard* — and the focused
pane is the one directory the old design watched. The bug it was written for is
invisible from there by construction.

The assertion that finds it is about the pane that does **not** have the
keyboard: change its directory, move the focus to it, and count the rows. With
one watch it is stale — `9 rows, wanted 10`, watched failing against a
deliberately reverted `retarget_watches`. The pane that cannot be wrong is not
the pane to look at.

## An invariant that lives in a doc comment is a comment

`menu::place` carried this, written when it was: "**Flipped, never clipped.** A
menu whose bottom rows are off the window is a menu whose last two entries do not
exist, and the last two entries of the row menu are Rename and Move to Trash."

The function flips the menu to the other side of the pointer and then clamps it
into the window. Neither can help when the menu is taller than the window it is
in, and nothing said so: the clamp silently produced a panel running off the
bottom edge, with its last rows unreachable. Six new commands took the background
menu past 520 logical pixels and the golden frame showed it immediately — a frame
nobody would have thought to take of "the menu, but longer".

The menu scrolls now, with an arrow at whichever end has more. The lesson is not
about menus: **a rule worth writing at the top of a function is worth a test
underneath it**, because the prose is what a reader believes and the code is what
runs. This one was believed for four milestones.

## Some code can only ever have unit tests, and that decides where it lives

Touchpad gestures have no test harness and never will: neither
`zwlr_virtual_pointer_manager_v1` nor `XTEST` can synthesise a swipe or a pinch,
so the 73 GUI scenarios cannot reach a single line of the feature.

That is a reason to put the *decisions* somewhere a unit test can see them. How
far a swipe travels before it counts, which axis wins when a hand sweeps
diagonally, and how much of a pinch is one step of zoom all live in
`sk_plat::input` — the same module the wheel notch and the double-click window
live in, and for the same reason. What is left in each backend is a `match` on a
protocol enum that calls them.

Five unit tests cover the policy, and two mutations were watched failing three of
them: accumulating a swipe's deltas by assignment rather than addition, and
measuring a pinch's step against where the fingers started rather than against
the last step. Neither mutation is far-fetched — the second is the obvious first
draft, and it emits a step per event on a slow pinch and none on a fast one.

The plumbing above is untested and says so. The alternative — policy inside the
backends, where it would be equally untestable *and* duplicated — is how the two
display servers come to disagree about what a gesture is.

## A test written against the wrong premise passes for the wrong reason

The Properties panel decides whether to print what a file *occupies* beside what
it claims. The first rule was "a tenth apart", and the test was written to say
that a small file in a big block stays quiet.

Both were wrong, and the test caught it in the first run: 4,096 bytes occupying
4,608 is 12.5% apart, so the rule printed the line the test said it would not.
The premise was the mistake — a 40-byte file occupies a hundred times what it
claims, and *no* percentage separates that from a sparse gigabyte.

The rule that works is not a ratio at all: print it when a file occupies **less**
than it claims, which is sparse or compressed, or more than a block of slack,
which is reserved space. Both are surprising and both are worth a line; a whole
number of blocks is neither.

The lesson is not "test first". It is that a test written from the *intent* — a
small file stays quiet, a sparse file does not — fails against a rule that
merely sounds right, where a test written from the implementation would have
agreed with it. Three cases were cheaper than the rule they replaced.

## The filter that hides a failure is the filter you wrote

Two scenario runs in one afternoon reported a failure whose name nobody could
read. The first was `./tools/gui-test --x11 2>&1 | tail -3`, which printed
`73 passed, 0 failed` from the *Wayland* half and, from the X11 half, nothing at
all — the run had refused on a stale `:80` lock and said so in a line beginning
`fixture:`, which the filter dropped. The second was
`grep -E "FAIL|passed,"` against a log kept only three lines deep, which reported
`74 passed, 1 failed` and threw away which one.

Both filters were written to keep the output short while something else was being
worked on. Both turned a specific failure into an unexplained number, and the
second cost a full re-run to recover what the first run already knew.

Keep the whole log and grep the file: `./tools/gui-test --x11 > run.log 2>&1`
then `grep -E "FAIL|passed," run.log`. A pipe discards; a file does not. This is
the same rule as **silence is not success** one level up — a filter narrow enough
to be tidy is narrow enough to hide the thing it was watching for.

## A suite is a load test of whatever else is running

The same X11 suite failed twice and passed twice in ten minutes. The two failures
were `cursor is '', wanted 'alpha'` and a copy whose progress line was already
`Copied 10 items` — a listing and a copy that had not finished, and one that had
finished early. Both ran while a `cargo build --release` and a second display
server were competing for the same cores; both passed alone, and the whole suite
passed at load average 1.75 with nothing else started.

Nothing was wrong with the code and nothing was wrong with the scenarios. What
was wrong was running them next to a build, which is the *same* 6× that
[the machine must be idle](#the-machine-must-be-idle-and-the-load-must-be-recorded)
prices for benchmarks — and which is easy to forget for a test suite, because a
test suite is supposed to be a yes or a no rather than a measurement. A scenario
that waits 300 ms for a directory read is a measurement.

**That last paragraph was wrong, and it was wrong in the direction that costs
most.** Months later the X11 half failed one or two scenarios per full sweep on a
*quiet* machine — three runs, three different names, and one of them on a commit
with nothing of that day's work applied. The conclusion above had excused four
real defects as somebody else's load, which is precisely what
[a flaky test is a defect in the test until proven otherwise](#five-flaky-scenarios-one-defect-a-sleep-where-a-wait-belonged)
says not to do. The evidence that settles it: with the four sleeps replaced by
waits, the same suite passes with **twelve spinning processes** on a
twenty-four-thread machine, which is far more load than the build that was blamed.

So the two lessons compose, and the order matters. Load *is* a confound, and a
figure taken beside a build is worth nothing. But for a suite, load is an
**instrument**: it does not create the failure, it reveals which scenarios are
measurements pretending to be assertions. One deliberately loaded sweep found
what four quiet sweeps and an hour had not, and a suite that survives one is
telling the truth about the code rather than about the afternoon.

### And a unit suite is one too — this one found a fixture two tests shared

The rule above was written about the GUI scenarios, where the failures are timing.
It applies unchanged to `cargo test`, where they are not.

`view.rs` had one `fixture()` helper building a directory of known contents at
`temp_dir()/skerry-view-{pid}`, and the first thing it does is delete that
directory. That was correct for as long as it had **one** caller. Adding a second
test that wanted the same fixture made it a `create_dir_all` racing another
test's `remove_dir_all`, because `cargo test` runs the tests of one crate on
threads of one process — so a path keyed by the process id is a path every test
in the crate shares.

It passed twelve consecutive runs on an idle machine, and failed one run in ten
with twelve spinning processes beside it. It also failed exactly once in the whole
session's real work, in a `./tools/check` run whose output was not kept, which is
the shape this class of bug has: a green suite, a single unexplained red, and
nothing to look at afterwards.

**A fixture is named after the test that asks for it, not the process that runs
it.** The process id keeps two concurrent `cargo test` invocations apart; only the
test's own name keeps the tests inside one invocation apart. Fourteen loaded runs
after the fix, zero failures.

The general form is the same as everywhere else in this file: a helper that was
correct with one caller became a race with two, and nothing in the type system,
the review or the quiet suite could say so. Load was the only instrument that
could.

## The received wisdom about a library is worth one probe

"Do not feed modifier keysyms to `xkb_compose_state_feed`" is advice that appears
in every toolkit's input code, and a guard for it went in with a confident
comment: a `Shift` press mid-sequence would cancel the sequence, so every capital
letter in one would break.

`xkbcommon-compose.h` says the opposite in its second paragraph — "some keysyms,
such as keysyms for modifier keys, are ignored" — and twenty lines of probe
confirmed it: feeding `Shift_L` between `Multi_key` and `a` returns `Ignored`,
leaves the status at `Composing`, and the sequence completes. The guard was a
guard against nothing, and its comment was a false explanation sitting where the
next reader would believe it.

The probe found the *real* invariant on the way past, which the guard had been
hiding. An ignored keysym does not trigger the automatic reset that a real one
does, so a `Shift` press after a sequence *finishes* leaves the status at
`Composed` — and a caller that had not reset reads the same character out of it
again and types `áá`. That reset is now load-bearing, has a test, and the test
was watched failing without it.

Two guards, identical in appearance: one dead, one holding up the feature. The
difference took one probe against the header on this machine, and no amount of
reading around the subject would have decided it.

## A protocol client with nobody on the other end is a client that cannot fail

`text-input-v3` is how an input method reaches a window on Wayland. Written
against the specification alone, it looked right, compiled, and typed nothing —
and there was no way to tell that from a client that worked, because with no
input method running the whole conversation is a client talking to itself.

So the harness got one: `skerry-drive --ime` speaks `zwp_input_method_v2`, the
protocol ibus and fcitx use, waits for the window to enable its text input, shows
a preedit, holds it, and commits. It found three defects on its first three runs,
none of which a reading of the specification had:

* **The serial rule read backwards.** `done` carries the number of commits the
  compositor has seen, and the first draft dropped any change whose serial did
  not match. The specification says the opposite in as many words — "must
  proceed with evaluating and applying the changes as normal" — and what a
  mismatch withholds is the *client's own* pending state.
* **`enter` and `leave` treated as nothing.** A comment said this window has one
  surface so there is nothing to do with either. An input method appearing while
  a field is open produces a `leave` and then an `enter`, and a client that
  enabled once at the start goes deaf the moment anybody starts one.
* **Enabling before `enter`.** The correction to the second defect re-enabled on
  every pass, which is a request the compositor answers with a `leave` — so the
  fix reproduced the symptom by a different route.

The golden frame found a fourth on its own: the preedit's underline was placed at
`snap(baseline + 2)`, and `baseline` is *already* device pixels. At 100% `snap`
is the identity and the line is exactly where it belongs. At 125% it is a quarter
of the way down the window and clipped away — which is why that frame is rendered
at 125%, and which is the trap
[CLAUDE.md](../CLAUDE.md) names in its third bullet.

The peer is a hundred and fifty lines. The rule is not "write a peer for every
protocol" — it is that **a protocol whose other end never speaks cannot be tested
by exercising this end**, and either a peer exists or the code ships as a hope.

## A count is not an identity, and a test that counts cannot see a swap

`Panes::close_leaf` removes the focused pane. Its test split three ways, closed
twice, asserted the count went 3 → 2 → 1, and asserted the last one refuses. It
passed from the day it was written, and the function **kept the leaf it was asked
to remove and closed its sibling** — because `collapse` named its branches
`keep_second` and set the flag the wrong way round.

Closing either of two leaves leaves one. Nothing a count can say tells them
apart, so the assertion was true of the bug and of the fix alike. It surfaced two
commits later, from a different direction: a tab dragged out of a leaf left the
*emptied* leaf standing and deleted the one with the files in it, and the new
test failed with `left: 1, right: 2` — a leaf that should have had two tabs and
had one.

The test now names the leaf: `let going = panes.focus()` before, and
`panes.leaves().all(|leaf| leaf.id != going)` after. Three lines, and watched
failing against the original inversion.

**Where a value has an identity, assert about the identity.** A count, a length
and a `is_some()` are all the same trap: they are true of the right answer and of
the wrong one that happens to be the same size.

## Five flaky scenarios, one defect: a sleep where a wait belonged

Five full runs of the GUI suite produced five failures in five *different*
scenarios, each passing on its own. It read as an unreliable machine. It was one
mistake, made five times:

    run.drive("sleep 400\n key down\n ...")   # is the listing there yet?

A directory read takes as long as it takes. A `sleep` before the first key is a
guess at that, and a guess calibrated on an idle machine is wrong on a busy one —
so `Down` moved a cursor that did not exist yet, a row count was read as zero and
every later assertion compared against that zero, a copy's progress was sampled
once after the copy had finished, and the idle-CPU gate started measuring while a
folder walk was still running.

The window already reports everything needed to *ask*: `loading`, `rows`, and how
many size walks are running. `listed(run)` polls the state until the read has
landed, and the four scenarios that guessed now wait. The copy samples throughout
its window instead of at one instant.

Two things worth keeping from this. The first is that **a flaky test is a defect
in the test until proven otherwise** — five different names made it look
environmental, and grouping them by *shape* rather than by name found the one
cause. The second is that every repair was checked for still being able to fail:
the idle gate against a frame loop that sleeps on an 8 ms timer, the watch
scenario against a watcher that watches only the focused pane. A test made
reliable by weakening it is worse than a flaky one, because nothing announces it.

**It happened again, in the four scenarios the first pass did not reach**, and the
second time the fix went one level down. Three of them guessed at an operation
rather than at a read — `busy` and `queue` say when a copy or a new folder has
finished, and `sleep 900` says when it finished on the machine the scenario was
written on. The fourth guessed at a *process exit*: `sleep 700` after the gesture
that closes the last tab, where the answer is `wait(timeout=)` on the child.

The level below is what stops there being a fifth. `Run.drive` now counts the
`key f12` lines in the script it was handed and does not return until the window
has printed that many answers, so the trailing `sleep 200` after an `F12` is a
lower bound instead of the whole guarantee — which is what one of the four was
failing on, three assertions later and under a message about the *window* rather
than about the wait. The sleeps stay: they are also what gives a pointer motion
time to be drawn, and a motion has nothing to print.

## A default is not the same answer as "no answer", and one of them leaks

Per-folder view settings looked right in every unit test and were unusable in a
window. `Views::for_path` answered `Option<View>`, and `None` — for a folder
nobody had chosen a layout for — was documented as *leave the pane alone*. The
reasoning was sound: a pane restored from a session already has a layout, and
overwriting it with a default would throw away what was restored.

What "leave it alone" actually leaves the pane with is **the previous folder's
layout**. So:

    set Downloads grouped        →  Downloads has a record
    walk into Pictures           →  no record, pane left alone, Pictures grouped
    un-group Pictures            →  already its answer, nothing recorded
    Downloads, then Pictures     →  no record, pane left alone, grouped again

Every step is the code doing exactly what it says. The defect is that a folder
with no opinion and a folder whose opinion is the default were the same value, and
the honest answer to "how should this folder look" is never "however the last one
did". `for_path` returns a `View` now and the default is a real answer.

The session had to give up its copy for the same reason. It carried the mode and
the grouping per tab, and the folder store carried them per path; two files that
can disagree about one thing is the shape of "state does not survive normal use",
whichever of them happens to win. The folder owns it, the session skips the tokens
it used to write, and a file from the two builds that wrote them still restores
its tabs.

**The general rule: an `Option` whose `None` means "inherit from whatever was
there before" is not an absence, it is a hidden read of mutable state.** If the
caller cannot name what `None` resolves to, it will resolve to something nobody
chose.

Two more from the same feature, both invisible to the tests that existed:

- The band of an entry came from `mtime`, which `getdents64` does not report, and
  a *name* sort was the one order that never asked for the metadata pass to redo
  it. So a grouped folder banded every entry on 1970 and put the lot under
  *Older*, permanently. The first test written for it asserted the bands and
  passed without the fix, because it recomputed them from metadata that had since
  landed; what breaks is the display *order*, and asserting that the band never
  goes backwards down the rows reproduces it.
- A heading's rule was drawn from `Painter::baseline`, which returns **device**
  pixels, through `Painter::hairline`, which takes **logical** and scales for
  itself. Right at 100% and 1.56× out at 125%. This page already carries that
  lesson under a different feature; the reason it recurred is that neither
  function's *name* says which space it speaks, and the call compiles either way.

## A gate whose premise is occasionally false is a gate that occasionally lies

`no-cpu-when-idle` is rule 1's gate: three seconds of a window nobody is
touching, and any CPU at all is a failure. It failed once in four or five full
sweeps, always with **exactly one tick**, and passed six times in isolation and
eight times under twelve spinning processes. Load was not the variable and the
scenario was not at fault.

The premise was. The scenario assumes nothing will happen to the window for three
seconds, and in a nested compositor that has mapped and unmapped a hundred
windows, occasionally something does. A window handed an event draws a frame for
it and is behaving *correctly* — it is simply not idle, so the measurement is
void rather than failed. **The invariant is not "no CPU", it is "no CPU without
something to do for it."**

The window already reports `frames`, so the two can be told apart: a loop on a
timer burns CPU in every window and draws nothing, and a stray event burns it in
one window and draws a frame. The gate now says that in as many words.

Two things this cost, and the second is the reusable one.

The first version counted *any* movement in `frames` as an event. Measured, an
`F12` draws exactly one frame by itself — the very report used to read the
counter moves it — so the "drew nothing" branch could never be reached and the
whole discriminator was dead code. The gate had quietly become a three-window
retry with an explanation attached to it. **A counter read by an instrument that
also moves the counter has to have the instrument's own contribution subtracted,
and the size of that contribution is a measurement rather than an assumption.**

The second is that the repair was checked against a real violation, which is
where the first version was caught. A 100 ms timer injected into the frame loop's
poll **passed both the old gate and the new one**: thirty wakeups that do nothing
cost less than a single scheduler tick, so the cheapest form of the thing this
gate exists to forbid is invisible to it. A zero-length timeout — a busy loop —
fails it on the first window, at 299 ticks in three seconds. The gate catches a
loop that spins and does not catch a loop that merely wakes, and that is worth
knowing about it rather than assuming otherwise.

## A wait that gives up quietly hands back the answer to the previous question

The fix above put the guarantee one level down: `Run.drive` counted the `key f12`
lines in its script and waited for the window to print that many answers. Three
things were wrong with it, and each hid the next.

**The counter was one short, always.** `reported()` counted occurrences of
`"\nskerry-state "` — the marker with a newline in front of it — so the *first*
state line in a log, which has nothing in front of it, was never counted. Every
scenario's opening `drive` therefore waited its full five seconds for an answer
that was already there, and every scenario paid that. It is invisible in a green
suite: the wait ends, the assertion passes, and the only symptom is a sweep that
takes longer than it should.

**The wait then gave up silently.** Five seconds — `A_FRAME`, a frame and not a
job — and then it returned, with no complaint. Deliberately: a drive that is
*meant* to go unanswered must not cost the long patience, and `yield_focus` asks
ten times.

**And the callers could not tell.** All three pollers did this next:

    run.drive("key f12\nsleep 120\n")
    state = run.state()        # the last `skerry-state` line in the log

When the answer had not come, that line was the one from *before* the question.
Nothing distinguishes a stale reading from a fresh one, and the values these
pollers wait on are exactly the values that are already right on an idle window:
`busy` and `loading` are both false before an operation starts and after it ends.
So `settled()` — the function whose whole job is to wait for the keystrokes to
land — returned **on its first try, immediately**, holding a state from before
them.

Measured: `a-permanent-delete-asks-in-a-panel` failed about once in nine on X11
under twelve spinning processes, and passed twenty-five times alone. It read as
the window ignoring two keystrokes. Every diagnostic made it read *more* like
that: the state dump at the failure was pristine — no panel, no queue, no undo
entry, the selection still exactly where the scenario had put it — because it was
the dump from before the keys were pressed. Three rounds of instrumenting the
window looked in the wrong process. The fourth printed the state from before the
presses beside the state from after, and they were identical field for field,
which no two readings of a live window ever are.

The same defect had eaten a second check whole. `Peer.yield_focus` asks the
compositor for the keyboard and then *verifies* it — with `run.state()` under a
`suppress(Failure)`, which reads the last line in a log that every scenario
already has lines in by the time it starts a peer. It could not raise. The loop
returned on its first attempt whether or not focus had moved, and had never once
gone round twice.

Three changes, and the third is the general one:

- Count the lines the way they are parsed. `reported()` now counts lines that
  start with the marker, which is what `states()` does.
- The pollers ask for an answer to **their own** `F12`. `Run.answered()` records
  the report count, drives, waits, and returns `None` — not a stale dict — unless
  the count moved. A poller is the one caller that must be able to be told no,
  because asking again is what it does.
- **The wait moved to where the answer is used.** `drive` no longer waits at all;
  it adds the `F12`s it sent to a running total, and `state()` blocks until the
  window has answered that many. A one-shot `drive(...)` then `state()` — which
  is 57 of the call sites, most of them opening with a `sleep` that guesses at a
  directory read — cannot read a stale line any more, and a slow window costs
  seconds instead of a verdict.

**Taking a stall out breaks whatever was leaning on it.** Five seconds per
scenario is not a delay anybody chose, but it was a delay everything ran with,
and two scenarios had grown into it.

`copy-and-paste-a-file` is the plainer of the two and the more general lesson. It
pressed `Ctrl+V` and then called `settled`, which waits for `busy` and `loading`
to be false — and in the moment after a keystroke the honest answer to *is
anything in flight* is **not yet**, which is the same answer as *not any more*.
The wait returned before the paste had started, one X11 run in three, and
reported an empty folder. **A wait for quiet is not a wait for an event**: the
fix is to wait for the row the paste is supposed to produce, which is also the
thing being asserted.

`a-paste-takes-what-another-application-copied` is the subtler one: the
scenario makes a folder in a directory the window is *already* listing, so the
row arrives by inotify rather than in the first read, and the stall was what gave
the watch time to fire before type-ahead went looking for the row. It failed the
moment the stall went, on the first assertion after the folder — which is the
right place for it to fail and eleven months later than it should have. It waits
for the row count to move now, and takes 1.5 s instead of 30.

Expect one of these for every hidden wait removed, and treat each as the defect
it always was rather than as a regression in the change that exposed it.

Two wrong turns on the way, both worth recording.

The first is worth recording because it was wrong in an
instructive way. Making `drive` *raise* when its answers had not arrived in five
seconds looked like the strict, honest choice; it failed six scenarios on the
first sweep. A ceiling short enough to be worth having is one a cold window
misses, and a wait whose only outcomes are "answered" and "the run is over" has
no way to express the ordinary case of *not yet*.

The second is subtler. The pollers counted **tries**, not seconds — forty of
them — which was a duration only by accident: each try took as long as a `drive`
took, so it had been minutes while every drive stalled for five seconds, and
became six seconds the moment that was fixed. Nothing in the code changed
meaning; a number that had never been written down quietly moved by a factor of
thirty, and an assertion that had never once been given time to be true began
failing at 5.8 s. **A budget measured in iterations is not a budget.** They wait
on a clock now.

The rule that survives both: **a wait that can time out must return whether it
timed out, and the caller decides what that means.** A `bool` nobody reads is a
latent stale read at every call site; a `raise` nobody can catch is a flaky
suite.

## A harness coordinate derived from another region is a coordinate that moves

Moving the four navigation buttons and the breadcrumbs out of one window-wide
strip and into each pane's own bar broke no test in `cargo test` and no golden
frame's *logic* — and it silently invalidated nine aim points in
`tools/gui-test`, each of which would have gone on producing a plausible answer.

They all read some variation of `body_top / 4`: a quarter of the way from the top
of the window to the top of the list. That was never a coordinate of anything. It
was a proxy for "somewhere in the chrome above the list", correct only while the
chrome above the list was exactly one strip. With a tab strip and a pane bar up
there instead, the same arithmetic lands in the tab strip — where a click
switches tabs, a hover reports nothing, and `tool_hover` reads `none`. The
scenario would have failed with "aimed at back and hit `None`", which reads as a
hit-testing bug in the code under test.

The harness already had the right pattern and used it everywhere else: `tab_aim`,
`divider_aim`, `separator_aim`, `menu_aim`, `plus_aim` and `erase_aim` all take
their coordinates from the rectangle the frame *drew*, reported on the diagnostic
line. Those needed no change at all, because a rectangle that moves reports that
it moved.

**A harness may compute an aim from the geometry of the thing it is aiming at,
and from nothing else.** Not from a neighbour, not from a fraction of the window,
not from a constant that happened to be inside the target when it was written.
The three chrome regions this change touched now report `bar_y`, `bar_h`,
`tools_x`, `title_h`, `parent_crumb_y` and `pane_bar`, and every aim that used to
be a fraction reads one of them.

The general form, which is the same shape as every other rule in this file: an
instrument that reports the right answer for the wrong reason is not an
instrument. It is a coincidence with a test name.

## An accessor written for a caller that never arrived

`Scroll::animating` returned whether a scroll owed another frame. Its doc comment
said so — "whether an animation is in flight, and therefore whether the frame
loop owes another frame" — and it had **no caller anywhere in the workspace**.

Nothing complained. It is `pub` in a library crate, so the dead-code lint has no
opinion; the crate compiles, the function is tested, and the tests pass. What it
cost was the feature it existed for. `App::timeout` decides how long the loop may
sleep, the loop requests no `wl_surface.frame` callbacks, and the scroll animation
was not among the deadlines — so after a frame was submitted, nothing would wake
the loop again. Every eased scroll moved once and then slept.

The reason nobody noticed for the life of the code is worth more than the defect.
`Scroll::advance` is handed the time since the *last* frame, and after an idle
window that is seconds. `1 - e^(-2/0.03)` is 1.0, so a wheel notch arriving on an
idle window covered its entire distance in the single frame it got. **The easing
was not visibly broken. It was invisibly absent** — every notch landed as a jump,
which is precisely the thing the module's own doc comment says the easing exists
to prevent.

The scenario that was supposed to cover this asserted the right thing in the
wrong units:

```python
if float(after["scroll"]) <= float(start["scroll"]):
    raise Failure(f"a wheel scroll moved nothing: {after['scroll']}")
```

A *direction*. Anything that moved down passed, including a list that jumped its
whole distance in one frame with no animation at all, and including a fling that
coasted three per cent of the way and stopped. Two of the rules already in this
file combine into this one: a test that can only pass has measured nothing, and a
gate nobody has watched fail is decoration.

Three things to take from it.

**An animation is a wake-up, and a wake-up has an owner.** Wherever the loop
decides how long to sleep is the complete list of things that can continue without
input. A feature that animates and is not on that list does not animate; it moves
once. The list is four lines long and it is the one place worth re-reading when
something moves and then stops.

**A `pub` accessor in a workspace of one binary is dead code the compiler cannot
see.** The eleven crates here are one application divided into levels, not
libraries for anybody else — which is stated in the workspace manifest for
`cargo-deny`'s benefit and is just as true for this. Any accessor whose only
purpose is to answer one question for one caller is worth grepping for a caller
the day it is written.

**Assert the distance, not the sign.** The wheel scenario now asserts against a
fixture filled until the content exceeds the window, and the flick scenario is
differential: the same twenty deltas sent twice, once lifting at speed and once
after resting 300 ms, with only the first allowed to coast. A compositor decides
what a finger's delta is worth, so an absolute number there would be a test of
Hyprland's configuration — but the *ratio* between a lift at speed and a lift at
rest is entirely this code's, and it is 1.0 when the feature is missing.

## Thirty-seven golden frames, one window size

Capping the name column changes the Details view's geometry on any pane wide
enough to hit the cap. Thirty-seven golden frames cover that view — two scales of
it, a light palette, a split, a grid, four groupings, a search, eight panels —
and **two of them changed**, both because a command had been added to a list.

The layout change was invisible to all of them. Every frame but one renders at
**900×520 logical pixels**, and at that width the name column has 410 pixels to
take and takes them: the ceiling is 520, so nothing was ever near it. The frames
are not wrong, and neither is the size — 900×520 is a real window and it is small
enough to keep a reference PNG cheap. What is wrong is that thirty-seven frames
share one premise, so a whole class of change is outside what any of them can see.

The class is specific and worth naming: **anything that only happens past a
threshold in a dimension the fixture holds constant.** A name column that stops
growing, a tab strip that starts scrolling, a column that sheds, a grid that adds
a sixth cell to a row — each is a branch that a fixture on one side of the
threshold cannot reach. Coverage of the *view* is not coverage of the *branch*.

Two frames of the same window at two sizes are cheap and would have caught this;
the suite already does exactly that for scale, where `scale-100` through
`scale-200` exist because arithmetic that is right at one scale is wrong at
another. Width had no such pair. It has one now — `wide-columns` at 1600×520,
whose only job is to be wide enough for the ceiling to bite.

The rule: **when a feature has a threshold, a fixture on each side of it, or the
feature is untested and the suite will not say so.** The suite reported 35 passes
while the thing under change was drawn nowhere. A green sweep is a statement about
the premises the fixtures encode, and the premises are as much a part of the
instrument as the assertions are.
