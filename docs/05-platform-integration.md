# Platform integration

Roughly a third of the effort has no File Pilot counterpart: it is the work of being a
Linux desktop citizen. Skipping any of it produces an app that feels foreign, which defeats
a usability-led project. All of it goes through freedesktop specifications rather than any
desktop's API — see [06-portability.md](06-portability.md) for why that is a hard rule.

## Trash, done to spec

The freedesktop trash specification, completely: `$XDG_DATA_HOME/Trash` with `files/` and
`info/`, per-volume `.Trash-$uid` directories for other mounts, `.trashinfo` files carrying
the original path and deletion time, correct handling of the top-level `.Trash` directory
with the sticky bit, and restore-to-original that actually works. Half-implementing this
loses user data, and rule 3 depends on it entirely. Owner: `sk-fs`.

## The operation engine

- `FICLONE` for instant reflinks on btrfs and xfs, `copy_file_range` otherwise, plain
  read/write as the floor — each probed at runtime, per filesystem.
- Sparse-file awareness via `SEEK_HOLE`/`SEEK_DATA`; `fallocate` to preallocate and avoid
  fragmentation.
- Preserve xattrs, POSIX ACLs, ownership where permitted, and timestamps. Report what could
  not be preserved rather than silently dropping it.
- Queue per device: parallel on NVMe, serialized on rotational disks and network mounts.
  Device class comes from `/sys/block/*/queue/rotational` and the mount's filesystem type.
- Conflict resolution pauses one item without freezing the queue (rule 7). Resume after a
  crash from the operation journal.
- `ENOSPC` mid-copy leaves no partial file in place and reports the shortfall in bytes.

## MIME and launching

Parse `shared-mime-info` ourselves — glob patterns first, then magic, then the
`inode/directory` and `inode/symlink` special cases — because the alternative is linking
GIO. Resolve handlers through `mimeapps.list` in XDG precedence order. Parse `.desktop`
`Exec` fields properly, including field codes (`%f %F %u %U`) and quoting rules.

Launch children detached from the file manager's lifetime: in a transient systemd scope via
`systemd-run --user --scope` when systemd is present, and a double-fork with a new session
when it is not. Both paths are exercised in CI. An application must not die because the
window that started it closed.

## Icons

Full XDG icon theme resolution: `index.theme` inheritance, size and scale directories,
`hicolor` as the terminal fallback, SVG rendering via `resvg`, PNG otherwise, everything
in a GPU atlas keyed by (name, size, scale, theme generation). Users notice a wrong
icon long before they notice a frame time.

[ADR 0042](adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md)
makes that sketch precise and settles the three things it leaves open. **Where a
mark comes from is a setting** — `marks`, `desktop` or a named theme — because a
file manager that draws the desktop's icons everywhere looks like every other
window on that desktop, and the built-in marks are a value somebody chooses rather
than the branch that runs when nothing else worked. **The atlas is its own**, four
bands of equal slots in one 1024² texture rather than the thumbnails' single grid,
because an icon is drawn from 16 device pixels to 128 and a thumbnail is one size.
**And there is no disk cache**: the whole working set is 19.0 ms of another
process's time for Adwaita and 6.4 ms for breeze, which is not worth a second
writable path in a sandbox whose value is that the grant list is short enough to
read.

What is drawn from a theme today is a row's and a cell's mark. The sidebar's
places, volumes and trash, the column view, the breadcrumb menus and the card
under a drag still draw the built-in marks.

## Being a first-class citizen

- Implement `org.freedesktop.FileManager1` (`ShowFolders`, `ShowItems`, `ShowItemProperties`)
  so "show in file manager" from Firefox, Chrome, Steam and every GTK application opens
  Skerry.
- Register as the handler for `inode/directory` and `x-scheme-handler/file`, with a
  reversible `--set-default` command rather than a silent grab at install time.
- Single-instance behaviour over the same bus name: opening a folder reuses the running
  window as a new tab, with `--new-window` to opt out.

## Where the sidebar's contents come from

Three files, all plain text, all written by other programs, none of them behind
an API:

- **`user-dirs.dirs`**, from the XDG user-directories specification. It is the
  reason Downloads is called `Descargas` on a Spanish desktop, and the reason
  finding it by the English name is not our business. Only the directories that
  exist are shown: a fresh account has every key in the file and half the
  directories missing.
- **GTK's `bookmarks`** — `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`. Not a
  specification and not GTK the library: a list of URIs, one per line, which
  Nautilus, Thunar, Nemo, Caja and PCManFM all read and write. Interoperating
  through the file is exactly what rule 9 means — the format is the contract, and
  nothing links against anything of GTK's. Bookmarks that duplicate a user
  directory are dropped, which on this machine is all five of them.
- **`/proc/self/mountinfo`**, filtered by an **allow-list of filesystem types**.
  This machine has sixty-four mounts and one qualifies. A deny-list is a promise
  to have heard of every pseudo-filesystem the kernel will ever add, and its
  failure mode is `bpf` in the sidebar; the allow-list's failure mode is a real
  disk nobody sees, which is one line to fix and visible the moment somebody
  plugs one in. Mount *points* are then filtered by a deny-list of the FHS
  system directories, because `/boot` is `ext4` and `/boot/efi` is `vfat` and
  neither is a place — those two were two of the four rows the first version
  showed.

The watcher polls `/proc/self/mountinfo` for `POLLPRI` and wakes the frame loop
when mounts change. Device events also trigger volume refreshes; see
[ADR 0030](adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).
The sidebar has an expandable tree, pin reordering, a resizable divider, and
bookmark persistence in GTK's file; see
[ADR 0021](adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md).

## The window frame

`zxdg_decoration_manager_v1` decides who draws the title bar, and **GNOME does not
advertise it at all** — measured, not assumed, in
[06-portability.md](06-portability.md). A client that does not draw a frame has no
title bar, no close button and no resize edges on the most common Linux desktop.

So the window draws one, and **the path bar is it**. Not a strip added above:
a strip that says where you are is already the strip a person reaches for to drag
the window, and a second one costs 36 logical pixels of list for a duplicate of
the same information. Drag it to move, double-click to maximize, right-click for
the compositor's own window menu, and three buttons at its right end.

Everything in it is read rather than assumed, because every part of it differs
between compositors:

- **Which buttons exist** comes from `xdg_toplevel.wm_capabilities`. Hyprland
  offers no minimise, so there are two there and three elsewhere. The absent one
  gets no slot: a gap where a button would be reads as a bug, and a button that
  does nothing reads as a worse one.
- **Whether the edges can be pulled** comes from the configure states. A
  maximized, fullscreen or tiled window has none, and an invisible six-pixel band
  that swallows clicks at the top of a maximized list is worse than no resize.
- **Whether to draw a frame at all** comes from what the decoration manager
  answered, and `--own-frame` asks for one everywhere — which is two things,
  because Hyprland refuses the `client_side` request and a preference a compositor
  can silently veto is not a preference.

Owed: no drop shadow, and so no shadow margin. A shadow needs a surface larger
than the window with `xdg_surface.set_window_geometry` naming the part that is the
window, which is also where a resize target beyond the edge would live. Six
logical pixels inside the frame is what there is today.

## Clipboard and drag-and-drop interop

The one integration nobody can fake, because it is judged against other applications:

- `text/uri-list` everywhere, plus GNOME's `x-special/gnome-copied-files` and KDE's
  cut-selection convention, so cut/copy/paste actually works between Skerry, Nautilus and
  Dolphin. **Also the portal types**: Nautilus asks for `application/vnd.portal.filetransfer`
  and `application/vnd.portal.files` *before* it asks for `text/uri-list`, and a sandboxed
  target may not fall back.
- XDND on X11, `wl_data_device` on Wayland, as both **source and target** — the source half
  is why Skerry owns its platform layer ([ADR 0002](adr/0002-own-platform-layer.md)).
- Copy, move and link semantics with the modifier conventions of the desktop, plus the
  right-drag menu that makes them explicit. **The two protocols do not share an action
  vocabulary**: XDND has copy, move, link, ask and private; `wl_data_device_manager` has
  copy, move and ask and no link. `sk-plat` exposes the union and the policy above it greys
  out what the display server cannot express.
- Drop targets that make sense: onto a folder, onto a pane, onto a tab, onto an
  executable as arguments. **Not onto an archive**, which this list used to
  promise: adding a file to one is *writing* an archive, and
  [ADR 0032](adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md)
  reads them only — a child that could write into an archive would be a child
  that needs an encoder and somewhere else to write it.
- **The primary selection is best-effort and nothing depends on it.** At least one shipping
  compositor discards a primary-selection claim with no protocol error, so the failure
  cannot be detected and the feature cannot even be greyed out.

Spike D was the highest-uncertainty item in the whole plan and is now
[resolved](../spikes/d-platform-and-dnd/README.md): a drag we start lands files on disk in
GTK 4 and Qt 6 targets on both display servers. What it changed here is above; the four
ordering invariants it produced are M0's, not this document's.

## Thumbnails

Write into `$XDG_CACHE_HOME/thumbnails/{normal,large,x-large,xx-large}` per the XDG
thumbnail managing standard, with `Thumb::URI` and `Thumb::MTime` PNG metadata, and read
what other applications have already produced. The cache is shared with the desktop rather
than duplicated, and failures go to `fail/skerry/` so a broken file is not retried forever.

Decoding happens in `skerry-thumbnailer`, a separate executable under `seccomp` and
`Landlock` with no network and no write access outside the cache. Image decoders are the
largest attack surface in a file manager; they also carry the largest dependency tree, so
the process boundary buys security and dependency isolation at once
([ADR 0006](adr/0006-out-of-process-decoders.md)).

## Devices and mounts

`udisks2` over D-Bus for mount, unmount and eject when it is present; `/proc/self/mountinfo`
(watched, not polled) for mount state always. Without udisks2, removable devices are shown
and mounted through whatever the user has — the feature degrades to read-only device
listing rather than disappearing. Graceful behaviour when a device vanishes mid-listing is
a tested case, not an aspiration.

Network shares in 1.0 mean "a mount that already exists behaves correctly, including when
it hangs". Mounting SMB and SFTP ourselves is post-1.0; hanging on a dead mount is never
acceptable, which is rule 1's canonical test.

## Accessibility and input methods

A custom renderer bypasses every toolkit's accessibility layer and every toolkit's input
method plumbing, so both are built deliberately:

- **AT-SPI, written here rather than taken from `accesskit`** —
  [ADR 0026](adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md), because
  that crate's Unix adapter brings `zbus` and an executor with it. The tree is derived
  once a frame from what the frame recorded — the rectangles a press is tested against —
  so it cannot fall out of sync with what is on screen, and it is published over the
  D-Bus this program already speaks.
- **`text-input-v3`** on Wayland and XIM/ibus on X11, so CJK input works in the rename
  field and the search box. Retrofitting IME into a text widget is a rewrite.
- Keyboard navigation for every action follows from rule 5; a visible focus ring is a
  golden-frame test.
