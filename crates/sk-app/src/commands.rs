// SPDX-License-Identifier: Apache-2.0
//! Every command the program has, registered once.
//!
//! Rule 2: the keyboard overlay, and later the palette, the menus and the config
//! schema, are all views of [`registry`]'s output. A command added here is
//! reachable by all of them; a command added anywhere else is reachable by none,
//! which is what `every_command_is_reachable_and_named` fails the build over.

use sk_cmd::{CommandSpec, Group, Registry};
use sk_model::SortColumn;

use crate::app::App;
use crate::layout::Tool;

/// Builds the registry.
///
/// A free function rather than a field on [`App`], so that running a command can
/// take `&mut App` without the registry being part of what is borrowed.
///
/// One function per group rather than one long table, because that is the shape
/// [docs/04-architecture.md](../../../docs/04-architecture.md#extension-points-how-features-are-added-without-editing-shared-files)
/// wants features to arrive in: a module registers its own commands and edits no
/// shared `match`.
#[must_use]
pub fn registry() -> Registry<App> {
    let mut registry = Registry::new();
    navigate(&mut registry);
    finding(&mut registry);
    panes(&mut registry);
    select(&mut registry);
    directions(&mut registry);
    view(&mut registry);
    places(&mut registry);
    colours(&mut registry);
    grouping(&mut registry);
    zoom(&mut registry);
    file(&mut registry);
    edit(&mut registry);
    window(&mut registry);
    help(&mut registry);
    field(&mut registry);
    registry
}

/// The six colours a folder can be given, and the one that takes it away.
///
/// **One command each**, which is [rule 2](../../../docs/03-principles.md): the
/// four groupings are four commands for the same reason, and it is what puts
/// every colour in the palette and on the menu with its chord beside it. Seven
/// rows is more than a submenu would take and a submenu is a thing this program
/// does not have — and the menu draws each row's swatch, so the list reads as a
/// palette rather than as seven words.
fn colours(registry: &mut Registry<App>) {
    for (index, colour) in crate::colours::Colour::ALL.into_iter().enumerate() {
        // `Ctrl+Alt+1` to `6`, in the order they are offered. A digit family for
        // a small numbered set is what `Ctrl+1..3` already does for the view
        // modes and `Alt+1..3` for the sorts — and **not `Ctrl+Shift+<digit>`**,
        // because shift is part of a digit's identity (`Chord::shift_matters`),
        // so `Ctrl+Shift+1` *is* `Ctrl+1` and would have quietly taken the
        // details view's chord. The registry's conflict test said so.
        let keys: &'static [&'static str] = match index {
            0 => &["ctrl+alt+1"],
            1 => &["ctrl+alt+2"],
            2 => &["ctrl+alt+3"],
            3 => &["ctrl+alt+4"],
            4 => &["ctrl+alt+5"],
            _ => &["ctrl+alt+6"],
        };
        registry.add(CommandSpec {
            id: colour.command(),
            name: colour.name(),
            group: Group::View,
            keys,
            enabled: App::can_colour,
            run: match colour {
                crate::colours::Colour::Red => |app: &mut App| {
                    app.colour_folder(Some(crate::colours::Colour::Red));
                },
                crate::colours::Colour::Orange => |app: &mut App| {
                    app.colour_folder(Some(crate::colours::Colour::Orange));
                },
                crate::colours::Colour::Yellow => |app: &mut App| {
                    app.colour_folder(Some(crate::colours::Colour::Yellow));
                },
                crate::colours::Colour::Green => |app: &mut App| {
                    app.colour_folder(Some(crate::colours::Colour::Green));
                },
                crate::colours::Colour::Blue => |app: &mut App| {
                    app.colour_folder(Some(crate::colours::Colour::Blue));
                },
                crate::colours::Colour::Purple => |app: &mut App| {
                    app.colour_folder(Some(crate::colours::Colour::Purple));
                },
            },
        });
    }
    registry.add(CommandSpec {
        id: crate::colours::NONE,
        name: "action.colour_none",
        group: Group::View,
        keys: &["ctrl+alt+0"],
        // Dimmed on a folder with no colour, so the row says whether there is
        // anything to take away.
        enabled: App::can_uncolour,
        run: |app: &mut App| app.colour_folder(None),
    });
}

/// The list of places: what is on it, and in what order.
///
/// Four commands and one target. [ADR
/// 0021](../../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md) is
/// that a pin is a line in GTK's bookmarks file and is [rule
/// 6](../../../docs/03-principles.md#6-state-survives-everything) state rather
/// than [rule 3](../../../docs/03-principles.md#3-everything-is-reversible)
/// state: it is written durably the moment it is made and it is not on the undo
/// stack, because rule 6 names bookmarks and its failure condition is `kill -9`
/// losing one, while rule 3's list is the file mutations. The way back is Unpin,
/// one row away in the same menu.
fn places(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "place.pin",
            name: "action.pin",
            group: Group::Navigate,
            // **`B` for the file this writes**, which is GTK's `bookmarks`, and
            // not the `Ctrl+D` Nautilus and every browser use — that one is
            // Duplicate here, which is a file operation and the more frequent of
            // the two. The inverse is the same letter with `Shift`, which is the
            // shape `Ctrl+Z` and `Ctrl+Shift+Z` already set.
            keys: &["ctrl+b"],
            enabled: App::can_pin,
            run: App::pin_place,
        },
        CommandSpec {
            id: "place.expand",
            name: "action.place_expand",
            group: Group::Navigate,
            // `E` for expand, in the `Ctrl+Shift` family the other three place
            // commands are in. The pointer's way is the twisty, which is the
            // control every tree on the desktop has.
            keys: &["ctrl+shift+e"],
            enabled: App::can_expand_place,
            run: App::expand_place,
        },
        CommandSpec {
            id: "place.unpin",
            name: "action.unpin",
            group: Group::Navigate,
            keys: &["ctrl+shift+b"],
            enabled: App::can_unpin,
            run: App::unpin_place,
        },
        CommandSpec {
            id: "place.raise",
            name: "action.place_up",
            group: Group::Navigate,
            // The pointer's way is dragging the row, which is what a list of
            // eleven things asks for; these two are the keyboard's half of it,
            // which is rule 5.
            keys: &["ctrl+shift+up"],
            enabled: App::can_raise_place,
            run: |app: &mut App| app.move_place(false),
        },
        CommandSpec {
            id: "place.lower",
            name: "action.place_down",
            group: Group::Navigate,
            keys: &["ctrl+shift+down"],
            enabled: App::can_lower_place,
            run: |app: &mut App| app.move_place(true),
        },
        CommandSpec {
            id: "volume.mount",
            name: "action.mount",
            group: Group::Navigate,
            // **What the chord acts on is the row a menu was opened over, else
            // the volume the pane is standing in** — see
            // `App::volume_in_question`. That fallback is what makes a chord
            // meaningful rather than a guess: a person browsing a USB stick and
            // pressing Eject means *this* stick, which is the only volume the
            // path they are looking at is on.
            keys: &["ctrl+alt+m"],
            enabled: App::can_mount,
            run: App::mount_volume,
        },
        CommandSpec {
            id: "volume.unmount",
            name: "action.unmount",
            group: Group::Navigate,
            keys: &["ctrl+alt+u"],
            enabled: App::can_unmount,
            run: App::unmount_volume,
        },
        CommandSpec {
            id: "volume.eject",
            name: "action.eject",
            group: Group::Navigate,
            keys: &["ctrl+alt+e"],
            enabled: App::can_unmount,
            run: App::eject_volume,
        },
    ] {
        registry.add(spec);
    }
}

/// Finding things: the index, and the two scopes of the search.
///
/// Its own group of registrations rather than more of `navigate`, which was at
/// the hundred-line limit — and they are one subject: a search's scope decides
/// which indexes it can use, and [ADR
/// 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)
/// is the record that ties them together.
fn finding(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "index.build",
            name: "action.index_build",
            group: Group::Navigate,
            // The pair sits on one letter on purpose: `Shift` builds the index
            // and `Alt` removes it, so finding either teaches the other. `Ctrl+I`
            // is already Properties, which is why neither is the bare chord.
            keys: &["ctrl+shift+i"],
            enabled: |app: &App| !app.indexing(),
            run: App::build_index,
        },
        CommandSpec {
            id: "index.build_all",
            name: "action.index_build_all",
            group: Group::Navigate,
            // **`Shift` on the same letter as the pair above**, so the three sit
            // together in the palette and finding one teaches the others.
            keys: &["ctrl+alt+shift+i"],
            enabled: |app: &App| !app.indexing() && app.has_volumes(),
            run: App::build_every_index,
        },
        CommandSpec {
            id: "index.forget",
            name: "action.index_forget",
            group: Group::Navigate,
            keys: &["ctrl+alt+i"],
            // Dimmed rather than hidden where there is nothing to delete, so the
            // menu is the same shape every time it opens — and so that a person
            // looking for "where is my index" finds the row that removes it.
            enabled: |app: &App| app.indexed() || app.indexing(),
            run: App::forget_index,
        },
        CommandSpec {
            id: "go.search_everywhere",
            name: "action.search_everywhere",
            group: Group::Navigate,
            // The same letter, one modifier out: `Ctrl+Shift+F` is the scope a
            // person asks for most and this is the one they widen to. `Alt`
            // rather than a different letter, because the two are one setting
            // with two values and the panel's own switcher says so.
            keys: &["ctrl+alt+f"],
            enabled: |_| true,
            run: App::search_everywhere,
        },
        CommandSpec {
            id: "go.search",
            name: "action.search",
            group: Group::Navigate,
            // `Ctrl+Shift+F` is search-in-project everywhere a person has met
            // one, and `Ctrl+F` is already the find popup over the listing on
            // screen — which is the same relationship those two keys have in
            // every editor: narrow what is here, or go looking.
            keys: &["ctrl+shift+f"],
            enabled: |_| true,
            run: App::open_search,
        },
    ] {
        registry.add(spec);
    }
}

/// Moving around: open, up, back, forward, home, reload.
fn navigate(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "go.open",
            name: "action.open",
            group: Group::Navigate,
            keys: &["enter"],
            enabled: |app: &App| app.panes.focused().focused().is_some(),
            run: App::open_focused,
        },
        CommandSpec {
            id: "go.open_new_tab",
            name: "action.open_new_tab",
            group: Group::Navigate,
            // `Ctrl Enter` because it is Open with a modifier, which is the
            // shape a browser gave every "…but in a tab" ten years ago. Not the
            // middle click that does this in Nautilus: that button is owed to
            // splitting, and one of the two has to be second.
            keys: &["ctrl+enter"],
            enabled: App::focused_is_a_directory,
            run: App::open_in_new_tab,
        },
        CommandSpec {
            id: "go.open_beside",
            name: "action.open_beside",
            group: Group::Navigate,
            // The family reads as one: `Enter` opens, and the modifier says
            // where. `Ctrl` is the tab, `Shift` is the pane, and both together
            // is the other axis — the same pairing `F3` and `Shift F3` already
            // use for a split of the directory already showing.
            keys: &["shift+enter"],
            enabled: App::focused_is_a_directory,
            run: App::open_beside,
        },
        CommandSpec {
            id: "go.open_below",
            name: "action.open_below",
            group: Group::Navigate,
            keys: &["ctrl+shift+enter"],
            enabled: App::focused_is_a_directory,
            run: App::open_below,
        },
        CommandSpec {
            id: "go.parent",
            name: "action.parent",
            group: Group::Navigate,
            // Both, because both are somebody's muscle memory: `Backspace` from
            // every file manager, `Alt ↑` from every browser and from GTK.
            keys: &["backspace", "alt+up"],
            enabled: |app: &App| app.panes.focused().allows(Tool::Parent),
            run: App::go_up,
        },
        CommandSpec {
            id: "go.back",
            name: "action.back",
            group: Group::Navigate,
            keys: &["alt+left"],
            enabled: |app: &App| app.panes.focused().allows(Tool::Back),
            run: App::go_back,
        },
        CommandSpec {
            id: "go.forward",
            name: "action.forward",
            group: Group::Navigate,
            keys: &["alt+right"],
            enabled: |app: &App| app.panes.focused().allows(Tool::Forward),
            run: App::go_forward,
        },
        CommandSpec {
            id: "go.home",
            name: "action.home",
            group: Group::Navigate,
            keys: &["alt+home"],
            enabled: |_| std::env::var_os("HOME").is_some(),
            run: App::go_home,
        },
        CommandSpec {
            id: "go.reload",
            name: "action.reload",
            group: Group::Navigate,
            keys: &["f5"],
            enabled: |app: &App| app.panes.focused().allows(Tool::Reload),
            run: App::reload,
        },
    ] {
        registry.add(spec);
    }
}

/// Choosing things, with the keyboard.
fn select(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "select.up",
            name: "action.cursor_up",
            group: Group::Select,
            keys: &["up"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Up, false),
        },
        CommandSpec {
            id: "select.down",
            name: "action.cursor_down",
            group: Group::Select,
            keys: &["down"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Down, false),
        },
        CommandSpec {
            id: "select.extend_up",
            name: "action.extend_up",
            group: Group::Select,
            keys: &["shift+up"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Up, true),
        },
        CommandSpec {
            id: "select.extend_down",
            name: "action.extend_down",
            group: Group::Select,
            keys: &["shift+down"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Down, true),
        },
        CommandSpec {
            id: "select.page_up",
            name: "action.page_up",
            group: Group::Select,
            keys: &["page_up"],
            enabled: |_| true,
            run: |app: &mut App| app.move_page(-1),
        },
        CommandSpec {
            id: "select.page_down",
            name: "action.page_down",
            group: Group::Select,
            keys: &["page_down"],
            enabled: |_| true,
            run: |app: &mut App| app.move_page(1),
        },
        CommandSpec {
            id: "select.top",
            name: "action.top",
            group: Group::Select,
            keys: &["home"],
            enabled: |_| true,
            run: App::go_top,
        },
        CommandSpec {
            id: "select.bottom",
            name: "action.bottom",
            group: Group::Select,
            keys: &["end"],
            enabled: |_| true,
            run: App::go_bottom,
        },
        CommandSpec {
            id: "select.all",
            name: "action.select_all",
            group: Group::Select,
            keys: &["ctrl+a"],
            enabled: |app: &App| app.panes.focused().listing.rows() > 0,
            run: |app: &mut App| {
                // **What is shown, not what the order holds.** A collapsed band's
                // rows are in the order and on no part of the screen, and
                // `Selection::select_all` is the comment that says why that
                // matters: a selection containing something invisible is a
                // selection a later operation acts on by surprise.
                let rows = app.panes.focused().shown_ids();
                app.panes.focused_mut().selection.select_all(&rows);
            },
        },
        CommandSpec {
            id: "select.toggle",
            name: "action.select_toggle",
            group: Group::Select,
            // **Not a bare `Space`**, which is a character a name field takes and
            // a type-ahead filter takes too. `Ctrl+Space` is what every list that
            // has this already uses, and `shift_matters` is false for a space, so
            // the binding covers the shifted one rather than leaving it silent.
            keys: &["ctrl+space"],
            enabled: |app: &App| app.panes.focused().selection.cursor().is_some(),
            run: |app: &mut App| {
                let Some(id) = app.panes.focused().selection.cursor() else { return };
                app.panes.focused_mut().selection.toggle(id);
            },
        },
        CommandSpec {
            id: "select.none",
            name: "action.clear_selection",
            group: Group::Select,
            keys: &["escape"],
            enabled: |_| true,
            run: App::clear,
        },
    ] {
        registry.add(spec);
    }
}

/// The four direction keys, which are one idea and are the reason `select` was
/// two functions.
///
/// **All four in both modes.** A one-column grid makes Left and Right previous
/// and next, which is what they already mean in a list, so nothing here is
/// bound per mode — see [ADR
/// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md).
fn directions(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "select.left",
            name: "action.cursor_left",
            group: Group::Select,
            // **Bound in both modes, and in Details it is Up.** A one-column
            // grid makes Left and Right previous and next, which is what they
            // already mean in a list — so there is no mode-dependent binding and
            // no key that does nothing in one view.
            keys: &["left"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Left, false),
        },
        CommandSpec {
            id: "select.right",
            name: "action.cursor_right",
            group: Group::Select,
            keys: &["right"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Right, false),
        },
        CommandSpec {
            id: "select.extend_left",
            name: "action.extend_left",
            group: Group::Select,
            keys: &["shift+left"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Left, true),
        },
        CommandSpec {
            id: "select.extend_right",
            name: "action.extend_right",
            group: Group::Select,
            keys: &["shift+right"],
            enabled: |_| true,
            run: |app: &mut App| app.step_cursor(crate::cells::Step::Right, true),
        },
    ] {
        registry.add(spec);
    }
}

/// Making, naming and removing things — and taking any of it back.
fn edit(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "file.new_folder",
            name: "action.new_folder",
            group: Group::File,
            keys: &["ctrl+shift+n"],
            enabled: |app: &App| !app.busy(),
            run: App::new_folder,
        },
        CommandSpec {
            id: "file.rename",
            name: "action.rename",
            group: Group::File,
            // F2 in every file manager on every desktop, which is why the frame
            // statistics moved off it.
            keys: &["f2"],
            enabled: |app: &App| app.panes.focused().focused().is_some() && !app.busy(),
            run: App::start_rename,
        },
        CommandSpec {
            id: "file.open_with",
            name: "action.open_with",
            group: Group::File,
            keys: &["ctrl+shift+o"],
            enabled: |app: &App| app.panes.focused().focused().is_some(),
            run: App::open_with,
        },
        CommandSpec {
            id: "file.properties",
            name: "action.properties",
            group: Group::File,
            // `Ctrl+I` from GNOME and macOS; `Alt+Enter` from Explorer and
            // Dolphin. Both, because both are somebody's muscle memory.
            keys: &["ctrl+i", "alt+enter"],
            enabled: |_| true,
            run: App::toggle_properties,
        },
        CommandSpec {
            id: "file.terminal",
            name: "action.terminal",
            group: Group::File,
            // What Nautilus, Dolphin and Thunar all use for the same thing.
            keys: &["ctrl+shift+`", "f4"],
            enabled: |_| true,
            run: App::open_terminal,
        },
        CommandSpec {
            id: "file.delete",
            name: "action.delete",
            group: Group::File,
            // What every file manager binds a permanent delete to, and the
            // second press is what makes it safe to have on a key at all.
            keys: &["shift+delete"],
            enabled: |app: &App| app.panes.focused().focused().is_some() && !app.busy(),
            run: App::delete_selection,
        },
        CommandSpec {
            id: "file.duplicate",
            name: "action.duplicate",
            group: Group::File,
            // What every file manager with a Duplicate binds it to.
            keys: &["ctrl+d"],
            enabled: |app: &App| app.panes.focused().focused().is_some() && !app.busy(),
            run: App::duplicate_selection,
        },
        CommandSpec {
            id: "file.trash",
            name: "action.trash",
            group: Group::File,
            keys: &["delete"],
            enabled: |app: &App| app.panes.focused().focused().is_some() && !app.busy(),
            run: App::trash_selection,
        },
        CommandSpec {
            id: "edit.undo",
            name: "action.undo",
            group: Group::Edit,
            keys: &["ctrl+z"],
            enabled: |app: &App| app.can_undo(),
            run: App::undo,
        },
        CommandSpec {
            id: "edit.redo",
            name: "action.redo",
            group: Group::Edit,
            keys: &["ctrl+shift+z", "ctrl+y"],
            enabled: |app: &App| app.can_redo(),
            run: App::redo,
        },
    ] {
        registry.add(spec);
    }
}

/// The window itself.
///
/// **Rule 2**: every action is one entry in one registry. These three are the
/// title bar on a desktop that draws no decorations, and a title bar whose
/// buttons the keyboard overlay cannot list and the palette cannot find is three
/// actions that exist only for a pointer. Their `enabled` reads the same
/// `wm_capabilities` answer the layout uses to decide whether to draw them.
fn window(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "window.minimize",
            name: "action.minimize",
            group: Group::Window,
            // The chords GNOME and KDE both use for these, so a person who has
            // met either already knows them.
            keys: &["super+h"],
            enabled: |app: &App| app.window_state().can_minimize,
            run: App::minimize_window,
        },
        CommandSpec {
            id: "window.maximize",
            name: "action.maximize",
            group: Group::Window,
            keys: &["super+up"],
            enabled: |app: &App| app.window_state().can_maximize,
            run: App::toggle_maximized,
        },
        CommandSpec {
            id: "window.close",
            name: "action.close",
            group: Group::Window,
            // Not `Ctrl+W`: that is `tab.close`, which closes the window when
            // the last tab goes.
            keys: &["ctrl+q"],
            enabled: |_| true,
            run: App::close_window,
        },
    ] {
        registry.add(spec);
    }
}

/// Tabs and splits: how many panes there are and which has the keyboard.
fn panes(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "tab.new",
            name: "action.new_tab",
            group: Group::Pane,
            keys: &["ctrl+t"],
            enabled: |_| true,
            run: App::new_tab,
        },
        CommandSpec {
            id: "tab.close",
            name: "action.close_tab",
            group: Group::Pane,
            // `Ctrl+W` closes the tab, and the window when it is the last one —
            // which is what every tabbed application does and what a person who
            // has used one expects. `window.close` keeps `Ctrl+Q`.
            keys: &["ctrl+w"],
            enabled: |_| true,
            run: App::close_tab,
        },
        CommandSpec {
            id: "tab.reopen",
            name: "action.reopen_tab",
            group: Group::Pane,
            keys: &["ctrl+shift+t"],
            enabled: |app: &App| app.panes.can_reopen(),
            run: App::reopen_tab,
        },
        CommandSpec {
            id: "tab.next",
            name: "action.next_tab",
            group: Group::Pane,
            keys: &["ctrl+tab", "ctrl+page_down"],
            enabled: |app: &App| app.panes.focused_leaf().panes.len() > 1,
            run: App::next_tab,
        },
        CommandSpec {
            id: "tab.previous",
            name: "action.previous_tab",
            group: Group::Pane,
            keys: &["ctrl+shift+tab", "ctrl+page_up"],
            enabled: |app: &App| app.panes.focused_leaf().panes.len() > 1,
            run: App::previous_tab,
        },
        CommandSpec {
            id: "pane.split_beside",
            name: "action.split_beside",
            group: Group::Pane,
            // Dolphin's key for the same thing, which is the one a Linux file
            // manager's users already have in their hands.
            keys: &["f3"],
            enabled: |_| true,
            run: App::split_beside,
        },
        CommandSpec {
            id: "pane.split_below",
            name: "action.split_below",
            group: Group::Pane,
            keys: &["shift+f3"],
            enabled: |_| true,
            run: App::split_below,
        },
        CommandSpec {
            id: "pane.close",
            name: "action.close_pane",
            group: Group::Pane,
            keys: &["ctrl+f3"],
            enabled: |app: &App| app.panes.leaf_count() > 1,
            run: App::close_pane,
        },
        CommandSpec {
            id: "pane.next",
            name: "action.next_pane",
            group: Group::Pane,
            // What GTK and Qt both cycle panes with, everywhere else on the
            // desktop.
            keys: &["f6"],
            enabled: |app: &App| app.panes.leaf_count() > 1,
            run: App::next_pane,
        },
    ] {
        registry.add(spec);
    }
}

/// How big the rows are.
///
/// Its own function rather than three more rows in `view`, because a zoom is a
/// question about the list's size and not about what it shows — and because the
/// pointer, the touchpad and the keyboard all arrive at these three.
fn zoom(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "view.zoom_in",
            name: "action.zoom_in",
            group: Group::View,
            // Both, because a keyboard without a numeric pad has `Ctrl +` on a
            // shifted key and every browser accepts `Ctrl =` for it.
            keys: &["ctrl+=", "ctrl+plus"],
            enabled: |app: &App| app.can_zoom(1),
            run: App::zoom_in,
        },
        CommandSpec {
            id: "view.zoom_out",
            name: "action.zoom_out",
            group: Group::View,
            keys: &["ctrl+minus"],
            enabled: |app: &App| app.can_zoom(-1),
            run: App::zoom_out,
        },
        CommandSpec {
            id: "view.zoom_reset",
            name: "action.zoom_reset",
            group: Group::View,
            keys: &["ctrl+0"],
            enabled: |app: &App| app.zoomed(),
            run: App::zoom_reset,
        },
    ] {
        registry.add(spec);
    }
}

/// What the listing shows and how it is ordered.
fn view(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "view.filter",
            name: "action.filter",
            group: Group::View,
            // `/` as well as `Ctrl+F`, which is what every list a keyboard user
            // has met does, and which costs nothing: a filename cannot contain
            // a slash, so no type-ahead is lost to it.
            keys: &["ctrl+f", "/"],
            enabled: |app: &App| !app.panes.focused().listing.is_empty(),
            run: App::toggle_filter,
        },
        CommandSpec {
            id: "view.path_entry",
            name: "action.path_entry",
            group: Group::Navigate,
            // `Ctrl+L` from every browser and from GTK's own places bar, and
            // `Alt+D` from Explorer, because both are somebody's muscle memory.
            keys: &["ctrl+l", "alt+d"],
            enabled: |_| true,
            run: App::toggle_path_entry,
        },
        CommandSpec {
            id: "view.palette",
            name: "action.palette",
            group: Group::Help,
            // `Ctrl+Shift+P` from VS Code and `Ctrl+P` from everything that has
            // a palette at all. Both, because the first is the one people who
            // know palettes reach for and the second is shorter.
            keys: &["ctrl+shift+p", "ctrl+p"],
            enabled: |_| true,
            run: App::toggle_palette,
        },
        CommandSpec {
            id: "view.menu",
            name: "action.menu",
            group: Group::View,
            // The key on a keyboard that has one, and the chord for the
            // keyboards that do not — which is every laptop made this decade.
            keys: &["menu", "shift+f10"],
            enabled: |_| true,
            run: App::toggle_menu,
        },
        CommandSpec {
            id: "view.sidebar",
            name: "action.sidebar",
            group: Group::View,
            // What Files, Nautilus and Nemo all use for the same pane.
            keys: &["f9"],
            enabled: |_| true,
            run: App::toggle_sidebar,
        },
        CommandSpec {
            id: "view.details",
            name: "action.view_details",
            group: Group::View,
            // `Ctrl+1` and `Ctrl+2` are what every file manager numbers its views
            // with, and the numbers stay stable as more arrive.
            keys: &["ctrl+1"],
            enabled: |_| true,
            run: |app: &mut App| app.set_mode(crate::cells::Mode::Details),
        },
        CommandSpec {
            id: "view.grid",
            name: "action.view_grid",
            group: Group::View,
            keys: &["ctrl+2"],
            enabled: |_| true,
            run: |app: &mut App| app.set_mode(crate::cells::Mode::Grid),
        },
        CommandSpec {
            id: "view.hidden",
            name: "action.toggle_hidden",
            group: Group::View,
            keys: &["ctrl+h"],
            enabled: |_| true,
            run: App::toggle_hidden,
        },
        CommandSpec {
            id: "view.theme",
            name: "action.toggle_theme",
            group: Group::View,
            // Not `Ctrl+T`, which is a new tab everywhere a person has met one.
            keys: &["ctrl+shift+d"],
            enabled: |_| true,
            run: App::toggle_theme,
        },
        CommandSpec {
            id: "view.sort_name",
            name: "action.sort_name",
            group: Group::View,
            keys: &["alt+1"],
            enabled: |_| true,
            run: |app: &mut App| app.sort_by(SortColumn::Name),
        },
        CommandSpec {
            id: "view.sort_size",
            name: "action.sort_size",
            group: Group::View,
            keys: &["alt+2"],
            enabled: |_| true,
            run: |app: &mut App| app.sort_by(SortColumn::Size),
        },
        CommandSpec {
            id: "view.sort_modified",
            name: "action.sort_modified",
            group: Group::View,
            keys: &["alt+3"],
            enabled: |_| true,
            run: |app: &mut App| app.sort_by(SortColumn::Modified),
        },
    ] {
        registry.add(spec);
    }
}

/// The grouping, which is its own axis and is why `view` was two functions.
///
/// Orthogonal to the geometry the two above pick: [ADR
/// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md)
/// puts grouping over the cell geometry rather than inside it, and these two
/// commands are that seam at the registry.
fn grouping(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "view.columns",
            name: "action.view_columns",
            group: Group::View,
            keys: &["ctrl+3"],
            enabled: |_| true,
            run: |app: &mut App| app.set_mode(crate::cells::Mode::Columns),
        },
        CommandSpec {
            id: "view.quick_look",
            name: "action.quick_look",
            group: Group::View,
            // **A bare `Space`, which is free.** `keys::type_ahead` refuses
            // whitespace — a filename search that begins with a space is not a
            // thing anybody does — so this takes no key that meant something, and
            // it is what every quick look on every desktop is bound to.
            keys: &["space"],
            enabled: |app: &App| app.panes.focused().selection.cursor().is_some(),
            run: App::toggle_quick_look,
        },
        CommandSpec {
            id: "view.band_toggle",
            name: "action.band_toggle",
            group: Group::View,
            // `G` for group, beside the four groupings on the digits. Shift is part
            // of a letter's identity — `Chord::shift_matters` — so this and the one
            // below are two chords rather than one that answers to both.
            keys: &["ctrl+g"],
            enabled: |app: &App| {
                app.panes.focused().grouping != crate::groups::Grouping::None
                    && app.panes.focused().cursor_row().is_some()
            },
            run: |app: &mut App| {
                let Some(row) = app.panes.focused().cursor_row() else { return };
                let Some(band) = app.panes.focused().band_of_row(row) else { return };
                app.toggle_band(band);
            },
        },
        CommandSpec {
            id: "view.bands_show",
            name: "action.bands_show",
            group: Group::View,
            keys: &["ctrl+shift+g"],
            enabled: |app: &App| app.panes.focused().closed.any(),
            run: App::show_every_band,
        },
        CommandSpec {
            id: "view.group_none",
            name: "action.group_none",
            group: Group::View,
            // **Not `Ctrl+0`**, which is Reset zoom here as it is in every
            // browser — the registry's own conflict check said so. The grouping
            // is a second axis to the `Ctrl+1` / `Ctrl+2` that pick a geometry,
            // so it takes the far end of the same row of digits: four groupings,
            // the four digits at that end, in the order the View menu lists them.
            keys: &["ctrl+6"],
            enabled: |_| true,
            run: |app: &mut App| app.set_grouping(crate::groups::Grouping::None),
        },
        CommandSpec {
            id: "view.group_time",
            name: "action.group_time",
            group: Group::View,
            keys: &["ctrl+7"],
            enabled: |_| true,
            run: |app: &mut App| app.set_grouping(crate::groups::Grouping::Time),
        },
        CommandSpec {
            id: "view.group_size",
            name: "action.group_size",
            group: Group::View,
            keys: &["ctrl+8"],
            enabled: |_| true,
            run: |app: &mut App| app.set_grouping(crate::groups::Grouping::Size),
        },
        CommandSpec {
            id: "view.group_kind",
            name: "action.group_kind",
            group: Group::View,
            keys: &["ctrl+9"],
            enabled: |_| true,
            run: |app: &mut App| app.set_grouping(crate::groups::Grouping::Kind),
        },
        // **Not `Ctrl+Shift+0`, which is `Ctrl+0`.** Shift is not part of a
        // digit's identity — `Chord::shift_matters`, and it is right, because
        // Shift changes which character a digit key produces from one layout to
        // the next. So that binding was the zoom's, silently, and the registry's
        // conflict check said nothing until it started comparing chords by what
        // they match. `Ctrl+Shift+R` for reset, and a letter's Shift *is* its own.
        CommandSpec {
            id: "view.columns_reset",
            name: "action.columns_reset",
            group: Group::View,
            keys: &["ctrl+shift+r"],
            enabled: |_| true,
            run: App::reset_columns,
        },
    ] {
        registry.add(spec);
    }
}

/// The clipboard.
fn file(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "file.copy",
            name: "action.copy",
            group: Group::File,
            keys: &["ctrl+c"],
            enabled: |app: &App| app.panes.focused().listing.rows() > 0,
            run: App::copy_selection,
        },
        CommandSpec {
            id: "file.copy_path",
            name: "action.copy_path",
            group: Group::File,
            keys: &["ctrl+shift+c"],
            enabled: |app: &App| app.panes.focused().listing.rows() > 0,
            run: App::copy_path,
        },
        CommandSpec {
            id: "file.copy_content",
            name: "action.copy_content",
            group: Group::File,
            // The third of the copy family, after the files themselves and the
            // path: `Ctrl+C`, `Ctrl+Shift+C`, `Ctrl+Alt+C`. No file manager has
            // this action, so there is no muscle memory to respect — only the
            // family to stay inside.
            keys: &["ctrl+alt+c"],
            enabled: App::focused_is_text,
            run: App::copy_content,
        },
        CommandSpec {
            id: "edit.history",
            name: "action.history",
            group: Group::Edit,
            // `H` for history, and `Ctrl+H` is Show hidden files — so this is the
            // shifted one, which for a letter is a chord of its own.
            keys: &["ctrl+shift+h"],
            enabled: |app: &App| app.has_history(),
            run: App::toggle_history,
        },
        CommandSpec {
            id: "edit.pause",
            name: "action.pause",
            group: Group::Edit,
            // The one key whose name is what it does — and most laptops have no
            // Pause key at all, which is why this is also on the background menu
            // and in the palette. A chord over a letter would be arbitrary and
            // would be the first thing a rebindable keymap has to fight over.
            keys: &["pause"],
            enabled: App::can_pause,
            run: App::toggle_pause,
        },
        CommandSpec {
            id: "edit.resume",
            name: "action.resume",
            group: Group::Edit,
            // `Ctrl+Shift+Y` beside the undo family: it is the one command that
            // says "carry on with what you were doing", and `Ctrl+Y` is already
            // Redo's second binding.
            keys: &["ctrl+shift+y"],
            enabled: App::can_resume,
            run: App::resume_operation,
        },
        CommandSpec {
            id: "edit.cancel",
            name: "action.cancel",
            group: Group::Edit,
            // No file manager binds this, because every one of them puts a Cancel
            // button in a progress window. There is no progress window here — the
            // count is in the status line — so the chord is the affordance, and
            // it is also on the menus and in the palette.
            keys: &["ctrl+."],
            enabled: App::can_cancel,
            run: App::cancel_operation,
        },
        CommandSpec {
            id: "file.cut",
            name: "action.cut",
            group: Group::File,
            keys: &["ctrl+x"],
            enabled: |app: &App| app.panes.focused().focused().is_some(),
            run: App::cut_selection,
        },
        CommandSpec {
            id: "file.paste",
            name: "action.paste",
            group: Group::File,
            // `Ctrl+V` is *this*, and it used to be Go to the copied folder —
            // which was the pulled-forward stand-in for a paste that could not
            // paste files yet. Now that one can, the universal chord belongs to
            // it and the stand-in moves down one.
            keys: &["ctrl+v"],
            enabled: App::can_paste,
            run: App::paste_files,
        },
        CommandSpec {
            id: "file.paste_location",
            name: "action.paste_location",
            group: Group::Navigate,
            keys: &["ctrl+shift+v"],
            enabled: |_| true,
            run: App::paste_location,
        },
    ] {
        registry.add(spec);
    }
}

/// Finding out what the program can do — rule 4.
fn help(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "settings.open",
            name: "action.settings",
            group: Group::Help,
            // **`Ctrl ,` is where every program a person has met keeps its
            // settings**, and now it opens one — ADR 0027. It used to write the
            // keymap out, which is a row inside the panel, so the thing somebody
            // learned this chord for is one press away and is now visible to
            // everybody who never knew the chord.
            keys: &["ctrl+,"],
            enabled: |_: &App| true,
            run: App::toggle_settings,
        },
        CommandSpec {
            id: "config.write",
            name: "action.config_write",
            group: Group::Help,
            // **Beside the panel it now lives in.** ADR 0027 moved this off
            // `Ctrl ,`, and it keeps a chord because
            // `every_command_is_reachable_and_named` asks one of every command.
            // `Ctrl Shift ,` is not available: a chord's shift is normalised
            // away for punctuation, so it *is* `Ctrl ,`.
            keys: &["ctrl+alt+,"],
            enabled: |_: &App| true,
            run: App::write_the_config,
        },
        CommandSpec {
            id: "help.keys",
            name: "action.show_keys",
            group: Group::Help,
            keys: &["f1"],
            enabled: |_| true,
            run: App::toggle_keys,
        },
        CommandSpec {
            id: "help.dump",
            name: "action.dump",
            group: Group::Help,
            keys: &["f12"],
            enabled: |_| true,
            run: App::dump,
        },
        CommandSpec {
            id: "help.stats",
            name: "action.show_stats",
            group: Group::Help,
            // **Not F2**, which every file manager on every desktop uses for
            // rename. A diagnostic overlay does not get to keep the key a
            // person's hands already know.
            keys: &["f8"],
            enabled: |_| true,
            run: App::toggle_stats,
        },
    ] {
        registry.add(spec);
    }
}

/// Commands a pointer reaches without going through a menu, and where.
///
/// Read by `every_command_can_be_reached_without_a_keyboard`. The toolbar and
/// the window buttons are asked rather than listed, so renaming the command
/// behind a button fails the test instead of quietly emptying it.
#[cfg(test)]
fn pointer_surfaces() -> Vec<(&'static str, &'static str)> {
    let mut reach: Vec<(&'static str, &'static str)> = Vec::new();
    for tool in Tool::ALL {
        reach.push((tool.command(), "a navigation button in the pane's own bar"));
    }
    for button in crate::layout::Button::ALL {
        reach.push((button.command(), "a window button"));
    }
    for id in ["view.sort_name", "view.sort_size", "view.sort_modified"] {
        reach.push((id, "a column header"));
    }
    reach.push(("go.open", "a double click on a row"));
    reach.push(("view.menu", "the secondary pointer button"));
    reach.push(("select.none", "a click on empty space"));
    reach.push(("select.toggle", "the check box over an entry's mark"));
    reach.push(("view.band_toggle", "a press on a band's heading"));
    reach.push(("edit.history", "a press on the history strip's header"));
    reach.push(("go.home", "the sidebar's Home row"));
    reach.push(("go.search_results", "the label at the end of the search's summary line"));
    reach.push(("view.path_entry", "a click on a pane's bar beside the breadcrumbs"));
    reach.push(("tab.close", "the cross on a tab, or a double click on it"));
    reach.push(("config.write", "a row in the settings panel"));
    for groups in [
        crate::menu::ON_ROW,
        crate::menu::ON_BACKGROUND,
        crate::menu::IN_A_LOOK,
        crate::menu::ON_A_PLACE,
    ] {
        for id in groups.iter().flat_map(|group| group.iter()) {
            reach.push((id, "the context menu"));
        }
    }
    reach
}

/// The commands a pointer is not expected to reach, and why.
///
/// **Every entry is a reason, not an exception.** Eight of them are cursor
/// movement, where the pointer's equivalent is clicking the row it would have
/// moved to — a menu item that moves a selection down one is furniture nobody
/// would use. The last two are diagnostics for a bug report.
#[cfg(test)]
const POINTER_EXEMPT: &[(&str, &str)] = &[
    ("select.up", "a pointer moves the cursor by clicking the row"),
    ("select.left", "a pointer moves the cursor by clicking the cell"),
    ("select.right", "a pointer moves the cursor by clicking the cell"),
    ("select.extend_left", "a pointer extends a selection by shift-clicking"),
    ("select.extend_right", "a pointer extends a selection by shift-clicking"),
    ("select.down", "a pointer moves the cursor by clicking the row"),
    ("select.extend_up", "a pointer extends a selection by shift-clicking"),
    ("select.extend_down", "a pointer extends a selection by shift-clicking"),
    ("select.page_up", "a pointer pages with the scroll wheel and the thumb"),
    ("select.page_down", "a pointer pages with the scroll wheel and the thumb"),
    ("select.top", "a pointer reaches the top by dragging the thumb"),
    ("select.bottom", "a pointer reaches the bottom by dragging the thumb"),
    ("tab.next", "a pointer switches tabs by clicking the one it wants"),
    ("tab.previous", "a pointer switches tabs by clicking the one it wants"),
    ("pane.next", "a pointer moves the keyboard by clicking in the pane"),
    // **Off the background menu, and not because the menu was long.** `Ctrl` with
    // the wheel and a two-finger pinch both zoom the list, which is what every
    // list and every browser does with them — so these three were the only rows
    // on that menu duplicating a gesture a pointer already has. Taking them off
    // is three rows and a divider back on a menu that is taller than a 720-pixel
    // window and scrolls, where the last rows are the hardest to reach.
    ("view.zoom_in", "a pointer zooms with Ctrl and the wheel, or a pinch"),
    ("view.zoom_out", "a pointer zooms with Ctrl and the wheel, or a pinch"),
    ("view.zoom_reset", "a pointer zooms with Ctrl and the wheel, or a pinch"),
    ("help.dump", "a diagnostic for a bug report, not an action on a file"),
    ("help.stats", "a diagnostic for a bug report, not an action on a file"),
    // A field's own four. A pointer selects text by dragging in the field, which
    // is a gesture rather than a menu row — and a menu cannot be opened over an
    // open field without closing it, so a row for these would be a row that can
    // never be reached.
    ("field.select_all", "a pointer selects text by dragging across it"),
    ("field.copy", "a pointer has the middle-click selection and the drag"),
    ("field.cut", "a pointer has the drag; a menu over a field would close it"),
    ("field.paste", "a pointer has the middle-click paste"),
    ("field.undo", "a field action; the pointer has no gesture for it either"),
    ("field.redo", "a field action; the pointer has no gesture for it either"),
];

/// The hint in the corner of the path bar, from the registry.
///
/// **Not a hardcoded chord.** This string said `F2 stats` for two commits after
/// `F2` became Rename, because it was a catalogue line nothing checked against
/// the keymap. And **not the diagnostic key**: `F12` prints a report for a bug,
/// which [`POINTER_EXEMPT`] describes as "not an action on a file" — advertising
/// it in permanent chrome was the same mistake in the other direction.
#[must_use]
pub fn hint(registry: &Registry<App>) -> String {
    let keys = registry
        .find("help.keys")
        .and_then(|id| registry.describe(id))
        .map(|command| command.keys)
        .unwrap_or_default();
    sk_model::i18n::format("chrome.hint", &[&keys])
}

/// What the chords a text field claims mean inside one.
///
/// **In the registry rather than in `Field::key`**, because these four chords mean
/// something else over a listing and the registry is the only place that
/// difference can live if a keymap is ever to be rebindable — `sk_cmd::Scope`. The
/// rest of a field's keys are not here: `Shift ←` and `Home` mean nothing over a
/// listing, so there is nothing for a scope to settle and they are text editing
/// rather than actions.
///
/// Every one of them is dimmed unless a field has the keyboard, which is what the
/// palette shows. Their **names end with ", in a field"** for the same reason: the
/// keyboard overlay is a reference, and a chord listed twice with no indication of
/// when reads as a contradiction rather than as a scope. At the end rather than the
/// start, because the overlay truncates a long row — the qualifier first left
/// `In a field: copy the selec…`, where the half that distinguishes it is the half
/// that went.
fn field(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "field.select_all",
            name: "action.select_all_text",
            group: Group::Edit,
            keys: &["ctrl+a"],
            enabled: App::in_a_field,
            run: App::select_all_text,
        },
        CommandSpec {
            id: "field.copy",
            name: "action.copy_text",
            group: Group::Edit,
            keys: &["ctrl+c"],
            enabled: App::in_a_field,
            run: App::copy_text,
        },
        CommandSpec {
            id: "field.cut",
            name: "action.cut_text",
            group: Group::Edit,
            keys: &["ctrl+x"],
            enabled: App::in_a_field,
            run: App::cut_text,
        },
        CommandSpec {
            id: "field.undo",
            name: "action.undo_text",
            group: Group::Edit,
            // **The other dangerous one.** `Ctrl+Z` while typing a name walked
            // back the last operation on disk, which is the opposite of what
            // anybody means by it inside a field.
            keys: &["ctrl+z"],
            enabled: App::in_a_field,
            run: App::undo_text,
        },
        CommandSpec {
            id: "field.redo",
            name: "action.redo_text",
            group: Group::Edit,
            keys: &["ctrl+shift+z", "ctrl+y"],
            enabled: App::in_a_field,
            run: App::redo_text,
        },
        CommandSpec {
            id: "go.search_results",
            name: "action.search_results",
            group: Group::Navigate,
            // **In the field scope, because that is where it happens.** Inside
            // the search `Enter` goes to one result and this takes all of them;
            // globally `Ctrl+Enter` already opens the selected folders in tabs,
            // and the registry's own test refuses two meanings for one chord.
            // The search box is a field, so the narrower scope is both true and
            // free — and `enabled` keeps it dark in the rename and path fields,
            // where there are no results to show.
            //
            // Registered rather than handled privately by the search, because
            // [rule 2](../../docs/03-principles.md) says every action is one
            // entry in one registry — which is also what puts it in the palette,
            // and the palette is how a pointer reaches it.
            keys: &["ctrl+enter"],
            enabled: App::search_has_results,
            run: App::flatten_search,
        },
        CommandSpec {
            id: "field.paste",
            name: "action.paste_text",
            group: Group::Edit,
            // **The one that was dangerous.** `Ctrl+V` while renaming started a
            // file paste into the directory being looked at, which is not what
            // anybody typing a name means by it.
            keys: &["ctrl+v"],
            enabled: App::in_a_field,
            run: App::paste_text,
        },
    ] {
        registry.add_in(sk_cmd::Scope::Field, spec);
    }
    look_scope(registry);
}

/// The two chords a text preview takes from the listing behind it.
///
/// **In the registry rather than handled privately**, because that is the only
/// place the difference between "select all the files" and "select all the text"
/// can live if a keymap is ever to be rebindable — the argument
/// [`sk_cmd::Scope`] was written for. The pointer reaches both through the
/// preview's own context menu, which is built from these same entries.
fn look_scope(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "look.select_all",
            name: "action.look_select_all",
            group: Group::Edit,
            keys: &["ctrl+a"],
            // **Either kind of text.** A page's words are text somebody wants a
            // line out of, which is the whole of ADR 0024 — and `Ctrl+A` over one
            // was dimmed because this asked whether the panel held a text *file*.
            enabled: App::looking_at_any_text,
            run: App::select_all_of_the_look,
        },
        CommandSpec {
            id: "look.copy",
            name: "action.look_copy",
            group: Group::Edit,
            keys: &["ctrl+c"],
            // Dimmed with nothing selected rather than copying the whole file,
            // because `Ctrl+C` on an empty selection copying everything is a
            // surprise somebody discovers by pasting.
            enabled: App::look_has_a_selection,
            run: App::copy_the_look_selection,
        },
        CommandSpec {
            id: "look.edit",
            name: "action.look_edit",
            group: Group::Edit,
            // `Ctrl+E` for edit, in the scope where it can only mean this: over a
            // listing the same chord is free, and the panel is what decides.
            keys: &["ctrl+e"],
            enabled: App::looking_at_text,
            run: App::toggle_editing_the_look,
        },
        CommandSpec {
            id: "look.source",
            name: "action.look_source",
            group: Group::View,
            // **`Ctrl+M` for markdown**, in the look's own scope. `Ctrl+R` would
            // have been *raw* and is already Discard here, which is the kind of
            // clash the registry refuses rather than silently resolves.
            keys: &["ctrl+m"],
            enabled: App::looking_at_markdown,
            run: App::toggle_the_rendering,
        },
        CommandSpec {
            id: "look.find",
            name: "action.look_find",
            group: Group::View,
            // **`Ctrl+F` in the scope where it can only mean this.** Over a
            // listing it filters the rows; with a page in front of you the rows
            // are behind a panel and the thing to search is the page.
            keys: &["ctrl+f"],
            enabled: App::looking_at_a_page_with_words,
            run: App::toggle_finding,
        },
        CommandSpec {
            id: "look.play",
            name: "action.look_play",
            group: Group::View,
            // **`Ctrl+Space` in the scope where it can only mean this.** Plain
            // `Space` is the key that closes a quick look, so it is not free —
            // and over a listing this chord toggles the cursor row's selection,
            // which is a row under a panel somebody is watching.
            keys: &["ctrl+space"],
            enabled: App::looking_at_an_animation,
            run: App::toggle_playing,
        },
        CommandSpec {
            id: "look.discard",
            name: "action.look_discard",
            group: Group::Edit,
            // **`Ctrl+R` in the scope where it can only mean this**, which is the
            // argument `Ctrl+E` already makes: reloading the file *is* what a
            // discard does, and over a listing the chord is free. `Escape` does
            // it too, and the panel draws a button for it — an edit that can only
            // be abandoned by a chord somebody guessed is rule 4's failure.
            keys: &["ctrl+r"],
            enabled: App::look_is_unsaved,
            run: App::discard_the_look,
        },
        CommandSpec {
            id: "look.save",
            name: "action.look_save",
            group: Group::Edit,
            // What `Ctrl+S` means everywhere. Dimmed with nothing to write, so
            // the row says whether there is an edit outstanding.
            keys: &["ctrl+s"],
            enabled: App::look_is_unsaved,
            run: App::save_the_look,
        },
    ] {
        registry.add_in(sk_cmd::Scope::Look, spec);
    }
}

/// The keyboard overlay's rows: what to press, and what it does.
///
/// Read out of the registry rather than written beside it, so the string on
/// screen cannot describe a key that is not the bound one.
#[must_use]
pub fn overlay_rows(registry: &Registry<App>) -> Vec<(String, String)> {
    let mut rows: Vec<_> = registry
        .iter()
        .filter(|command| !command.keys.is_empty())
        .map(|command| (command.keys.clone(), command.name.to_owned()))
        .collect();
    rows.push((
        sk_model::i18n::lookup("keys.typing").to_owned(),
        sk_model::i18n::lookup("action.type_ahead").to_owned(),
    ));
    rows
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{POINTER_EXEMPT, hint, overlay_rows, pointer_surfaces, registry};

    /// Rule 2 and rule 4 as one assertion: the whole set is sound, every command
    /// has a name a person can read, and no two of them fight over a key.
    #[test]
    fn the_registry_has_no_duplicate_ids_unparseable_chords_or_conflicts() {
        let registry = registry();
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());
        assert!(registry.len() >= 20, "only {} commands", registry.len());
    }

    /// The two index chords reach the two index commands, in the listing.
    ///
    /// **Both were unreachable when they were written**, because they were added
    /// to the `Scope::Field` block at the end of the file rather than the global
    /// one — a mistake that is invisible in the registry's own consistency
    /// checks, invisible in the palette, which runs a command by its id, and
    /// visible only by pressing the key in a window. This asserts the scope.
    #[test]
    fn the_index_chords_reach_their_commands_outside_a_field() {
        use sk_plat::{Key, KeyEvent, Modifiers};
        let registry = registry();
        let press = |key: char, shift: bool, alt: bool| KeyEvent {
            key: Key::Character(key),
            text: None,
            mods: Modifiers { ctrl: true, shift, alt, logo: false },
            repeat: false,
        };
        let named = |event: &KeyEvent| {
            registry
                .resolve(event, sk_cmd::Scope::Global)
                .and_then(|id| registry.describe(id))
                .map(|it| it.id)
        };
        assert_eq!(named(&press('I', true, false)), Some("index.build"));
        assert_eq!(named(&press('i', false, true)), Some("index.forget"));
        // And the bare chord is still Properties, which is what they sit beside.
        assert_eq!(named(&press('i', false, false)), Some("file.properties"));
    }

    #[test]
    fn every_command_is_reachable_and_named() {
        let registry = registry();
        for command in registry.iter() {
            assert!(!command.name.is_empty(), "`{}` has no name", command.id);
            assert_ne!(
                command.name, command.id,
                "`{}` has no catalogue entry — its name resolved to its own key",
                command.id
            );
            assert!(!command.keys.is_empty(), "`{}` has no key", command.id);
            assert!(command.id.contains('.'), "`{}` is not a namespaced id", command.id);
        }
    }

    /// The defect that started this: `Alt ←` was bound to Parent, which is Back
    /// everywhere else a person has met it.
    #[test]
    fn the_navigation_keys_are_the_ones_a_person_already_knows() {
        let registry = registry();
        let keys_of = |id: &str| registry.describe(registry.find(id).expect(id)).expect(id).keys;
        assert_eq!(keys_of("go.back"), "Alt ←");
        assert_eq!(keys_of("go.forward"), "Alt →");
        assert!(keys_of("go.parent").contains("Alt ↑"));
        assert!(keys_of("go.parent").contains("Backspace"));
    }

    /// The other half of the promise the keyboard half already keeps: every
    /// command is reachable with a pointer alone, or is listed above with the
    /// reason why a pointer has a better way of doing that particular thing.
    #[test]
    fn every_command_can_be_reached_without_a_keyboard() {
        let registry = registry();
        let reach = pointer_surfaces();
        for command in registry.iter() {
            let by = reach.iter().find(|(id, _)| *id == command.id).map(|(_, where_)| *where_);
            let exempt = POINTER_EXEMPT.iter().find(|(id, _)| *id == command.id);
            assert!(
                by.is_some() || exempt.is_some(),
                "`{}` has no pointer at all: put it on a menu in `menu.rs`, on a button, \
                 or in `POINTER_EXEMPT` with the reason a pointer does not need it",
                command.id
            );
            assert!(
                by.is_none() || exempt.is_none(),
                "`{}` is both reachable by {} and exempt — one of the two is stale",
                command.id,
                by.unwrap_or_default()
            );
        }
    }

    /// A menu row whose identifier the registry does not have is drawn as
    /// nothing at all, which is a menu that silently loses an entry.
    #[test]
    fn every_menu_row_and_every_exemption_names_a_real_command() {
        let registry = registry();
        let named = pointer_surfaces().into_iter().chain(POINTER_EXEMPT.iter().copied());
        for (id, _) in named {
            assert!(registry.find(id).is_some(), "`{id}` is not a command");
        }
    }

    /// The one string in permanent chrome that names a chord. It said `F2 stats`
    /// for two commits after `F2` became Rename.
    #[test]
    fn the_hint_names_the_chord_that_is_actually_bound() {
        let registry = registry();
        let shown = hint(&registry);
        let keys = registry
            .describe(registry.find("help.keys").expect("help.keys"))
            .expect("described")
            .keys;
        assert!(shown.contains(&keys), "{shown:?} does not name {keys:?}");
        // And nothing else: a hint that lists a diagnostic is a diagnostic in
        // permanent chrome.
        for other in registry.iter().filter(|command| command.id != "help.keys") {
            assert!(
                !shown.contains(&other.keys),
                "{shown:?} also names {}'s {:?}",
                other.id,
                other.keys
            );
        }
    }

    #[test]
    fn the_overlay_shows_every_bound_command_plus_type_ahead() {
        let registry = registry();
        let rows = overlay_rows(&registry);
        assert_eq!(rows.len(), registry.len() + 1);
        for (keys, name) in &rows {
            assert!(!keys.is_empty() && !name.is_empty(), "{keys:?} {name:?}");
        }
    }
}
