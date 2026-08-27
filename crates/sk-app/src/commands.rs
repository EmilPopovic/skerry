//! Every command the program has, registered once.
//!
//! Rule 2: the keyboard overlay, and later the palette, the menus and the config
//! schema, are all views of [`registry`]'s output. A command added here is
//! reachable by all of them; a command added anywhere else is reachable by none,
//! which is what `every_command_is_reachable_and_named` fails the build over.

use sk_cmd::{CommandSpec, Group, Registry};
use sk_model::SortColumn;

use crate::app::App;

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
    select(&mut registry);
    view(&mut registry);
    file(&mut registry);
    edit(&mut registry);
    window(&mut registry);
    help(&mut registry);
    registry
}

/// Moving around: open, up, back, forward, home, reload.
fn navigate(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "go.open",
            name: "action.open",
            group: Group::Navigate,
            keys: &["enter"],
            enabled: |app: &App| app.pane.focused().is_some(),
            run: App::open_focused,
        },
        CommandSpec {
            id: "go.parent",
            name: "action.parent",
            group: Group::Navigate,
            // Both, because both are somebody's muscle memory: `Backspace` from
            // every file manager, `Alt ↑` from every browser and from GTK.
            keys: &["backspace", "alt+up"],
            enabled: |app: &App| app.pane.path.parent().is_some(),
            run: App::go_up,
        },
        CommandSpec {
            id: "go.back",
            name: "action.back",
            group: Group::Navigate,
            keys: &["alt+left"],
            enabled: |app: &App| app.pane.can_go().0,
            run: App::go_back,
        },
        CommandSpec {
            id: "go.forward",
            name: "action.forward",
            group: Group::Navigate,
            keys: &["alt+right"],
            enabled: |app: &App| app.pane.can_go().1,
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
            enabled: |_| true,
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
            run: |app: &mut App| app.move_cursor(-1, false),
        },
        CommandSpec {
            id: "select.down",
            name: "action.cursor_down",
            group: Group::Select,
            keys: &["down"],
            enabled: |_| true,
            run: |app: &mut App| app.move_cursor(1, false),
        },
        CommandSpec {
            id: "select.extend_up",
            name: "action.extend_up",
            group: Group::Select,
            keys: &["shift+up"],
            enabled: |_| true,
            run: |app: &mut App| app.move_cursor(-1, true),
        },
        CommandSpec {
            id: "select.extend_down",
            name: "action.extend_down",
            group: Group::Select,
            keys: &["shift+down"],
            enabled: |_| true,
            run: |app: &mut App| app.move_cursor(1, true),
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
            enabled: |app: &App| app.pane.listing.rows() > 0,
            run: |app: &mut App| {
                let rows = app.pane.listing.order().to_vec();
                app.pane.selection.select_all(&rows);
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
            enabled: |app: &App| app.pane.focused().is_some() && !app.busy(),
            run: App::start_rename,
        },
        CommandSpec {
            id: "file.trash",
            name: "action.trash",
            group: Group::File,
            keys: &["delete"],
            enabled: |app: &App| app.pane.focused().is_some() && !app.busy(),
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
            keys: &["ctrl+w", "ctrl+q"],
            enabled: |_| true,
            run: App::close_window,
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
            enabled: |app: &App| !app.pane.listing.is_empty(),
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
            keys: &["ctrl+t"],
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

/// The clipboard.
fn file(registry: &mut Registry<App>) {
    for spec in [
        CommandSpec {
            id: "file.copy",
            name: "action.copy",
            group: Group::File,
            keys: &["ctrl+c"],
            enabled: |app: &App| app.pane.listing.rows() > 0,
            run: App::copy_selection,
        },
        CommandSpec {
            id: "file.copy_path",
            name: "action.copy_path",
            group: Group::File,
            keys: &["ctrl+shift+c"],
            enabled: |app: &App| app.pane.listing.rows() > 0,
            run: App::copy_path,
        },
        CommandSpec {
            id: "file.paste_location",
            name: "action.paste_location",
            group: Group::File,
            keys: &["ctrl+v"],
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
    for tool in crate::layout::Tool::ALL {
        reach.push((tool.command(), "a navigation button"));
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
    reach.push(("go.home", "the sidebar's Home row"));
    for groups in [crate::menu::ON_ROW, crate::menu::ON_BACKGROUND] {
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
    ("select.down", "a pointer moves the cursor by clicking the row"),
    ("select.extend_up", "a pointer extends a selection by shift-clicking"),
    ("select.extend_down", "a pointer extends a selection by shift-clicking"),
    ("select.page_up", "a pointer pages with the scroll wheel and the thumb"),
    ("select.page_down", "a pointer pages with the scroll wheel and the thumb"),
    ("select.top", "a pointer reaches the top by dragging the thumb"),
    ("select.bottom", "a pointer reaches the bottom by dragging the thumb"),
    ("help.dump", "a diagnostic for a bug report, not an action on a file"),
    ("help.stats", "a diagnostic for a bug report, not an action on a file"),
];

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

    use super::{POINTER_EXEMPT, overlay_rows, pointer_surfaces, registry};

    /// Rule 2 and rule 4 as one assertion: the whole set is sound, every command
    /// has a name a person can read, and no two of them fight over a key.
    #[test]
    fn the_registry_has_no_duplicate_ids_unparseable_chords_or_conflicts() {
        let registry = registry();
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());
        assert!(registry.len() >= 20, "only {} commands", registry.len());
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
