# 0041 — Panic diagnostics use a preopened local log

Status: Accepted

## Context

The default panic hook writes to stderr, which is often invisible after a desktop
launch. Session checkpoints can lag the latest interaction by two seconds.

## Decision

Before the window opens, open a private append-only diagnostic file under the
state directory. A panic hook attempts a nonblocking mutex acquisition and writes
the build identity, timestamp and panic information there, then calls the original
stderr hook. It does not serialize application state, recover the UI, allocate a
backtrace or acquire application locks. Opening the log is best effort and a
failure is reported on stderr. The existing checkpoint interval remains explicit.

## Consequences

A local report survives a desktop launch without a terminal. Panic text may name
private paths, so diagnostics are not uploaded automatically. The last checkpoint
is the recovery boundary; a panic does not make unsaved state trustworthy.
