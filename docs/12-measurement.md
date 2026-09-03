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

## A driver that fakes the time cannot test a velocity

`touchpad-flick` was written blind, two commits before there was a machine free to
run it on. It failed, and the three things it took to make it pass are each a
different way for a harness to be wrong about the thing it is driving.

**One: the distance could not be sent.** `skerry-drive` asked
`zwlr_virtual_pointer_v1` for a finger-source `axis` and the client saw a distance
of zero — the same limitation `Driver::wheel` had already recorded and worked
around with `axis_discrete`. So the flick's distance rides on a notch count, which
is a pair no real touchpad sends: a finger source beside a discrete step. What
that costs is one thing and what it keeps is everything downstream — `axis_source`
is still what makes the client track 1:1 rather than ease, `axis_stop` is still
what ends the gesture, and the fling is still the fling. The scenario is
differential for exactly this reason.

**Two: the diagnosis took three guesses and one measurement.** A flick that moved
the list but threw nothing could have been a missing `axis_source`, a missing
`axis_stop`, or a velocity that came out at zero, and nothing on screen tells them
apart. Three counters on the diagnostic line — scrolls, how many said a finger
sent them, lifts — plus the clock and a count of flings actually started, settled
it in one run each. Two of the three guesses before that were wrong. **A window
that cannot say what arrived is a window you debug by rebuilding it.**

**Three, and the real defect: the clock was a fiction.** `Driver::tick` advanced
by a fixed eight milliseconds per call. It was written that way for a good reason
— the click counter under test decides a double click from these stamps, so they
have to be monotonic and distinct — and the fixed step made every *duration* the
client measures a lie. A `flick` that rested three hundred real milliseconds
before lifting reported a lift eight milliseconds after the last movement. So the
client measured a finger still moving and threw the list, the flick and the drag
that stopped travelled the same distance, and the scenario's only complaint was
that two numbers were equal.

The counters said `2 flings` for two gestures, which is what turned it from a
mystery into a subtraction.

**A velocity is a distance over a time, and a driver that synthesises the time can
only test the distance.** The clock is real elapsed milliseconds now, still
strictly increasing so the click counter keeps what it needed. The general form:
when a harness fabricates one of the quantities under test, every assertion about
that quantity is an assertion about the harness — and it will pass.

## A gate that compares two things has to compare the right two

`Ctrl+Shift+0` was bound to "reset the column widths" for a whole commit, and what
it actually did was reset the zoom.

`Chord::shift_matters` is why, and it is right: Shift is part of a letter's
identity and not part of a digit's, because Shift on a digit changes which
character the key produces from one layout to the next. So `Chord::matches`
ignores Shift for `0`, and `ctrl+shift+0` and `ctrl+0` are two values and one
chord.

The registry checks for exactly this. It refuses a chord already taken in the same
scope, a test asserts the whole set is sound, and both were green — because the
check compared chords with `==`. Two chords that are not equal can still match the
same press, and the one that had been registered first won at lookup. The gate was
asking whether two *values* were the same when the question is whether two
*behaviours* collide.

It was found by a scenario that had nothing to do with either command:
`the-list-zooms-and-comes-back-exactly` reported `back at 22.0, started at 26.0`,
because `Ctrl+0` had stopped resetting the zoom.

The fix is `Chord::canonical` — the chord as matching sees it, with Shift dropped
where it does not count and a letter's case folded — and the registry compares
those. It reported the collision immediately, which is how the binding moved to
`Ctrl+Shift+R`, where Shift *is* part of the identity.

**Whenever a check exists to prevent two things colliding, the comparison it makes
has to be the same one the runtime makes.** Anywhere those two differ, the check
is a test of its own equality operator.

## The limit that runs out is not always the one in the error

A sweep failed with `No space left on device` on a `/tmp` with twenty-six
gigabytes free.

What had run out was **inodes**. `df -h` says nothing about them and `df -i` says
everything: 971,072 of 1,048,576 used, of which 727,658 belonged to a hundred and
fourteen `skerry-gui-*` work directories that interrupted runs had left behind,
and 200,711 to a 200,000-file benchmark fixture. The harness's own cleanup is
correct and runs in a `finally`; a killed run never reaches it.

Two things came of it. `tools/gui-test` now sweeps work directories older than an
hour before it starts, so the failure mode cannot accumulate. And a benchmark
fixture of two hundred thousand empty files is 200,000 inodes on a filesystem
with a million — it belongs somewhere that is not the same tmpfs the suite runs
on, and it gets deleted when the measurement is taken.

The general form, which is the same shape as every other rule here: an error
message names the resource the kernel checked last, not the one you are short of.

## A golden frame is a reference for whatever somebody looked at

The column view shipped broken at every scale but 100%, and the frame that would
have shown it was in the suite, green, blessed by me an hour earlier.

`columns-200` was added at the same time as `columns`, with this reason written
beside it:

> And at 200%, because the columns are a fixed logical width and the reveal that
> keeps the last one on screen is arithmetic over it — right at one scale and
> wrong at another is the class of defect a second scale exists to catch.

It caught exactly that. `column_row` computed a name box's left edge from
`icon_box.right()` — a rectangle that has already been through `Painter::rect`
and is therefore in **device** pixels — and measured its width against
`area.right()`, which is logical. At 100% those are the same number and the row
draws correctly. At 125% the name starts a quarter of a column too far right. At
200% the first column has no names at all and the second draws one character and
an ellipsis.

The 200% frame rendered that. I ran `tools/golden --bless`, the suite went green,
and the defect became the reference. I had looked at `columns.png` and not at
`columns-200.png` — the one added because the code was most likely to be wrong
there.

**Blessing is not a neutral operation.** `--bless` does not record a frame, it
asserts that a frame is correct; the reference is only worth what the person who
blessed it looked at. A new frame is the case where that matters most, because
there is nothing to diff against and the whole of the check is one pair of eyes.

Two things followed from it.

**Every new frame gets looked at, and the ones added for a reason get looked at
for that reason.** A second scale exists because the first one cannot see a class
of bug; blessing it unexamined turns the second scale into decoration with a
comment explaining why it is not.

**And the arithmetic moved somewhere it cannot make the mistake.** `column_parts`
is a free function taking an `Area` and `Metrics`, with no `Painter` in reach, so
it has no device pixel to accidentally mix in — CLAUDE.md's rule about logical
above the `Painter` and device below it, obeyed structurally rather than by care.
The test that goes with it asserts the three parts tile the column and leave a
name box worth reading, and it fails against a left edge scaled by two.

The general form is the one this file keeps arriving at from different directions:
an instrument nobody has read is not an instrument. A gate nobody has watched fail
is decoration; a reference nobody has looked at is a photograph of the bug.

## A move to where the pointer already is sends no event

`column-scroll` passed alone, passed alone under twelve spinning processes, and
failed every time it ran after another scenario.

The diagnostic line said why once it was asked the right question. Four scroll
events had arrived, all four had gone to a column with no extent, and the window
believed the pointer was at `0.0:0.0` — the position it starts at, which is a real
one and not a sentinel.

The scenario before it left the pointer where this one wanted it. The new window
mapped underneath a pointer that had not moved, this compositor sends no
`wl_pointer.enter` for that, and a `motion_absolute` to the coordinates the
pointer is already at is not a change and produces nothing. So the aim did nothing
at all, every press and every wheel in the scenario landed wherever the pointer had
been left, and the failure surfaced three steps later as a column that would not
scroll.

**The harness already knew this.** `point_at` nudges by half a row per attempt and
then *reads back* what the program says it is hovering, with a comment saying the
only honest way for a test to know it pointed at what it meant to is to ask. The
column scenarios aimed and assumed.

They read back now, through `column_over` — which column the window believes the
pointer is in — and nudge until it agrees. Two things that came with it are worth
keeping separately.

**A weak assertion hid inside a passing test.** `column-view` pressed the chosen
row of an ancestor column and asserted the path and the trail were unchanged.
A press that missed the column entirely and landed on the sidebar leaves both
unchanged too, so that scenario would have passed with the pointer at the origin —
and in a sweep, it did. An assertion whose failure mode and whose success mode
have the same signature is not an assertion. It proves the pointer is in the
column first now.

**And "passes alone" is not a result.** This suite has now produced three
different flavours of it: a sleep where a wait belonged, a fixture two tests
shared, and a pointer another scenario left somewhere. What they have in common is
that running one scenario is a different experiment from running the suite, and
only the second one is the claim. Run the sweep.

## Three frames' worth of the same gap, so here is the shape of it

"This folder is empty" had never been visible in the shipping configuration. It
was drawn ten logical pixels from the left of the *window* rather than of the
pane, so with the sidebar open — the default — it and every read error were
underneath it. Forty golden frames covered this view and not one of them could
have shown it, because **every one of them had a listing to draw**.

That is the third time in this file that a suite went green over a case its
fixtures could not reach, and the three are worth naming together:

- every golden frame but one rendered at **900 logical pixels wide**, where a name
  column's ceiling is never reached;
- the one wide frame's fixture **fitted its window**, so the scrollbar it was added
  to cover was not drawn in it;
- and every frame had **a directory with something in it**.

Each was a premise nobody chose. The width, the row count and the readability of
the directory were not decisions about what to test — they were whatever the
fixture happened to be, shared by every frame because there is one fixture.

So, alongside "when a feature has a threshold, put a fixture on each side of it":
**a fixture's incidental properties are premises, and a suite of frames over one
fixture shares all of them.** The way to find them is to ask what the fixture
cannot be. It is never empty, never unreadable, never larger than the window,
never wider than the ceiling — and each of those sentences is a frame that does
not exist, holding a defect that cannot be seen.

`no-listing` and `no-listing-columns` are two of those sentences. The remaining
ones are worth writing down when somebody next reaches for this file.

## A reference the test computed itself agrees with the test

A folder grouped by time drew **Earlier this year** over a file from two days
ago, in among files from March. Everything was green: two unit tests over the band
edges, three golden frames of the grouped view, forty-two frames in total.

The near bands were the calendar's — "earlier this week" from Monday, "earlier
this month" from the first. On Tuesday 1 September 2026 the week had begun the day
before and the month that morning, so both were empty by construction and the
first heading with room in it was the year's. That is not a rare alignment: the
week's band is empty every Monday and holds one day every Tuesday, and the month's
is empty on every first. **The taxonomy was thinnest exactly where a file was
newest.**

The unit test asserted that the edges never climb. That is the *clamp* — the line
that collapses the band — so the test asserted the collapse happened cleanly. It
could not have failed.

The frames are the interesting half. `grouped`, `grouped-grid` and `grouped-200`
each drew four headings over four days of files, at two scales and in two
geometries. They were rendered from edges the capture wrote by hand:

```
Cuts::new(&[base + DAY, base, base - DAY, base - 2 * DAY, base - 7 * DAY])
```

Four edges, four bands, four headings, all present and correctly spaced — and
`day_bands`, the function that decides where a band begins in the product, is not
called anywhere in that frame. The capture had computed the answer itself, so the
frame agreed with the capture rather than with the program, whatever the program
said. The fiction only became visible when the headings were renamed to name their
windows: the fourth band held a file four days old, under *Previous 30 days*.

The capture asks the product now — `grouping.banding(base)`, the real edges taken
about the fixture's newest file rather than about the clock. It is still
reproducible tomorrow, because every mtime in the fixture is a whole number of
days from one local midnight and which band each falls in is the same arithmetic
on any day of any year. The frames changed when it landed, which is the whole
point: they had been asserting the wrong thing at ΔE 0.

**So: a reference has to come from the thing under test or from outside the
project, never from a second implementation written next to it.** This file
already says a byte-for-byte comparison is not a correctness test and that an
assertion between two derived numbers can pass while both are wrong. This is the
version with a picture attached, and pictures are the ones that feel like
evidence.

And the assertion that does catch it is about meaning rather than mechanism: *a
file two days old is in the week's window, and one eight days old is in the
month's* — written as offsets from `now` rather than as dates, so it holds on every
one of the fourteen months of hours it runs over. Against the calendar edges it
fails on the first iteration, naming today's timestamp.

## A gate that matches a syntax cannot see a table

Every user-visible string goes through the catalogue, and
`tools/check-invariants.py` proves the key exists by matching
`lookup("some.key")` in the source. Renaming two band headings and forgetting the
catalogue passes every gate — put a key that is in no catalogue into
`Grouping::label` and `check-invariants.py` reports `ok`, which is how this was
confirmed rather than assumed. `label` does not call `lookup`: it holds
twenty-nine keys in four arrays and hands one back, and the call is somewhere else
entirely, with a variable in it.

A missing key renders as itself, so this would have shipped a heading reading
`band.previous_7_days`. The gate is not wrong; it covers the call sites written in
the shape it matches, which is most of them. **A table of the same strings is
invisible to it, and a table is where the strings collect once there are more than
three.**

The cheap repair is a unit test where the table is: every label of every grouping,
asserted to resolve to something other than itself. It is one assertion inside a
loop that already existed, it fails loudly on a rename, and it lives beside the
array rather than in a Python script that would have to learn Rust to find it.

## A frame cannot show you a gap that has no right answer

A band heading is a label and then a hairline running to the pane's edge. The
hairline's x was `at.x + gutter + advance + gutter`, where `advance` is what
`Painter::label` returns — the width of the shaped run, **in device pixels**,
because a glyph is measured in them. Every other term on that line is logical.

So the rule started a quarter of the label's width too far right at 125% and a
whole label's width out at 200%, and had done since headings were written. Three
golden frames covered the grouped view, two of them at scales chosen precisely
because "right at one scale and wrong at another" is what a second scale exists to
catch. All three were green. I had looked at all three.

**They were green because a golden frame fails on a change, and a reference can
only be as good as a quantity somebody could have checked.** The distance between
a label and a decorative rule has no correct value a person can see: twelve pixels
or thirty both look like a gap. Blessing it recorded whatever was there, and every
later run agreed with it — which is the same failure as blessing a frame unread,
arrived at from the other side. The frame *was* read. There was nothing in it to
read the defect off of.

What exposed it was adding a second thing to the same line. A count beside the
label turned an arbitrary gap into a relationship between two pieces of text,
which does have an obviously right answer — and at 200% it was visibly wrong.

Two things to take:

- **A decorative offset is not covered by a frame that contains it.** Only a
  measurable relationship is: two runs of text side by side, an edge against
  another edge, a box against the mark it covers. When arithmetic has to be
  correct and its result is a gap, put something on the other side of the gap or
  assert the number in a test.
- **The boundary needs a named crossing in both directions.** `Painter::px` went
  logical to device and there was no inverse, so the way back was a bare division
  somebody had to remember to write. `Painter::logical` is that inverse, and the
  reason it is a method rather than a `/ scale` at the call site is that a named
  crossing is one a reviewer can look for.

## Load makes a scenario late; a bug makes it wrong the same way twice

Four scenarios failed in a full sweep. I had run `tools/check` beside it — a build
and forty-five lavapipe frames — so I wrote that the sweep was contaminated, said
so out loud, and re-ran it. Three failed again. I blamed my own foreground
`cargo test` and re-ran a third time.

The failures were saying something else the whole time:

```
navigation-buttons:      path is '…/fixture/beta', wanted '…/fixture/alpha'
breadcrumb-navigation:   wanted {'path': '…/alpha', 'rows': '2'}, last saw {'…/beta', '1'}
```

**Two scenarios, the same wrong answer, off by exactly one row.** Load does not do
that. Contention makes a scenario *late* — a read that has not landed, a cursor
that is still empty, a progress line one step behind — and late is a different
name every time. A consistent wrong answer is a consistent cause, and the third
failure named the mechanism: a heading had appeared where none was expected, so
every row was 1.2 rows lower than the arithmetic that clicked it assumed.

The cause was mine. `collapse-a-group` presses `Ctrl+9`, `set_grouping` calls
`remember_view`, and `XDG_STATE_HOME` is shared by the whole sweep — so a scenario
that was not `fresh` recorded `by-kind` against the *shared* fixture, and the
per-folder inheritance handed it to `alpha` and `beta` as well. Three scenarios
that click a row by arithmetic over `row_h` then clicked one row too far.

This file already contains the correction I needed, one section up: the
load-contamination entry says in as many words that its own first conclusion "was
wrong, and it was wrong in the direction that costs most", having excused four real
defects as somebody else's load. Reading it afterwards is not the same as reaching
for it first. So, as a discriminator rather than a principle:

- **Late and differently each time** — a missing wait, and load is the instrument
  that reveals it.
- **Wrong, identically, twice** — a defect, and the machine is innocent. Read the
  two messages side by side before touching anything; the difference between them
  is the diagnosis.

And the mechanical half, which is the part a harness can enforce: `fresh` is needed
by anything that changes what is **remembered about** the shared fixture, not only
by anything that changes what is in it. A per-folder setting with subtree
inheritance is the widest possible leak — one record reaches every scenario over
that path and every path beneath it. Auditing the other scenarios for the same
shape took one regular expression and found one more, safe for a reason worth
knowing: a folder's record holds a layout and a grouping, so a sort leaks nowhere.

## A comment that claims a measurement, from a measurement never taken

Rule 1's canonical test needed a filesystem that stops answering. The first version
of `tools/hung-mount` replied to the kernel's `FUSE_INIT` and then to nothing, on
the theory that an unhandshaked mount would *fail* calls rather than queue them —
ninety lines of versioned `fuse_init_out` layout, with a comment beside them that
read:

> **Measured** — a first draft that only held the device fd made `stat` exit 1 and
> `ls` exit 2, which would have made every assertion below pass against a
> filesystem that was merely broken.

**That measurement was never taken.** What had happened was a `timeout 2 stat`
whose exit code I read as the command's rather than as `timeout`'s, in a shell
block that had already gone wrong for another reason. Asked properly — two
variants, three calls each, durations printed — `stat`, `ls` and `open` block past
three seconds *either way*, and `/proc/self/mountinfo` shows the same line for
both. The handshake bought nothing observable, so it is gone, and the file now says
that instead.

This project's rule is that every claim about performance is a number with a
method. The failure mode nobody warns about is the claim that *has* a number and a
method and neither one happened: it reads exactly like evidence, it justifies code
that then cannot be removed without an argument, and it is a sentence a reviewer
has no way to check. **A number in a comment is a promise that it can be
reproduced.** Two of them here could not be.

What caught it was trying to watch the test fail. Deleting the handshake left the
self-test green — and the first reflex, that the negative control was badly chosen,
was wrong: the control was fine and the code was decoration. This file already
records the same shape from the fling work, where two clears and a gap constant
were all unreachable. The difference this time is that a comment had been written
to defend one.

Two smaller findings from the same afternoon, both worth the line each:

**A rig whose failure mode is a hang.** Substituting `true` for `fusermount3` — the
only way to ask what happens when nothing gets mounted — left `recv_fds` waiting
for a descriptor that was never coming, so the self-test hung for two minutes
instead of failing in one line. A deadline on the socket turned it into
`fusermount3 sent no descriptor: timed out`. A test rig that can hang will one day
stop a whole sweep rather than fail one scenario in it.

**And "the window is still answering" cannot be asserted with `state`.** The
scenario's first draft read `run.state()` after each keystroke, on the reasoning
that a frozen frame loop could not produce an answer and the scenario would time
out. It does not: `state` returns the last line in the log and `reports_reached`
gives up quietly after a frame, so a frozen window hands back the dump from before
the question. Spliced a `read_dir` into the frame path to watch it fail, and it
failed on *"the pane went to <fixture> rather than the mount"* — a stale answer
from before the navigation, three assertions away from the real one. Through
`answered`, which returns nothing when nothing fresh arrived, the same violation
fails on *"the window stopped answering when the pane reached a hung mount"*. **A
liveness assertion has to be built on a call that can say no.**

## The same gate, two blind spots, one day — and a fifth of it was dark

The catalogue gate proves that every key a source file asks for exists, because a
missing one renders as itself: `properties.items` as a panel title. This morning it
turned out it could not see a key held in an array — twenty-nine of them in
`Grouping::label` — and that is
[written up above](#a-gate-that-matches-a-syntax-cannot-see-a-table). This
afternoon, adding plural forms, it turned out it could not see this either:

```rust
sk_model::i18n::format(
    "properties.items",
    &[&sk_ui::format::grouped(many.len())],
)
```

The scan was line by line. The call is on one line and the key is on the next, so
the regex matched neither. **Measured before fixing it: 19 of 95 call sites were
invisible — one in five.** Among them `status.counts`, `notice.progress_eta`,
`notice.paused` and four of the statistics overlay's.

So the gate was reporting on 80% of what it claimed, and which 80% was decided by
`rustfmt` — a call that fitted on one line was checked and the same call wrapped by
one character was not. Nothing about the code's correctness moved it in or out.

Both repairs are the same shape: **match the whole file, not the line.** `re.DOTALL`
over the source text with the line number computed from the match offset, so a
wrapped call is seen and the failure still points at the line the key is on. The
array case needed a unit test instead, because no regex reaches a table.

Two things worth keeping separately.

**A gate's coverage is measurable, and almost never measured.** Counting what a
tightened pattern newly matches took twelve lines and turned "this gate is fine"
into "this gate was 80%". Any gate built on a pattern can be asked the same
question, and the answer is a number rather than an opinion.

**And the plural work found the bug in three committed golden frames.** `columns`,
`columns-200` and `check-box-columns` all showed **`1 items`** in the status line,
blessed by me, one of them the same day. The corner of the frame nobody was reading
was the corner with the defect in it — which is
[the reference nobody looked at](#a-golden-frame-is-a-reference-for-whatever-somebody-looked-at)
again, at a smaller scale: I had checked those frames for the thing I had just
changed, and the status line was not it.

## The accessor written for a caller that never arrived was also the wrong shape

`History::entries` returned the changes on the undo stack, oldest first. Its doc
comment said what it was for and admitted the rest:

> For the visible history rule 3 asks for. Not yet drawn anywhere, which is why it
> is the one method here with no caller — and why deleting it would be deleting the
> reason the stack keeps its entries at all.

The caller arrived, months later, and could not use it. A history panel lists the
**whole timeline** — the applied entries *and* the undone ones, which are the future
a person can travel back into — and it needs to know where the present is. `entries`
returned one branch and no marker. It was replaced by `timeline` and `applied`, and
`entries` is gone.

So the accessor was not merely unused. **It was unusable, and the comment defending
it made that harder to see**: a reader arriving at the panel would reasonably start
from the method that claimed to be for it, and the shape it hands back is wrong in a
way that is only obvious once you know the undone branch is stored as a stack —
`undone.last()` is the *next* redo, so listing it as stored puts the future
backwards.

Two more of the same in one afternoon, both caught by clippy rather than by
judgement: `Edits::walking`, written "so the panel can say so and refuse a second",
which the panel had no reason to refuse; and `HistoryParts::row_at`, written for a
hit test that ended up reading the rectangles the frame recorded instead, because
those account for scrolling and clipping and the arithmetic did not.

The pattern across all four is one thing, and it is not "delete dead code":

**Speculative API is a design decision made without the constraint that decides
it.** Every one of these was written while imagining the caller, and every one was
the wrong shape when the caller turned up with a real requirement — a scrolled list,
a clipped row, a stack stored backwards. The comment explaining why it has no caller
yet is the tell: it is an argument for keeping a guess, and the guess costs more
than the keystrokes to write it again.

The rule that follows is narrow enough to act on: **write the accessor in the commit
that calls it.** If the data structure needs to keep something for a future feature,
keep the *data* — `History` keeping its entries was always right — and let the
feature bring its own way of reading them.

## A sandbox that works announces itself by refusing something you wanted

`skerry-preview-doc` renders a PDF by driving `pdftoppm`. It answered
`Refusal::NoRenderer` for every PDF on a machine with poppler installed, and its
own `--probe` said in the same breath that the renderer was there.

Twice, for two different reasons, and both are worth keeping:

**`execve` needs its own Landlock right**, and the first ruleset granted read and
nothing else. That one is obvious in hindsight and was not in foresight: "the
child may read any file" sounds like it covers running one.

**And a dynamically linked program needs that right on its loader and every
library it maps.** Granting execute on the directories in `PATH` — the precise,
careful version — still refused, because `ld-linux` lives in `/lib64` and
poppler's libraries in `/usr/lib`, neither of which is on `PATH`. Chasing those
would mean tracking one distribution's layout, so the grant is the root; what is
actually load-bearing is that the process cannot write a byte anywhere, and *that*
was checked rather than assumed, with a five-line probe that tried.

The general shape is the useful part. **A security control that has never refused
anything has not been observed working.** Both of these presented as a feature
that did not work, and the temptation each time was to widen the sandbox until the
symptom went away — which is how a confinement becomes a comment. The discipline
that avoided it was cheap: write the probe that asks *"can this process still do
the thing I am trying to stop it doing?"*, and keep it in the file as the reason
the grant is shaped the way it is.

## A fixture nobody rendered is a picture of nothing

Three golden frames of thumbnails, blessed, green. Two rows in each had **lost
their marks entirely** — no thumbnail, no file glyph, nothing — and the diff was
a narrow vertical strip I nearly waved through as "the mark column, expected".

`holiday 🏖️.jpeg` and `img10.png` are in the shared fixture as junk bytes under
picture names. The capture asked for their thumbnails, got `Undecodable`, and
recorded the atlas key anyway — so the row drew `painter.image` for a picture that
was never there, and skipped the mark that would have gone in its place. The window
did not have this bug; only the capture did, because the two had the same rule
written twice.

Two things it teaches, and the second is the one I would not have got from the
first:

**A fallback and its replacement have to be exclusive by construction.** The rule
is "record only what the atlas holds", and it now lives in a comment on both
paths — but what actually made it safe is that `Pane::thumbs` is *only* filled
where `holds_image` says yes, so the drawing needs no fallback logic at all and
has no second branch to get wrong.

**And a hand-made fixture is code that has to run.** The PDF for the quick-look
frame was assembled by hand, and the first version put its text at y=680 on a page
595 tall — off the paper. The frame rendered a blank white page, which looks
exactly like a page that rendered. The capture already prints how many thumbnails
landed, for precisely this reason; a document preview has no such count, and the
only thing that caught it was looking at the picture.

## A frame of a state the program could no longer reach

The text preview learned to wrap: a line wider than the panel is cut into rows,
and the row is what a click, an `↑` and a `↓` are about. The rows are computed
once when the panel's width changes, because computing them per frame is a pass
over the file inside the 8 ms budget.

Then `tools/golden` passed. All sixty-three frames, including `quick-look-text`,
whose fixture holds a deliberately long line — and whose reference showed that
line **elided with an ellipsis**, which is what the program did the day before.
The frame was green because it was a frame of the old behaviour and the capture
still produced the old behaviour: the window fits the rows in `App::render`, and
the capture does not go through `App` at all. It builds its own sample and calls
`view::draw`.

So the gate was measuring the capture's copy of the program rather than the
program. Both halves needed the call, and the reference needed re-blessing —
after which the same frame caught something the tests had not: an empty line
inside a selection had stopped drawing its stub, because the new row-clamping
guard returned early on a row where `from == to`. The tests asserted about bytes
and rows and all passed; the notch was three pixels of missing highlight that only
a picture shows.

**The rule this adds to the two above it: a capture that samples its own state
is a second implementation, and every per-frame step the window takes has to be
taken there too — or the frame is of a state the program cannot reach.** The
fixture's own text said "elided rather than wrapped", which is the tell I should
have read first: a fixture that describes the behaviour is a comment that goes
stale, and this one had.

## A gate catches the shape it was written for, and this was a different shape

Two defects in one afternoon's scripted edits, both invisible in a diff read as an
addition, and the pair is more useful than either.

**The doc comment that was spliced onto the end of a paragraph.**
`spliced_comment_check` exists for exactly this class — it has caught six of them
— and it looks for the shape it has always taken: a summary line followed
immediately by the start of another sentence. This one landed after the *prose*,
four lines down, so the block read as one comment with an odd last paragraph and
`fn properties` below it was left with no comment at all. The half that is
reliably detectable is the orphan, not the splice, and the lint for it —
`clippy::missing_docs_in_private_items` — reports **300 items** across the
workspace today. That is a cleanup, not a gate, and calling it one before doing
the cleanup would put a warning nobody reads in front of a check nobody trusts.
So the blind spot is written down here and in the check's own docstring, and the
gate is honest about what it covers.

**The catalogue key that was written twice** had no gate at all. A duplicate is
silent by construction: the loader keeps one, nothing says which, and a
translator maintains two lines that can drift into disagreeing. Four lines in
`catalogue_check` fix it, and the first thing they reported was a duplicate
`notice.terminal` that had been sitting there since the terminal command landed —
which is the argument for the gate rather than for the fix.

The general rule is narrower than "write more gates". **A gate's coverage is the
shape it matches, not the class it is named after**, and the distance between the
two is only visible when something in the class gets past it. When that happens
the cheap move is to write the distance down; widening the gate is worth it only
when the widened version has been run against the whole tree and the false
positives counted. Both numbers above — six caught, 300 undocumented — came from
running it.

## A queue pumped before the thing that fills it

`Space` opened a quick look and the panel stayed blank. Any unrelated event — a
click, a window focus change, the pointer crossing a row — and the picture
appeared at once. Every test passed, because a test's next instruction *is* an
unrelated event.

The frame does this, in this order:

```
settle_thumbnails()   // queues what the visible rows want, then pumps
settle_quick_look()   // queues what the panel wants
```

The pump was the last line of `settle_thumbnails`, so the panel's request was
queued *after* the only pump of the frame. No child was spawned; nothing was left
to wake the loop, because the loop sleeps on descriptors and no descriptor had
anything coming. The next frame would have pumped it — and a frame is only drawn
when something is dirty, so "the next frame" is however long until the person does
something else.

**A once-a-frame pump belongs after everything that can fill its queue, and the
way to make that true is to have one caller rather than to order two.** It is now
a line in `settle_rows` after both settles, which is the only place that can see
both.

What makes this worth a section is how it hid. The symptom — "it works as soon as
I touch anything" — reads like a repaint problem, and the two hours it deserves go
to the renderer. The tell was that the *thumbnails* in the same folder appeared
immediately: same host, same child, same channel, different call order. **When two
users of one mechanism behave differently, the mechanism is not the bug.**

## Two insets, one of which scaled

A grid cell's name was drawn one logical pixel wider than the selection behind it
at the default row height, and five wider at the largest. The pill is inset from
the cell by half the gutter, which `Metrics::zoomed` scales; the name box was inset
by a hard `4.0`, which it does not. At 100% the overlap is a pixel and reads as a
rendering artefact. At 175% the name crosses the selection's border on both sides,
which is what got reported.

The fix is not a better constant. **Two numbers that have to agree should be one
number**, so the label's box is now derived from the pill's own inset, and its
vertical band is anchored to the pill's *bottom* rather than to `0.58` of the
cell's height — a fraction can only be checked against one combination of zoom and
scale at a time, and there are twenty-eight of them.

The test is the shape this document keeps arriving at: **a sweep over every
combination, asserting a relation rather than a value.** Seven zoom steps by four
cell widths, and the assertion is "the name is inside the pill and below the
mark". Run against the old geometry it fails at zoom 0.85 — not at 1.0, which is
why looking at one screenshot at one scale had not found it.

## A diagnostic that ran under a stricter sandbox than the thing it diagnoses

`skerry-preview-doc --probe` printed `renderer: Some(PdfToPpm)` and `pages: None`
on a machine where `pdfinfo` answers correctly in ten milliseconds.

The probe applied its own confinement — read the root, execute nothing — while
the served process grants execute at the root and can therefore spawn the counter.
So the probe was answering a question about a process that does not exist. It is
the same defect as [a green assertion between two derived
numbers](#an-assertion-between-two-derived-numbers-can-be-green-while-both-are-wrong):
the measurement was of the harness.

**A probe has to run under exactly the confinement of the thing it reports on**,
and the way to guarantee that is to call the same function rather than to write a
second one that looks like it. `--probe` now calls `confine()`, the same one
`main` does.

This is the second time the sandbox has produced a wrong answer that looked like a
missing feature — the first is
[above](#a-sandbox-that-works-announces-itself-by-refusing-something-you-wanted) —
and both times the shape was a capability quietly absent rather than an error. A
confinement has no way to tell you it is the reason; that has to come from a probe,
and a probe that is not the real thing is worse than none.

## Where a preview's latency actually was, and where it looked like it was

"Make the previews load quicker" is the kind of request that invites optimising
the decode. Measured first, over the helpers' own wire, seven requests each after
a warm-up, on a 4000×2250 JPEG and a 66-page PDF:

| output size | image | PDF page |
| --- | --- | --- |
| 128 px | 60–85 ms | — |
| 320 px | 55–127 ms | 60–70 ms |
| 400 px | — | 61–76 ms |
| 640 px | — | 98–138 ms |
| 1200 px | 67–155 ms | 135–200 ms |

Spawning a helper is **1 ms**, so the process boundary ADR 0006 chose costs
nothing here. Three things fall out of the table, and only one of them is what
the request sounded like:

**An image costs the same at every size.** 60 ms at 128 pixels and 67 ms at 1200:
the JPEG decode dominates and `zune-jpeg` has no DCT-scaled decode, so a
coarse-first pass would be paying full price twice. What is free instead is the
128-pixel thumbnail already in the atlas — the row `Space` was pressed on was on
screen — so the panel draws that at once and replaces it when the render lands.
Time to first pixel: **0 ms**, and the only cost is a lookup.

**A page costs what it draws.** 61–76 ms at 400 pixels against 135–200 ms at
1200, because rendering is proportional to the output and not to the document
(ADR 0019's table). So a document is fetched twice — a third-size draft, then the
full size — and time to first pixel halves. The two changes are opposite
decisions from the same table, which is the point of having measured rather than
picked a strategy.

**And the largest number was not in the table at all.** `BATCH` was 32, one batch
holds the child for its whole run, and a batch cannot be recalled — so a `Space`
pressed while a photo grid was thumbnailing waited behind up to 32 round trips.
**1.9 seconds**, and it reads exactly like the window ignoring the key. Previews
now form their own batch and go first, and `BATCH` is 8, which bounds the
unavoidable case — a batch already in flight — at about 480 ms.

**A fourth change was measured and declined.** `skerry-preview-doc` asks
`pdftoppm -scale-to N` for the panel's *long edge*, so an A4 page in a landscape
panel is rendered at 848×1200 and drawn at 565×800 — 2.2× the pixels, thrown away
by the sampler. Fitting the render to the box needs the page's aspect, which the
`pdfinfo` call already in the path could report for nothing. Measured at the
fitted size: **128–161 ms against 135–200 ms**, about 13% with the two ranges
overlapping. That is below the noise floor of this measurement, so the second code
path is not worth having and the waste stays — written down here so the next
person does not re-derive it.

The rule this is the third instance of: **measure the thing the person described,
not the thing the description made you think of.** The decode was the obvious
suspect and is the one place nothing could be won. Two of the three wins were in
scheduling and in something already on the GPU, and neither would have been found
by profiling a decoder.

## A sweep that had been passing on luck

`a-drag-in-from-another-application-lands` failed in a full X11 sweep, and the
first instinct — "that scenario is nothing to do with what I changed" — is the
instinct this document exists to distrust. So it was measured on both sides,
eight runs each, the previous commit built in a worktree beside the current one:

| | passed | failed |
| --- | --- | --- |
| with the change | 6 | 2 |
| at the previous commit | 6 | 2 |

**Identical.** A pre-existing flake at 25%, which means every "0 failed" sweep
this scenario has ever been part of was a coin landing the right way three times.
That is worse than a red test: a gate that fails one run in four teaches everyone
reading it to discount a failure, which is the whole value of the gate gone.

The cause was a wait in the wrong order. `XdndFinished` — the peer's "drag ended"
— is sent *after* the drop has been acted on, so waiting for it before waiting for
the file to arrive was waiting on the far side of a job with a six-second budget.
The scenario now waits for the file, which is the claim, and asserts the
notification afterwards, by which time it has long since landed. Measured again:
**one failure in sixteen**, and it fails differently — "nothing landed", the
synthetic drag not being delivered at all. That is XTEST and pacing, it is owed,
and it is written down here rather than left as folklore about a scenario that
"sometimes goes red".

Two things to take from it. **A flake rate is a number and can be measured in two
minutes** — sixteen runs of one scenario — so "probably unrelated" is never the
last word available. And **the same experiment settles both questions at once**:
whether the failure is yours, and how bad it was before you arrived.

## The same bug twice, one layer apart

A preview stayed blank until something else happened, and it was fixed:
[the pump ran before the thing that filled its
queue](#a-queue-pumped-before-the-thing-that-fills-it). Then a *text* preview
stayed blank until something else happened, and it was the same bug one layer
along — the drain rather than the pump.

`QuickLook::drain` was the first line of `settle_quick_look`, which runs from
`before_frame`, which runs **only when something is already dirty**. So the read
finished, woke the loop, and found nothing that would draw a frame: the text sat
in its channel until an unrelated event marked the window dirty for its own
reasons.

The rule, stated once for both: **a pump and a drain belong beside the other
drains, where the wake-up lands — never inside the frame they are supposed to
cause.** A frame is a consequence of state changing; anything that changes state
cannot be inside it.

**And the first attempt at a test for it was decoration.** Every observation this
harness makes drives an `F12`, and a keystroke marks the window dirty — so *asking
about the bug fixed it*, which is why it survived two rounds. What is observable
is the frame counter across an interval with no input at all, and the first
version of that assertion said "at least three frames". Measured: **three with
the bug and four without it.** It passed against the bug three times out of three
before the numbers were taken rather than guessed.

That is the third time in this document a threshold has been wrong in the safe
direction. The habit that catches it is cheap and has no exceptions: **write the
assertion, then run it against the defect on purpose.** A gate that has not been
watched failing is a gate that has not been tested, and the version of it that
takes two minutes is to break the code back and run it.

## Two ways to be blank, and neither of them said anything

A PDF preview came back as an empty panel with a title, a page number of `1 / 28`,
and nothing else — for ever. The count proved a reply had arrived, so the child
was working; the panel simply had nothing to draw and nothing to say about it.

Reading the code for **states that draw nothing and explain nothing** found two,
and the honest thing to record is that neither was reproduced from the report.
Forty real PDFs driven through the helper at a large box produced no refusal at
all, so the trigger stayed out of reach — but both paths are reachable by
construction, and one of them is a hole this document had already been told about:

**An evicted page the host still believed in.**
[ADR 0020](../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md) says,
in its own words, that "an entry here whose texture has been evicted resolves to
nothing and is rendered again, which is the right degradation" — and the code did
not do it. The renderer holds 24 MiB of pages; at a large panel one page is
**11.9 MB**, so opening a document with two prefetches behind it evicts almost at
once. The host's page-number map kept the key, handed it out, and the drawing
emitted no quad for a key the renderer had never heard of. `wants` was false
because the shot matched what had been asked. **An ADR's consequence is not
implemented by being written down.**

**A child that died with nothing to say.** `run_batch` broke out of its loop on a
pipe error without pushing an `Answer`, so the host learned nothing: no pixels, no
refusal, and a panel that had recorded its request and would never make another.
Every want in a batch now comes back with something — `Refusal::Interrupted`,
which says the helper stopped rather than that the file is bad — except a batch
that was cancelled, which is a folder somebody has left.

And a third thing, which is the design rule the pair of them argues for:
**a refusal that reaches an open panel is never allowed to be silent.** Three of
them — `NotItsKind`, a well-formed reply with no pixels, and running off the end
of a document — deliberately have no sentence, because they decide whether the
panel *opens*. Once it is open the same value means an empty rectangle, which a
person cannot tell from a program that has stopped. There is a catch-all now, and
a golden frame of it: a junk `.pdf` in the fixture, whose refusal has no message of
its own, drawing "This file could not be shown."

The general rule is about *states*, not about bugs. **Enumerate the states in
which a surface shows nothing, and require each of them to be either transient
with a known bound or legible.** The panel had four — a picture, text, a message,
and "not yet" — and "not yet" turned out to be reachable permanently by two routes
and for four and a half seconds at a time by a third. None of them was a wrong
pixel; all three were an absence, and an absence is the one defect a screenshot
cannot diagnose and a state table can.

## A 234× win that is also a 200× loss, in the same measurement

The whole-machine search was measured twice over the same 6,905,073 entries: once
walking, once from an index of `/`.

| | walked | indexed |
| --- | --- | --- |
| Complete answer | 8,472 ms | **36.2 ms** |
| First result | **0.17 ms** | 35.7 ms |

The first row is the one the feature exists for and it is 234×. The second row is
the same change measured at the other end of the latency, and it is 200× the wrong
way — because a walk *streams*, so it has a match on screen in a fifth of a
millisecond and finishes eight and a half seconds later, while the index scan is
one parallel pass that reports when it is finished: nothing, then everything.

Neither number is wrong and neither is the whole story. A single figure would have
been, and "the index made search 234× faster" is the figure anybody would have
quoted — including in a commit message, where it would have been true and would
have described a keystroke that now waits 36 ms where it used to wait for nothing.

Both are inside the 50 ms first-result bar, which is the only reason this is an
observation here rather than a defect in the tracker. The rule it earns is about
what to measure rather than about what to fix: **a latency has two ends, and a
change that improves the aggregate can make the first pixel worse.** Measure the
first report and the last one, in the same run, and write both down — the
project's own bar is on the *first* one, and an optimization that moves the last
one is not automatically allowed to move the first.

It also predicts where the next defect will be. A one-character query in the
indexed scope pauses for 36 ms and then draws two thousand rows; the same query
walked draws four rows and keeps going. The two feel different, and only one of
them was ever designed.

## A wait in one accessor and not in its sibling

Two scenarios failed in a full Wayland sweep and passed alone, with errors that
named neither timing nor the window:

```
ValueError: not enough values to unpack (expected 4, got 3)
ValueError: could not convert string to float: ''
```

Both are the same defect in two places. `Run.state` waits for the window to
answer every `F12` the scripts asked for — the wait is documented there, and the
reason is that a stale line is indistinguishable from a fresh one. `Run.states`,
which nine scenarios call and four unpack by position, did not: it read the log
and returned whatever was in it. On an idle machine the answers are always there
by the time it is called; under a full sweep they are not, and the scenario fails
three lines later as an unpacking error against a list that is one short.

The second is the same shape one layer up. `until(run, erasing="1")` waits for the
flag that says the delete question is open — and the *rectangles* of its two
buttons are recorded by the frame that draws them, which is a frame later. So the
state that satisfied the wait had `erase_aim=""`, and the scenario divided an
empty string. `menu_aim` had already needed exactly this fix, and the wait it
needed — `holds`, on the aims being non-empty rather than on the flag — is the one
this needed too.

The rule: **a wait belongs on the value the scenario is about to use, not on the
flag that says it is coming.** And where two accessors return the same thing in
different shapes, they need the same guarantee — a wait in one of them is a wait
that covers whichever scenario happened to call the right one.

## The failure that was not the failure it said it was

`a-drag-in-from-another-application-lands` fails about one run in eight with
`nothing landed`, and the comment beside it said why: *"the synthetic drag itself
is not delivered, which is XTEST and pacing rather than a wait in the wrong
place"*. That was a guess, it was written as if it were a finding, and it is
wrong.

**The first thing the message said was that it was wrong.** The failure prints
what the directory holds, and the dropped file *was in the listing* — the
scenario had spent eight seconds asking `exists()` about a path that the very
next line then listed. What the failure means is "not yet", not "never".

Instrumenting the whole path — the selection request, the `SelectionNotify`, the
property, the pipe, the read, the channel, the wake — showed every link
completing, with timestamps:

```
TRACE take_drop asked
TRACE request_from target=274
TRACE arrived property=272 target=274 pending=1
TRACE handing over 59 bytes
TRACE dropped text "file:///…/outside/from-the-peer.txt\r\n" into "…/fixture-…"
TRACE paths ["/…/outside/from-the-peer.txt"]
TRACE job sent=true at 1788378978.769282434
TRACE job woke  at 1788378978.769291341
TRACE drain_paste took one at 1788378978.769338900
```

Fifty-six microseconds from the job's wake-up to the frame loop taking the
message, with the paths right and the destination right — so `finish_drop` and
`Edits::paste_all` both ran. Thirty-three seconds later the window reported no
operation pending, an empty queue and no undo entry, and **one key press made the
file appear within two hundred milliseconds.**

Three hypotheses died on the way, and each is written down so nobody spends the
afternoon again:

- **A lost wake-up.** Ruled out by the timestamps above: the wake and the drain
  are 56 µs apart.
- **An event asleep in x11rb's buffer while `poll` waits on the socket** — the
  classic X11 hang. Ruled out by reading: `pump` drains every buffered event
  before it sleeps, and `drain_server` loops until `poll_for_event` says none.
- **A refused conversion leaving a pipe's write end alive**, which is a bug this
  module has had once and records in `Incoming::target`'s own comment. Ruled out:
  the property was 272 and not `None`, and 59 bytes were handed over.

What is left is the last link — `paste_all` is reached and the copy does not start
— and it is **timing-sensitive enough that instrumenting it hides it**: an
`eprintln!` between `finish_drop` and `paste_all` bought 27 consecutive passes
where the rate had been one in eight. A stderr write is a syscall on the frame
loop, which is the one thing this loop is built not to do, so that is not a
coincidence to shrug at — it is the shape of the remaining defect.

**What the hunt did find is a silent refusal.** `Edits::queue_all` returns early
when an operation is already pending, and it used to return with the paths
dropped on the floor and nothing said. One operation at a time is M3's decision
and it stands; a refusal nobody is told about does not, and for a drag from
another application those are somebody else's files. It now returns whether it
took the batch and every caller says so. Whether that is *this* defect is
unproven — the window reported nothing pending — which is exactly why it is
recorded as a defect found by reading rather than as the fix.

The rule: **a failure message that describes a cause is a hypothesis, and it
should be written as one.** "Nothing landed" was a fact and cost nothing; the
sentence after it cost a wrong diagnosis that survived three commits, because
every reader after it trusted the explanation instead of the observation.
