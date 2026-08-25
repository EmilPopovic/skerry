# 0005 — One owned job pool; no async runtime in the core

**Status:** Accepted

## Context

Everything expensive in a file manager is a blocking syscall on a thread: `getdents64`,
`statx`, `copy_file_range`, decode. None of it is the many-idle-connections workload that
async runtimes exist for. Meanwhile rule 1 requires precise control over *which* thread
runs what and *when* work is cancelled, and the per-device queueing in
[05-platform-integration.md](../05-platform-integration.md) requires scheduling policy that
a general-purpose executor does not express.

The specific hazard is plurality. A `rayon` global pool, plus a runtime's blocking pool,
plus our own threads means uncoordinated oversubscription — which shows up as a latency bug
only under load, exactly when the budget matters most.

## Decision

One job pool for the process, in `sk-util`: `ncpu - 2` workers, priority lanes, cancellation
tokens on every job, and per-device queues (parallel on NVMe, serialized on rotational and
network mounts). No `tokio`, no `async-std`, no `rayon`. Results reach the UI thread as
versioned snapshots over a channel.

`io_uring` is evaluated as an optimization inside `sk-fs` only if it measurably wins; it is
not an architecture.

## Consequences

- Every dependency that requires an executor is a problem to be contained. `zbus` is the
  known case: it is confined to `sk-desktop` on its own thread, and
  [07-dependencies.md](../07-dependencies.md) records the exit.
- We write the pool, the cancellation semantics and the priority lanes. All three are things
  we would have had to tune anyway.
- Cancellation is a first-class concept rather than a bolted-on `AbortHandle`: navigating
  away from a directory cancels its enumeration, its size walk and its thumbnail batch in
  one call.

## Revisit trigger

If a future requirement is genuinely I/O-concurrency-shaped — many simultaneous network
mounts, say — reconsider for that subsystem alone, behind its own thread. Never for the
core.
