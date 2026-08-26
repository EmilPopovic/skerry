#!/usr/bin/env python3
"""A GTK clipboard peer, used the way Nautilus is used: as somebody else's
implementation to test against.

It is a separate process on purpose. GTK is a banned dependency for Skerry
(rule 9), and it stays banned — but the *interop* being measured here is interop
with GTK, so a GTK peer that can be driven from a script is worth more than a
peer that has to be clicked. Which display server it uses is decided by
GDK_BACKEND in the environment, so the same script is both halves of the matrix.

  gtk-peer.py read clipboard          print what is on the clipboard
  gtk-peer.py read primary            the same for the primary selection
  gtk-peer.py own clipboard TEXT SEC  own the clipboard for SEC seconds
"""
import sys, gi
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk, GLib


def clipboard(which):
    atom = Gdk.SELECTION_PRIMARY if which == "primary" else Gdk.SELECTION_CLIPBOARD
    return Gtk.Clipboard.get(atom)


def read(which):
    cb = clipboard(which)
    print("backend", Gdk.Display.get_default().__class__.__name__)
    targets = cb.wait_for_targets()
    names = [t.name() for t in targets[1]] if targets and targets[0] else []
    print("targets", " ".join(names))
    for want in ("text/uri-list", "text/plain;charset=utf-8", "UTF8_STRING"):
        if want in names:
            data = cb.wait_for_contents(Gdk.Atom.intern(want, False))
            if data is not None:
                raw = bytes(data.get_data() or b"")
                print(f"read {want} bytes {len(raw)} first {raw[:80]!r}")
                return 0
    text = cb.wait_for_text()
    if text:
        print(f"read text bytes {len(text)} first {text[:80]!r}")
        return 0
    print("read nothing")
    return 1


def own(which, payload, seconds):
    # `set_with_data` is not introspectable, so this peer owns text targets only.
    # That is enough for what it is here to answer — whether a real toolkit's
    # selection crosses the display-server boundary at all — and the file-URI case
    # is covered by Nautilus.
    cb = clipboard(which)
    cb.set_text(payload, -1)
    print("owning", which, "for", seconds, "s")
    sys.stdout.flush()
    GLib.timeout_add_seconds(int(seconds), Gtk.main_quit)
    Gtk.main()
    return 0 if ok else 1


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "read"
    which = sys.argv[2] if len(sys.argv) > 2 else "clipboard"
    if mode == "read":
        sys.exit(read(which))
    if mode == "own":
        sys.exit(own(which, sys.argv[3] if len(sys.argv) > 3 else "file:///tmp/gtk-peer.txt",
                     sys.argv[4] if len(sys.argv) > 4 else 15))
    print(__doc__)
    sys.exit(2)
