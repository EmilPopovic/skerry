# 0025 — The config file is the keymap written out, and the program never writes it behind you

**Status:** Accepted

## Context

[08-roadmap.md](../08-roadmap.md) names M6's config file as "a serialization of
the same registry", and [rule 2](../03-principles.md) is that every action is one
entry in one registry — which is the thing that makes a rebindable keymap
possible at all: a chord belongs to a command, a command belongs to a scope, and
[`sk_cmd::Scope`](../../crates/sk-cmd/src/registry.rs) exists precisely so
`Ctrl+A` can mean *select the files* over a listing and *select the text* inside
a field without either being a special case.

What has to be decided is smaller than "a settings system", and three questions
in it are expensive to get wrong.

**Who owns the file.** A program that rewrites its own configuration owns it: a
comment a person adds is deleted on the next save, an option they set by hand is
reordered, and the file stops being a thing anybody edits. The alternative —
never writing it — makes the feature undiscoverable, because a file that does not
exist teaches nobody what may be in it.

**What is in it, given that a session file already exists.**
[ADR 0011](0011-one-plain-text-state-file-written-atomically.md) writes the
window's *state* — tabs, splits, sorts, the theme as it was left — and restores
it. A configuration answers a different question: not "how was it left" but "how
should it start". The two overlap on every setting a person can change from
inside the window, and a precedence rule between them is a design decision each
setting has to make individually.

**What a bad line does.** A keymap that a typo breaks silently is a keymap nobody
trusts. The registry already answers this for its own defaults: everything wrong
is recorded in `Registry::problems` rather than panicking, and the window prints
them at startup.

## Decision

**`$XDG_CONFIG_HOME/skerry/config` is a plain-text keymap, read at startup and
written only when asked.**

- **One line per command**: `# comment` lines, blank lines, and
  `command.id = chord, chord`. The same shape as the two other files this program
  reads by hand — GTK's `bookmarks` and its own session — parsed by
  `sk_app::config` in about a hundred lines. No format crate: rule 10's question
  is whether a dependency earns its row, and `key = value` does not.
- **A line replaces a command's chords rather than adding to them.** `= ` with
  nothing after it unbinds. Merging would make "how do I get rid of this
  binding" unanswerable, which is the question a rebindable keymap exists to
  answer.
- **The program never writes the file on its own.** `config.write` — a command,
  on the palette and the background menu — writes the *whole* registry out,
  every command with its current chords, commented. So the file is a thing a
  person opens, edits and owns, and the way to discover what may be in it is to
  ask for it once. A file that already exists is not overwritten without a fresh
  answer; the command says what it did in the status line either way.
- **A bad line is a problem, not a crash and not a silence.** An id nothing
  registers, a chord that does not parse, a chord already taken in that scope:
  each is recorded where the defaults' problems already go, printed at startup,
  and the *default* binding stays. A keymap is not an all-or-nothing document.
- **Keys only, for now.** Settings — the theme, the sidebar, the default view —
  are named as owed rather than smuggled in, because every one of them needs the
  precedence rule against ADR 0011's session file that this record deliberately
  does not invent. What a keymap needs from that rule is nothing: a chord is not
  something the window changes as it runs.

## Consequences

**A chord has to survive a round trip through text.** `Chord::parse` had a
display form beside it and no serialiser, so writing the file needed
`Chord::text` — `ctrl+shift+f3`, the exact input `parse` accepts — and the two
are tested against each other over every chord the registry binds. That test is
the file's real contract: what is written back can be read back.

**Rebinding is a registry operation, so the registry gained one.**
`Registry::rebind` drops a command's bindings and installs the given chords,
refusing a chord another command holds *in the same scope* — the same rule
`add_in` applies to the defaults, and now the only place that rule lives.

**The keyboard overlay and the palette follow for free**, because both read the
registry rather than a table beside it. A rebound chord shows in `F1` and in the
palette's rows with no further work, which is what rule 2 buys.

**A missing file is not an error and is the common case.** No file, no lines, no
problems: the defaults are the keymap. The first thing a person does is run the
command that writes one.

## Revisit trigger

**The first setting that is not a chord.** The moment a config line has to mean
something the window can also change while it runs — the theme, the sidebar's
width — this record's "keys only" stops being a scope and becomes a gap, and the
successor is the one that decides precedence against
[ADR 0011](0011-one-plain-text-state-file-written-atomically.md)'s session file.

**Or a keymap that a person cannot express**, such as a chord sequence, a
per-directory binding, or one command bound in two scopes from a single line. All
three are the file growing a grammar, which is a different format and therefore a
different decision.
