# Installing Skerry: four binaries and one desktop entry.
#
# `DESTDIR` and `PREFIX` and nothing else, which is what
# `docs/06-portability.md` promises: no post-install script, no assumption about
# who owns /usr, and nothing written outside the two of them. A packager sets
# `DESTDIR` to a staging root; a person building for themselves sets
# `PREFIX=$HOME/.local`.
#
# **No `org.freedesktop.FileManager1.service` is installed, and that is a
# decision rather than an omission.** The path a package would put it at is
# `$(datadir)/dbus-1/services/org.freedesktop.FileManager1.service`, which on a
# machine with Nautilus installed already belongs to Nautilus's package — and
# shipping it would take the desktop's *show me this file* away from a file
# manager the person already chose, at install time, which is the same grab
# `sk_app::service` refuses to make at run time. The file is written into
# `$XDG_DATA_HOME` by the settings panel's *Open folders with Skerry* row
# instead, where it wins over this directory by order; see
# `crates/sk-desktop/src/activation.rs`.
#
# The three helpers go in `bindir` beside the binary rather than in `libexec`,
# because `sk_preview::host` looks beside itself first and treats a helper found
# on `PATH` as a version it cannot trust. A packager who wants them in libexec
# can move them there and put that directory on the launcher's `PATH`; both
# layouts are supported and the first needs no configuration.

PREFIX ?= /usr/local
DESTDIR ?=
CARGO ?= cargo
bindir = $(DESTDIR)$(PREFIX)/bin
datadir = $(DESTDIR)$(PREFIX)/share

BINARIES = skerry skerry-thumbnailer skerry-preview-doc skerry-archive

.PHONY: all build install uninstall check clean

all: build

# The standard library's notices are staged beside the binaries here, so that
# `install` needs no `rustc`: under `sudo` it is usually not on `PATH`, and when
# it is, it is a rustup proxy running as root.
build:
	$(CARGO) build --release --workspace
	cp "$$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html" target/release/RUST-COPYRIGHT.html

# `install -D` makes the parent, so there is no `mkdir -p` to keep in step with
# the paths below.
#
# **The desktop entry names the binary by its absolute path**, because the
# graphical session's `PATH` is not the shell's: a display manager starting a
# compositor reads no `~/.profile`, so `PREFIX=$HOME/.local` once installed an
# entry its own launcher could not start. Quoted, because the specification's
# `Exec` splits on spaces and a prefix may hold one.
#
# **`install` builds nothing.** It used to depend on `build`, so `sudo make
# install` ran every build script and proc-macro in the dependency graph as
# root. Building is `make`, as the person; this only copies what that left, and
# stops when it left nothing.
install:
	@for file in $(BINARIES) RUST-COPYRIGHT.html; do \
		if [ ! -f "target/release/$$file" ]; then \
			echo "target/release/$$file is missing: run \`make\` first, without sudo, then \`make install\`" >&2; \
			exit 1; \
		fi; \
	done
	for binary in $(BINARIES); do \
		install -Dm755 "target/release/$$binary" "$(bindir)/$$binary" || exit 1; \
	done
	install -d "$(datadir)/applications"
	sed 's|^Exec=skerry |Exec="$(PREFIX)/bin/skerry" |' packaging/dev.skerry.Skerry.desktop \
		> "$(datadir)/applications/dev.skerry.Skerry.desktop"
	install -Dm644 packaging/icons/dev.skerry.Skerry.svg "$(datadir)/icons/hicolor/scalable/apps/dev.skerry.Skerry.svg"
	install -Dm644 LICENSE "$(datadir)/licenses/skerry/LICENSE"
	install -Dm644 NOTICE "$(datadir)/licenses/skerry/NOTICE"
	install -Dm644 packaging/THIRD-PARTY-NOTICES.txt "$(datadir)/licenses/skerry/THIRD-PARTY-NOTICES.txt"
	install -Dm644 target/release/RUST-COPYRIGHT.html "$(datadir)/licenses/skerry/RUST-COPYRIGHT.html"

uninstall:
	for binary in $(BINARIES); do rm -f "$(bindir)/$$binary"; done
	rm -f "$(datadir)/applications/dev.skerry.Skerry.desktop"
	rm -f "$(datadir)/icons/hicolor/scalable/apps/dev.skerry.Skerry.svg"
	rm -f "$(datadir)/licenses/skerry/LICENSE" "$(datadir)/licenses/skerry/NOTICE"
	rm -f "$(datadir)/licenses/skerry/THIRD-PARTY-NOTICES.txt" "$(datadir)/licenses/skerry/RUST-COPYRIGHT.html"

check:
	./tools/check

clean:
	$(CARGO) clean
