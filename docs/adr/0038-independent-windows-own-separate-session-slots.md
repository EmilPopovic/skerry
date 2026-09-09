# 0038 — Independent windows own separate session slots

Status: Accepted

## Context

Default D-Bus handover prevents opening a second window. A second process writing
the original session file would replace the first window's saved layout.

## Decision

`--new-window` bypasses handover and global name ownership while retaining a
session-bus connection for desktop integration. New Window is also a registered
command. Ordinary invocations retain their existing handover behavior.

Before opening the display, each process claims an exclusive advisory lock beside
its session file. The ordinary slot remains `session`; independent windows claim
the first free `session-window-N` slot. Locks remain held for the process lifetime.
At most 64 slots are searched; failure is reported instead of sharing a writer.
Each slot retains its own checkpoint and is restored when reused without an
explicit path. Slot files are not removed at exit, so crash recovery remains
possible. No lock or filesystem discovery occurs in the frame loop.

## Consequences

Two windows cannot overwrite each other's checkpoints, including when no session
bus exists. Configuration and folder-view preferences remain shared. A window's
undo and pending-operation queues remain local to that process.
