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
| libc | **glibc 2.28, or musl 1.2** | Debian 10 / RHEL 8 era. `rustix`'s `linux_raw` backend means most syscalls bypass libc entirely, so this floor is mostly about the dynamic loader and the Vulkan loader. |
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
| xdg-desktop-portal | D-Bus name | Direct filesystem paths; no portal-mediated open |
| `wp_fractional_scale_v1` | Wayland registry | Integer scaling with supersampled glyph rasterization |
| `xdg_activation_v1`, `cursor_shape_v1`, `text_input_v3` | Wayland registry | Documented feature loss per protocol; never a crash |
| Vulkan | Loader present and a device enumerates | GL backend; then llvmpipe |
| `FICLONE` | Attempt, `EOPNOTSUPP` → next | `copy_file_range`, then read/write |
| `copy_file_range` | Attempt, `EXDEV`/`ENOSYS` → next | read/write loop |
| `statx` | `ENOSYS` → fallback | `fstatat` |
| Landlock, seccomp | Kernel version + `prctl` probe | Helper processes run unsandboxed **and log a warning**; never silently unsandboxed |
| `io_uring` | Probe, and only used if it measurably wins | Threaded syscalls, which is the default path anyway |
| `fanotify` with FID | Requires privilege | inotify, which is the default path |
| A notification daemon | D-Bus name | In-app toasts only |
| `ffmpeg` / document renderer helpers | `PATH` lookup at first use | That preview kind is unavailable; nothing else changes |

Two rules govern this table:

1. **A capability is probed, not compiled.** One binary runs everywhere. There is no
   `--features=systemd`, because a distro-agnostic binary cannot have its behaviour decided
   by whoever built it. Build-time features exist only for things that cannot be probed —
   developer tooling, and the `tracing` instrumentation.
2. **Every fallback path is tested.** CI forces each degradation with an environment
   override (`SKERRY_DISABLE=udisks2,portal,vulkan,landlock`) rather than trusting that the
   code compiles.

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

- **The primary artifact is one relocatable binary** plus a small data directory
  (`.desktop` file, icons, MIME associations, catalogues). `make install` honours `DESTDIR`
  and `PREFIX` and nothing else — no post-install scripts, no assumptions about who owns
  `/usr`.
- **Generic**: a tarball and an AppImage, both built in CI for both architectures.
- **Distro packages** are welcome downstream and the build is designed to make them
  trivial: no vendored system libraries, no network access during build, reproducible
  output, a version string from the release tag.
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
