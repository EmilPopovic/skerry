# 0026 — AT-SPI is a tree derived from the frame, published on the bus we already speak

**Status:** Accepted

## Context

[10-risks.md](../10-risks.md) has carried one High risk since before there was
code: *a custom renderer means no toolkit accessibility*. Its mitigation reads
"`accesskit` wired in at M1, not M7, and produced by the same immediate-mode pass
as the draw commands so it cannot drift", and
[07-dependencies.md](../07-dependencies.md) lists `accesskit` with the note
"None expected; the alternative is no accessibility, which is not an option."

Neither happened. The window has been usable for months and is invisible to a
screen reader: no application on the accessibility bus, no tree, no events. A
public pre-release makes that a promise broken in front of people who need it,
which is a different thing from a milestone that is late.

Three routes were live.

**`accesskit` with its Unix adapter.** The obvious one, and it is the crate the
dependency table already blessed. `accesskit_unix` speaks AT-SPI over `zbus`,
and `zbus` is an async D-Bus library with an executor under it — which is
[ADR 0005](0005-no-async-runtime.md)'s single forbidden thing and the exact
reason [ADR 0023](0023-d-bus-is-a-socket-we-speak-ourselves.md) declined `zbus`
for `FileManager1` fourteen months of decisions ago. Taking it here would put
the executor in the graph through the accessibility door after refusing it at
the desktop-integration door, and the twenty crates behind it would arrive with
it. The dependency table's note is stale: it was written before ADR 0023
existed, and it priced `accesskit` against *no accessibility* rather than
against *AT-SPI on our own socket*, which was not a thing that could be said
then.

**AT-SPI over our own D-Bus.** ADR 0023 built a D-Bus client and server in 1,197
lines: `EXTERNAL` authentication, a marshaller, one thread blocking on a read to
serve. AT-SPI is D-Bus and nothing else — its whole interface surface is method
calls and signals on a second bus whose address the session bus hands out. What
it needs that we did not have is three more wire types, a long-lived connection
that both serves and emits, and the interfaces themselves.

**Nothing, documented.** Honest, cheap, and what the previous version of this
plan proposed. It was rejected because "not an option" is what the risk register
already says, and because the gap grows: every surface added before the tree
exists is a surface to retrofit into it.

The second question is *where the tree comes from*, and it is the one that
decides whether this rots. A tree assembled by hand beside the renderer drifts
from what is on screen within a month — this project has already caught that
exact failure twice, in a press tested against a rectangle it recomputed
([12-measurement.md](../12-measurement.md)) and in a check box drawn from one
geometry and hit-tested from another.

## Decision

**The accessibility tree is derived, once per frame, from what the frame
recorded — and published over `sk_desktop::bus` on the accessibility bus.**

- **`sk_ui::Hits` is the source.** The draw already records the rectangle of
  every interactive thing it puts on screen, because a press is tested against
  it; the tree is those rectangles with a role and a name attached, assembled in
  `sk_app::a11y` from `Hits` and the state the frame drew from. A surface that
  is not in `Hits` is not in the tree, which makes "add a control" and "make it
  reachable" the same commit rather than two.
- **No `accesskit`, and `07-dependencies.md`'s row for it is withdrawn.** The
  transport is `sk_desktop::atspi`, on the connection type ADR 0023 already
  built. The wire gains `Struct` and `i32`, which AT-SPI needs and
  `FileManager1` did not.
- **A second connection, on its own thread, long-lived.** The accessibility bus
  is not the session bus: its address comes from `org.a11y.Bus.GetAddress` on
  the session bus, and a client registers by calling `Embed` on the registry.
  That connection outlives a call — it serves the tree *and* emits events — so
  it blocks on `poll` over the socket and an eventfd, the same pair the frame
  loop waits on.
- **The frame loop writes no bytes to it.** A frame publishes a snapshot into a
  mutex and wakes the thread; the thread does the marshalling and the writes.
  [Rule 1](../03-principles.md) is about the frame loop never blocking on
  anything, and a socket to a daemon is exactly the thing it must not block on.
- **Absent means absent.** No accessibility bus, no `Embed`, no registry: the
  thread is not started and nothing else changes.
  [ADR 0007](0007-runtime-capability-detection.md) — a desktop with no AT is the
  common case, and paying for it is not allowed.
- **Roles are the ones a screen reader acts on**: a frame, a list, a list item,
  a push button, a text field, a menu, a menu item, a page tab and a page tab
  list. Interfaces are `Accessible`, `Application`, `Component` and `Text`.
  `Table`, `Selection`, `Value` and `Action` are not implemented in this record's
  scope, and each is a named gap rather than a silence.

## Consequences

**A second thing to keep in step with the draw, and one place where it can go
wrong.** `Hits` was designed for hit-testing, so it holds rectangles and not
names: the assembly in `sk_app::a11y` reads names out of the same state the draw
read them from, and where those two disagree the tree is wrong in a way no
existing test catches. The mitigation is a test that walks the published tree
and asserts every node's rectangle is one the frame recorded — mechanical, and
it fails when somebody adds a control and forgets the name.

**Events are the expensive half.** A tree that answers questions is what an AT
walks once; what makes a screen reader usable is being *told* — focus moved, the
caret moved, the selection changed, the window activated. Each is a signal with
a body of `(souua{sv})`, and each has to be emitted exactly when the thing it
describes changed rather than once a frame. This record covers the four above and
nothing else.

**We now own an accessibility implementation.** When an AT does something we do
not answer, the bug is ours and the fix is in this repository — which is the
cost of every "we write it ourselves" decision in this project, taken here with
the same eyes open as the other five.

**`accesskit`'s row leaves `07-dependencies.md`.** The crate remains the right
answer for a program that has an async runtime already, and this one will not.

## Revisit trigger

**A screen reader that cannot drive the window.** Concretely: Orca, on GNOME,
cannot move through a listing, open a folder, rename a file and read the status
line — measured by doing it, not by reading the tree. If the missing piece is an
interface this record left out, that is an extension; if it is the shape of the
tree or the way it is published, this decision is wrong and its successor says
so.

**Or a second consumer of the same tree.** A test harness, a remote-control
protocol, or the scenario driver reading names instead of coordinates: two
consumers make the tree an API, and an API needs a record about its stability
that this one does not attempt.
