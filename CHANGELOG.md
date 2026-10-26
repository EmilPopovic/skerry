<!-- markdownlint-disable MD024 -->

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A "Decode files without Landlock" setting. `F2` now shows whether the helpers
  are sandboxed.

### Changed

- A unified custom icon set now covers files, menus and controls. New
  configurations use Skerry icons by default.
- Without Landlock, previews, thumbnails and archive unpacking are refused
  unless that setting is on (ADR 0046).
- Invisible characters in file names (controls, bidi and zero-width marks) show
  as a dimmed `<U+XXXX>`. Emoji presentation selectors are honoured, so symbols
  such as ☕ and ⭐ draw as colour emoji.
- `make install` no longer builds. Run `make` first.

### Security

- Helpers are confined further: no changing other files' metadata, signalling
  other processes, using the keyring or leaving their process group. Each runs
  in its own session with a minimal environment.
- Copies and moves walk folders through descriptors, so a symlink swapped in
  mid-copy is refused. Copies drop setuid and setgid bits and other users'
  ACLs.
- A move between filesystems removes only what it copied. Delete for good stops
  at mount points.
- Malformed or oversized D-Bus messages are refused without stalling the
  connection. Paste and drop data is limited to 16 MiB and 5 seconds.
- Index files and thumbnails are written 0600 in 0700 folders and never through
  a symlink. An SVG can no longer draw a local file into its thumbnail.
- Release builds no longer hold a write token, and published archives carry
  build provenance.

### Fixed

- Markdown preview no longer holds the window on long or deeply nested
  documents.
- A session file with a non-ASCII sort token no longer crashes startup.
- Large tar archives unpack fully. Unpacking stops before the disk runs out of
  inodes.
- PDF pages with a crop box are shown whole.
- An interlaced GIF frame with zero width no longer stops a thumbnail batch.

### Fixed

- A secondary click on a quick look opens the menu of the file being previewed.
  Before, it opened the menu of whichever row lay under the panel, so Move to
  Trash named a file nobody could see. Inside a text, animation or PDF preview
  the panel's own menu still opens; the file's menu is on the panel's title
  strip.
- Empty the Trash empties a trash that holds a read-only folder. Before, it
  stopped at the first file inside one, deleting only some of the trash or
  none of it. An item that still cannot be deleted no longer keeps the rest.

## [1.0.0-alpha.2] - 2026-10-25

### Added

- Empty the Trash is on the Trash's own context menu and on the sidebar Trash
  row's menu. Before, the only way to it with a pointer was the menu over a
  trashed file.
- A New Window action in the `.desktop` file, which a dock offers on a
  secondary click.

### Changed

- Launching `skerry` again with no folder named opens a second window, rather
  than handing over to the window already open. A launch that names a folder
  still opens it in the running window. See
  [ADR 0044](docs/adr/0044-a-launch-that-names-nothing-is-a-new-window.md).

### Fixed

- Launching Skerry a second time from a launcher appeared to do nothing on
  Wayland.
- A tab dragged over the middle of another pane now shades that whole pane.
  Before, the only highlight was a thin bar at the end of its tab strip, so the
  drop looked as though it would do nothing.

## [1.0.0-alpha.1] - 2026-10-25

### Added

- Initial release.
