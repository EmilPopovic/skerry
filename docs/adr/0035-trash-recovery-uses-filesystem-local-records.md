# 0035 — Trash recovery uses filesystem-local records

Status: Accepted

## Context

Home-only trash fails across mount boundaries, while session-only undo leaves no
recovery path after restarting. Restore introduces persistent untrusted metadata
and another reversible operation.

## Decision

`sk-fs::trash` owns filesystem-aware XDG storage and validated recovery records.
It uses home trash on the same mount, otherwise checked `.Trash/$uid` or
`.Trash-$uid` storage. Moves refuse replacement. Metadata is reserved exclusively
and synced before moving; failures retain the source. Private directories are
opened without following symlinks and checked for ownership and access rights.

Trash locations are ordinary, clearly labelled sidebar listings, including mounted
volumes. Restore reads bounded metadata on a job, resolves the original path,
refuses unsafe relative paths and existing destinations, and records an inverse
that returns the restored item to its reserved trash location. The UI prevents
ordinary renames, moves and trashing of trash entries; restore remains a command
available by keyboard and pointer. Empty Trash is outside this release.

## Consequences

Recovery survives restart and interoperates with other XDG trash clients. An
unavailable volume cannot be restored until mounted. No cross-device copy/delete
fallback or permanent deletion is inferred from a failed trash operation.

Storage follows the [XDG Trash specification](https://specifications.freedesktop.org/trash/latest/).
