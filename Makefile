# Installing Skerry: three binaries and one desktop entry.
#
# `DESTDIR` and `PREFIX` and nothing else, which is what
# `docs/06-portability.md` promises: no post-install script, no assumption about
# who owns /usr, and nothing written outside the two of them. A packager sets
# `DESTDIR` to a staging root; a person building for themselves sets
# `PREFIX=$HOME/.local`.
#
# The two helpers go in `bindir` beside the binary rather than in `libexec`,
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

build:
	$(CARGO) build --release --workspace

# `install -D` makes the parent, so there is no `mkdir -p` to keep in step with
# the paths below.
install: build
	for binary in $(BINARIES); do \
		install -Dm755 target/release/$$binary $(bindir)/$$binary || exit 1; \
	done
	install -Dm644 packaging/dev.skerry.Skerry.desktop \
		$(datadir)/applications/dev.skerry.Skerry.desktop
	install -Dm644 LICENSE $(datadir)/licenses/skerry/LICENSE
	install -Dm644 NOTICE $(datadir)/licenses/skerry/NOTICE

uninstall:
	for binary in $(BINARIES); do rm -f $(bindir)/$$binary; done
	rm -f $(datadir)/applications/dev.skerry.Skerry.desktop
	rm -rf $(datadir)/licenses/skerry

check:
	./tools/check

clean:
	$(CARGO) clean
