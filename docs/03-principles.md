# Principles

Ten rules. Each is written so that a violation is *detectable* — in CI, or in a
five-minute manual pass — rather than debatable in a review. Rules 1–8 are product design;
rules 9 and 10 are engineering constraints that exist to keep 1–8 achievable for years
rather than for one release.

When two rules conflict, the lower number wins.

---

### 1. Never block the frame

No syscall that can touch a disk, a network mount or a decoder ever runs on the UI thread.
Directory contents stream in progressively; selection, typing and scrolling all work while
enumeration is still running.

**Start-up is the exception, and it is one place with a list.** Before the first frame the
main thread reads font directories, the timezone database, and — where the compositor
answers `zwp_linux_dmabuf_v1` — three files under `/sys/dev/char` naming the GPU it renders
on. Each is bounded, each is on a local filesystem by construction, and each is something
the first frame would otherwise read while a person is looking at it. Nothing may be added
to that list without a measurement of what it costs, because the list is exactly how a
rule with no exceptions becomes a rule with several.

> **Fails if:** any frame exceeds 8 ms while an sshfs mount is unplugged mid-listing — or a
> syscall that can touch a disk appears on the UI thread anywhere but the start-up list.

### 2. Everything is a command

Every action, toggle and setting is one entry in one registry. The palette, the menus, the
keymap and the config file are four views of that single list, so they cannot drift apart.

> **Fails if:** a menu item or a setting exists that the palette cannot find.

### 3. Everything is reversible

Rename, move, trash, create and batch-rename all push onto one undo stack with a visible
history. Destructive paths route through the XDG trash by default; permanent deletion is an
explicit, differently-shaped action.

> **Fails if:** any mutation exists that `Ctrl+Z` cannot walk back.

### 4. Discoverable, not just configurable

Shortcuts appear next to the thing they act on. Rebinding happens where you discovered the
command, not in a settings tree. A new user should find split panes without documentation.

> **Fails if:** a feature is reachable only by reading the config file or the manual.

### 5. Pointer and keyboard are peers

Rubber-band selection, drag-to-split, drag-select, and full drag-and-drop interop with GTK
and Qt applications — alongside a complete keymap with sequences and chords. Neither input
mode is a compatibility shim for the other.

> **Fails if:** a task is doable one way but not the other.

### 6. State survives everything

Tabs, split layouts, scroll offsets, selections, per-folder view settings, sort orders,
bookmarks and recents are restored on relaunch, and written durably enough to survive a
hard power cut.

> **Fails if:** `kill -9` loses a bookmark.

### 7. Honest progress

Copies show real throughput, real ETA, and per-file conflict resolution that pauses one
item without freezing the queue. Long operations stay visible, cancellable and resumable.
No indeterminate spinners.

> **Fails if:** a progress bar reaches 100% and then keeps working.

### 8. One selection, one path, one filter

The same selection semantics, path model and filter grammar in every view, every pane and
the inspector. Consistency is what makes muscle memory possible.

> **Fails if:** two views exist where `Shift`+click means different things.

---

### 9. Distro-agnostic by construction

Skerry targets Linux, not a desktop environment and not a distribution. Interoperability
comes from freedesktop specifications, which every desktop implements; never from a
desktop's API. Every environmental integration — systemd, udisks2, portals, a compositor
protocol, a kernel feature — is probed at runtime and has a documented fallback. Wayland
and X11 are both first-class; x86-64 and aarch64 are both first-class; glibc and musl are
both supported.

> **Fails if:** GLib, GTK, Qt, KIO or GVfs appears anywhere in the dependency graph; if a
> feature is enabled by a build-time flag that should have been a runtime probe; or if the
> binary refuses to start on a systemd-less musl aarch64 system.

Full policy: [06-portability.md](06-portability.md).

### 10. Every dependency earns its place

A crate enters the graph only when the alternative — writing that code ourselves — is
demonstrably worse, and the reasoning is written down. Optional capabilities live in
separate helper executables so their dependency trees never enter the main binary. The
total dependency count is a CI gate, not an aspiration.

> **Fails if:** a dependency lands without an entry in
> [07-dependencies.md](07-dependencies.md); if the transitive count exceeds the budget in
> [09-performance-budgets.md](09-performance-budgets.md); or if a second async runtime,
> thread pool or logging framework enters the workspace.

This is not asceticism. A file manager that adopts a desktop's object system inherits that
desktop's blocking-I/O conventions and its main loop, which is precisely what rule 1
forbids. Small dependency graphs are downstream of rule 1, not a separate hobby.
