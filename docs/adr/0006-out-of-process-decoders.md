# 0006 — Decoders in sandboxed helper processes

**Status:** Accepted

## Context

Three separate pressures point the same way.

**Security:** image, video and document decoders are the largest attack surface in a file
manager, and a file manager runs them on files chosen by whatever landed in a download
directory. In-process decoding means a malformed JPEG is a path to the user's whole home
directory.

**Dependencies:** the codec crates are by far the largest tree in the workspace. In-process,
they blow rule 10's budget on their own.

**Portability:** a document renderer may need a C toolchain, which
[06-portability.md](../06-portability.md) forbids for the main binary.

**Latency:** a decoder that hangs or allocates without bound is a rule 1 violation that no
amount of care inside the decoder prevents.

## Decision

Decoding happens in helper executables: `skerry-thumbnailer` (images) and
`skerry-preview-doc` (documents, built only where its toolchain exists), plus external tools
such as `ffmpeg` discovered on `PATH` at first use. Each runs under `seccomp` and `Landlock`
with no network and write access only to the thumbnail cache, spawned with a timeout and a
memory cap. `sk-preview` owns the wire protocol and the child lifecycle; it decodes nothing
itself.

Where the kernel cannot sandbox (Landlock absent), helpers still run out of process and the
reduced isolation is logged — never silently accepted.

## Consequences

- A crash or hang in a decoder loses one preview, not the window.
- The main binary's dependency graph stays inside the budget.
- Process spawn cost per thumbnail is real, so helpers are pooled and handle batches rather
  than being spawned per file.
- A missing helper degrades exactly one preview kind, which is also the portability story
  for optional features.
