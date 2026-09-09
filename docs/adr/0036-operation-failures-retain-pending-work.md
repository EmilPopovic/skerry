# 0036 — Operation failures retain pending work

Status: Accepted

## Context

The operation queue discarded later items on an error or metadata warning. The
copy engine correctly reported partial work, but the UI lost what remained.

## Decision

Completed metadata-only warnings remain visible and do not stop later files.
Partial copy results carry a typed retry flag. Hard failures and retryable partial
copies retain the current item and pending order in the existing stopped queue.
Retry, skip the failed item, and explicitly discard pending work are commands.
A new operation cannot silently replace a stopped queue. Retry resumes only the
interrupted item; untouched items use normal conflict resolution.

Warnings/errors remain visible until acknowledged or a new accepted operation
starts. Completed work remains in undo history. The retained queue is in memory,
consistent with ADR 0011; it is not automatically replayed after restarting.

## Consequences

A warning does not claim that an entire batch failed. Skipping an item does not
undo its partial output. Restoring, trashing and copying share recovery controls;
archive partial output still requires undo and a fresh extraction rather than
merging a new decoder into unverified old output.
