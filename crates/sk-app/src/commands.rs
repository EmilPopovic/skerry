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

/// What the listing shows and how it is ordered.
fn view(registry: &mut Registry<App>) {
    for spec in [
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
            keys: &["f2"],
            enabled: |_| true,
            run: App::toggle_stats,
        },
    ] {
        registry.add(spec);
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

    use super::{overlay_rows, registry};

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
