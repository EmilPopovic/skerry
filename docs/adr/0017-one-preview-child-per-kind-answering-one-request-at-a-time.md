# 0017 — One preview child per kind, answering one request at a time

**Status:** Accepted. Refines [0006](0006-out-of-process-decoders.md), which decided
that decoding happens out of process and left the protocol, the pooling and the
cache open.

## Context

[ADR 0006](0006-out-of-process-decoders.md) settled the hard part: decoders are the
largest attack surface in a file manager and they run on whatever landed in a
download directory, so they run in helper executables under a sandbox, and
`sk-preview` "owns the wire protocol and the child lifecycle; it decodes nothing
itself".

What it did not settle is what that protocol *is*, and three of the questions
underneath it have answers that are expensive to change once a helper ships.

**Who reads the cache.** The XDG thumbnail specification puts a PNG at
`$XDG_CACHE_HOME/thumbnails/normal/<md5 of the file's URI>.png`, shared with every
other file manager on the machine — which is the interoperability
[08-roadmap.md](../08-roadmap.md) asks for and also the problem: a cached thumbnail
is a **PNG written by somebody else**. Reading it in the host would put a decoder
back in the main process for exactly the files most likely to have been planted,
which is ADR 0006 undone by the back door.

**What a reply carries.** A thumbnail is at most 128×128 — the specification's
`normal` size — which is 64 KiB of RGBA. A preview is a screenful. Both are small
enough to hand back whole and large enough that a second round trip to fetch them
would be the dominant cost.

**How many children.** A spawn is about a millisecond and a grid of a hundred
images would pay it a hundred times. ADR 0006 already says helpers are "pooled and
handle batches rather than being spawned per file", which is a requirement on the
protocol: it has to be a *stream* of requests rather than one invocation per file.

## Decision

**A child is a long-lived filter: length-prefixed requests in on `stdin`,
length-prefixed replies out on `stdout`, in order, one in flight at a time.**

- **One child per kind, spawned on the first request that needs it** and kept for
  the life of the process. `skerry-thumbnailer` decodes images and SVG in pure
  Rust; `skerry-preview-doc` renders the first page of a document by driving a
  renderer it finds on `PATH`. A window that opens no image spawns nothing.
- **The child reads and writes the cache, not the host.** A request names a path
  and a wanted size; the reply is raw `RGBA8` and its dimensions. Whether that came
  from a cached PNG or from decoding the original is the child's business, and the
  host never opens either.
- **One request in flight, with a deadline.** In-order replies mean no request ids
  and no correlation table; a child that misses its deadline is killed and
  respawned on the next request, which costs one preview. Concurrency across
  *kinds* comes from there being a child per kind, and concurrency within a kind is
  not worth a protocol that can interleave.
- **Errors are a status byte, not a dead child.** A file that is not an image, a
  renderer that is not installed, a decode that failed: each is a reply the host
  can turn into a message. Only a crash, a hang or a protocol violation costs the
  child.
- **The sandbox is applied by the child to itself, immediately after start-up and
  before it reads a single byte of a request.** `PR_SET_NO_NEW_PRIVS`, an address
  space cap, a CPU cap, and Landlock restricting the filesystem to read-only on the
  roots it is told about plus write on the cache directory. Where the kernel has no
  Landlock the child says so on `stderr` and carries on, which is ADR 0006's
  "reduced isolation is logged — never silently accepted".

## Consequences

- The host's dependency graph gains nothing. `tools/dep-budget.py` measures the
  `skerry` binary's tree, and a helper is a different binary — which is ADR 0006's
  dependency argument made mechanical rather than aspirational. The helpers'
  direct dependencies still need their rows in
  [07-dependencies.md](../07-dependencies.md), because `declared()` walks every
  manifest.
- A cache hit costs a round trip rather than a file read. Measured before it is
  defended: the number belongs in
  [09-performance-budgets.md](../09-performance-budgets.md) beside the 200 ms
  *thumbnail appears* row.
- **Ordering is the protocol's only correlation mechanism**, so a child that
  answers out of order is a child that has violated it, and the host treats
  desynchronisation the way it treats a crash. This is the cost of having no
  request ids and it is the thing to revisit first if a child ever needs to
  parallelise internally.
- A second display server, a second architecture and a machine with no `pdftoppm`
  all degrade the same way: one kind of preview is absent and says so.

## Revisit trigger

A preview that is not a single image — a scrollable document, a video with a
scrubber, anything where the child owns state across requests. In-order replies
with no ids cannot express a subscription, and the honest move at that point is a
new protocol rather than a flag on this one.
