# Spike D, arm 2 — the same list through `winit`

The bar, the method and the verdict live in
[d-platform-and-dnd](../d-platform-and-dnd/README.md). This arm exists for one
sentence in [ADR 0002](../../docs/adr/0002-own-platform-layer.md):

> If Spike D demonstrates that a general-purpose windowing crate can act as a DnD
> source against GTK and Qt applications on both display servers **and** handle
> `text-input-v3` adequately, take it and delete this decision — six weeks is a
> large price for a problem someone else has solved.

Six weeks of M0 is a large enough price that the answer should be a table rather
than an opinion, which is why this is built the way
[Spike A](../a-million-rows-gpui/README.md) was built twice.

## What is measured

`reachable` means **through the public API alone**. Dropping to
`raw_window_handle` and speaking the protocol directly is not counted, because a
raw-protocol escape hatch is exactly the thing ADR 0002 says we would end up
fighting: at that point the crate is a layer we work around rather than a layer we
use.

Each claim is checked against the crate's own source in the cargo registry, and
the line that was found is printed, so the classification can be disagreed with
rather than taken on trust. A bare substring search would not do:

```
  PASS  not reachable, no declaration        clipboard
        event_loop.rs:622: // TODO(kchibisov): Remove `cfg` when the clipboard will be added.
```

`clipboard` does occur in `winit` 0.30 — in a `TODO` saying the clipboard has not
been added. That is the difference between an API and an intention, and an audit
that cannot tell them apart has not audited anything. The check therefore
classifies a match as a declaration only when it is not a comment and is either a
`pub` item or shaped like a variant of a public enum.

## Result

`winit` 0.30.13, on Hyprland 0.53.3 at a 1.25 scale — the same session as arm 1.

| Capability | Through `winit` | Evidence |
| --- | --- | --- |
| window, `xdg_shell` | yes | `window.rs:35: pub struct Window` |
| fractional scale | yes | `event.rs:379: ScaleFactorChanged` |
| keyboard, xkb | yes | `event.rs:516: pub struct RawKeyEvent` |
| IME / `text-input-v3` | yes | `window.rs:1283: pub fn set_ime_allowed` |
| drag-and-drop target | partly | `event.rs:180: DroppedFile(PathBuf)` |
| drag-and-drop source | **no** | `start_drag` does not occur in the crate |
| clipboard | **no** | one occurrence, and it is the `TODO` above |
| primary selection | **no** | `primary_selection` does not occur |
| dnd actions | **no** | `DndAction` does not occur |

**Four of nine are unreachable**, and they are the four a file manager spends its
day in.

The three that are reachable are reachable *well*: `set_ime_allowed` plus
`Ime::Preedit` with a cursor range is the same shape as our own
`zwp_text_input_v3` handling, and `ScaleFactorChanged` does negotiate the
fractional scale. Two details still cost something.

- **The scale arrives as an `f64`.** The protocol sends a numerator over 120, and
  1.25 is exact as `150/120` and inexact as a float. Arm 1 keeps the integer and
  derives the buffer size from it, because that is the number the compositor
  actually sent. Through `winit` the integer is not exposed.
- **`DroppedFile(PathBuf)` is paths and nothing else.** No MIME list, no action,
  and no way to *refuse* a drop. A file manager has to accept everything or
  nothing, and cannot distinguish a drop onto a folder row from a drop onto the
  pane behind it, because by the time it hears about the drop the negotiation is
  over.

## Verdict

**ADR 0002 stands.** The revisit trigger asks for two things: a drag *source*
against GTK and Qt on both display servers, and adequate `text-input-v3`. `winit`
delivers the second and has no API for the first. It also has no clipboard, no
primary selection and no action negotiation, so taking it would mean a windowing
crate plus a clipboard crate plus a drag crate plus an action shim — which is
rules 9 and 10's argument stated as a dependency list, and a second platform layer
built out of four maintainers' assumptions.

`winit` is not a bad crate; it is a crate for applications that draw in a window.
A file manager is mostly the part outside the window.

## Running it

```
cargo run --release -- api      # the audit alone, no window
cargo run --release             # the audit, then a window for six seconds
cargo run --release -- run 20   # hold the window for twenty seconds

# IME through winit, driven by arm 1 acting as the input method:
../d-platform-and-dnd/target/release/spike-d-platform input-method 30 &
cargo run --release -- run 20
```
