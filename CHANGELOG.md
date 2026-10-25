<!-- markdownlint-disable MD024 -->

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- A secondary click on a quick look opens the menu of the file being previewed.
  Before, it opened the menu of whichever row lay under the panel, so Move to
  Trash named a file nobody could see. Inside a text, animation or PDF preview
  the panel's own menu still opens; the file's menu is on the panel's title
  strip.

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
