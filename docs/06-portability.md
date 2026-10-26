# Distro-agnostic by construction

Rule 9. Skerry targets **Linux**, not a desktop environment and not a distribution. This
document is the policy that makes that concrete, because "works everywhere" decays into
"works on the maintainer's distro" within a year unless it is a constraint with tests
attached.

## What is assumed to exist

The complete list. Anything not here is probed.

| Assumption | Floor | Why this floor |
| --- | --- | --- |
| Linux kernel | **5.10** | The oldest LTS still in wide use (Debian 11). Gives `statx`, `copy_file_range`, `FICLONE`, `inotify`, `SEEK_HOLE`. |
| libc | **Source target: glibc 2.28, or musl 1.2.** The `0.1.0-alpha.1` binaries require **glibc 2.39** | Nothing in the source needs more than 2.28, but the published tarballs are built on their CI runner and inherit its glibc, which excludes Debian 12 and Ubuntu 22.04. `BUILD-INFO` records each artifact's measured symbol floor and `tools/package.py` refuses one above the declared baseline. Reaching 2.28 needs an older build baseline and runtime testing on it; neither has happened, so no 2.28 binary claim is made. See [RELEASE-NOTES.md](../RELEASE-NOTES.md). |
| Architecture | **x86-64 and aarch64** | Both built and tested in CI from the first commit. Adding a third is then a CI line, not a port. |
| Display server | **Wayland or X11** | Both first-class. Neither is a fallback for the other. |
| GPU | **anything, including none** | Vulkan where available, GL otherwise, llvmpipe as the floor — which is also what CI runs on. |
| Filesystem layout | **XDG Base Directory spec** | `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_DIRS` — never a hardcoded `/usr/share`. |

## What is probed at runtime, never assumed

Each row is a capability, a probe, and a documented degradation. This table is the actual
portability contract, and each row has a test that forces the fallback path.

| Capability | Probe | If absent |
| --- | --- | --- |
| systemd (user session) | `sd_booted` equivalent: `/run/systemd/system` exists | Launch children via double-fork + `setsid` instead of a transient scope |
| udisks2 | D-Bus name activation | Devices listed read-only from `/proc/self/mountinfo`; no mount/eject buttons |
| A session bus | `DBUS_SESSION_BUS_ADDRESS`, then a connect and `EXTERNAL` | No `FileManager1` and no single instance: a second `skerry` opens a second window instead of handing over. Everything else is unchanged, because nothing else needs one |
| `org.freedesktop.FileManager1` unowned | `RequestName` without `REPLACE_EXISTING` | Another file manager answers *show me this file* and we do not. Never taken from a running owner — [ADR 0023](adr/0023-d-bus-is-a-socket-we-speak-ourselves.md) |
| The desktop can *start* us for it | `$XDG_DATA_HOME/dbus-1/services/org.freedesktop.FileManager1.service` names this binary | *Show me this file* from a closed application starts whichever file manager's package owns that name under `/usr/share`, and Skerry answers only while a window is already open. The settings panel's *Open folders with Skerry* row writes the file; nothing installs one, because `/usr/share/dbus-1/services/` belongs to another package and `$XDG_DATA_HOME` wins over it by order |
| A font for an SVG's text | The four generic families resolved against the machine's own font database, once | An SVG's text is drawn in whatever family the machine *has*, Noto ahead of DejaVu — and on a machine with no fonts at all, not drawn. `fontdb`'s defaults are Microsoft's names, which is a machine-specific answer masquerading as a default |
| `pdftotext`, else `mutool` | On `PATH`, probed per request beside the renderer | A page renders and cannot be selected out of: no highlight, and `Ctrl+A`, `Ctrl+C`, `Ctrl+F` and a drag stay dimmed over it. Poppler answers with word boxes and mupdf with character quads, which this makes words of — [ADR 0024](adr/0024-a-page-arrives-with-its-words.md). With neither, the same answer a scanned document gets: no text layer to find |
| xdg-desktop-portal | D-Bus name | Direct filesystem paths; no portal-mediated open; the desktop's light-or-dark is unknown and the theme is what the flag, the session file or the default says |
| `wp_fractional_scale_v1` | Wayland registry | Integer scaling with supersampled glyph rasterization |
| `xdg_activation_v1`, `cursor_shape_v1`, `text_input_v3` | Wayland registry | Documented feature loss per protocol; never a crash |
| `zwp_pointer_gestures_v1` | Wayland registry; **X11 has none** — XInput 2.4's gestures need a server, a driver and a version this does not require | No pinch. Every zoom is still reachable: `Ctrl` and the wheel over the picture, `Ctrl +` / `Ctrl -` / `Ctrl 0` from the keyboard, and a drag to pan |
| Vulkan | Loader present and a device enumerates | GL backend; then llvmpipe |
| `FICLONE` | Attempt, `EOPNOTSUPP` → next | `copy_file_range`, then read/write |
| `copy_file_range` | Attempt, `EXDEV`/`ENOSYS` → next | read/write loop |
| `statx` | `ENOSYS` → fallback | `fstatat` |
| Landlock, seccomp | Landlock's version query, in the host before a spawn and in the helper before it reads a byte | Without Landlock a helper **refuses to decode**, and the window says so once and on the `F2` line; the `decode_without_landlock` setting lets it decode unsandboxed — [ADR 0046](adr/0046-a-helper-that-cannot-be-confined-refuses-and-a-confined-one-reaches-nothing-but-its-files.md). seccomp is mandatory |
| `io_uring` | Probe, and only used if it measurably wins | Threaded syscalls, which is the default path anyway |
| `fanotify` with FID | Requires privilege | inotify, which is the default path |
| A notification daemon | `NameHasOwner`, then `ListActivatableNames`, on the session bus — asked once and cached, because `GetServerInformation` would *start* a daemon a probe has no business starting | The status line and nothing else. An operation that finished unfocused says so where it always did, and the log names the absent facility once |
| `ffmpeg` / document renderer helpers | `PATH` lookup at first use | That preview kind is unavailable; nothing else changes |
| `wl_pointer.axis_source` and `.axis_stop` | The events arrive, or they do not | No flick: a touchpad's deltas are still tracked 1:1, and a gesture that ends has no end to detect, so the list stops where the fingers stopped. **X11 is this case permanently** — core buttons 4 to 7 are notches with no source and no end |

Two rules govern this table:

1. **A capability is probed, not compiled.** One binary runs everywhere. There is no
   `--features=systemd`, because a distro-agnostic binary cannot have its behaviour decided
   by whoever built it. Build-time features exist only for things that cannot be probed —
   developer tooling, and the `tracing` instrumentation.
2. **Every fallback path is tested.** CI forces each degradation with an environment
   override rather than trusting that the code compiles. The comma-separated names are
   `vulkan`, `portal`, `udisks2`, `watch`, `notifications`, `helpers`, and any single
   helper by program name. **`landlock` is deliberately not one of them**: confinement is mandatory, and
   [ADR 0037](adr/0037-runtime-degradation-is-visible-and-injectable.md) makes the
   override able to simulate an absent optional facility and unable to relax a required
   one. A scenario asserts the *visible* outcome of the two a person would notice — an
   unavailable decoder and a refused watch — because surviving a degradation silently is
   the failure this is guarding against.

## What is never in the graph

GLib, GObject, GIO, GTK, Qt, KIO, KDE Frameworks, GVfs, D-Bus's `libdbus`, GStreamer.

Not tribalism: adopting any of them means adopting its main loop, its object model and its
blocking-I/O conventions, and rule 1 forbids the last of those. It also means the binary
stops being portable across distro release boundaries. D-Bus is spoken as a protocol over
its socket, not through `libdbus`.

Also excluded: any dependency that requires a C toolchain to build, unless it is optional
and confined to a helper binary. `cargo build` on a fresh machine with a Rust toolchain and
nothing else must produce a working `skerry`.

## Packaging

- **The current artifact contains four binaries** plus a desktop entry and licence files.
  The helpers must stay beside `skerry`. `make install` honours `DESTDIR`
  and `PREFIX` and nothing else — no post-install scripts, no assumptions about who owns
  `/usr`.
- **Generic**: a glibc tarball for each architecture, built in CI by a job with
  no write token and attested by the one that publishes it — `gh attestation
  verify` checks it. Its bundled Makefile installs without Rust or source code.
  AppImage and musl binary artifacts are not implemented. See
  [release readiness](13-release-readiness.md).
- **Distro packages** are welcome downstream and the build is designed to make them
  straightforward: no vendored system libraries, offline builds after fetching the
  lockfile, and a version string with the commit. Packaging identical binaries is
  reproducible; independent binary builds have not been proven reproducible.
- **Flatpak ships late and is never the primary channel.** A sandbox fights a file
  manager's entire purpose; the portal-mediated build is a compatibility offering, and its
  limitations are documented rather than papered over.
- **No auto-updater in distro builds.** The tarball and AppImage may check for a newer
  version, off by default, opt-in only.

## Desktop conventions without desktop dependencies

Interoperability comes from specifications every desktop implements:

| Concern | Spec |
| --- | --- |
| Paths | XDG Base Directory |
| Trash | freedesktop trash spec |
| Thumbnails | XDG thumbnail managing standard |
| Icons | XDG icon theme spec |
| Applications | Desktop Entry spec, `mimeapps.list` |
| File types | shared-mime-info |
| "Show in file manager" | `org.freedesktop.FileManager1` |
| Third-party menu items | Desktop-entry actions, and KDE service menus, both read as data |
| Light/dark and accent | `org.freedesktop.appearance` via the settings portal, with a config override |

That last row is the whole theming policy: read the system's light/dark preference and
accent colour, ship a strong opinionated default, and offer a colour-scheme file. Skerry
does not chase a GTK or Qt theme, because matching a toolkit's look requires adopting the
toolkit.

## What three compositors actually did

Measured by running the product under each. Two of them are headless fixtures
started by `tools/gui-test` and by hand; the third is the session this was
developed on. The point of the table is that every row was a surprise to
somebody.

| | Hyprland | `kwin_wayland --virtual` | `gnome-shell --headless` |
| --- | --- | --- | --- |
| `wp_fractional_scale_v1` | yes | yes | yes |
| `wp_viewporter` | yes | yes | yes |
| `zxdg_decoration_manager_v1` | yes, and **always** server-side | yes, and grants server-side | **not advertised at all** |
| `wl_data_device_manager` | yes | yes | yes |
| `zwp_primary_selection_device_manager_v1` | yes, but see below | yes | yes |
| `zwp_text_input_manager_v3` | yes | yes, and v1 and v2 | yes |
| `zwp_input_method_manager_v2` | yes | no | no |
| `wp_cursor_shape_v1` | yes | yes | yes |
| `zwp_virtual_keyboard_manager_v1` | yes | **no** | no |
| `zwlr_virtual_pointer_manager_v1` | yes | **no** | no |

Five things follow that were not obvious before running it:

- **GNOME has no decoration manager**, so a client that does not draw a frame has
  no title bar, no close button and no resize edges there. That was the largest
  single gap the product had; the window draws its own frame now, and `--own-frame`
  asks for it everywhere so it can be seen and tested on a desktop that would
  otherwise decorate.
- **Only Hyprland can be driven by a test.** The virtual-keyboard and
  virtual-pointer protocols are wlroots', and neither KWin nor mutter implements
  them, so the input-replay fixture is Hyprland-only and the other two are
  checked by hand.
- **A headless seat has no pointer**, and both KWin and mutter present one. The
  capability probe reports `pointer` absent and the program runs; a client that
  assumed a pointer exists would not.
- **All three name cursor shapes**, which is what lets the pointer have an image
  at all: `wp_cursor_shape_v1` hands a name to the compositor, which owns the
  theme, the size and the scale. Without it the pointer keeps whatever the client
  it came from left on it — the protocol says the image is undefined on enter —
  and there is no fallback here, because the fallback is an `XCursor` theme
  parser and a shm buffer. It is staging and recent, so an older desktop is a
  window with no cursor policy and an invisible resize band.
- **Hyprland discards a primary-selection write** unless `middle_click_paste` is
  on, silently, with no error — which is Spike D's finding and remains a
  documented limitation with no fallback.

## What a compositor says about the window it is showing you

Four differences found by drawing a frame and asking the compositor what to put
in it. Every one of them is a reason the frame reads state rather than assuming
it, and three of them are Hyprland's.

- **A decoration request can be refused.** `zxdg_toplevel_decoration_v1.set_mode`
  is a request, and Hyprland answers `server_side` whatever it is asked for. So
  `--own-frame` is two things and not one: it asks for `client_side`, *and* it
  draws the frame whatever comes back. A preference a compositor can silently
  veto is not a preference — and where the request is refused, asking for it
  means two frames.
- **`wm_capabilities` is not decoration.** Hyprland offers no minimise, so there
  are two buttons there and three under a compositor that does. The layout drops
  the absent one and packs the rest against the edge: a slot left empty for an
  action that does not exist reads as a bug, and a button that does nothing reads
  as a worse one.
- **A window can be told it is maximized when it is not.** Hyprland sends
  `maximized` together with all four `tiled` flags for a floating 320×240 window,
  and ignores `unset_maximized` for a tiled one. The maximise button therefore has
  no assertable effect there, which is recorded rather than worked around — the
  GUI scenario covers the half that is ours, from a pixel to
  `xdg_toplevel.set_maximized`, and stops at the compositor.
- **A tiled window has no edges to pull.** The six-pixel resize band is off
  whenever the compositor reports `maximized`, `fullscreen` or any `tiled` state,
  because an invisible band that swallows clicks at the top of a maximized list is
  worse than no resize at all.

Owed from the frame: no drop shadow, and so no shadow margin. A shadow needs a
surface larger than the window with `xdg_surface.set_window_geometry` naming the
part that is the window, which is also where a comfortable resize target beyond
the edge would live. Six pixels inside the frame is what there is today.

## A hybrid machine can put the client and the compositor on different GPUs

The nested mutter killed the connection with `error 7: failed to import supplied
dmabufs: Could not bind the given EGLImage to a CoglTexture2D`. It is not the
nested path and it is not GNOME. Its own log says why:

```
Added device '/dev/dri/renderD128' (nvidia-drm) using no mode setting.
Added device '/dev/dri/renderD129' (amdgpu) using no mode setting.
GPU /dev/dri/renderD128 selected as primary
```

**mutter chose the discrete NVIDIA GPU and `wgpu` chose the integrated AMD one**,
because [ADR 0003](adr/0003-wgpu-first-backend.md)'s adapter request asks for
`LowPower` — a file manager is not a game. The buffer is then an AMD dmabuf handed
to an NVIDIA EGL, and the import fails. Forcing the client onto the same GPU with
`VK_LOADER_DRIVERS_SELECT=nvidia_icd.json` makes it work: the window maps, draws
its own frame, and reports `own-frame` in its capability line.

This is a real configuration and not a laboratory one — it is every hybrid laptop
where the compositor has picked the discrete card, and the symptom is that the
program does not start.

**Fixed.** `zwp_linux_dmabuf_v1` version 4 answers `main_device` with the `dev_t`
of the GPU the compositor renders on; `/sys/dev/char/<major>:<minor>/device`
turns that into a PCI vendor and device id; and the adapter is chosen to match
before `LowPower` is consulted. Where nothing matches — one GPU, or a device with
no PCI parent — the request is exactly what it was. Verified by running under the
nested GNOME with no environment overrides at all: 234, 244, 331 ms to first
frame on the NVIDIA card, where before it was a protocol error and an exit.

Owed: the same probe on X11, where the answer is `DRI3`'s open of the provider
rather than a Wayland global. Nothing has needed it there yet — the X11 backend
reports `render_device: None`, which means "no opinion" and lets the adapter be
chosen the way it was before the probe existed.

Owed: none of this is a real GNOME *session*, only a headless one.
