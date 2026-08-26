//! One table for what the keyboard does.
//!
//! Rule 2 says every action is one entry in one registry, and rule 4 says the
//! shortcut appears next to the thing it acts on. The full registry is `sk-cmd`
//! and arrives in M6. This is its smallest honest ancestor: [`BINDINGS`] is read
//! by the key handler *and* by the help overlay, and
//! `every_action_is_in_the_table` fails the build if an action is ever handled
//! without being listed. That is the mechanism, not the discipline — an action
//! nobody can discover cannot be added by accident.

use sk_plat::{Key, KeyEvent, Modifiers, Named};

/// Everything the keyboard can do in this milestone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    CursorUp,
    CursorDown,
    ExtendUp,
    ExtendDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Open,
    Parent,
    SelectAll,
    ClearSelection,
    ToggleHidden,
    Reload,
    ToggleTheme,
    ShowKeys,
    ShowStats,
    SortName,
    SortSize,
    SortModified,
}

/// Every action, the keys that reach it, and its catalogue entry.
pub const BINDINGS: &[(Action, &str, &str)] = &[
    (Action::CursorUp, "↑", "action.cursor_up"),
    (Action::CursorDown, "↓", "action.cursor_down"),
    (Action::ExtendUp, "Shift ↑", "action.extend_up"),
    (Action::ExtendDown, "Shift ↓", "action.extend_down"),
    (Action::PageUp, "Page Up", "action.page_up"),
    (Action::PageDown, "Page Down", "action.page_down"),
    (Action::Top, "Home", "action.top"),
    (Action::Bottom, "End", "action.bottom"),
    (Action::Open, "Enter", "action.open"),
    (Action::Parent, "Backspace", "action.parent"),
    (Action::SelectAll, "Ctrl A", "action.select_all"),
    (Action::ClearSelection, "Esc", "action.clear_selection"),
    (Action::ToggleHidden, "Ctrl H", "action.toggle_hidden"),
    (Action::Reload, "F5", "action.reload"),
    (Action::ToggleTheme, "Ctrl T", "action.toggle_theme"),
    (Action::SortName, "Alt 1", "action.sort_name"),
    (Action::SortSize, "Alt 2", "action.sort_size"),
    (Action::SortModified, "Alt 3", "action.sort_modified"),
    (Action::ShowKeys, "F1", "action.show_keys"),
    (Action::ShowStats, "F2", "action.show_stats"),
];

/// Which action a key press reaches, if any.
#[must_use]
pub fn resolve(event: &KeyEvent) -> Option<Action> {
    let Modifiers { ctrl, shift, alt, logo } = event.mods;
    if logo {
        return None;
    }
    Some(match (event.key, ctrl, shift, alt) {
        (Key::Named(Named::Up), false, false, false) => Action::CursorUp,
        (Key::Named(Named::Down), false, false, false) => Action::CursorDown,
        (Key::Named(Named::Up), false, true, false) => Action::ExtendUp,
        (Key::Named(Named::Down), false, true, false) => Action::ExtendDown,
        (Key::Named(Named::PageUp), false, _, false) => Action::PageUp,
        (Key::Named(Named::PageDown), false, _, false) => Action::PageDown,
        (Key::Named(Named::Home), false, _, false) => Action::Top,
        (Key::Named(Named::End), false, _, false) => Action::Bottom,
        (Key::Named(Named::Enter), false, false, false) => Action::Open,
        (Key::Named(Named::Backspace), false, false, false)
        | (Key::Named(Named::Left), false, false, true) => Action::Parent,
        (Key::Character('a'), true, false, false) => Action::SelectAll,
        (Key::Named(Named::Escape), false, false, false) => Action::ClearSelection,
        (Key::Character('h'), true, false, false) => Action::ToggleHidden,
        (Key::Named(Named::Function(5)), false, false, false) => Action::Reload,
        (Key::Character('t'), true, false, false) => Action::ToggleTheme,
        (Key::Character('1'), false, false, true) => Action::SortName,
        (Key::Character('2'), false, false, true) => Action::SortSize,
        (Key::Character('3'), false, false, true) => Action::SortModified,
        (Key::Named(Named::Function(1)), false, false, false) => Action::ShowKeys,
        (Key::Named(Named::Function(2)), false, false, false) => Action::ShowStats,
        _ => return None,
    })
}

/// Whether a key press should go to the type-ahead buffer instead of an action.
///
/// A bare printable character and nothing else. `Ctrl+A` types nothing, and a
/// space would otherwise start a jump to a filename beginning with a space.
#[must_use]
pub fn type_ahead(event: &KeyEvent) -> Option<&str> {
    if !event.mods.none() {
        return None;
    }
    let text = event.text.as_deref()?;
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Action, BINDINGS, resolve, type_ahead};
    use sk_model::i18n;
    use sk_plat::{Key, KeyEvent, Modifiers, Named};

    const EVERY_ACTION: &[Action] = &[
        Action::CursorUp,
        Action::CursorDown,
        Action::ExtendUp,
        Action::ExtendDown,
        Action::PageUp,
        Action::PageDown,
        Action::Top,
        Action::Bottom,
        Action::Open,
        Action::Parent,
        Action::SelectAll,
        Action::ClearSelection,
        Action::ToggleHidden,
        Action::Reload,
        Action::ToggleTheme,
        Action::ShowKeys,
        Action::ShowStats,
        Action::SortName,
        Action::SortSize,
        Action::SortModified,
    ];

    fn press(key: Key, mods: Modifiers) -> KeyEvent {
        KeyEvent { key, text: None, mods, repeat: false }
    }

    /// Rule 2 as an assertion: an action that cannot be found is a bug, not a
    /// documentation gap.
    #[test]
    fn every_action_is_in_the_table() {
        for action in EVERY_ACTION {
            assert!(
                BINDINGS.iter().any(|(listed, ..)| listed == action),
                "{action:?} is handled but not discoverable"
            );
        }
        assert_eq!(BINDINGS.len(), EVERY_ACTION.len(), "the table has an entry with no action");
    }

    /// Rule 4: the label a user reads has to exist.
    #[test]
    fn every_binding_has_a_label_and_a_key_to_show() {
        for (action, keys, label) in BINDINGS {
            assert!(!keys.is_empty(), "{action:?} has no key to show");
            assert_ne!(i18n::lookup(label), *label, "{action:?} has no catalogue entry");
        }
    }

    #[test]
    fn the_arrow_keys_resolve_and_shift_extends_instead() {
        let plain = Modifiers::default();
        let shifted = Modifiers { shift: true, ..Modifiers::default() };
        assert_eq!(resolve(&press(Key::Named(Named::Down), plain)), Some(Action::CursorDown));
        assert_eq!(resolve(&press(Key::Named(Named::Down), shifted)), Some(Action::ExtendDown));
    }

    #[test]
    fn a_super_chord_is_the_compositors_and_never_ours() {
        let logo = Modifiers { logo: true, ..Modifiers::default() };
        assert_eq!(resolve(&press(Key::Character('a'), logo)), None);
        let logo_ctrl = Modifiers { logo: true, ctrl: true, ..Modifiers::default() };
        assert_eq!(resolve(&press(Key::Character('a'), logo_ctrl)), None);
    }

    #[test]
    fn an_unbound_key_resolves_to_nothing_rather_than_to_something_nearby() {
        assert_eq!(resolve(&press(Key::Named(Named::Insert), Modifiers::default())), None);
        assert_eq!(resolve(&press(Key::Unknown(0x1234), Modifiers::default())), None);
    }

    #[test]
    fn type_ahead_takes_bare_characters_and_nothing_else() {
        let typed = |text: &str, mods| KeyEvent {
            key: Key::Character(text.chars().next().unwrap_or('x')),
            text: Some(text.to_owned()),
            mods,
            repeat: false,
        };
        assert_eq!(type_ahead(&typed("m", Modifiers::default())), Some("m"));
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        assert_eq!(type_ahead(&typed("m", ctrl)), None);
        assert_eq!(type_ahead(&typed(" ", Modifiers::default())), None, "a space is not a jump");
        assert_eq!(type_ahead(&press(Key::Named(Named::Down), Modifiers::default())), None);
    }
}
