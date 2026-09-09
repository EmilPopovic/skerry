# 0040 — Bounded parser mutation campaigns

Status: Accepted

## Context

Tar, ZIP and the D-Bus wire process untrusted length fields and nested structures.
Example-based adversarial tests alone do not exercise combinations of malformed
fields. The architecture document claimed fuzzing without a runnable campaign.

## Decision

Start with deterministic mutation targets compiled as tests beside each parser.
Seed corpora contain valid archive and wire structures; mutations include bit
changes, truncation, insertion, deletion and length-boundary values. Every target
also consumes its unmodified seeds. Panics retain the input and iteration for
reproduction. Input, output, process memory and campaign duration are bounded.

The ordinary test suite runs short campaigns. A scheduled job runs larger
campaigns with a recorded seed on both supported architectures. This is mutation
fuzzing, not coverage-guided fuzzing; no coverage or exhaustiveness claim follows.
Coverage-guided instrumentation and broader parser targets remain follow-up work.

## Consequences

The first runnable campaigns require no production dependency. A failure becomes
a regression corpus entry and a focused test before a release can proceed.
