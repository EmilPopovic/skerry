# 0033 — Operation failures retain the work that already happened

**Status:** Accepted

## Context

The September implementation review reproduced data loss in temporary-file
ownership, replacement rollback, and undo. A failure is not evidence that an
operation changed nothing. Concurrent writers and helper processes have the same
problem: a requested effect and an acknowledged effect are different states.

## Decision

Temporary files are exclusively created and owned by one invocation. User-file
edits preserve metadata and refuse link semantics they cannot preserve safely.
Recursive removal uses directory descriptors rather than reopening joined paths.
Undo of copied or extracted output preserves the current contents in trash.

Batch replay reports completed and remaining changes on failure. History records
both parts, and every queue exit records the successful work. Extraction redo
claims a fresh destination and never cleans up a preexisting directory.

Session writes are ordered and acknowledged. Helper spawning and interruptible
pipe I/O belong to workers; a deadline covers the entire exchange. Sandbox rights
are negotiated individually, and output limits are enforced while writing.
Mount identity is read before a target probe; device queues serialize initial
reads and a reserved worker handles control jobs. Operational filename bytes are separate from the text displayed to a person.

## Consequences

Safety sometimes requires refusing an operation, rather than guessing whether a
path still belongs to it. Undo can consume trash space. Partial replay remains
visible in history instead of pretending that a failed gesture did nothing.
No additional runtime or dependency is introduced.

## Revisit trigger

A supported filesystem cannot implement these guarantees, or a user needs link
editing semantics beyond the conservative refusal. Record that policy separately
instead of silently weakening publication or ownership checks.
