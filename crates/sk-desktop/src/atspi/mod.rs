// SPDX-License-Identifier: MIT OR Apache-2.0
//! Accessibility: the tree a screen reader reads, published over AT-SPI.
//!
//! **[ADR 0026](../../../../docs/adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md).**
//! AT-SPI is D-Bus and nothing else, and [ADR
//! 0023](../../../../docs/adr/0023-d-bus-is-a-socket-we-speak-ourselves.md)
//! already built a D-Bus. So this is a second connection — to the accessibility
//! bus, whose address the session bus hands out — on a thread of its own, which
//! answers an assistive technology's questions from a snapshot the frame loop
//! publishes and emits the signals that say what changed.
//!
//! # What the frame loop does, and what it must not
//!
//! It builds a [`Tree`], puts it behind the mutex with [`Accessibility::publish`]
//! and returns. Every byte on the socket is written by this thread, because
//! [rule 1](../../../../docs/03-principles.md) is that the loop blocks on
//! nothing — and a daemon that has stopped reading is exactly the thing it would
//! block on.
//!
//! # Absent is the common case
//!
//! No accessibility bus, no registry, or a refused `Embed`: [`Accessibility::start`]
//! answers `None`, nothing is spawned, and the window is what it was. A desktop
//! with no assistive technology running must pay nothing, which is
//! [ADR 0007](../../../../docs/adr/0007-runtime-capability-detection.md).

mod serve;
pub mod tree;

use std::sync::{Arc, Mutex};

pub use tree::{Change, Node, Rect, Role, Text, Tree, state};

use crate::bus::{Bus, Message, Value};

/// The published tree and the thread that answers questions about it.
pub struct Accessibility {
    /// What the last frame said is on screen.
    tree: Arc<Mutex<Tree>>,
    /// Signals the thread has not sent yet.
    pending: Arc<Mutex<Vec<Change>>>,
    /// The thread's own eventfd: written when there is something new.
    wake: Arc<std::os::fd::OwnedFd>,
}

impl Accessibility {
    /// Connects, registers with the registry, and starts answering.
    ///
    /// `None` where there is no accessibility bus to talk to, which is a desktop
    /// with no assistive technology on it — the common case, and it costs one
    /// method call on a connection that is already open.
    #[must_use]
    pub fn start(name: &str) -> Option<Self> {
        let address = address()?;
        let mut bus = Bus::connect(&address)?;
        let unique = bus.unique().to_owned();
        // **`Embed` is the registration.** An application that skips it is on the
        // bus and in nobody's tree: the registry learns about us here and asks
        // its questions afterwards, on this same connection.
        let plug =
            Value::Struct(vec![Value::Str(unique.clone()), Value::Path(serve::ROOT.to_owned())]);
        let embed = Message::call(
            "org.a11y.atspi.Registry",
            "/org/a11y/atspi/accessible/root",
            "org.a11y.atspi.Socket",
            "Embed",
            vec![plug],
        );
        let embedded = bus.call(embed)?;
        // The registry answers with the desktop's own reference. Nothing here
        // needs it — the application's parent is reported as the desktop by
        // path — but a reply that never came means the registry is not there,
        // and then neither is any of this.
        let _ = embedded;
        let wake = Arc::new(
            rustix::event::eventfd(
                0,
                rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
            )
            .ok()?,
        );
        let tree = Arc::new(Mutex::new(Tree::default()));
        let pending = Arc::new(Mutex::new(Vec::new()));
        serve::spawn(
            bus,
            unique,
            name.to_owned(),
            Arc::clone(&tree),
            Arc::clone(&pending),
            Arc::clone(&wake),
        );
        Some(Self { tree, pending, wake })
    }

    /// Publishes what the frame just drew, and queues what changed.
    ///
    /// **The diff happens here and the writing does not.** Comparing two trees is
    /// arithmetic the frame loop can afford; marshalling a signal and writing it
    /// to a socket is not.
    pub fn publish(&self, next: Tree) {
        let Ok(mut held) = self.tree.lock() else { return };
        let changed = tree::changes(&held, &next);
        *held = next;
        drop(held);
        if changed.is_empty() {
            return;
        }
        if let Ok(mut queue) = self.pending.lock() {
            queue.extend(changed);
        }
        let _ = rustix::io::write(&*self.wake, &1u64.to_ne_bytes());
    }

    /// Whether anything is published yet, for a diagnostic report.
    #[must_use]
    pub fn nodes(&self) -> usize {
        self.tree.lock().map_or(0, |tree| tree.nodes.len())
    }
}

/// Where the accessibility bus is, if this session has one.
///
/// Two questions on the session bus: whether accessibility is enabled at all,
/// and the address. Asked once, on a short-lived connection — the same shape
/// every other call in [`crate::bus`] has.
fn address() -> Option<String> {
    if let Some(address) = std::env::var_os("AT_SPI_BUS_ADDRESS")
        .map(|it| it.to_string_lossy().into_owned())
        .filter(|it| !it.is_empty())
    {
        return Some(address);
    }
    let mut session = Bus::session()?;
    let ask =
        Message::call("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", Vec::new());
    let reply = session.call(ask)?;
    reply.text().map(str::to_owned).filter(|it| !it.is_empty())
}
