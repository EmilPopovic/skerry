# 0023 — D-Bus is a socket we speak ourselves: one thread to serve, one short-lived connection per call

**Status:** Accepted

## Context

M7 is desktop citizenship, and most of it is D-Bus: `org.freedesktop.FileManager1`
so another application can say *show me this file*, a well-known name so a second
`skerry` hands its arguments to the first instead of opening a second window,
udisks2 for mounting and ejecting, and the settings portal for the desktop's
light-or-dark preference.

[Rule 9](../03-principles.md) forbids linking `libdbus`, and
[06-portability.md](../06-portability.md) already says what that means: "D-Bus is
spoken as a protocol over its socket, not through `libdbus`." What it did not
settle is *whose* code speaks it.

- **`zbus`.** [07-dependencies.md](../07-dependencies.md) accepts it in tier 2
  with a stated exit, and the exit is now the reason: it brings an async
  executor, and "no async runtime, no second scheduler" is one of the eight
  invariants this project settles arguments with
  ([ADR 0005](0005-no-async-runtime.md)). The crate budget makes the same point
  arithmetically — 27 non-GPU crates is the ratchet, the tree is at 32 today, and
  `zbus` with its `zvariant`, `async-io`, `futures-*` and `nix` is another twenty.
  A dependency whose exit condition is already met on arrival is a dependency
  taken by inertia.
- **Ours.** That same file prices it: "the protocol is a Unix socket, `EXTERNAL`
  auth and a marshaller, roughly 1,500 lines", and already lists "the D-Bus
  service side of FileManager1" under *what we write ourselves*. The protocol is
  small and frozen: a fixed header, eight basic types we need six of, and four
  alignment rules.

The second question is threading, and it is the one that decides the shape.
A bus connection is a socket that can be read at any time, and this program's
frame loop sleeps on descriptors it owns in `sk-plat` — a bus descriptor in that
poll set would put D-Bus in the platform layer, which is the crate that must not
know what a file manager is.

## Decision

**`sk_desktop::bus` speaks the protocol on a Unix socket. Serving is one thread
per bus; calling is a short-lived connection on a job.**

- **The marshaller covers what we send and receive and nothing else**: `y u b s o
  g v`, arrays of them, `a{sv}`, and the struct rule that everything aligns to
  its own width with structs at eight. Anything else is a parse that returns
  `None`, which is a message we did not understand and therefore did not act on.
- **`EXTERNAL` authentication**, the uid in hex, `NEGOTIATE_UNIX_FD` never asked
  for: no feature here passes a descriptor, and a `h` in a signature we cannot
  honour would be a lie told at connect time. `BEGIN`, then `Hello`, then the
  connection is a connection.
- **Serving is a thread that blocks on `read`**, exactly the shape
  [`sk_fs::Watcher`](../../crates/sk-fs/src/watch.rs) already has: it answers what
  it can itself, and what belongs to the window it sends down a channel and wakes
  the frame loop with the same eventfd every job uses. A thread blocked in a read
  is not a scheduler, which is what ADR 0005 forbids.
- **Calling is a connection of its own, on a job, thrown away after the reply.**
  This is the simplification that removes an entire layer: no shared socket, no
  write mutex, no serial-to-caller table, no reply timeouts to reconcile with the
  frame loop. A mount request costs a `connect`, an auth round trip and a call —
  measured below — on a thread that is already off the frame path, and the
  alternative is a correlation table that exists to save two milliseconds.
- **The name is `dev.skerry.Skerry`**, which is the `app_id` the window already
  sets — one identity for the window, the `.desktop` file and the bus, because a
  desktop matches them to each other by string equality.
- **`org.freedesktop.FileManager1` is requested without replacing an owner.**
  Nautilus may hold it; taking it from a running file manager because we started
  is not ours to do. Whether we got it is a capability, probed at run time and
  reported — [ADR 0007](0007-runtime-capability-detection.md) — never a build
  feature.
- **No bus is not an error.** A bare Sway session with no systemd and no D-Bus is
  the case [06-portability.md](../06-portability.md)'s bar names, and every
  feature here degrades to what the filesystem can answer: the sidebar's volumes
  come from `/proc/self/mountinfo`, their labels from `/dev/disk/by-label` and
  their free space from `statvfs`, none of which needs a bus.

## Consequences

**We own a wire format.** A marshalling bug is a message another program
misreads, and the mitigations are that the surface is small, that every type is
tested against bytes written by hand from the specification, and that a parse
which does not understand something answers nothing rather than guessing.

**A call costs a connection.** Connect, `EXTERNAL`, `Hello`, the call: four round
trips on a Unix socket to a daemon on the same machine. Measured against this
machine's own session bus at **0.45 ms to connect and 0.09 ms for a call** —
`ListNames`, whose reply is 112 names — so the whole sequence is half a
millisecond, which is a mount button's cost and not a frame's, and it is on a job
either way. If something
ever needs a call *per frame*, this decision is wrong and the revisit trigger
below says so.

**Two connections can disagree about who we are.** Each has its own unique name,
so a signal subscribed on one is not delivered on the other. Nothing here
subscribes yet; the first thing that does gets a long-lived connection of its own
and this record gets a successor.

**The service thread outlives the window by design.** It holds the name until the
process exits, which is what makes single instance work — and it means a hung
service thread would keep the name after the window has gone. It answers on the
socket and never touches the window, so there is nothing in it that can hang that
is not the kernel.

## Revisit trigger

**A D-Bus call on the frame path**, or more than ten calls a second from the job
pool: at that rate the per-call connection is the cost rather than the
convenience, and the answer is one long-lived client connection with a serial
table — which is a different decision from this one.

**Or a feature that needs to receive signals** — udisks2's `InterfacesAdded` for
a disk plugged in, say. The mount table is polled for `POLLPRI` today and that is
what tells the sidebar; a feature that genuinely needs the signal needs a
subscribing connection, and that is where this shape stops fitting.
