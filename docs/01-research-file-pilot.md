# The prior art: File Pilot

Researched 25 August 2026, against beta v0.8.3.
Sources: [filepilot.tech](https://filepilot.tech/),
[Starlog changelog](https://filepilot.tech/starlog),
[roadmap](https://filepilot.tech/roadmap),
[Handmade Network project page](https://filepilot.handmade.network/), plus hands-on
write-ups from [Andrew Lock](https://andrewlock.net/windows-explorer-replacement-filepilot-is-awesome/),
[Windows Latest](https://www.windowslatest.com/2025/02/24/hands-on-with-file-pilot-a-new-alternative-to-windows-11s-file-explorer/)
and [MakeUseOf](https://www.makeuseof.com/file-pilot-fast-file-manager-alternative/).

## What it is

A Windows file manager written by one person, Vjekoslav Krajačić, in C, with essentially
no dependencies — one vendored header, `stb_sprintf.h`. The platform layer, the GPU
renderer, the immediate-mode UI framework, the containers, the UTF-8 string handling and
the math library are all in-house. It ships as a single ~2.5 MB executable requiring no
installer, and has been in free public beta since February 2025.

| | |
| --- | --- |
| Language | C, no external libraries |
| Rendering | Custom OpenGL renderer; Direct3D became the default in v0.7.1 |
| UI | Custom immediate-mode framework on top of that renderer |
| Binary | ~2.5 MB, Windows 7+ , x86-64 only |
| Licence | Free during beta; paid at release |
| Open roadmap items | Miller columns, undo/redo, Unicode support, localization, CLI, extension API, virtual folders, MTP, cloud, ARM |

## Why anyone cares

Not the feature list. Every review leads with latency:

- Opening a tab is instantaneous; cold start is effectively immediate.
- Scrolling a directory of roughly a million entries stays smooth.
- Expanding 200 GB of nested folders to compute sizes completes in under two seconds.
- Thumbnail scaling and view-mode switching never hitch.
- Sorting is asynchronous; the UI stays live while it happens.

The conclusion to carry forward: **the latency is the product.** Every feature below only
reads as delightful because that holds. Ship the same features on a stack that drops
frames and you have built another mid-tier file manager.

## Feature inventory, ranked by how much reputation each carries

`core` = Skerry is pointless without it. `v1.1` = can arrive after 1.0 without anyone
feeling cheated.

### 1. Sub-frame responsiveness — `core`
Instant start, instant tab open, smooth scroll at a million entries, asynchronous
sorting, live recursive folder-size computation, thumbnail zoom without stutter.

### 2. Nested split panes inside tabs — `core`
Arbitrary horizontal and vertical splits within one tab, panes added freely, tabs
rearranged by drag with snap-assist overlays, middle-click a folder to open it in a split,
full session and layout persistence, a searchable tab popup, tab history with
reopen-closed.

### 3. Flattened search and fuzzy filter — `core`
Collapse a subtree — or an entire drive — into one flat list in milliseconds, then
fuzzy-filter it fzf-style and narrow by extension. Two scopes coexist: a global search box
and a current-folder box with a subfolder toggle. Ignore rules for `.git` and `.svn`,
asynchronous sort, sortable by recursive directory size.

### 4. Inspector, including folder previews — `core`
`Space` for macOS-style quick look; `Ctrl+I` for a docked inspector that previews text,
images, EXIF-oriented photos, and **other folders as clickable, navigable listings**. That
last case is the sleeper feature: it turns the inspector into a second navigation surface
rather than a static preview pane.

### 5. Command palette over every action — `core`
`Ctrl+Shift+P` searches every command, toggle and setting, grouped and context-aware, and
binds a hotkey inline from the result row. Bindings support key sequences, the numpad and
aliases. The UI surfaces the relevant shortcut next to whatever the pointer is touching.

### 6. Five view modes with per-folder memory — `core`
Details with reorderable columns and disk-usage bars, Grid with a zoom slider and
`Ctrl`+scroll, inline Tree expansion, Explorer-style List columns, and Group By into
collapsible sections — plus hierarchical view settings, so a choice applies to one folder,
a subtree, or globally.

### 7. Mouse craft equal to keyboard craft — `core`
Rubber-band selection, drag-select across checkboxes, right-drag context menu, and
drag-and-drop that works system-wide: into browsers and mail clients, into ZIP archives,
onto executables as arguments.

### 8. Interactive batch rename — `core`
Multi-file rename with live preview, incremental IDs, file dates, case conversion.

### 9. GoTo — `core`
One popup over system paths, bookmarks and recents with autocompletion; the keyboard
equivalent of the sidebar.

### 10. Searchable, pinnable context menu — `v1.1`
The full menu including third-party entries, with a filter box and pinning for frequent
actions. No second-level "show more options".

### 11. Deep visual customization — `v1.1`
Font size, item spacing, corner radius, alternating row colours, inactive-pane dimming,
animation toggles, colour schemes, content zoom, per-monitor DPI scaling, optional
vim-style movement.

### 12. Boring reliability plumbing — `core`
Correct change tracking as the disk mutates underneath you, sane removable-drive handling
across eject and reconnect, durable bookmarks and recents, registering as the system
default file manager. Unglamorous, and the whole difference between a demo and a daily
driver.

## Where a clone starts ahead

File Pilot's own open roadmap marks the places where matching it is not the ambition:

- **No undo/redo.** On Linux, with the freedesktop trash spec available, total
  reversibility is achievable from the first release. It becomes rule 3 in
  [03-principles.md](03-principles.md).
- **No full Unicode support.** A non-negotiable defect to avoid: Skerry does real shaping,
  bidi and fallback from day one, and carries adversarial filenames as CI fixtures.
- **No PDF preview.** Solvable with an out-of-process renderer, which we want anyway for
  sandboxing and dependency isolation.
- **No extension API.** Deliberately also a non-goal here, for different reasons — see
  [11-non-goals.md](11-non-goals.md).
- **Windows-only, x86-64 only.** Ours is Linux-only, and multi-architecture from the
  first CI run.
