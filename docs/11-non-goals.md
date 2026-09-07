# Non-goals

For 1.0. Each of these has sunk a file-manager project before, and each is declined in
review by pointing at this file rather than by re-arguing it.

- **Desktop icons and wallpaper management.** A separate product wearing a file manager's
  clothes.
- **Cloud storage integration.** Every provider is a different API with a different auth
  story and a different failure mode. A mounted cloud filesystem already works.
- **MTP and phone browsing.** A protocol tar pit. Post-1.0 at the earliest, and only
  through an existing mount.
- **A plugin API with a stable ABI.** See below.
- **Virtual filesystems beyond archive listing.** No FUSE layer, no `kio`-style
  slaves — and the listing is a *panel*, not a path: no pane navigates into an
  archive, no breadcrumb names anything inside one, and nothing in one can be
  renamed, dragged or watched.
- **An embedded terminal.** See below.
- **A bundled image editor, media player or archive manager.** Launch the user's.
  Reading one is not managing one: `Space` lists what is inside an archive and
  `Extract` unpacks it, which is what a file manager does with a download — while
  *creating* one, adding to one, encrypting one or repairing one is the program
  this declines to be
  ([ADR 0032](adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md)).
- **A document viewer.** The preview shows a page, selects and copies its words and
  finds text on the one in front of you — [ADR
  0024](adr/0024-a-page-arrives-with-its-words.md) — because the panel already
  makes that promise about a text file and declining it for a PDF was an
  inconsistency. An outline, links, forms, annotations and a document-wide search
  are a PDF reader, which is the thing to launch.
- **Windows and macOS ports.** The platform layer, the integration surface and the
  portability policy are all Linux-shaped on purpose.
- **A file manager that also does system tasks** — disk partitioning, permissions editors
  beyond the basics, service management.

## Two that deserve an argument

**The plugin ABI.** A stable ABI is a permanent compatibility obligation taken on before
the internal interfaces have settled, and it fossilizes exactly the layers that most need to
move — the registries in [04-architecture.md](04-architecture.md). The cheap substitute
delivers most of the value with none of the debt: user-defined actions in the config file,
plus reading the desktop-entry actions and KDE service menus that third parties already
ship. Users get "my tool appears in the context menu" without us owning an ABI forever.

**The embedded terminal.** The single most-requested Dolphin feature, and still the wrong
call. "Open a terminal in this directory", bound to a key and honouring the user's chosen
terminal, covers the actual need. Embedding one means absorbing a terminal emulator's
maintenance burden — escape sequences, fonts, scrollback, shell integration — into a
project whose differentiator is a frame budget.

## The rule behind the list

A feature belongs in 1.0 if it serves one of the ten rules in
[03-principles.md](03-principles.md). Everything above serves a checklist instead.
