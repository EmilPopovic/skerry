// SPDX-License-Identifier: Apache-2.0
//! The accessibility tree, assembled from what the frame drew.
//!
//! **[ADR 0026](../../../docs/adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md).**
//! Once a frame, from the same geometry the draw used and the same state it read:
//! [`crate::view::Hits`] for everything whose rectangle a press is tested
//! against, and [`crate::groups::Bands`] for the rows. A control that is not in
//! one of those two is not in the tree, which makes "draw it" and "announce it"
//! one commit rather than two.
//!
//! Nothing here talks to a bus. [`sk_desktop::atspi`] does that, on its own
//! thread, from the snapshot this produces.

use sk_desktop::atspi::{Node, Rect, Role, Text, Tree, state};

use crate::app::App;

/// The window's own node. Everything else hangs off it.
const FRAME: u32 = 1;

/// Node numbers are handed out in this order, so a row's number is stable
/// between frames as long as it is the same row of the same pane.
mod ids {
    /// The listing of the focused pane.
    pub const LIST: u32 = 2;
    /// The sidebar's places.
    pub const PLACES: u32 = 3;
    /// The line at the bottom.
    pub const STATUS: u32 = 4;
    /// The field being typed into, when there is one.
    pub const FIELD: u32 = 5;
    /// The open menu, and the palette's list of results.
    pub const MENU: u32 = 6;
    /// The quick look's text.
    pub const LOOK: u32 = 7;
    /// Rows of the listing start here; one number per row index.
    pub const ROWS: u32 = 1_000;
    /// The sidebar's rows.
    pub const PLACE_ROWS: u32 = 100_000;
    /// A menu's or a palette's rows.
    pub const MENU_ROWS: u32 = 200_000;
}

/// What is on screen, as a tree an assistive technology can walk.
///
/// **The focused pane only.** A split has two listings and a screen reader has
/// one point of attention; publishing both would make "the list" ambiguous in
/// every announcement. The pane that has the keyboard is the one that is
/// described, and switching panes republishes.
pub(crate) fn tree_of(app: &App) -> Tree {
    let mut nodes = Vec::with_capacity(64);
    let scale = app.a11y_scale();
    let mut frame = Node::new(FRAME, Role::Frame, app.a11y_title())
        .with(if app.a11y_active() { state::ACTIVE } else { 0 })
        .at(scaled(0.0, 0.0, app.a11y_window().width, app.a11y_window().height, scale));
    let mut root = Node::new(0, Role::Application, "Skerry");
    root.children = vec![FRAME];

    let mut focus = None;
    let listing = listing_of(app, scale, &mut nodes, &mut focus);
    let places = places_of(app, scale, &mut nodes);
    let status = Node::new(ids::STATUS, Role::StatusBar, app.a11y_status()).at(scaled(
        0.0,
        app.a11y_status_area().y,
        app.a11y_status_area().width,
        app.a11y_status_area().height,
        scale,
    ));

    frame.children = vec![places, listing, ids::STATUS];
    if let Some(field) = field_of(app, scale, &mut nodes) {
        frame.children.push(field);
        focus = Some(field);
    }
    if let Some(menu) = menu_of(app, scale, &mut nodes, &mut focus) {
        frame.children.push(menu);
    }
    if let Some(look) = look_of(app, scale, &mut nodes) {
        frame.children.push(look);
        focus = Some(look);
    }

    nodes.push(status);
    nodes.push(frame);
    nodes.push(root);
    Tree { nodes, focus, active: app.a11y_active() }
}

/// The listing, and one node per row that is on screen.
///
/// **On screen, not in the directory.** A million rows would be a million
/// objects on a bus, and every one of them costs the assistive technology a
/// round trip; what a reader moves through is what is drawn, and the cursor
/// keeps the drawn window over itself.
fn listing_of(app: &App, scale: f32, nodes: &mut Vec<Node>, focus: &mut Option<u32>) -> u32 {
    let pane = app.panes.focused();
    let cells = app.cells();
    let body = app.a11y_body();
    let mut list = Node::new(ids::LIST, Role::List, app.a11y_listing_name())
        .with(state::FOCUSABLE | state::MULTISELECTABLE)
        .at(scaled(body.x, body.y, body.width, body.height, scale));
    let cursor = pane.selection.cursor();
    let has_keyboard = app.a11y_list_focused();
    for row in cells.visible(pane.scroll.offset()) {
        let Some(id) = pane.listing.at(row) else { continue };
        let cell = cells.at(row, pane.scroll.offset());
        let number = ids::ROWS + u32::try_from(row).unwrap_or(0);
        let chosen = pane.selection.contains(id);
        let here = cursor == Some(id);
        let mut node = Node::new(number, Role::ListItem, pane.listing.name(id))
            .with(state::FOCUSABLE | state::SELECTABLE)
            .with(if chosen { state::SELECTED } else { 0 })
            .with(if here && has_keyboard { state::FOCUSED } else { 0 })
            .at(scaled(cell.x, cell.y, cell.width, cell.height, scale));
        // What a row *is*, said once here rather than left to the reader to
        // infer from a name: a directory and a file sound identical otherwise.
        node.description = app.a11y_row_description(id);
        list.children.push(number);
        nodes.push(node);
        if here && has_keyboard {
            *focus = Some(number);
        }
    }
    nodes.push(list);
    ids::LIST
}

/// The sidebar's places, where it is open.
fn places_of(app: &App, scale: f32, nodes: &mut Vec<Node>) -> u32 {
    let mut list = Node::new(ids::PLACES, Role::List, sk_model::i18n::lookup("a11y.places"));
    for (at, area) in app.a11y_places().iter().enumerate() {
        let number = ids::PLACE_ROWS + u32::try_from(at).unwrap_or(0);
        let name = app.a11y_place_name(at);
        list.children.push(number);
        nodes.push(Node::new(number, Role::ListItem, name).with(state::SELECTABLE).at(scaled(
            area.x,
            area.y,
            area.width,
            area.height,
            scale,
        )));
    }
    if let (Some(top), Some(bottom)) = (app.a11y_places().first(), app.a11y_places().last()) {
        let height = (bottom.y + bottom.height - top.y).max(0.0);
        list.extents = scaled(top.x, top.y, top.width, height, scale);
    }
    nodes.push(list);
    ids::PLACES
}

/// The one field being typed into, if any.
fn field_of(app: &App, scale: f32, nodes: &mut Vec<Node>) -> Option<u32> {
    let (label, field, area) = app.a11y_field()?;
    let value = field.text.clone();
    let caret = characters(&value, field.caret);
    let selection =
        field.selection().map(|(from, to)| (characters(&value, from), characters(&value, to)));
    let mut node = Node::new(ids::FIELD, Role::Entry, label)
        .with(state::EDITABLE | state::FOCUSABLE | state::FOCUSED)
        .at(scaled(area.x, area.y, area.width, area.height, scale));
    node.text = Some(Text { value, caret, selection });
    nodes.push(node);
    Some(ids::FIELD)
}

/// The open menu or palette, as a menu with its rows.
fn menu_of(app: &App, scale: f32, nodes: &mut Vec<Node>, focus: &mut Option<u32>) -> Option<u32> {
    let rows = app.a11y_menu();
    if rows.is_empty() {
        return None;
    }
    let mut menu = Node::new(ids::MENU, Role::Menu, sk_model::i18n::lookup("a11y.menu"));
    for (at, (name, area, active)) in rows.iter().enumerate() {
        let number = ids::MENU_ROWS + u32::try_from(at).unwrap_or(0);
        menu.children.push(number);
        nodes.push(
            Node::new(number, Role::MenuItem, name.clone())
                .with(state::FOCUSABLE)
                .with(if *active { state::FOCUSED } else { 0 })
                .at(scaled(area.x, area.y, area.width, area.height, scale)),
        );
        if *active {
            *focus = Some(number);
        }
    }
    nodes.push(menu);
    Some(ids::MENU)
}

/// The quick look's text, where it is showing any.
fn look_of(app: &App, scale: f32, nodes: &mut Vec<Node>) -> Option<u32> {
    let look = app.a11y_look()?;
    let value = look.spoken_text()?;
    let area = app.a11y_look_area();
    let mut node = Node::new(ids::LOOK, Role::Text, look.title())
        .with(state::FOCUSABLE | state::FOCUSED | state::MULTILINE)
        .at(scaled(area.x, area.y, area.width, area.height, scale));
    node.text = Some(Text { value, caret: 0, selection: None });
    nodes.push(node);
    Some(ids::LOOK)
}

/// A logical rectangle in the screen pixels AT-SPI asks for.
///
/// **Screen and window are the same origin here, and that is a limitation.** A
/// Wayland client is not told where it is; X11 could answer and does not, because
/// one number that is right on one backend is worse than one that is documented
/// on both — see ADR 0026's revisit trigger.
fn scaled(x: f32, y: f32, width: f32, height: f32, scale: f32) -> Rect {
    let round = |value: f32| (value * scale).round() as i32;
    Rect { x: round(x), y: round(y), width: round(width), height: round(height) }
}

/// A byte offset as the character offset AT-SPI counts in.
fn characters(text: &str, byte: usize) -> i32 {
    // **Floored to a boundary, not fallen back to the whole string.** A byte
    // inside `Ž` is not a caret position, and answering "the end" for it would
    // put a screen reader's cursor at the end of a name somebody is editing in
    // the middle.
    let mut at = byte.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let head = text.get(..at).unwrap_or("");
    i32::try_from(head.chars().count()).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::characters;

    /// An offset is characters, so a caret after `Ž` is one and not two.
    #[test]
    fn a_caret_is_reported_in_characters() {
        assert_eq!(characters("Žofia", 0), 0);
        assert_eq!(characters("Žofia", 2), 1);
        assert_eq!(characters("Žofia", 6), 5);
        // Past the end, and inside a character: both clamp rather than panic.
        assert_eq!(characters("Žofia", 900), 5);
        assert_eq!(characters("Žofia", 1), 0);
    }
}
