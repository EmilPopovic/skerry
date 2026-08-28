# 0009 — An X11 backend behind a runtime-selected enum, with scale from `Xft.dpi`

**Status:** Accepted

## Context

M0 owes X11 and the whole of `x11rb`. Until now `sk-plat` has had no seam at
all: `lib.rs` re-exports `wayland::Window` under the name `Window`, and every
caller above it names the Wayland type directly. A second display server forces
the seam into existence, and two parts of it are expensive to reverse — how a
backend is chosen, and how scale is decided where there is no protocol that
states it.

### Choosing a backend

The obvious answer is a Cargo feature and `#[cfg]`. It is also the one
[ADR 0007](0007-runtime-capability-detection.md) exists to refuse, and rule 9
prices it: a binary that can talk to one display server is a binary a
distribution builds twice and a user can install the wrong half of. The common
machine has **both** — this one runs Hyprland with an Xwayland on `:0` — so the
choice is not even a property of the machine, it is a property of the session,
and a build flag cannot see a session.

The second answer is a trait and `Box<dyn Backend>`. It was genuinely
considered and it is the one this repository's instincts point at, because a
trait is what "a seam we own" usually means. Two things lost it. `pump` is on
the frame loop and is called once per wakeup, so its dispatch is in the budget
that [09](../09-performance-budgets.md) measures; a vtable there is not
expensive, but it is not free and it buys nothing. The heavier objection is
that a trait forces every backend-specific fact through a lowest common
denominator: `Capabilities` on Wayland carries the compositor's DRM render
device read from `zwp_linux_dmabuf_v1`, which is not a question X11 is being
asked, and `RawHandles` on X11 is a connection, a screen and a 32-bit window id
where Wayland has two pointers. A trait models those as `Option`s that one
implementation always answers `None` to, or as downcasts, and both are worse
than a match.

The third answer is an enum with one variant per backend. Every call site is a
two-arm match the branch predictor gets right, there is no allocation, each
backend keeps its own private state, and the compiler names every method a
third backend forgets. Its cost is real and it is boilerplate: roughly twenty
methods written twice, in a dispatch module that does nothing but forward.

### Deciding a scale

Wayland states the scale exactly. `wp_fractional_scale_v1` sends a numerator
over 120 and `wp_viewporter` maps a physical buffer onto a logical surface, and
that pair is the reason text is crisp at 125% — the crispness row in
[09](../09-performance-budgets.md) is about those two objects.

X11 has no equivalent. It has four sources and none of them is authoritative:

- **`Xft.dpi`** in the X resource database on the root window. What GTK, Qt and
  every desktop's settings daemon read and write. Watchable: it arrives as a
  `PropertyNotify` on `RESOURCE_MANAGER`, so a change reaches a running client.
- **RANDR physical size.** Pixels over millimetres is a real DPI when the EDID
  is right. EDIDs are frequently wrong — a projector reporting 160 mm, a
  monitor reporting zero — and one X screen spans several outputs with
  different answers.
- **`GDK_SCALE` and friends.** Toolkit-private, integer only, and reading
  another toolkit's environment variable is guessing at its policy.
- **The command line**, which `--scale` already provides for captures and
  scenarios.

The temptation is to combine them: take `Xft.dpi`, sanity-check it against
RANDR, fall back to the physical size when it is absent. That is the design
this decision rejects. A wrong EDID then produces a wrong scale with nothing on
screen saying so, and "the text is slightly wrong on this monitor" is a bug
report nobody can act on. A single stated source that can be overridden is
worse on exotic hardware and honest everywhere.

### Two smaller forks, recorded because they look like details and are not

**The keyboard.** The `xkbcommon` crate has an `x11` feature that links
`libxkbcommon-x11` and hands over `xkb_x11_keymap_new_from_device`, which is the
textbook route. It is also a **link-time** dependency, so a machine with
`libxkbcommon.so` and no `libxkbcommon-x11.so` would fail to start `skerry` at
all — including on Wayland, where X11 is not wanted. That is verbatim the
failure condition rule 9 cites when it explains why `wayland-client` is taken
with `dlopen` and pays three crates for it.

**The pointer image.** X11 has no `wp_cursor_shape_v1`. The core cursor font is
always present and is a set of 1987 bitmaps that look wrong beside any theme
installed in the last twenty years. The alternative is parsing an Xcursor
theme, which [07](../07-dependencies.md) already describes as "a library".

## Decision

**`sk_plat::Window` becomes an enum with `Wayland` and `X11` variants, chosen at
run time by `Window::open`.** The order is `WAYLAND_DISPLAY`, then `DISPLAY`,
then a failure naming both. `SKERRY_BACKEND=x11|wayland` forces one, so a
machine with both can be made to exercise either without a second binary. Every
public method dispatches through a match; no caller above `sk-plat` learns which
backend it has, and `tools/check-invariants.py` gains a rule that no crate above
`sk-plat` may name either variant.

`Capabilities`, `Cursor`, `Edge`, `WindowState`, `WindowConfig` and `Event`
move out of `wayland` into backend-neutral modules. `RawHandles` becomes an enum
of `Wayland { display, surface }` and `X11 { connection, screen, window }`, and
`sk-gpu` matches on it to build the corresponding `raw-window-handle` pair.

**`x11rb` 0.14 is taken with `dl-libxcb`**, which loads `libxcb` at run time for
the same reason and by the same mechanism `wayland-client` loads
`libwayland-client`: an X11-only machine and a Wayland-only machine both start.
`dl-libxcb` also yields a real `xcb_connection_t *`, which is what
`VK_KHR_xcb_surface` needs and what a pure-Rust connection cannot provide.
Extensions taken: `xkb`, `xfixes`, `render`, `cursor` and `resource_manager`.
Not `randr`: the scale does not come from it, and the only thing asked about
monitors is whether the extension is present at all, which is a core request.

**Scale is `--scale` if given, else `Xft.dpi` from `RESOURCE_MANAGER`, else 96,**
converted to the same `scale_120` numerator Wayland reports so that nothing
above `sk-plat` knows there are two models. `RESOURCE_MANAGER` is watched for
`PropertyNotify`, so changing the desktop's scale reaches a running window
without a restart. RANDR's physical size is **not** consulted.

**The keymap comes from `_XKB_RULES_NAMES` on the root window**, fed to
`xkb_keymap_new_from_names` through the `xkbcommon` already in the graph. Where
the property is absent, xkbcommon's own defaults resolve against the system's
XKB configuration. `libxkbcommon-x11` is not linked. Auto-repeat is turned into
press-only events by asking XKB for detectable auto-repeat, and the repeat
timing is the client-side one `sk-plat` already implements for Wayland, so both
backends repeat identically.

**The pointer takes `xcursor` through `x11rb`'s `cursor` feature**, one crate, and
falls back to the core cursor font where no theme resolves.

## Consequences

**The dependency ratchet moves from 27 to 32.** Five crates enter the graph:
`x11rb`, `x11rb-protocol`, `as-raw-xcb-connection`, `gethostname` and `xcursor`.
`libloading`, `cfg-if`, `once_cell`, `libc`, `rustix` and `bitflags` are already
there and are not counted twice. The row and the re-measure land in the same
commit as the dependency, which is what rule 10 requires.

**Roughly twenty methods are now written twice**, and a third backend would
write them a third time. This is the cost that buys the absence of `#[cfg]`, and
it is paid in a dispatch module that contains no logic — a reviewer can read it
in a minute and a mistake in it is a compile error rather than a behaviour.

**One capability has no X11 equivalent and is reported absent rather than
faked.** `show_window_menu` is `xdg_toplevel.show_window_menu` on Wayland; EWMH
has no request for it and GTK uses a private `_GTK_SHOW_WINDOW_MENU` that only
mutter answers. `WindowState::can_show_menu` is false on X11 and the frame does
not draw an affordance for it, which is the same treatment Wayland's absent
primary selection already gets.

**Scale is wrong, on purpose, on a machine whose desktop scales without setting
`Xft.dpi`.** It renders at 100% and `--scale` fixes it. The alternative was
being wrong in a way nobody could see.

**Two harnesses gain a second axis and one of them gets cheaper.** The golden
frames are unaffected — `--capture` has never needed a display server. The 61
GUI scenarios currently stay out of CI because Wayland's virtual-input protocols
need a compositor with a seat, and a runner has no seat to give; **XTEST needs
none**, so the same scenarios driven against `Xvfb` are runnable on a plain
runner. That is a consequence of this decision rather than a motivation for it,
and it is the largest hole in [08](../08-roadmap.md)'s CI row closing by
accident.

## Revisit trigger

**The enum becomes a trait** when a third backend arrives that is not a display
server — a remote or recording backend beyond `--capture` — or when the dispatch
module exceeds **30 forwarded methods**. Two backends and twenty methods is
boilerplate a reviewer can hold in their head; three and thirty is not, and at
that point the vtable is cheaper than the duplication.

**RANDR joins the scale chain, below `Xft.dpi`,** on the first desktop
environment found to run at a non-100% scale without setting `Xft.dpi` in
`RESOURCE_MANAGER`. GNOME-on-Xorg, KDE Plasma on X11 and XFCE all set it today;
one that does not is the number that changes this.

**`libxkbcommon-x11` is linked after all** if `_XKB_RULES_NAMES` is found absent
or wrong on any X server that is not `Xvfb` — that is, if the property turns out
not to be the universal fact this decision assumes. `Xvfb` itself is expected to
lack it and to fall through to xkbcommon's defaults, which is why it is excluded
from the trigger.
