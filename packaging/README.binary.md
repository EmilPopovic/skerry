# Skerry binary distribution

This archive contains Skerry and all three helper executables. No Rust toolchain
or source checkout is required.

Run directly from the extracted directory:

```sh
./usr/bin/skerry --version
./usr/bin/skerry /path/to/folder
```

Install for your account, or stage files for a package:

```sh
make install PREFIX="$HOME/.local"
make install DESTDIR=/tmp/skerry-package PREFIX=/usr
```

Use `make uninstall` with the same `PREFIX` and `DESTDIR` to remove these files.
Installation does not change the desktop's default file manager.

This is an alpha. Keep backups of important files. Copy and extraction undo move
output into trash; trash must be available on a compatible filesystem. Editing
symlinks and hardlinked files is refused. Unsupported archive features are
reported instead of treated as successful extraction.

A Wayland or X11 session, libxkbcommon, fonts, and Vulkan or OpenGL libraries are
needed for a window. Software rendering is supported. Document previews and
compressed archives may need external renderers or decompressors. Helper
confinement depends on the host kernel; `--probe` on a helper reports its status.

`BUILD-INFO` identifies the architecture, build commit and minimum GLIBC symbol
version referenced by these binaries. This does not establish compatibility with
every distribution: shared libraries and GPU drivers have their own requirements.
The tarball is not a static musl build or an AppImage.

`THIRD-PARTY-NOTICES.txt` and `RUST-COPYRIGHT.html` contain distribution notices
for the bundled Rust dependencies and standard library. The installer keeps them
with Skerry's LICENSE and NOTICE.

Helpers require seccomp, no-new-privileges and resource limits. Their syscall
filter denies networking and selected process/kernel interfaces. Landlock adds
filesystem restrictions where supported: output directories are writable, and
only resolved external tools and their ELF interpreters can execute. Kernels
without usable Landlock retain the syscall and resource restrictions but have
reduced filesystem isolation. Helpers can read ordinary user-readable files;
this is not a confidentiality boundary against everything in your home folder.
