# The first public release

What stands between [`0.1.0-alpha.1`](../RELEASE-NOTES.md) and a file manager
somebody can set as the handler for folders and then stop thinking about.

This document settles one question — **which features the release is allowed to
be missing** — and it settles it the way the rest of the repository settles
things: each row names the rule it serves, where the code goes, and the gate that
proves it arrived. It does not restate [08-roadmap.md](08-roadmap.md); every row
names the milestone it belongs to, so the roadmap stays the map and this is the
order the map is walked in for one release.

## Where the work stands

**This table is the handover.** A session that picks this document up reads it
first and edits it before it starts, so that two sessions cannot claim one row and
a row nobody finished says what it still owes. It is edited in place; the rows
themselves are below and do not move.

| Row | Status | Where | What is left |
| --- | --- | --- | --- |
| 1 · activation file | **merged** | `main` | — |
| 2 · icon themes | **merged** | `main`, ADR 0042 | `marks` is Skerry's own set: thirty-one shapes in [`sk_ui::marks`](../crates/sk-ui/src/marks.rs), a specimen sheet (`--capture --marks`, goldens `marks` and `marks-light`), frame p99 0.281 → 0.325 ms best of five. The fresh-install default stays `desktop`, as ADR 0042 settles |
| 3 · always open with this | **merged** | `main` | — |
| 4 · Empty Trash | **merged** | `main` | — |
| 5 · new file, symlink, Templates | **merged** | `main` | — |
| 6 · Properties that writes | **merged** | `main` | — |
| 7 · a notification when unfocused | **merged** | `main` | The *Show in folder* button needs row 12 |
| 8 · recents | **merged** | `main`, ADR 0043 | See *Owed* below |
| 9 · path completion | **merged** | `main` | — |
| 10 · middle click, PRIMARY, a drag that asks | **merged** | `main` | **No right-drag.** A drag moves, `Ctrl` copies, `Alt` opens *Move here / Copy here / Link here / Cancel* where it lands, for a drop this window takes |

**R1 is on `main`**, merged in the order below, with the refused-thumbnail fix
first. The merged tree passes every gate except the three golden frames that drift
on the development machine at untouched `HEAD` — `keyboard-overlay`, `carrying`,
`palette` — and its first full sweeps, run serially at a load average of 2 to 4,
were **161 of 163 on X11 and 165 of 166 on Wayland**. The one failure both
servers share is `release-qt-cut-clipboard-works-in-both-directions`, whose Qt
peer needs `PyQt6` and this machine has none. The other X11 failure was
`path-completion-is-a-list-and-not-a-mode`, which had only ever run alone:
`…/fixture` has real completions once a sweep leaves `fixture-<scenario>` beside
it. It now takes a fixture of its own.

R2 is next, and nothing in it has started.

### Merging R1, as it was done

1. `fix/refused-thumbnail-spins`
2. `r1/01-activation-file`
3. `r1/03-07-defaults-and-notice`
4. `r1/10-drag-asks`, which is `r1/04-05-trash-and-creation` and one commit
5. `r1/06-properties`
6. `r1/09-10-path-field-and-pointer`
7. `r1/02-icons`, carrying ADR 0042 and the icon set
8. `r1/08-recents`, carrying ADR 0043

Every conflict was an adjacent insertion. Two functions went over clippy's
hundred lines once both halves were in and were split — `commands::hand_over`
out of `commands::edit`, `App::overlay_line` out of `App::chrome_state` — and
`recents.rs` now calls `sk_desktop::data_home`. ADRs 0037 to 0041 became rows of
the index table rather than bullets under it.

### Owed from R1, and why

- **Recents have no sidebar row and sort the way the pane does.** The sidebar's
  rows are paths throughout — trees, pins, reordering and drop targets — and a
  row with no directory behind it is its own change.
- **A theme's icons in the sidebar, the breadcrumb menus and the drag card.**
  They draw Skerry's own marks whatever `icons` says — a place its emblem, a
  volume its drive — which is chrome rather than a view of files, so not rule
  8's; it is a consistency still worth having.
- **The shared recents write** stays behind the measurement ADR 0043 names, and
  a notification's *Show in folder* behind row 12.

**A commit whose subject begins `WIP:` is a snapshot, not a result.** It was
taken so work survives a session ending; nothing in one has been checked, and no
branch is ready to merge while one is on it.

**A row is done when three things are true**: the gate named in its row exists as
a test, `./tools/check` passes on the branch, and the line above says so with the
commit. Two standing warnings for anyone working here:

- **Do not re-bless the golden frames.** Three of them — `keyboard-overlay`,
  `carrying` and `palette` — fail at untouched `HEAD` on the development machine
  and have since September 2026. Blessing them inside unrelated work hides
  whatever else moved.
- **The Wayland half of the scenarios and every frame time are local.** CI runs
  `tools/gui-test --x11` under `Xvfb` and nothing else, so a number or a Wayland
  scenario is the author's job before the branch is proposed.
- **No sweep taken while the other branches were building is worth anything.**
  Several of these rows were developed in parallel, and a machine at load 6 to 9
  failed `a-hung-mount-never-blocks-the-frame-loop` — 5 of 7 frames over budget,
  p99 34 ms — against a scenario that passes four times in four in isolation.
  That is [12-measurement.md](12-measurement.md#a-suite-is-a-load-test-of-whatever-else-is-running)'s
  own lesson arriving again, and it means the sweeps are **serial work on a quiet
  machine after the branches land**, not something each branch can claim for
  itself.

## The bar

> **Somebody makes Skerry the handler for folders on a machine they use for work,
> and does not open another file manager to finish a task.**

That is detectable in a week and it is the only test here. Three things send a
person back, and the rows below are those three things and nothing else:

1. **The desktop hands it something it cannot answer.** A browser's *Open
   containing folder* while Skerry is closed, a drag into a sandboxed
   application, a file that should open in the program they chose for it last
   time.
2. **A daily task cannot be finished without leaving.** Making a file, fixing a
   permission, emptying the trash, renaming forty photographs.
3. **It is visibly poorer than what it replaced.** Which is almost entirely
   icons: every other surface in this product is already better drawn than the
   thing it competes with, and a folder that is a coloured page next to a
   desktop full of themed icons reads as unfinished whatever the frame time is.

What the bar is **not**: parity with Nautilus, and anything in
[11-non-goals.md](11-non-goals.md). A row is declined by pointing there rather
than by re-arguing it — remote filesystems, cloud providers, MTP, desktop icons,
an embedded terminal and a plugin ABI are all out, and out permanently.

## What puts a row in the release

A row is in when the product fails one of three tests without it, and out when it
fails none:

| Test | Reading |
| --- | --- |
| **The role** | The desktop, or another application on it, hands Skerry something it cannot answer. These are not features; they are the job of being the default |
| **The day** | A task somebody does weekly cannot be completed without a second file manager |
| **The record** | A rule in [03-principles.md](03-principles.md) is *stated unmet* in [the README's own list](../README.md#what-this-alpha-does-not-do) |

Two rules are admitted unmet today and both are on this list.
[Rule 4](03-principles.md#4-discoverable-not-just-configurable) — rebinding
happens where you discovered the command — is answered by a config file and
nothing else. [Rule 6](03-principles.md#6-state-survives-everything) names
recents in its own sentence and there are no recents. A release that ships with a
rule knowingly unmet is not disqualified; a release that ships with one
*silently* unmet is, and that is the difference this table is drawing.

## R1 — the role itself

Ten rows. Without these the product is a good file browser that the desktop
around it does not know what to do with.

| | What | Where | Rule | Gate |
| --- | --- | --- | --- | --- |
| 1 | **A D-Bus activation file.** `org.freedesktop.FileManager1.service` naming the installed binary, so `ShowItems` from a closed application starts one. Today the desktop entry declares `Implements=` and nothing ships the `.service`, so on a machine with Nautilus installed the call activates Nautilus and Skerry answers only while it is already open | [`packaging/`](../packaging/) and the [`Makefile`](../Makefile) | 9 | A scenario that calls `ShowItems` over `gdbus` with no window open and asserts a window appears on the named file |
| 2 | **Icon themes, as one of two choices.** `index.theme` parsed and the freedesktop lookup run in `sk-desktop`; the file rasterised by the helper that already decodes PNG, JPEG and SVG; drawn from **an atlas of its own**. **Our own marks stay a first-class answer**, selected by a setting rather than left as the fallback nobody chose — see below | `sk-desktop` for the lookup, `sk-app` for the request, `sk-gpu` for the atlas | 9 | A golden frame of each source over the same directory, and the *Scroll a large listing* row unmoved from 0.342 ms p99 |
| 3 | **Always open with this.** [`sk_desktop::mimeapps::set_default`](../crates/sk-desktop/src/mimeapps.rs) already writes one association safely for `inode/directory`; Open With gains a second row that calls it for the type under the cursor | `sk-app` menu over the existing `sk-desktop` call | 2 | The existing `mimeapps` round-trip tests extended to an arbitrary type, plus the refusal path on a file this grammar cannot re-emit |
| 4 | **Empty Trash**, and a size on the trash place. [`sk_fs::trash`](../crates/sk-fs/src/trash.rs) already enumerates every trash it can restore from; emptying is the same walk and an erase | `sk-fs` | 3 | It takes the *delete for good* shape — a panel that names what goes and puts nothing on the stack — **and it settles the stack it invalidates**, below |
| 5 | **New file, new symlink, and Templates.** [`places`](../crates/sk-desktop/src/places.rs) already reads `user-dirs.dirs`, so `XDG_TEMPLATES_DIR` costs a key rather than a parser — and the sidebar's own list stays as it is, for the reason its comment already gives | `sk-fs::ops` and a `Change` variant each; the menu in `sk-app` | 3 | Each is one entry on the one stack, asserted through the window by a scenario that makes one and presses `Ctrl+Z` |
| 6 | **Properties that writes.** The nine permission bits, and account names instead of `1000:1000`. [`sk_fs::facts`](../crates/sk-fs/src/facts.rs) already carries the numbers and already says why the names are not there — `getpwuid` can reach the network on a machine with LDAP — so this is that comment's caller arriving, on a job | `sk-fs::facts` for the lookup and the `chmod`, `sk-app` for the panel | 1, 3 | A permission change is undoable — a new `Change` variant carrying both modes — and a hung name service costs no frame, on the `tools/hung-mount` pattern |
| 7 | **A notification when a long operation finishes unfocused.** `org.freedesktop.Notifications` is a method call on the session bus this program already speaks; *when* to send one is `sk-app`'s, because focus is `sk-plat`'s and policy is never `sk-desktop`'s | `sk-desktop::bus` for the call, `sk-app` for the decision | 7 | Absent notification daemon is a probe and a documented fallback, not an error — the [rule 9](03-principles.md#9-distro-agnostic-by-construction) shape every other integration already has |
| 8 | **Recents**, which [rule 6](03-principles.md#6-state-survives-everything) names in its own sentence. **Read** `recently-used.xbel` and show it merged with a store of our own; the shared **write** is staged behind a measurement, below | `sk-desktop` for the read, `$XDG_STATE_HOME` for the store, `sk-app` for the merge | 6 | The list survives `kill -9`, and reading it is a job: 2.727 ms to parse a file at GTK's 1,000-entry cap is a third of a frame |
| 9 | **Path completion in `Ctrl+L`.** A `readdir` on the job pool feeding the field that already exists | `sk-app::field` over `sk-fs` | 1, 4 | No syscall on the UI thread while completing, and one flag per question — having the keyboard and being in effect stay separate, which is [rule 2](03-principles.md#2-everything-is-a-command)'s own wording |
| 10 | **Middle click, PRIMARY, and a drag that asks.** [`Button::Middle`](../crates/sk-plat/src/event.rs) already arrives; what is missing is two commands in the registry — open in a new tab, paste the primary selection — and the right-drag menu that is the discoverable form of the modifier keys a drag already honours | `sk-cmd` registry, `sk-app` hit test | 2, 4, 5 | [`every_command_can_be_reached_without_a_keyboard`](../crates/sk-app/src/commands.rs) already fails the build for a row that adds a menu item and no registry entry |

**Rows 9 and 10 are one piece of work.** Both land in the path field, and both are
places where rule 2's *one flag per question* is easy to break: a completion list
that takes the keyboard is not a new mode, and PRIMARY pasting into a field is the
field's command rather than a branch in the event path.

**Row 4 invalidates part of the undo stack, and that is the row's real content.**
The stack holds `Change::Restored(trash::Entry)` and undoes it with `put_back`.
Emptying the trash makes every one of those un-undoable, silently — which is
[rule 3](03-principles.md#3-everything-is-reversible)'s own failure test. Empty
Trash marks them in the shape
[ADR 0036](adr/0036-operation-failures-retain-pending-work.md) already has for
work that is kept with a note on it, so `Ctrl+Z` says what happened instead of
failing quietly.

**Row 2 is the longest pole in R1, and it is not thumbnails.**
[ADR 0018](adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)'s grid
is 128×128 slots keyed by path, mtime and size; an icon is keyed by theme, name,
size and scale, is drawn between 16 and 64 pixels, and the freedesktop per-size
directories exist precisely because 16-pixel art is not 48-pixel art downscaled.
One 16-pixel icon in a 64 KiB slot also spends a 256-slot budget on the wrong
thing. So it is a second geometry, and the working set is tens of images rather
than one per file — which is why the first frame is not the problem the atlas
was: the same *one texel until the first thumbnail exists* trick applies, and
today's geometric mark is what is drawn until the real one lands, at the final
position, to the logical pixel.

**And the mark is a choice, not a fallback.** A file manager that draws the
desktop's icons everywhere is one that looks like every other window on that
desktop; a person who wants Skerry's own marks — nine hues, legible at thirteen
pixels, 0.012 ms a frame — should not have to uninstall an icon theme to get
them. So the source is a setting in
[ADR 0027](adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)'s
shape, with two real values and a third that asks the desktop: **our marks**,
**the icon theme**, and **whatever the desktop is using**, which is what a fresh
install takes. Built-in marks are then a path that stays tested rather than a
branch that rots — which is also what makes row 2 safe to ship the day the theme
half is not finished.

## R2 — what makes the release honest

Nine rows that are not the role, but each of which is either a rule with a stated
gap or an interoperability claim the release would otherwise have to narrow.

| | What | Where | Rule | Gate |
| --- | --- | --- | --- | --- |
| 11 | **Unix file descriptors on our own bus.** `NEGOTIATE_UNIX_FD` at authentication, the `h` type marshalled as an *index into the fd array*, the `UNIX_FDS` header field, and `sendmsg`/`recvmsg` with a control message. [`bus/wire.rs`](../crates/sk-desktop/src/bus/wire.rs)'s `Value` has thirteen variants and none of them is a descriptor | `sk-desktop::bus` | 9, 10 | The wire's existing mutation campaign ([ADR 0040](adr/0040-bounded-parser-mutation-campaigns.md)) extended to messages that claim descriptors they do not carry — and a message the parser answers `None` to closes its descriptors, because *did not understand, did not act* becomes an fd leak the moment fds exist |
| 12 | **A connection that lives, and signals on it.** `AddMatch` and a subscriber, where today there is a short-lived connection per call and one thread serving. Two rows and one notification button are waiting on it | `sk-desktop::bus` | 1, 9 | A wake-up with an owner, not a timer — and the desktop switching to dark mid-session moves the window, which is the cheapest thing to assert over it |
| 13 | **Portal `FileTransfer`**, so a drag or a paste into a sandboxed application arrives. Today it arrives as nothing at all, which is the one interoperability claim in [RELEASE-NOTES.md](../RELEASE-NOTES.md) that is a silent failure rather than a narrow one. Needs rows 11 **and** 12: the transfer is fds, and `TransferClosed` is a signal | `sk-desktop` for the call, `sk-app` for the offer | 5, 9 | A real Flatpak peer receives the files; absent portal is a probe and the existing GNOME and KDE paths still work |
| 14 | **Batch rename**, which [M3](08-roadmap.md#m3--5-wk--operations-and-undo) names and which is the task most likely to send somebody to a terminal | `sk-fs::ops`, `sk-app` | 3 | One gesture is one `Change::Batch`, undone backwards, asserted over a hundred files |
| 15 | **A column model.** Sort by kind, invert a sort, choose the columns, select by pattern. The four columns are a struct in [`layout.rs`](../crates/sk-app/src/layout.rs) and there is no registry of them, so this row is *building the model* and then the picker on top of it — not a menu over an existing list | `sk-model::sort`, `sk-app` | 8 | The radix agrees with the comparison order it accelerates — the assertion that already exists, extended to the new keys |
| 16 | **Search that skips what you did not mean**, and facets by extension. [M4](08-roadmap.md#m4--3-wk--search-and-flattened-views) owes both; a search over a checkout walks `.git` today | `sk-index`, `sk-app` | 8 | First result still inside 50 ms with the rules applied, measured the way [09](09-performance-budgets.md#what-first-search-result-measures-and-what-it-does-not) measures it |
| 17 | **Rebinding where the command is** — [rule 4](03-principles.md#4-discoverable-not-just-configurable) in its own words. [`config::write`](../crates/sk-app/src/config.rs) refuses to overwrite a file that exists, which is [ADR 0025](adr/0025-the-config-file-is-the-keymap-written-out.md)'s rule about who owns it, so a rebind has nowhere to go until that is settled | `sk-cmd`, `sk-app` palette | 4 | A chord bound from the palette survives `kill -9`, and a conflict is refused with the command that holds it named |
| 18 | **Encrypted volumes.** `Encrypted.Unlock` beside the `mount`, `unmount` and `eject` already in [`udisks.rs`](../crates/sk-desktop/src/udisks.rs) — the same one-call shape [ADR 0030](adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md) settled — plus a passphrase field, which is the part that needs a record | `sk-desktop::udisks`, `sk-app` | 9 | Every artefact a failure produces is searched for the passphrase and does not hold it; udisks2 absent is a sentence, as the other three calls already are |
| 19 | **AT-SPI Table, Selection and Action**, and one pass with Orca over a real task — [ADR 0026](adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md)'s own revisit trigger, and the one row here that cannot be closed by a test | [`atspi/tree.rs`](../crates/sk-desktop/src/atspi/tree.rs) | 5 | The `gdbus` walk extended to the three interfaces, plus a recorded manual pass with what Orca actually said |

**Localisation is deliberately not a row.** Every string goes through `t!()` and
the catalogue is keyed, which is the expensive half and it is done. What is
missing is a `LANG` lookup and a catalogue that is not `include_str!` — and that
is two decisions rather than a task: `lookup` returns `&'static str`, so a
runtime catalogue is a load that is never freed, and reading one before the first
frame is an addition to [rule 1](03-principles.md#1-never-block-the-frame)'s
start-up list, which by that rule's own wording arrives with a measurement in the
same commit. It ships as English and says so.

## Two seams these rows share

Both of the largest rows are joins between crates that cannot see each other, and
naming the join now is cheaper than discovering it in a review.
[Dependencies point strictly downward](04-architecture.md#layers), so two level-2
crates are invisible to one another.

**An icon is three level-2 crates and a caller above them.** The lookup is
`sk-desktop`, the pixels come through `sk-preview`'s helper host, and the atlas is
`sk-gpu` — none of which may call another. `sk-app` is the only place that can ask
for a name, receive a decode and hand the result to a slot, which is the same
shape kind detection already has for the same reason.

**A portal transfer is a `sk-plat` offer carrying a `sk-desktop` token.** The drag
and the clipboard belong to level 1; the portal is level 2 and may depend on it,
but the offer is made by the layer underneath. So the policy — *publish a transfer
token beside the file list, and fall back to the list alone where no portal
answers* — lives in `sk-app`, and neither of the two crates that do the work
learns about the other.

## Six decisions that come before their code

[ADRs are immutable](adr/) and one that changes is superseded by a new one
linking back. Each of these is expensive to reverse, and each is written before
the row it belongs to, not after.

| For | What it has to settle |
| --- | --- |
| Row 2 | Where an icon is rasterised, at what sizes, and where the result is kept. [ADR 0034](adr/0034-helper-execution-and-network-rights-are-explicit.md) grants the helper the XDG **thumbnail** cache and nothing else, so icons need either their own granted directory or no cache at all — and writing them into the thumbnail cache would put entries other file managers read into a cache that would misread them. The alternative, re-rasterising a few dozen SVGs at start-up, is a number to measure before choosing. It also settles that the built-in marks are one of the setting's values rather than the absence of the others, so the lookup has a documented answer for *the person asked for no theme* that is not the same as *no theme was found* |
| Row 8 | Where recents live, now that the research is in. [04-architecture.md](04-architecture.md#configuration-and-state) puts bookmarks and recents in `$XDG_STATE_HOME`; interoperating means `recently-used.xbel`. The record has to settle the merge — keyed by the exact URI, timestamped by the later of the two, filtered the way `recent:///` filters — that **our store is authoritative**, and what a removal means when one store has the entry and the other does not, which is a tombstone rather than a union. It also settles that the reader is ours: the grammar is a single-pass scan over one fixed shape, so the budget stays at 27 of 27 |
| Row 11 | Descriptors on the wire, as a successor to [ADR 0023](adr/0023-d-bus-is-a-socket-we-speak-ourselves.md), whose Decision says `NEGOTIATE_UNIX_FD` is never asked for. That premise was factual — no feature passed a descriptor — rather than principled, and the record's framing survives. It must settle what happens when the peer refuses the negotiation, and that a control message rides on a byte, so a partial read can leave descriptors banked ahead of the message that names them |
| Row 12 | A connection that lives, which is [ADR 0023](adr/0023-d-bus-is-a-socket-we-speak-ourselves.md)'s own revisit trigger quoted exactly: a feature that needs to receive signals needs a subscribing connection, and that is where the short-lived shape stops fitting |
| Row 17 | Whether a rebind writes the config or the session. [ADR 0025](adr/0025-the-config-file-is-the-keymap-written-out.md) makes the config the home for keys and says the window does not write it; [ADR 0027](adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md) makes the session the home for what the window changed, and names *a second program writing our config* as its own revisit trigger. Neither answers this, and `config::write` refusing to overwrite is the shape of the unanswered question |
| Row 18 | What a secret field is — one record, not one per surface. A passphrase must be absent from the session file ([0011](adr/0011-one-plain-text-state-file-written-atomically.md)), the state dump a capture writes, the degradation and panic diagnostics ([0037](adr/0037-runtime-degradation-is-visible-and-injectable.md), [0041](adr/0041-panic-diagnostics-use-a-preopened-local-log.md)), and the AT-SPI `Text` interface, which today answers `GetText` for any node in the tree |

### Three things the recents research settled, which the ADR starts from

- **A bad line in `bookmarks` loses a line; a bad byte here loses the file.** GLib
  rejects the whole document on a duplicate `href`, an unknown attribute, a
  foreign metadata block with children, or a timestamp it cannot read — then
  discards its in-memory container, and the next application to add anything
  writes a file holding only its own entry. The only trace is a `g_warning` on a
  stderr nobody reads. That is why the writer, when it ships, **splices one
  contiguous byte range** rather than re-serialising, and why the invariant to
  test is that the output differs from the input in exactly that range.
- **The shared file cannot satisfy [rule 6](03-principles.md#6-state-survives-everything) on its own**, which is what decides the two stores. Other programs are
  licensed to delete what we put there: GTK clamps to 1,000 entries and thirty
  days, KDE to 300 and drops anything under a dotted directory, `clear()` deletes
  the file outright, and GNOME's File History switch empties it. A rule-6 store
  another desktop can empty is not one. `kill -9` was never the threat here.
- **[`places::file_uri`](../crates/sk-desktop/src/places.rs) must not be reused
  unchanged.** Its comment says it deliberately escapes more than
  `g_filename_to_uri` does, and for `bookmarks` that reasoning is sound. Recents
  match on the exact URI string, so a file named `Q&A.txt` would get a second
  bookmark under `%26` and show twice in every GTK Recent view. The row needs an
  encoder that agrees with GLib byte for byte, and a decoder that yields bytes
  rather than today's lossy string.

## R3 — what the release ships with, in writing

These are not work. They are the sentences
[13-release-readiness.md](13-release-readiness.md#publication-checks-and-declared-limits)
already requires, restated here so the plan does not read as though the list will
be empty.

- **Four budget rows missed**, each with its number rather than a widened target:
  cold start at 92.5 ms best against 80, the 1M sort at 166.0 ms against 120, one
  scroll run in five breaching at 973,759 entries with a worst frame of 16.4 ms,
  and a 117.6 MB resident floor against 90, of which 116.3 MB is the driver's.
  [09-performance-budgets.md](09-performance-budgets.md#what-m0-measured) holds
  the method for all four.
- **A performance regression gate does not exist.** [M8](08-roadmap.md#m8--4-wk--hardening-and-10)'s
  first job, and not this release's: `--bench` on a shared runner measures the
  runner's neighbours, so the gate needs a machine before it needs code.
- **Fuzzing covers tar, ZIP and the D-Bus wire.** The markdown, `.desktop`, MIME
  and keymap parsers have adversarial unit tests and no campaign.
- **glibc 2.39**, measured from the artefacts. The 2.28 floor
  [06-portability.md](06-portability.md) targets needs an older build baseline and
  its own runtime testing, and neither has happened.
- **Six measurements are still owed** — an Intel integrated GPU, network mounts, a
  slow device, a second compositor, Dolphin, and NFS — and they are listed in the
  roadmap rather than hidden in a footnote.

## What is left out on purpose

Naming these is the point of the document: a plan whose omissions are invisible
reads as a plan that forgot them.

- **Media metadata, hex, and a docked inspector rather than an overlay.** All
  three are [M5](08-roadmap.md#m5--3-wk--inspector-and-quick-look)'s and none of
  them is why somebody opens a second file manager.
- **7z and rar.** A sentence naming the format is the honest answer, and the line
  [ADR 0032](adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md)
  draws — a container we can parse we parse, a compressor is the machine's —
  survives this release unchanged.
- **Permissions beyond the nine bits.** Row 6 is the basics
  [11-non-goals.md](11-non-goals.md) allows; a recursive apply, setuid and sticky
  toggles, and an ACL or xattr editor are the *permissions editor* that document
  declines. Row 18 is not on that list either: unlocking a volume is the
  one-call shape mounting one already has, and is not partitioning a disk.
- **Everything else in [11-non-goals.md](11-non-goals.md)**, which is where the
  requests most likely to arrive in the first week of a public release already
  have their answers: remote filesystems and `smb://`, cloud providers, MTP,
  desktop icons, an embedded terminal, and a plugin ABI.

## The order, and why it is this one

The roadmap's rule applies unchanged: *a step that makes the next step cheaper
comes first*, and a row that closes a stated gap for nearly nothing comes before
a row that is merely large.

1. **Rows 1, 3 and 4** — the activation file, always-open-with, Empty Trash. Each
   is small and none needs a decision, except row 4's note on the stack it
   invalidates. Row 1 is the cheapest thing on this page and it is the difference
   between answering the desktop and answering it only when already open.
2. **Row 12, then 11, then 13.** The subscribing connection first, because three
   things wait on it and one of them is row 11's own `TransferClosed`; then the
   descriptors; then the portal, which is a caller of both rather than a rewrite
   of either.
3. **Row 2**, starting as early as anything and running beside the above, because
   it is the largest, the most visible, and the one whose failure mode is a frame
   budget rather than a missing feature — which is the kind of problem worth
   finding early.
4. **Rows 5, 6, 9, 10, 14** — the daily tasks. Five rows that share one shape: a
   mutation, a `Change` variant, a registry entry, a scenario. Rows 9 and 10 are
   one piece of work and are done together.
5. **Row 7, then row 8.** The notification is small once its policy is placed.
   Recents is not: the reader, the merge, the tombstones and our own store are
   the size of the tar and ZIP reader rather than the size of a `Change`
   variant, and the ordering above was wrong about it before the research.
6. **Rows 15, 16, 17.** Row 15 comes before row 19, because AT-SPI `Table` needs a
   column model: shipped in the other order, `Table` hardcodes four columns and is
   then rewritten.
7. **Row 18's decision, then row 18, then row 19.** The secret field's record
   comes before the accessibility surface that would otherwise publish it, which
   is the one failure here that cannot be tested for afterwards.

## What each row owes the gates

Nothing here changes what `./tools/check` asks of a change, and three of its
demands are worth restating because every row above touches them:

- **Every command is one entry in one registry**, reachable by keyboard alone and
  by pointer alone. `every_command_is_reachable_and_named` and
  `every_command_can_be_reached_without_a_keyboard` in
  [`commands.rs`](../crates/sk-app/src/commands.rs) already fail the build for a
  row that adds a menu item and no registry entry.
- **Every mutation is one entry on the one stack.** Rows 5, 6 and 14 each add a
  `Change` variant; row 4 deliberately adds none, and says in the panel that it
  cannot be undone.
- **Every string goes through `t!()`**, including the ones in the panels rows 6
  and 18 add.
