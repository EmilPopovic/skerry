// SPDX-License-Identifier: Apache-2.0
//! What this window answers on the bus, and what a second instance does instead.
//!
//! Two halves of one name.
//!
//! * **`dev.skerry.Skerry`** is the `app_id` the window already sets, and owning
//!   it is what makes a second `skerry /some/folder` a *request* rather than a
//!   second window: the one that finds the name taken hands its folder to the one
//!   that has it and exits. A second `skerry` naming nothing is a second window —
//!   [ADR 0044](../../../docs/adr/0044-a-launch-that-names-nothing-is-a-new-window.md).
//! * **`org.freedesktop.FileManager1`** is how the rest of the desktop says *show
//!   me this file* — a browser's Open containing folder, an editor's Reveal, a
//!   notification's Show in folder. Asked for without replacing an owner: Nautilus
//!   may hold it, and taking it from a running file manager because we started is
//!   not ours to do.
//!
//! The thread here answers the bus and never touches the window:
//! [ADR 0023](../../../docs/adr/0023-d-bus-is-a-socket-we-speak-ourselves.md) is
//! that serving is a thread blocking on a read, and what a message *means* is the
//! frame loop's, one channel away.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use sk_desktop::bus::{Bus, Kind, Message, Named, Value};
use sk_util::WakeHandle;

/// Our own name, which is the window's `app_id`.
pub const NAME: &str = "dev.skerry.Skerry";

/// The basename of the `.desktop` file this program installs.
///
/// **The same string as [`NAME`] with a suffix**, because a desktop matches an
/// application to its window by string equality between the `app_id`, the bus
/// name and this file's basename — see
/// [08-roadmap.md](../../../docs/08-roadmap.md#m7--4-wk--desktop-citizenship).
pub const DESKTOP_ID: &str = "dev.skerry.Skerry.desktop";

/// The desktop's name for a file manager.
const FILE_MANAGER: &str = "org.freedesktop.FileManager1";

/// Its object, which the specification fixes.
const OBJECT: &str = "/org/freedesktop/FileManager1";

/// What somebody on the bus asked this window to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ask {
    /// Show these folders — open each in a tab.
    Folders(Vec<PathBuf>),
    /// Show these files — open the folder each is in, with it selected.
    Items(Vec<PathBuf>),
    /// Show what these files *are*, which is the Properties panel.
    Properties(Vec<PathBuf>),
}

/// One request, with the token that says who asked.
///
/// **The startup id is an activation token.** `FileManager1` passes one as its
/// second argument and a desktop fills it in from whatever the person clicked;
/// on Wayland it is the only thing that lets this window come forward, because
/// `xdg_activation_v1` exists so that a client cannot raise itself.
pub struct Asked {
    /// What to do.
    pub ask: Ask,
    /// The token, where the caller passed one.
    pub token: Option<String>,
}

impl Ask {
    /// The paths it names.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        match self {
            Self::Folders(paths) | Self::Items(paths) | Self::Properties(paths) => paths,
        }
    }
}

/// What the bus had to say about this process's names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Standing {
    /// This process owns the name, and calls arrive here.
    First,
    /// Another instance owns it, and has been given our arguments.
    Second,
    /// Another instance owns it and nothing was named, so this is a window of
    /// its own, as `--new-window` makes: no names, and a session slot apart.
    Another,
    /// There is no bus, or it would not have us. Everything else still works.
    Alone,
}

/// Takes the name, or hands over to whoever has it.
///
/// **Before the window, which is why it is synchronous.** A second instance that
/// opened a window and closed it again would flash one, and the decision needs the
/// bus's answer — half a millisecond, measured, against a cold start of ninety.
/// The same place the session file is read, and for the same reason: there is no
/// frame to block yet.
///
/// `paths` is what this process was asked to open, which is what gets handed over.
#[must_use]
pub fn take_the_name(path: Option<&std::path::Path>) -> (Standing, Option<Bus>) {
    let paths: Vec<PathBuf> = path.map(std::path::Path::to_path_buf).into_iter().collect();
    let Some(mut bus) = Bus::session() else { return (Standing::Alone, None) };
    match bus.request_name(NAME) {
        Named::Ours => ours(bus),
        Named::Already => {
            // **A launch that names nothing asks for a window.** Handing it over
            // asked the running one to come forward with no token to do it on,
            // which a Wayland compositor refuses — so clicking the launcher a
            // second time did nothing anyone could see.
            if paths.is_empty() && answers(&mut bus) {
                return (Standing::Another, Some(bus));
            }
            if !paths.is_empty() && hand_over(&mut bus, &paths) {
                return (Standing::Second, None);
            }
            // **Asked for a second time, because the first answer was about a
            // process that is dying.** A relaunch a second after a quit finds the
            // name still registered — the owner's socket has not finished closing
            // — and the hand-over then fails against a peer that is already gone.
            // Without this, that window runs nameless for its whole life and the
            // desktop's *show me this file* goes nowhere for the rest of the
            // session. Measured: one restart in four on this machine.
            match bus.request_name(NAME) {
                Named::Ours => ours(bus),
                _ => (Standing::Alone, None),
            }
        }
        Named::Refused => (Standing::Alone, None),
    }
}

/// An independent window retains a unique bus connection without owning global names.
#[must_use]
pub fn independent_bus() -> Option<Bus> {
    Bus::session()
}

/// The name is ours, so ask for the desktop's one too and keep the connection.
///
/// Best effort, and its own answer: a desktop with Nautilus running has an owner
/// for `FileManager1` already, and a file manager that refused to start because
/// of that would be a file manager nobody can try out.
fn ours(mut bus: Bus) -> (Standing, Option<Bus>) {
    let _ = bus.request_name(FILE_MANAGER);
    (Standing::First, Some(bus))
}

/// Whether the desktop says it prefers a dark appearance.
///
/// **The freedesktop settings portal**, which is the one answer GNOME, KDE and
/// the portal-backed compositors all give — `org.freedesktop.appearance`'s
/// `color-scheme`, where 1 is dark and 2 is light and 0 is "no preference".
/// `None` where there is no portal, which is most of a bare Sway session and is
/// not an error: nothing changes, and the theme is what the flag or the file says.
///
/// **Read once, at startup, on the connection we already have** — 0.15 ms p50
/// over fifty reads against this machine's `xdg-desktop-portal`, worst 0.99, on
/// a socket that is open either way, against a cold start of ninety. Following the
/// setting as it changes needs a subscription, and a subscribing connection is
/// the successor ADR 0023's revisit trigger names — so a desktop that switches to
/// dark while this window is open is a window that is still light, and a person
/// who cares presses `Ctrl+Shift+D`.
#[must_use]
pub fn desktop_prefers_dark(bus: &mut Bus) -> Option<bool> {
    if sk_util::disabled("portal") {
        return None;
    }
    let call = Message::call(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
        "Read",
        vec![
            Value::Str("org.freedesktop.appearance".to_owned()),
            Value::Str("color-scheme".to_owned()),
        ],
    );
    // Doubly wrapped: `Read` answers `v`, and the value inside it is itself the
    // variant the namespace stores. `Value::number` unwraps both.
    match bus.call(call)?.body.first()?.number()? {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// What the desktop says its icon theme is called.
///
/// **The same portal and the same connection as the colour scheme above**, under
/// `org.gnome.desktop.interface`'s `icon-theme` — which is the key GTK itself
/// reads through the portal and the one KDE's portal backend answers too. `None`
/// where there is no portal, which is not an error: `sk_desktop::icons` reads the
/// desktop's own settings files next, and `hicolor` after that.
///
/// **Read once, at start-up** — [ADR 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).
/// Following the setting as it changes needs a subscription, and a subscribing
/// connection is the successor ADR 0023's revisit trigger names.
#[must_use]
pub fn desktop_icon_theme(bus: &mut Bus) -> Option<String> {
    if sk_util::disabled("portal") {
        return None;
    }
    let call = Message::call(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
        "Read",
        vec![
            Value::Str("org.gnome.desktop.interface".to_owned()),
            Value::Str("icon-theme".to_owned()),
        ],
    );
    // Doubly wrapped, as the colour scheme is: `Read` answers `v`, and the value
    // inside it is the variant the namespace stores.
    let name = bus.call(call)?.body.first()?.text()?.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// Whether the instance that owns the name is alive enough to answer.
///
/// The question the hand-over used to answer as a side effect: a name whose
/// owner is still closing its socket is a name about to be free, and taking it
/// then is better than a second window beside a first that is already gone.
fn answers(bus: &mut Bus) -> bool {
    let call =
        Message::call(NAME, OBJECT, "org.freedesktop.DBus.Introspectable", "Introspect", vec![]);
    bus.call(call).is_some()
}

/// Asks the running instance to show `paths`. Whether it answered.
fn hand_over(bus: &mut Bus, paths: &[PathBuf]) -> bool {
    let uris = paths.iter().map(|path| Value::Str(sk_desktop::file_uri(path))).collect();
    let call = Message::call(
        NAME,
        OBJECT,
        FILE_MANAGER,
        "ShowFolders",
        vec![
            Value::Array(uris),
            // The startup id, which is how a desktop hands focus over. We have none
            // to pass on, and an empty string is what the specification says to send.
            Value::Str(String::new()),
        ],
    );
    bus.call(call).is_some()
}

/// Answers the bus until it goes away.
///
/// **Its own thread, blocking on a read.** Every message is answered here — a
/// caller waiting on a reply must not wait for a frame — and what the window has
/// to do about it goes down the channel with a wake-up, which is the same eventfd
/// every job uses.
pub fn serve(mut bus: Bus, asked: Sender<Asked>, wake: WakeHandle) {
    let _ = std::thread::Builder::new().name("skerry-bus".to_owned()).spawn(move || {
        while let Some(message) = bus.receive() {
            if message.kind != Kind::Call {
                continue;
            }
            let Some(member) = message.member.clone() else { continue };
            if message.interface.as_deref() == Some("org.freedesktop.DBus.Introspectable")
                && member == "Introspect"
            {
                let _ = bus.reply(&message, vec![Value::Str(INTROSPECTION.to_owned())]);
                continue;
            }
            let paths = paths_of(&message);
            let ask = match member.as_str() {
                "ShowFolders" => Some(Ask::Folders(paths)),
                "ShowItems" => Some(Ask::Items(paths)),
                "ShowItemProperties" => Some(Ask::Properties(paths)),
                _ => None,
            };
            // The second argument of all three methods, and empty is the value
            // the specification tells a caller with nothing to pass.
            let token = message
                .body
                .get(1)
                .and_then(Value::text)
                .filter(|it| !it.is_empty())
                .map(str::to_owned);
            let Some(ask) = ask else {
                let _ = bus.refuse(
                    &message,
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "this file manager does not have that method",
                );
                continue;
            };
            // **Answered before the window has acted on it.** The reply says the
            // request arrived, which is all `FileManager1` promises — a caller
            // that waited for the folder to be on screen would be waiting for a
            // read of a directory it does not know the size of.
            let _ = bus.reply(&message, Vec::new());
            if asked.send(Asked { ask, token }).is_err() {
                return;
            }
            wake.wake();
        }
    });
}

/// The `file://` URIs in a call's first argument, as paths.
///
/// Anything that is not a local file — a `sftp://` URI from a desktop that has
/// `GVfs`, say — is dropped rather than guessed at, which is
/// [rule 9](../../../docs/03-principles.md) again: reaching one needs the thing
/// this program does not link.
fn paths_of(message: &Message) -> Vec<PathBuf> {
    message
        .body
        .first()
        .map(Value::items)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.text())
        .filter_map(sk_plat::decode_uri)
        .collect()
}

/// What `busctl introspect` gets, which is how a person finds out we answer.
///
/// Hand-written because it is a constant: the interface is somebody else's and it
/// has three methods.
const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"><arg name="xml" type="s" direction="out"/></method>
  </interface>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg name="URIs" type="as" direction="in"/>
      <arg name="StartupId" type="s" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg name="URIs" type="as" direction="in"/>
      <arg name="StartupId" type="s" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg name="URIs" type="as" direction="in"/>
      <arg name="StartupId" type="s" direction="in"/>
    </method>
  </interface>
</node>"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Ask, Message, Value, paths_of};

    fn call(member: &str, uris: &[&str]) -> Message {
        Message::call(
            super::NAME,
            super::OBJECT,
            super::FILE_MANAGER,
            member,
            vec![
                Value::Array(uris.iter().map(|uri| Value::Str((*uri).to_owned())).collect()),
                Value::Str(String::new()),
            ],
        )
    }

    /// The URIs a caller sends become paths, and the ones we cannot open do not.
    #[test]
    fn a_call_names_the_local_files_it_asked_about() {
        let asked = call(
            "ShowItems",
            &[
                "file:///home/tester/a%20file.txt",
                "sftp://box.example/srv/thing",
                "file:///tmp",
                "not a uri at all",
            ],
        );
        let paths = paths_of(&asked);
        assert_eq!(
            paths,
            [std::path::PathBuf::from("/home/tester/a file.txt"), std::path::PathBuf::from("/tmp"),]
        );
        assert_eq!(Ask::Items(paths.clone()).paths(), paths);
    }

    /// A call with no array, or an empty one, names nothing rather than panicking.
    #[test]
    fn a_call_with_nothing_in_it_names_nothing() {
        assert!(paths_of(&call("ShowFolders", &[])).is_empty());
        let bare = Message::call(
            super::NAME,
            super::OBJECT,
            super::FILE_MANAGER,
            "ShowFolders",
            Vec::new(),
        );
        assert!(paths_of(&bare).is_empty());
    }

    /// The introspection is well formed enough to name the three methods, which
    /// is what a desktop reads it for.
    #[test]
    fn the_introspection_names_what_this_answers() {
        for member in ["ShowFolders", "ShowItems", "ShowItemProperties", "Introspect"] {
            assert!(super::INTROSPECTION.contains(member), "{member}");
        }
        assert_eq!(super::INTROSPECTION.matches("<node>").count(), 1);
        assert_eq!(
            super::INTROSPECTION.matches("<interface").count(),
            super::INTROSPECTION.matches("</interface>").count()
        );
    }
}
