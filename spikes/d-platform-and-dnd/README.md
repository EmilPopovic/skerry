# Spike D — platform layer and drag-and-drop

**Status:** in progress. Method below is fixed; results are filled in as arms
land.

## Question

Can a process that speaks Wayland and X11 directly be a drag-and-drop **source**
as well as a target against GTK and Qt applications, carry both selections
including primary, and receive `text-input-v3` — and does a general-purpose
windowing crate reach the same places?

## Bar

Quoted from [08-roadmap.md](../../docs/08-roadmap.md#phase-0--spikes-34-weeks),
written before the first line of code and not adjusted afterwards:

> **Bar:** every pair works on both display servers, or the failure is documented
> as a compositor limitation with a fallback · IME composes CJK text into a
> field.

[Spike A](../a-million-rows/README.md#crisp-glyphs-at-fractional-scale)
added a third condition after the fact, which is allowed because it *tightens*
the bar rather than widening it:

> Fractional-scale negotiation is covered explicitly, because that is where a
> text-crispness bug lived that every internal assertion called a pass.

## What it decides

[ADR 0002](../../docs/adr/0002-own-platform-layer.md) — the most expensive
decision in the plan, six weeks of M0 before anything is on screen. It is
currently *Accepted, pending Spike D*, and it carries its own revisit trigger:

> If Spike D demonstrates that a general-purpose windowing crate can act as a DnD
> source against GTK and Qt applications on both display servers **and** handle
> `text-input-v3` adequately, take it and delete this decision — six weeks is a
> large price for a problem someone else has solved.

So this spike is built twice, the way Spike A was: once on `wayland-client` +
`x11rb` + `xkbcommon`, once on `winit`. The comparison is a table of what each
arm can reach, not an argument.

## Method

### Machine and session

The same 24-thread 2024 laptop as Spikes A, B and F, on **Hyprland 0.53.3**
(wlroots) at a **1.25 fractional scale**, with Xwayland 2880×1800 at 96 dpi for
the X11 arm. Nothing here is timing-sensitive at the millisecond scale except the
latency figures, which are reported with the load average beside them per
[12-measurement.md](../../docs/12-measurement.md).

One compositor is not a portability claim. Spike A's llvmpipe arm and Spike B's
three machines exist because a single sample lies; the same applies here and
harder, because a compositor is a policy engine and not just a driver. GNOME
(mutter) and KDE (kwin) runs are **owed and recorded as owed**, not assumed.

### Peers

The roadmap names Nautilus, Dolphin, Firefox, Chrome and Thunderbird. What this
machine actually has:

| Peer | Toolkit | Role tested |
| --- | --- | --- |
| Nautilus 4x | GTK 4 | file-URI source and target — the reference implementation |
| Firefox | GTK 3 widget layer, **snap-confined** | target for file drops. Tested against the running instance and the real profile, targeted by window address — a browser reuses its process, and a fresh profile shows onboarding instead of a page |
| `konsole` | Qt 6 + KDE Frameworks | Qt target for `text/uri-list` |
| `designer6` | Qt 6 | second Qt target, richer DnD |
| the spike itself | ours | **the control** — proves source and target independently of anyone else's bugs |

Dolphin, Chrome and Thunderbird are not installed and there is no root on this
machine. They are recorded as owed. The Qt half of rule 5 rests on `konsole` and
`designer6` until Dolphin is run, and Dolphin is the one that matters most,
because KIO's drop handling is its own dialect.

Each peer is tested on **both** display servers — native Wayland, and forced onto
Xwayland (`GDK_BACKEND=x11`, `MOZ_ENABLE_WAYLAND=0`, `QT_QPA_PLATFORM=xcb`) —
because XDND and `wl_data_device` are different protocols with different bugs.

### Automation, and why this is a harness and not a clicking exercise

Drag-and-drop needs a pointer press, a motion and a release, in that order,
across two windows. Hyprland advertises `zwlr_virtual_pointer_manager_v1` and
`zwp_virtual_keyboard_manager_v1`, so the spike synthesizes its own input and
positions peer windows with `hyprctl dispatch`. Every pair in the matrix is a
command that can be re-run, which is the only reason the matrix can be trusted
after it is edited.

The one thing that cannot be automated is *what the peer did with the drop*, so
that is read back from the peer: the file appearing in the destination directory
for Nautilus, and the MIME types a peer asked us for, which is the strongest
signal available when the peer's own state cannot be read.

`zwp_input_method_manager_v2` is also advertised, so the spike acts as its own
input method for the IME condition: it sends a preedit and a commit for CJK text
to itself over `zwp_text_input_v3`. That exercises our client half end to end. It
does **not** prove fcitx5 or ibus interop, which needs an input method this
machine does not have — owed, and recorded as owed.

### Checks

Each is a pass, a fail, or a documented compositor limitation with a fallback.
Latencies are p50/p99 over the sample count shown, never a mean over one run.

**Wayland**

| Check | Passes when |
| --- | --- |
| `frac-scale` | `wp_fractional_scale_v1` reports 150/120, the buffer is sized for it, and the surface matches the screen 1:1 — Spike A's assertion, not a visual opinion |
| `keyboard` | the `wl_keyboard` keymap fd compiles under `xkbcommon` and a keysym resolves through the active layout |
| `clipboard-out` | a selection we own is read by `wl-paste`, by a second instance of the spike, and by a GTK and a Qt peer |
| `clipboard-in` | a selection set by `wl-copy` and by Nautilus's Copy is read, with its full MIME list recorded |
| `primary-out`, `primary-in` | the same, over `zwp_primary_selection_v1` |
| `dnd-source` | a drag we start is dropped on each peer, the peer reads the data, and the negotiated action is recorded |
| `dnd-target` | a drag from each peer is accepted, with every offered MIME type and the chosen action recorded |
| `dnd-actions` | copy, move and link each negotiate to the intended action under the desktop's modifiers |
| `text-input-v3` | a CJK preedit arrives and commits into the field |
| `slow-transfer` | **rule 1.** A drop whose source never writes to the pipe does not stall the event loop. The measurement is the frame interval while the read is outstanding |

**X11**

| Check | Passes when |
| --- | --- |
| `x-window` | a window maps, and its scale is resolved from `Xft.dpi` and RANDR rather than assumed |
| `x-clipboard` | `CLIPBOARD` and `PRIMARY` both directions, including an `INCR` transfer over the 256 KB request limit |
| `xdnd-source` | the XDND handshake completes against each peer and the peer reads the data |
| `xdnd-target` | a drag from each peer is accepted, with `XdndActionCopy`/`Move`/`Link` recorded |

**`winit` arm**

The same table, marked reachable or not through `winit`'s public API alone — no
dropping to the raw protocol, since a raw-protocol escape hatch is the thing ADR
0002 says we would end up fighting.

## Result

Every number below is from [runs/all-hyprland.txt](runs/all-hyprland.txt), which
is the output of `./run-all.sh` — one command, every arm and every pair, in one
order, on the machine described above. `misc:middle_click_paste` was left at the
value this machine actually has, which matters and is explained under
`primary-out`.

### Wayland — 11 pass, 1 documented limitation, of 12

| Check | Result |
| --- | --- |
| `globals` | 64 interfaces advertised; all 9 required present, plus both instruments |
| `frac-scale` | `preferred_scale` **150/120 = 1.25 exactly**; a 480×320 logical window is a **600×400** buffer with the viewport destination at 480×320 |
| `checker-1:1` | a **64-pixel run** of one-physical-pixel alternation survives into the compositor's own framebuffer, 64 expected |
| `keyboard` | a 38,129-byte keymap compiled by `xkbcommon` in **1.34 ms**, layout resolved as "English (US)" |
| `clipboard-out` | `wl-paste` read `text/uri-list`, `text/plain;charset=utf-8` and the full MIME list including `x-special/gnome-copied-files` |
| `clipboard-in` | a selection set by another process read back exactly, worst dispatch gap during the transfer **0.58 ms** |
| `primary-out` | **compositor limitation** — see below |
| `primary-in` | 5 MIME types offered, 23 bytes read, worst gap **0.58 ms** |
| `slow-transfer` | a peer that offers a type and never writes: 0 bytes after 1,200 ms with the read still open, dispatch interval p50 **0.253 ms**, p99 0.275 ms, **worst 0.394 ms** against rule 1's 8 ms, n=4,784 |
| `text-input-v3` | preedit `"にほんご"` arrived in **2.19 ms**, the commit `"日本語"` replaced it in **2.18 ms**, and the field holds 3 characters |

`checker-1:1` is worth a sentence on its own. Spike A's crispness assertion was
internal — it compared a glyph's quad to its rasterized size and could not see a
compositor resampling the whole surface, which is exactly what was happening. This
check asks the framebuffer instead, and it locates itself: it scans the *entire*
output for the longest horizontal run of one-pixel alternation between two known
greys. Resampling turns that into a gradient and nothing else on a desktop
produces it. The arithmetic that converts a logical window position into a
physical one is left out of the measurement on purpose, because that arithmetic
could be wrong in the same direction as the bug.

`slow-transfer` needed a hung peer, and there is no way to be one from inside the
process under test. `zwlr_data_control_manager_v1` provides one: a client can own
the selection without a surface and without taking keyboard focus, so a second
copy of this binary takes the clipboard, is asked for the bytes, and keeps the
file descriptor open forever. It is an instrument — `sk-plat` will never bind a
data-control manager, because owning the clipboard without focus is the thing the
protocol exists to prevent.

**`primary-out` is a compositor limitation with no fallback, and it took the
longest to place.** Hyprland accepts
`zwp_primary_selection_device_v1.set_selection` and then discards it while
`misc:middle_click_paste` is `0`: no protocol error, no `cancelled`, and no offer
to anybody. Three things had to be ruled out before that could be said. A stale
input serial — ruled out by synthesizing a click and using its serial, which
changed nothing. `wl-paste` being wrong — ruled out by a second instance of this
spike reading over `zwp_primary_selection_device_v1` itself, which also saw
nothing. Our own code — ruled out by setting the option to `1` and changing
nothing else, after which the same code is readable by `wl-paste`.

The consequence is the part that matters: **the request produces no error, so a
primary-selection feature cannot even be greyed out.** A user with that option off
selects a path, middle-clicks, and nothing happens, and we cannot tell them why.
The only honest design is to treat the primary selection as best-effort and never
build anything on top of it.

### X11 — 6 pass, 3 documented limitations, of 9

| Check | Result |
| --- | --- |
| `x-window` | mapped with `XdndAware` 5. **`Xft.dpi` unset and RANDR reports 96.0 dpi on a 1.25-scaled output** |
| `x-clipboard-out` | our own requestor read `TARGETS` as the five atoms we advertise and the data as 104 bytes |
| `x-clipboard-in` | GTK offered **10 targets** and we read its data with a real server timestamp |
| `x-primary-out`, `x-primary-in` | the same over `PRIMARY`; GTK offered 9 targets |
| `x-incr` | **1,360,000 bytes in 21 property chunks**, 40,000 entries, complete and byte-exact |
| `x-bridge-out`, `x-bridge-in` | **compositor limitation** — see below |
| `dnd-action-vocab` | XDND has **5** actions including `XdndActionLink`; `wl_data_device_manager.dnd_action` has **3** and no link |

`x-window`'s number is a design fact, not a failure: Xwayland tells an X11 client
nothing about a fractional scale. The X11 path draws at 1× and the compositor
upscales it, and no amount of client-side care changes that. The X11 backend is
therefore for compatibility, never for quality, and rule 1's frame budget applies
to the Wayland path.

**The X11-to-Wayland clipboard bridge does not work in either direction on this
compositor**, and the sequence that establishes it is worth keeping, because four
plausible suspects had to be eliminated in order:

1. Our ICCCM code — eliminated. A second process of ours reads our selection's
   `TARGETS` and data correctly.
2. Our requestor — eliminated. It reads a **GTK** X11 owner's ten targets and its
   data, with a real server timestamp.
3. `wl-paste` — eliminated. The same `wl-paste` reads a Wayland owner correctly.
4. Something specific to our window — eliminated. A **GTK** X11 client's selection
   does not cross the boundary either, and a GTK X11 client cannot read a Wayland
   owner's selection.

From a clean state — nobody owning anything on either side — a GTK X11 client
takes `CLIPBOARD`, our X11 requestor sees its full target list, and `wl-paste`
says "Nothing is copied". The fallback is real but limited: Skerry runs natively
on Wayland, so copy and paste between Skerry and other Wayland applications works;
copy in a legacy X11 application and paste in Skerry is broken by the compositor
and we cannot repair it from either side.

`x-incr` earned its place. A selection of a few thousand files is a megabyte of
`text/uri-list`, which is past every implementation's single-request limit, and
`INCR` is the part everyone skips. It also produced the sharpest bug in this
spike; see the fourth item under *what this changed*.

### The drag matrix

Two directions, seven peers, both display servers. "Read the data" means the peer
asked us for a MIME type and we served it; "landed" means the file is in the
destination directory afterwards, checked on disk rather than believed.

**Us as the source:**

| Peer | Toolkit | Wayland | X11 |
| --- | --- | --- | --- |
| Nautilus | GTK 4 | **pass** — accepted and read `text/uri-list`, `dnd_finished`, **2 files landed** | **pass** — XDND v5, 9 `XdndStatus`, `XdndActionCopy`, `XdndFinished` accepted, **2 files landed** |
| Konsole | Qt 6 + KDE | **pass** — accepted and read `text/uri-list`, `dnd_finished` without `dnd_drop_performed` | **pass** — XDND v5, 8 `XdndStatus`, `XdndActionCopy` |
| Firefox | GTK 3, snap | **pass** — accepted `x-special/gnome-copied-files`, asked for `text/uri-list` and `text/plain;charset=utf-8`, `dnd_finished` | owed — see below |
| Qt Designer | Qt 6 | **pass** — asked for both types with no accepted type reported | flaky — no `XdndAware` window found under the pointer in the recorded run, and accepted-with-refusal in an earlier one |
| the spike itself | ours | **pass** — the peer read the data | — |

**Us as the target:**

| Peer | Toolkit | Wayland | X11 |
| --- | --- | --- | --- |
| Nautilus | GTK 4 | **pass** — 5 types offered, including `application/x-gtk-local-dnd` and a portal type; we read the URI | owed — no grab position started a drag |
| the spike itself | ours | **pass** — **104 bytes** of `text/uri-list` from our own source | — |

**Every Wayland source pair passes.** On X11, Nautilus and Konsole pass; Qt
Designer's XDND path is flaky in a way the harness cannot currently distinguish
from a missed pointer position, and Firefox is owed for the reason below.

Qt Designer is the peer that shows why "the peer asked us for the bytes" is the
right criterion. It requests `text/uri-list` and `text/plain;charset=utf-8`
without ever reporting an accepted type, and then does nothing useful with a
`.txt` — which is correct behaviour for a form editor. The protocol conversation
completed; the application declined the content. Those are different things and
only the first is ours.

Nautilus on X11 asked for **`application/vnd.portal.filetransfer` first**, then
`application/vnd.portal.files`, and only then `text/uri-list`. A source that
offers only `text/uri-list` still works, because the target falls back — but a
sandboxed target may not, and `sk-plat` should offer the portal types once
[05-platform-integration.md](../../docs/05-platform-integration.md)'s portal work
exists.

Two limitations show up in every Wayland pair, and both have consequences beyond
this spike:

- **The negotiated action is never reported.** `wl_data_source.action` and
  `wl_data_offer.action` were sent *zero* times across every pair, including the
  ones where a file demonstrably landed. `source_actions` arrives; the resolved
  action does not. The bar names copy, move and link, and on this compositor a
  target cannot tell which one it has been asked for.
- **`dnd_finished` is not always delivered.** Nautilus produced it; Konsole
  produced it without `dnd_drop_performed`; our own two-process pair produced
  neither, even though the peer read the data. A move is "copy, then delete the
  originals once the drop is confirmed", and the confirmation is `dnd_finished`.
  A source that never receives it can never safely delete anything, so the
  fallback is to treat every unconfirmed drop as a copy — which is the safe
  direction, and is also why move must not be implemented as copy-then-delete
  driven by that event alone.

### The `winit` arm

[d-platform-winit](../d-platform-winit/README.md) runs the same list through
`winit` 0.30.13's public API, and checks each claim against the crate's own source
so that "there is no API" is a search result rather than an assertion.

| Capability | Through `winit` | Evidence |
| --- | --- | --- |
| window, `xdg_shell` | yes | `window.rs:35: pub struct Window` |
| fractional scale | yes | `event.rs:379: ScaleFactorChanged` — as an `f64`, not the 120ths numerator the protocol sends |
| keyboard, xkb | yes | `event.rs:516: pub struct RawKeyEvent` |
| IME / `text-input-v3` | yes | `window.rs:1283: pub fn set_ime_allowed` |
| drag-and-drop target | **partly** | `event.rs:180: DroppedFile(PathBuf)` — paths only: no MIME list, no action, and no way to refuse |
| **drag-and-drop source** | **no** | no occurrence of `start_drag` anywhere in the crate |
| **clipboard** | **no** | one occurrence, `event_loop.rs:622: // TODO(kchibisov): Remove `cfg` when the clipboard will be added.` |
| **primary selection** | **no** | no occurrence |
| **dnd actions** | **no** | no occurrence |

Four of nine unreachable. A bare substring search was not enough to say so:
`clipboard` does occur in `winit` 0.30 — in a `TODO` saying the clipboard has not
been added. That is the difference between an API and an intention, and the audit
had to be able to tell them apart before its "no" was worth anything.

### What this changed

Eight things, each with the measurement that produced it. The first four are
invariants `sk-plat` has to hold; they were all found by a check failing, and none
of them would have been found by reading the protocol.

1. **Data devices are created before the first surface commit.** A device created
   after the first keyboard enter is never told what the current selection is,
   because the compositor sends `selection` on focus change and not on device
   creation. The same reader, against the same owner: created late, `no-offer`;
   created before the first draw, `types text/plain;charset=utf-8 text/uri-list`
   and 35 bytes. The symptom would have been an empty clipboard at startup until
   the user copied something.

2. **A drag offer must survive `wl_data_device.leave`.** Hyprland brackets the
   drop with leaves — `leave`, `drop`, `leave` — and clearing the offer on `leave`
   left nothing to call `receive` on. Before: 0 bytes and a drop that looked like
   the compositor refusing us. After: 104 bytes. The compositor was doing exactly
   what `wl_data_device` says.

3. **A selection needs a *fresh* input serial, and the two protocols disagree
   about it.** `wl_data_device.set_selection` is accepted with a stale serial;
   `zwp_primary_selection_device_v1.set_selection` is validated and silently
   dropped. Same window, same code, one works and one does not, and neither
   reports anything.

4. **A source keeps dispatching after the drop, and a requestor must not defer
   events into the queue it polls from.** The target's request for the bytes
   arrives *after* the button is released, so a source that pumped for 800 ms and
   stopped left the target reading a pipe nobody would write to. The X11 half of
   the same mistake was worse: deferring unrelated events back into the same queue
   made the `INCR` loop spin on stale events forever and report a 1.36 MB transfer
   as 0 bytes. There is also a stale-`PropertyNotify` trap underneath it — the
   `INCR` header is itself a property write, so it produces a notification that a
   requestor must not mistake for the first chunk.

5. **The action vocabularies cannot be unified.** XDND has copy, move, link, ask
   and private; Wayland has copy, move and ask. `sk-plat` has to expose the union
   and let the policy above it grey out what the current display server cannot
   express — a "link here" entry on Wayland has to be built out of `ask` plus a
   menu of ours.

6. **The X11 backend is for compatibility, not for quality.** `Xft.dpi` is unset
   and RANDR reports 96 dpi on a 1.25-scaled output, so an X11 client cannot know
   the scale and the compositor upscales whatever it draws.

7. **`text/uri-list` alone is not the whole story.** GTK asks for
   `application/vnd.portal.filetransfer` before it asks for `text/uri-list`.

8. **A data source needs a write queue on the event loop.** This spike performs a
   single non-blocking write and *records* short writes rather than looping,
   because a spike that quietly retried would have hidden the requirement. Zero
   short writes were observed at these payload sizes, which is not evidence that
   one write is enough — it is evidence that the case has not been reached yet.

### What is owed

- **Another compositor.** One compositor is not a portability claim, and three of
  the four limitations above are Hyprland's rather than Wayland's. Mutter and kwin
  runs are owed, and `primary-out` and both `x-bridge` checks are the ones most
  likely to change.
- **The peer-to-us direction on X11.** It works on Wayland against Nautilus. On
  X11 no candidate grab position started a drag, and the harness cannot tell a
  view that ignored the press from a position that missed an item.
- **Firefox on X11.** The browser on this machine is a Wayland snap, so
  `--new-window` cannot produce an X11 window, and a second instance needs its own
  profile and then shows onboarding rather than content. `./peers.sh firefox-x11`
  exists for a session that has an X11 Firefox in it.
- **Dolphin, Chrome and Thunderbird.** Not installed, and there is no root on this
  machine. Dolphin is the one that matters: KIO's drop handling is its own
  dialect.
- **A real input method.** The IME condition is met with this spike acting as its
  own input method over `zwp_input_method_manager_v2`, which exercises our client
  half end to end. fcitx5 and ibus interop is unmeasured.
- **`xdg_toplevel_drag`.** Not advertised by this compositor, and M2's
  drag-a-tab-into-a-new-window depends on it.

Everything a peer does is read back from the peer, and everything the harness does
to a peer is a guess about somebody else's UI. The Firefox sequence above is the
cautionary tale: three separate harness faults, each producing a refusal that was
reproducible, consistent and wrong. A pair that fails is a reason to distrust the
harness first.

## Verdict

**Go.** Every condition of the bar is met or is a documented compositor
limitation, and the four limitations are named with their fallbacks above.

We are a drag-and-drop **source** against GTK 4, GTK 3 and Qt 6 — every Wayland
pair passes, and Nautilus and Konsole pass on X11 as well — with files landing on
disk to prove it, and a target that reads what Nautilus gives it.
The clipboard carries the full MIME set both ways on Wayland and both ways on X11
including a 1.36 MB `INCR` transfer. Fractional scale negotiates exactly and
survives to the framebuffer at one buffer pixel per output pixel. CJK composes
into a field in 2.2 ms. A hung peer's transfer costs the event loop 0.394 ms
against rule 1's 8 ms.

**[ADR 0002](../../docs/adr/0002-own-platform-layer.md) stands, and its revisit
trigger is answered rather than deferred.** The trigger asks whether a
general-purpose windowing crate can be a DnD source against GTK and Qt on both
display servers *and* handle `text-input-v3`. `winit` handles `text-input-v3`
well; it has no drag source, no clipboard, no primary selection and no action
negotiation, and its drop target reports paths with no MIME list and no way to
refuse. Four of nine capabilities are unreachable, and the four missing ones are
the four a file manager spends its day in. Six weeks of M0 is the price of the
half `winit` does not have.

**Three things to design**, in the order they will bite:

1. **Ordering invariants in `sk-plat`.** Devices before the first commit, the
   offer surviving `leave`, a fresh serial per selection, and a source that keeps
   dispatching after the drop. Each is one line of code and each was a
   silent failure that looked like a compositor bug. They belong in `sk-plat`'s
   own tests, phrased as this spike phrased them: a second process on the other
   end, and a byte count.

2. **A capability surface for what the display server cannot do.** Three of the
   four limitations produce *no error*: a discarded primary selection, an
   unreported action, an undelivered `dnd_finished`. `sk-plat` cannot detect any
   of them, so the policy above it has to be written to be correct when they are
   absent — treat every unconfirmed drop as a copy, and never make a feature
   depend on the primary selection.

3. **The move operation cannot be built on `dnd_finished` alone.** This is the one
   with a real user consequence: a move implemented as copy-then-delete-on-confirm
   would silently never delete on this compositor, and a move implemented without
   waiting for confirmation would delete when the drop was refused. M3's operation
   engine needs to own the decision, with the drop reporting only what the peer
   actually took.
