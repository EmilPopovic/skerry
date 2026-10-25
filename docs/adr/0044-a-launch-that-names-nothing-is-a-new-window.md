# 0044 — A launch that names nothing is a new window

**Status:** Accepted
**Supersedes:** the sentence of [0038](0038-independent-windows-own-separate-session-slots.md)
that reads "Ordinary invocations retain their existing handover behavior." The
session slots it decided are unchanged.

## Context

[ADR 0023](0023-d-bus-is-a-socket-we-speak-ourselves.md) made a second `skerry` a
request: it finds `dev.skerry.Skerry` taken, calls `ShowFolders` on the owner and
exits before opening a window. With a folder named, that is what a browser's
*Open containing folder* and `xdg-open ~/Downloads` want. With nothing named,
the call carried an empty list, and its only effect was the owner asking its
own window to come forward.

That request is the one Wayland exists to refuse. `xdg_activation_v1` grants a
raise on a token the compositor issued for a user action, and the owner had
none: the token belonged to the launcher click, and it was in the environment of
the process that was about to exit. So clicking Skerry's launcher a second time
did nothing anyone could see. The only way to a second window was `Ctrl+N` from
inside the first, and a person who had not found that chord had no way at all.

The argument for keeping it: one window per launch is what an "application" is
on macOS, and a second window that silently restores a different session slot
is a surprise. It lost, for two reasons. First, the behaviour it protected was
invisible on the display server this project treats as primary, so it protected
nothing. Second, every file manager a Linux user arrives from — Nautilus,
Dolphin, Thunar, Nemo — opens a window for a bare second launch, and a launcher
that looks broken is worse than one that opens a window.

The retry already in `take_the_name` constrains the design. A relaunch a second
after a quit finds the name still registered by a process that is closing its
socket, measured one restart in four on the development machine. A second launch
that opened an independent window on that answer would put somebody's main
session in a `session-window-N` slot every fourth restart.

## Decision

In `sk_app::service::take_the_name`, when `RequestName` answers that the name is
taken:

- **A path was named:** hand it over with `ShowFolders` and exit, as before.
- **Nothing was named:** call `org.freedesktop.DBus.Introspectable.Introspect` on
  the owner. If it answers, return `Standing::Another`, keeping the connection.
  `bin/skerry` then starts exactly as `--new-window` does, taking no global
  names and claiming the first free `session-window-N` slot under 0038.
- **The owner did not answer** (either case): ask for the name again, as before.
  A dying owner gives way to a process that becomes the first instance and
  restores the main session.

`packaging/dev.skerry.Skerry.desktop` gains a `new-window` Desktop Action with
`Exec=skerry --new-window`, so a dock's secondary click offers New Window.

## Consequences

- A bare second launch is an `Introspect` round trip plus a cold start, about
  90 ms, where it used to be a `ShowFolders` round trip and an exit. The
  scenario `a-second-skerry-hands-its-folder-over-and-exits` names a folder and
  still holds.
- A second window's state is its own slot. Its tabs come back the next time a
  bare launch finds the first window running, and not when the first window is
  gone. That is 0038's arrangement, now reachable from the launcher.
- Undo stacks and pending operations stay per process, as 0038 says. Two
  windows from two launches are two queues.
- **Not yet paid:** the window does not read `XDG_ACTIVATION_TOKEN` from its
  environment. A compositor that holds a newly mapped toplevel back without a
  token maps the second window unfocused. Hyprland focuses it anyway, which is
  the only compositor the scenarios run on.

## Revisit trigger

If a scenario shows a bare relaunch within 2 s of a quit landing in a
`session-window-N` slot, the `Introspect` probe is not a sound liveness test
and this needs a different one. If the activation-token gap above is closed,
nothing here changes.
