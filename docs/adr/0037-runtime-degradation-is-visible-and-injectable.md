# 0037 — Runtime degradation is visible and injectable

Status: Accepted

## Context

Missing decoder helpers and refused watches could leave apparently unfinished
previews or a silently stale listing. The portability document promised an
environment override that the program did not implement.

## Decision

`SKERRY_DISABLE` is a comma-separated runtime probe override. It may simulate
unavailable optional facilities; it cannot bypass mandatory helper confinement.
Supported names are documented beside the runnable degradation checks.

The watcher publishes a reason when registration is refused or unsupported.
Known failures are retried on explicit refresh or navigation, not a short endless
timer. Lost watches still get an automatic registration attempt. The focused
listing keeps a visible stale-listing explanation and a refresh instruction.

Every failed decoder request receives a terminal answer. Missing or crashing
helpers produce a visible fallback instead of leaving requests pending forever.
Failure injection follows the same request and presentation paths as real failure.

## Consequences

The alpha can demonstrate fallbacks on machines where optional components are
installed. Failure to preview one file does not turn the rest of the file manager
into an error screen. Refresh is explicit on filesystems without change events.
