# 0027 — A setting is a line in the config; the session remembers what the window changed

**Status:** Accepted

## Context

[ADR 0025](0025-the-config-file-is-the-keymap-written-out.md) made
`$XDG_CONFIG_HOME/skerry/config` a keymap and stopped there, deliberately:

> **Keys only, for now.** Settings — the theme, the sidebar, the default view —
> are named as owed rather than smuggled in, because every one of them needs the
> precedence rule against ADR 0011's session file that this record deliberately
> does not invent.

Its revisit trigger was *the first setting that is not a chord*, and this is it.
Two things forced it at once. A pre-release has no way to change anything except
a key, and **default-handler registration** — "make Skerry the file manager for
folders" — is a line in `mimeapps.list` that
[08-roadmap.md](../08-roadmap.md#m7--4-wk--desktop-citizenship) declines to give a
command with no pointer surface, because [rule
5](03-principles.md) would fail it.

The hard part is the one 0025 named. [ADR
0011](0011-one-plain-text-state-file-written-atomically.md)'s session file
already holds the theme, the sidebar's width and whether it is open, the sort
order and the view mode — written on every change so `kill -9` loses nothing.
A configuration file holding the same names asks a question the keymap never
had: when the file says `theme = light` and the session says the window was dark
when it closed, which wins?

Three answers were considered.

**Config wins always.** Then `Ctrl+Shift+D` changes the theme until the next
start, and a person who set it once can never change it from inside the window
again. That is a setting that fights the interface.

**Session wins always.** Then the config line is dead the first time anybody
touches the window, and a file that does nothing is worse than no file.

**Config is the starting value; the session records the change.** The file says
what a *fresh* window does; the session says what *this* window was last left
doing. That is what every program with both a config and a session already does,
and it is the only one of the three where both files keep meaning something.

## Decision

**The config file gains settings beside its keys, and the session file wins
where both speak — but only for a value the window itself changed.**

- **One file, two kinds of line.** `theme = dark` sits beside
  `view.theme = ctrl+shift+d` in the same `key = value` grammar 0025 defined. A
  name that is not a command id is a setting; a name that is neither is a problem
  printed at startup, which is what a bad chord already does.
- **The session remembers only what the window changed.** ADR 0011's file gains
  a per-setting *touched* mark. A window that never toggled the theme restores
  the config's value; one that did restores what it was left at. So editing the
  config changes a machine that has not been told otherwise, and never argues
  with a person who set it from inside.
- **Six settings and no more in this record**: `theme`, `sidebar`,
  `sidebar_width`, `view` (the default view mode), `hidden` (whether hidden files
  show), and `confirm_trash`. Each is one the window already changes at runtime,
  which is what makes the rule above testable; a setting the window cannot change
  needs no precedence and can be added without a record.
- **A settings surface, because rule 4 is discoverability.** `Ctrl+,` stops
  writing the keymap and opens a panel over the window listing every setting with
  its current value and where that value came from — the file, the session, or the
  default. Writing the keymap keeps its command and moves to the palette. The
  panel is the pointer's way to everything in it, and every row is reachable by
  keyboard, which rule 5's test enforces the same way it does for a menu.
- **Default handler registration is a row in that panel**, not a command in the
  palette: it writes `inode/directory=dev.skerry.Skerry.desktop` into
  `$XDG_CONFIG_HOME/mimeapps.list`, by hand, preserving every other line — the
  same shape as GTK's `bookmarks` in
  [ADR 0021](0021-a-pin-is-a-line-in-gtks-bookmarks-file.md). It says what it did
  in the panel, and it can be undone by the same row.

## Consequences

**Three places now name a setting**: the config's parser, the session's writer,
and the panel. A seventh setting means touching all three, and the test that
keeps them in step is a table walked by all of them rather than three lists —
which is the same shape `sk_cmd::Registry` already imposes on commands.

**`mimeapps.list` is a file other programs write.** Preserving what we did not
write means parsing and re-emitting it, and a file with a syntax we do not
understand must be left alone rather than rewritten — the failure mode is
somebody's default browser, and it is worth the refusal.

**The session file grows a mark per setting**, which is a format change to a file
that already exists in people's state directories. It reads as "not touched" when
absent, so an old file restores the config's values, which is the behaviour a
first upgrade should have.

**`Ctrl+,` changes what it does.** It is the one binding this record moves, and
somebody who learned it as *write the keymap* will meet a panel. The panel has a
row that writes the keymap, so the thing they wanted is one press away and now
visible to somebody who never knew the chord.

## Revisit trigger

**A setting that is not a value** — an ordered list, a per-folder override, a
table of user-defined actions. All three are the file growing a grammar, which
[ADR 0025](0025-the-config-file-is-the-keymap-written-out.md) already names as a
different decision, and this record inherits that boundary rather than moving it.

**Or a second program writing our config.** The moment anything but a person
edits that file, "the window never writes it" stops being true and the precedence
rule above has a third party in it.
