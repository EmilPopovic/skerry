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
cached into a GPU atlas keyed by (name, size, scale, theme generation). Users notice a wrong
icon long before they notice a frame time.

## Being a first-class citizen

- Implement `org.freedesktop.FileManager1` (`ShowFolders`, `ShowItems`, `ShowItemProperties`)
  so "show in file manager" from Firefox, Chrome, Steam and every GTK application opens
  Skerry.
- Register as the handler for `inode/directory` and `x-scheme-handler/file`, with a
  reversible `--set-default` command rather than a silent grab at install time.
- Single-instance behaviour over the same bus name: opening a folder reuses the running
  window as a new tab, with `--new-window` to opt out.

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
- Drop targets that make sense: onto a folder, onto a pane, onto a tab, onto an archive,
  onto an executable as arguments.
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

- **AT-SPI via `accesskit`**, wired in at M1 rather than M7. The accessibility tree is
  produced by the same immediate-mode pass that produces draw commands, so it cannot fall
  out of sync — but only if it is there from the start.
- **`text-input-v3`** on Wayland and XIM/ibus on X11, so CJK input works in the rename
  field and the search box. Retrofitting IME into a text widget is a rewrite.
- Keyboard navigation for every action follows from rule 5; a visible focus ring is a
  golden-frame test.
