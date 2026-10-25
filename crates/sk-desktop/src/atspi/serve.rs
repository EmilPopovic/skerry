// SPDX-License-Identifier: Apache-2.0
//! The AT-SPI interfaces, answered from the published tree.
//!
//! One thread, one connection, and a `poll` over two descriptors: the socket an
//! assistive technology asks down, and the eventfd the frame loop writes when
//! the tree changed. Everything here runs on that thread — the frame loop writes
//! no bytes to a bus, which is [rule
//! 1](../../../../docs/03-principles.md).
//!
//! **Four interfaces and no more.** `Accessible`, `Application`, `Component` and
//! `Text` are what a screen reader needs to announce a listing and move through
//! it; `Table`, `Selection`, `Value` and `Action` are named as absent in
//! [ADR 0026](../../../../docs/adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md)
//! and refused by name rather than answered wrongly.

use std::sync::{Arc, Mutex};

use rustix::event::{PollFd, PollFlags};

use super::tree::{Change, Node, Role, Tree};
use crate::bus::{Bus, Kind, Message, Value};

/// The object path of the application itself.
pub(super) const ROOT: &str = "/org/a11y/atspi/accessible/root";

/// What a reference to nothing looks like on this bus.
const NULL: &str = "/org/a11y/atspi/null";

/// Starts the thread that answers and emits.
pub(super) fn spawn(
    bus: Bus,
    unique: String,
    name: String,
    tree: Arc<Mutex<Tree>>,
    pending: Arc<Mutex<Vec<Change>>>,
    wake: Arc<std::os::fd::OwnedFd>,
) {
    let _ = std::thread::Builder::new().name("skerry-a11y".to_owned()).spawn(move || {
        let mut server = Server { bus, unique, name, tree, pending, wake };
        server.run();
    });
}

/// The thread's own state.
struct Server {
    bus: Bus,
    unique: String,
    name: String,
    tree: Arc<Mutex<Tree>>,
    pending: Arc<Mutex<Vec<Change>>>,
    wake: Arc<std::os::fd::OwnedFd>,
}

impl Server {
    /// Waits on the socket and the eventfd, for as long as the bus is there.
    fn run(&mut self) {
        loop {
            // The borrow of the socket ends with the block, because answering a
            // call needs the connection back.
            let Some((readable, woken)) = self.wait() else { return };
            if woken {
                let mut buffer = [0u8; 8];
                let _ = rustix::io::read(&*self.wake, &mut buffer);
                self.emit_pending();
            }
            if readable {
                // One read per readable socket, and every message it completed:
                // this thread must come back to the eventfd rather than sit on a
                // second read that may never arrive.
                let Some(messages) = self.bus.receive_ready() else { return };
                for message in &messages {
                    self.answer(message);
                }
            }
        }
    }

    /// Waits for the socket or the eventfd. `None` when the poll itself failed.
    fn wait(&self) -> Option<(bool, bool)> {
        let socket = self.bus.as_fd();
        let mut fds =
            [PollFd::new(&socket, PollFlags::IN), PollFd::new(&*self.wake, PollFlags::IN)];
        rustix::event::poll(&mut fds, None).ok()?;
        Some((fds[0].revents().contains(PollFlags::IN), fds[1].revents().contains(PollFlags::IN)))
    }

    /// Sends every signal the frame loop queued.
    fn emit_pending(&mut self) {
        let queued = match self.pending.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => return,
        };
        for change in queued {
            for message in self.signals_for(change) {
                let _ = self.bus.send(message);
            }
        }
    }

    /// The signals one change is spelled with.
    fn signals_for(&self, change: Change) -> Vec<Message> {
        let sender = self.reference(0);
        let empty = || Value::Variant(Box::new(Value::Str(String::new())));
        match change {
            Change::Focused(id) => vec![
                event(
                    &path_of(id),
                    "Object",
                    "StateChanged",
                    ("focused", 1, 0),
                    empty(),
                    sender.clone(),
                ),
                // **The deprecated one as well.** `Focus` was removed from the
                // specification and Orca still listens for it; sending both is
                // one extra message per focus change and the difference between
                // being announced and not.
                event(&path_of(id), "Focus", "Focus", ("", 0, 0), empty(), sender),
            ],
            Change::Unfocused(id) => vec![event(
                &path_of(id),
                "Object",
                "StateChanged",
                ("focused", 0, 0),
                empty(),
                sender,
            )],
            Change::Caret(id, offset) => vec![event(
                &path_of(id),
                "Object",
                "TextCaretMoved",
                ("", offset, 0),
                empty(),
                sender,
            )],
            Change::Selection(id) => vec![event(
                &path_of(id),
                "Object",
                "TextSelectionChanged",
                ("", 0, 0),
                empty(),
                sender,
            )],
            Change::Children(id) => {
                vec![event(&path_of(id), "Object", "ChildrenChanged", ("", 0, 0), empty(), sender)]
            }
            Change::Active(active) => {
                let member = if active { "Activate" } else { "Deactivate" };
                vec![event(&path_of(1), "Window", member, ("", 0, 0), empty(), sender)]
            }
        }
    }

    /// A `(so)` naming one of our objects.
    fn reference(&self, id: u32) -> Value {
        Value::Struct(vec![Value::Str(self.unique.clone()), Value::Path(path_of(id))])
    }

    /// A `(so)` naming nothing.
    fn nothing(&self) -> Value {
        Value::Struct(vec![Value::Str(self.unique.clone()), Value::Path(NULL.to_owned())])
    }

    /// Answers one call, or refuses it by name.
    fn answer(&mut self, message: &Message) {
        if message.kind != Kind::Call {
            return;
        }
        let (Some(path), Some(interface), Some(member)) =
            (message.path.as_deref(), message.interface.as_deref(), message.member.as_deref())
        else {
            return;
        };
        let Some(id) = id_of(path) else {
            let _ = self.bus.refuse(
                message,
                "org.freedesktop.DBus.Error.UnknownObject",
                "no such accessible",
            );
            return;
        };
        let answer = {
            let Ok(tree) = self.tree.lock() else { return };
            // **A root that answers before the first frame.** `Embed` puts us in
            // the registry's tree and it asks its questions at once — before any
            // frame has published anything — so an application with nothing in it
            // yet is an application with a name and no children rather than an
            // error the registry remembers us by.
            let fallback;
            let node = match tree.node(id) {
                Some(node) => node,
                None if id == 0 => {
                    fallback = Node::new(0, Role::Application, self.name.clone());
                    &fallback
                }
                None => {
                    drop(tree);
                    let _ = self.bus.refuse(
                        message,
                        "org.freedesktop.DBus.Error.UnknownObject",
                        "that accessible is gone",
                    );
                    return;
                }
            };
            self.dispatch(&tree, node, interface, member, &message.body)
        };
        match answer {
            Some(body) => {
                let _ = self.bus.reply(message, body);
            }
            None => {
                let _ = self.bus.refuse(
                    message,
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "this application does not implement that",
                );
            }
        }
    }

    /// One method or property, or `None` for one we do not implement.
    fn dispatch(
        &self,
        tree: &Tree,
        node: &Node,
        interface: &str,
        member: &str,
        body: &[Value],
    ) -> Option<Vec<Value>> {
        match interface {
            "org.freedesktop.DBus.Properties" => self.property(tree, node, member, body),
            "org.freedesktop.DBus.Introspectable" if member == "Introspect" => {
                Some(vec![Value::Str(INTROSPECTION.to_owned())])
            }
            "org.a11y.atspi.Accessible" => self.accessible(tree, node, member, body),
            "org.a11y.atspi.Application" => Self::application(member),
            "org.a11y.atspi.Component" => self.component(tree, node, member, body),
            "org.a11y.atspi.Text" => Self::text(node, member, body),
            _ => None,
        }
    }

    /// `Get` and `GetAll`, which is how AT-SPI reads everything cheap.
    fn property(
        &self,
        tree: &Tree,
        node: &Node,
        member: &str,
        body: &[Value],
    ) -> Option<Vec<Value>> {
        let asked = body.first().and_then(Value::text).unwrap_or_default().to_owned();
        match member {
            "Get" => {
                let wanted = body.get(1).and_then(Value::text)?;
                let value = self.read(tree, node, &asked, wanted)?;
                Some(vec![Value::Variant(Box::new(value))])
            }
            "GetAll" => {
                let names: &[&str] = match asked.as_str() {
                    "org.a11y.atspi.Accessible" => {
                        &["Name", "Description", "Parent", "ChildCount", "Locale", "AccessibleId"]
                    }
                    "org.a11y.atspi.Application" => {
                        &["ToolkitName", "Version", "AtspiVersion", "Id"]
                    }
                    "org.a11y.atspi.Text" => &["CharacterCount", "CaretOffset"],
                    _ => &[],
                };
                let pairs = names
                    .iter()
                    .filter_map(|name| {
                        self.read(tree, node, &asked, name).map(|it| ((*name).to_owned(), it))
                    })
                    .collect();
                Some(vec![Value::Dict(pairs)])
            }
            // A property nobody may set: everything here is a function of what is
            // on screen, and an assistive technology that changed one would be
            // told a lie by the next frame.
            "Set" => Some(Vec::new()),
            _ => None,
        }
    }

    /// One property's value.
    fn read(&self, tree: &Tree, node: &Node, interface: &str, name: &str) -> Option<Value> {
        match (interface, name) {
            (_, "Name") => Some(Value::Str(node.name.clone())),
            (_, "Description") => Some(Value::Str(node.description.clone())),
            (_, "Locale") => Some(Value::Str(locale())),
            (_, "AccessibleId") => Some(Value::Str(node.id.to_string())),
            (_, "Parent") => Some(
                tree.parent_of(node.id)
                    .map_or_else(|| self.nothing(), |(at, _)| self.reference(at)),
            ),
            (_, "ChildCount") => Some(Value::I32(i32::try_from(node.children.len()).unwrap_or(0))),
            (_, "ToolkitName") => Some(Value::Str("skerry".to_owned())),
            (_, "Version") => Some(Value::Str(env!("CARGO_PKG_VERSION").to_owned())),
            // The version of the *protocol*, which is what the registry checks.
            (_, "AtspiVersion") => Some(Value::Str("2.1".to_owned())),
            (_, "Id") => Some(Value::I32(0)),
            (_, "CharacterCount") => {
                Some(Value::I32(count_of(node.text.as_ref().map_or("", |it| &it.value))))
            }
            (_, "CaretOffset") => Some(Value::I32(node.text.as_ref().map_or(0, |it| it.caret))),
            _ => None,
        }
    }

    /// `org.a11y.atspi.Accessible`.
    fn accessible(
        &self,
        tree: &Tree,
        node: &Node,
        member: &str,
        body: &[Value],
    ) -> Option<Vec<Value>> {
        match member {
            "GetRole" => Some(vec![Value::U32(node.role as u32)]),
            "GetRoleName" | "GetLocalizedRoleName" => {
                Some(vec![Value::Str(node.role.spelled().to_owned())])
            }
            "GetState" => {
                let low = u32::try_from(node.states & 0xffff_ffff).unwrap_or(0);
                let high = u32::try_from(node.states >> 32).unwrap_or(0);
                Some(vec![Value::Array(vec![Value::U32(low), Value::U32(high)])])
            }
            "GetChildAtIndex" => {
                let at = usize::try_from(body.first()?.number()?).ok()?;
                Some(vec![
                    node.children.get(at).map_or_else(|| self.nothing(), |id| self.reference(*id)),
                ])
            }
            "GetChildren" => Some(vec![Value::Array(
                node.children.iter().map(|id| self.reference(*id)).collect(),
            )]),
            "GetIndexInParent" => Some(vec![Value::I32(
                tree.parent_of(node.id).and_then(|(_, at)| i32::try_from(at).ok()).unwrap_or(-1),
            )]),
            "GetApplication" => Some(vec![self.reference(0)]),
            "GetInterfaces" => Some(vec![Value::Array(
                node.interfaces().into_iter().map(|it| Value::Str(it.to_owned())).collect(),
            )]),
            // Nothing relates to anything else here, and an empty set still has a
            // signature — see `Value::Empty`.
            "GetRelationSet" => Some(vec![Value::Empty("(ua(so))".to_owned())]),
            "GetAttributes" => Some(vec![Value::Empty("{ss}".to_owned())]),
            _ => None,
        }
    }

    /// `org.a11y.atspi.Application`.
    fn application(member: &str) -> Option<Vec<Value>> {
        match member {
            "GetLocale" => Some(vec![Value::Str(locale())]),
            // The registry sends these to every application and ignores the
            // answer; refusing them fills its log with our name.
            "RegisterEventListener" | "DeregisterEventListener" => Some(Vec::new()),
            _ => None,
        }
    }

    /// `org.a11y.atspi.Component`.
    fn component(
        &self,
        tree: &Tree,
        node: &Node,
        member: &str,
        body: &[Value],
    ) -> Option<Vec<Value>> {
        let it = node.extents;
        match member {
            "GetExtents" => Some(vec![Value::Struct(vec![
                Value::I32(it.x),
                Value::I32(it.y),
                Value::I32(it.width),
                Value::I32(it.height),
            ])]),
            "GetPosition" => Some(vec![Value::Struct(vec![Value::I32(it.x), Value::I32(it.y)])]),
            "GetSize" => {
                Some(vec![Value::Struct(vec![Value::I32(it.width), Value::I32(it.height)])])
            }
            // Layer 3 is `WIDGET`, which everything this window draws is.
            "GetLayer" => Some(vec![Value::U32(3)]),
            "Contains" => {
                let x = i32::try_from(body.first()?.number()?).ok()?;
                let y = i32::try_from(body.get(1)?.number()?).ok()?;
                let inside = x >= it.x && y >= it.y && x < it.x + it.width && y < it.y + it.height;
                Some(vec![Value::Bool(inside)])
            }
            "GetAccessibleAtPoint" => {
                let x = i32::try_from(body.first()?.number()?).ok()?;
                let y = i32::try_from(body.get(1)?.number()?).ok()?;
                Some(vec![
                    tree.at_point(x, y).map_or_else(|| self.nothing(), |id| self.reference(id)),
                ])
            }
            // **Refused rather than faked.** Focus is what the keyboard is on,
            // and moving it from outside is a command this program has no way to
            // route through its own registry — ADR 0026 names it absent.
            "GrabFocus" => Some(vec![Value::Bool(false)]),
            _ => None,
        }
    }

    /// `org.a11y.atspi.Text`, in character offsets throughout.
    fn text(node: &Node, member: &str, body: &[Value]) -> Option<Vec<Value>> {
        let text = node.text.as_ref()?;
        let characters: Vec<char> = text.value.chars().collect();
        let length = i32::try_from(characters.len()).unwrap_or(i32::MAX);
        match member {
            "GetText" => {
                let from = i32::try_from(body.first()?.number()?).ok()?.clamp(0, length);
                let to = i32::try_from(body.get(1)?.number()?).ok()?;
                let to = if to < 0 { length } else { to.clamp(from, length) };
                let slice: String = characters
                    [usize::try_from(from).unwrap_or(0)..usize::try_from(to).unwrap_or(0)]
                    .iter()
                    .collect();
                Some(vec![Value::Str(slice)])
            }
            "GetCharacterCount" => Some(vec![Value::I32(length)]),
            "GetCaretOffset" => Some(vec![Value::I32(text.caret)]),
            "GetNSelections" => Some(vec![Value::I32(i32::from(text.selection.is_some()))]),
            "GetSelection" => {
                let (from, to) = text.selection.unwrap_or((0, 0));
                Some(vec![Value::Struct(vec![Value::I32(from), Value::I32(to)])])
            }
            // The whole line, whatever granularity was asked for: this window has
            // one line per node, so word and sentence and line are the same
            // answer and a smaller one would be a lie about where it ends.
            "GetTextAtOffset" | "GetStringAtOffset" => Some(vec![Value::Struct(vec![
                Value::Str(text.value.clone()),
                Value::I32(0),
                Value::I32(length),
            ])]),
            _ => None,
        }
    }
}

/// One AT-SPI event, in the `siiva{sv}` shape every one of them has.
fn event(
    path: &str,
    family: &str,
    member: &str,
    detail: (&str, i32, i32),
    any: Value,
    sender: Value,
) -> Message {
    let (kind, one, two) = detail;
    Message::signal(
        path,
        &format!("org.a11y.atspi.Event.{family}"),
        member,
        vec![
            Value::Str(kind.to_owned()),
            Value::I32(one),
            Value::I32(two),
            any,
            Value::Dict(vec![("sender".to_owned(), sender)]),
        ],
    )
}

/// The object path one node answers on.
fn path_of(id: u32) -> String {
    if id == 0 {
        return ROOT.to_owned();
    }
    format!("/org/a11y/atspi/accessible/{id}")
}

/// Which node a path names, if it is one of ours.
fn id_of(path: &str) -> Option<u32> {
    if path == ROOT {
        return Some(0);
    }
    path.strip_prefix("/org/a11y/atspi/accessible/")?.parse().ok()
}

/// How many characters, which is what every offset in AT-SPI counts.
fn count_of(text: &str) -> i32 {
    i32::try_from(text.chars().count()).unwrap_or(i32::MAX)
}

/// The locale, as the environment states it.
fn locale() -> String {
    for name in ["LC_MESSAGES", "LC_ALL", "LANG"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return value;
        }
    }
    "C".to_owned()
}

/// What `busctl introspect` gets. Hand-written, because the interfaces are
/// somebody else's and the list of what we answer is the point.
const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.a11y.atspi.Accessible">
    <property name="Name" type="s" access="read"/>
    <property name="Description" type="s" access="read"/>
    <property name="Parent" type="(so)" access="read"/>
    <property name="ChildCount" type="i" access="read"/>
    <method name="GetChildAtIndex"><arg type="i" direction="in"/><arg type="(so)" direction="out"/></method>
    <method name="GetChildren"><arg type="a(so)" direction="out"/></method>
    <method name="GetIndexInParent"><arg type="i" direction="out"/></method>
    <method name="GetRole"><arg type="u" direction="out"/></method>
    <method name="GetRoleName"><arg type="s" direction="out"/></method>
    <method name="GetState"><arg type="au" direction="out"/></method>
    <method name="GetApplication"><arg type="(so)" direction="out"/></method>
    <method name="GetInterfaces"><arg type="as" direction="out"/></method>
  </interface>
  <interface name="org.a11y.atspi.Component">
    <method name="GetExtents"><arg type="u" direction="in"/><arg type="(iiii)" direction="out"/></method>
    <method name="GetPosition"><arg type="u" direction="in"/><arg type="(ii)" direction="out"/></method>
    <method name="GetSize"><arg type="(ii)" direction="out"/></method>
    <method name="GetAccessibleAtPoint"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="in"/><arg type="(so)" direction="out"/></method>
  </interface>
  <interface name="org.a11y.atspi.Text">
    <method name="GetText"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="s" direction="out"/></method>
    <method name="GetCaretOffset"><arg type="i" direction="out"/></method>
    <method name="GetCharacterCount"><arg type="i" direction="out"/></method>
  </interface>
</node>"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{ROOT, count_of, id_of, path_of};

    /// A path names a node, and the root is spelled differently from the rest.
    #[test]
    fn a_path_and_a_node_number_are_the_same_thing_twice() {
        assert_eq!(path_of(0), ROOT);
        assert_eq!(id_of(ROOT), Some(0));
        for id in [1u32, 7, 4_000_000_000] {
            assert_eq!(id_of(&path_of(id)), Some(id), "{id}");
        }
        assert_eq!(id_of("/org/a11y/atspi/accessible/nope"), None);
        assert_eq!(id_of("/somewhere/else"), None);
    }

    /// Offsets are characters, because that is what every one of them means.
    #[test]
    fn a_count_is_characters_and_not_bytes() {
        assert_eq!(count_of("abc"), 3);
        assert_eq!(count_of("Žofia"), 5);
        assert_eq!(count_of("日本語"), 3);
    }
}
