// SPDX-License-Identifier: Apache-2.0
//! Telling the desktop that something finished, when the window cannot.
//!
//! **Two method calls on the session bus this program already speaks** —
//! `org.freedesktop.Notifications.Notify`, and `GetCapabilities` before it,
//! because whether the body is read as markup is the daemon's to say. [ADR
//! 0023](../../../docs/adr/0023-d-bus-is-a-socket-we-speak-ourselves.md)'s
//! short-lived connection per call is the right shape for it and its revisit
//! trigger — a feature that needs to *receive* signals — is not fired, which is
//! the whole reason this is the shape it is.
//!
//! # No actions, and that is a decision rather than an omission
//!
//! A notification with a *Show in folder* button is a notification that has to
//! hear `ActionInvoked`, which is a signal on a connection that lives. There is
//! no such connection, so there are no buttons: a notification here says what
//! happened and is closed by whoever reads it. The reply carries the daemon's id
//! for the notification and this drops it, because the only thing an id is for
//! is replacing or closing one later — both of which are the same missing
//! subscription in a different hat.
//!
//! # Does not own when to send one
//!
//! Which operation is long enough and whether the window has the keyboard are
//! `sk-app`'s questions: focus comes from `sk-plat`, and this crate owns no
//! policy about when to do something. What is here is the call and the probe.

use crate::bus::wire::Value;
use crate::bus::{Bus, Message};

/// The service, its object, and its one interface.
const SERVICE: &str = "org.freedesktop.Notifications";
const OBJECT: &str = "/org/freedesktop/Notifications";

/// Why the desktop was not told.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// There is no session bus, which is a session with no D-Bus at all.
    NoBus,
    /// The bus is there and nothing on it serves notifications — a bare
    /// compositor with no daemon, or one the person turned off.
    NoDaemon,
    /// The daemon answered with an error.
    Failed(String),
}

impl Refused {
    /// The detail for the log, which is the chain rather than a sentence.
    ///
    /// **There is no `label_key` here on purpose.** Nothing a person did failed:
    /// the operation finished and the window said so. A machine with no
    /// notification daemon gets the status line and no second sentence about a
    /// facility it does not have.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::NoBus => "no session bus",
            Self::NoDaemon => "no owner for org.freedesktop.Notifications",
            Self::Failed(it) => it,
        }
    }
}

/// Whether this machine has something to send a notification to.
///
/// **Probed, never compiled in** — [ADR
/// 0007](../../../docs/adr/0007-runtime-capability-detection.md). Two questions
/// to the bus itself rather than one to the daemon: `GetServerInformation` would
/// answer as well and would *start* a daemon that is merely activatable, which
/// is a side effect a probe may not have.
///
/// An activatable name counts as present, because the `Notify` that follows is
/// what starts it and the bus queues the call behind the activation.
#[must_use]
pub fn available() -> bool {
    if sk_util::disabled("notifications") {
        return false;
    }
    let Some(mut bus) = Bus::session() else { return false };
    owned(&mut bus) || activatable(&mut bus)
}

fn owned(bus: &mut Bus) -> bool {
    let answer = bus.call(Message::call(
        crate::bus::BUS.0,
        crate::bus::BUS.1,
        crate::bus::BUS.2,
        "NameHasOwner",
        vec![Value::Str(SERVICE.to_owned())],
    ));
    matches!(answer.as_ref().and_then(|it| it.body.first()), Some(Value::Bool(true)))
}

fn activatable(bus: &mut Bus) -> bool {
    let Some(answer) = bus.call(Message::call(
        crate::bus::BUS.0,
        crate::bus::BUS.1,
        crate::bus::BUS.2,
        "ListActivatableNames",
        Vec::new(),
    )) else {
        return false;
    };
    match answer.body.first() {
        Some(Value::Array(names)) => names.iter().any(|it| it.text() == Some(SERVICE)),
        _ => false,
    }
}

/// Tells the desktop that `summary` happened, with `body` under it.
///
/// **Runs on a job**: a connection, an authentication and two round trips, and
/// the daemon at the other end may be doing anything at all.
///
/// `icon` is a themed icon name — ours is the desktop entry's `Icon=` — and the
/// `desktop-entry` hint is what lets a daemon that ignores the icon still find
/// the application this came from. `-1` for the timeout is the specification's
/// "however long the daemon thinks", which is the only honest answer: how long a
/// person needs to read something is the desktop's setting and not ours.
///
/// # Errors
///
/// [`Refused`], whose detail is for the log. There is nothing here to tell a
/// person: the window already said what finished.
pub fn send(
    application: &str,
    desktop_id: &str,
    icon: &str,
    summary: &str,
    body: &str,
) -> Result<(), Refused> {
    if sk_util::disabled("notifications") {
        return Err(Refused::NoDaemon);
    }
    let mut bus = Bus::session().ok_or(Refused::NoBus)?;
    tell(&mut bus, application, desktop_id, icon, summary, body)
}

/// [`send`], on a connection that is already open.
fn tell(
    bus: &mut Bus,
    application: &str,
    desktop_id: &str,
    icon: &str,
    summary: &str,
    body: &str,
) -> Result<(), Refused> {
    let body = if reads_markup(bus) { escaped(body) } else { body.to_owned() };
    let call = Message::call(
        SERVICE,
        OBJECT,
        SERVICE,
        "Notify",
        vec![
            Value::Str(application.to_owned()),
            // **Zero, which means "a new one".** Replacing the previous
            // notification needs its id kept across calls, and an id is only
            // worth keeping on a connection that can also hear it being closed.
            Value::U32(0),
            Value::Str(icon.to_owned()),
            Value::Str(summary.to_owned()),
            Value::Str(body),
            Value::Empty("s".to_owned()),
            Value::Dict(vec![(
                "desktop-entry".to_owned(),
                Value::Str(desktop_id.trim_end_matches(".desktop").to_owned()),
            )]),
            Value::I32(-1),
        ],
    );
    match bus.call_for(call) {
        Ok(_) => Ok(()),
        Err(error) if error.contains("ServiceUnknown") || error.contains("NameHasNoOwner") => {
            Err(Refused::NoDaemon)
        }
        Err(error) => Err(Refused::Failed(error)),
    }
}

/// Whether the daemon reads markup in a body.
///
/// **Asked, because escaping is right only where markup is read.** The
/// Desktop Notifications specification, 1.3, on the `body-markup` capability:
/// "If marked up text is sent to a server that does not give this cap, the
/// markup will show through as regular text" — and so would an escape, as a
/// literal `&amp;` in the middle of a folder's name.
fn reads_markup(bus: &mut Bus) -> bool {
    let asked = Message::call(SERVICE, OBJECT, SERVICE, "GetCapabilities", Vec::new());
    bus.call(asked).is_some_and(|reply| {
        let capabilities = reply.body.first().map(Value::items).unwrap_or_default();
        capabilities.iter().any(|it| it.text() == Some("body-markup"))
    })
}

/// `text`, with what markup would read as markup spelled as entities.
///
/// **`&`, `<` and `>`, and not the quotes.** The specification calls its markup
/// XML-based and names no escaping of its own; in XML text `&` and `<` must be
/// escaped and `>` may be, while a quote means something only inside an
/// attribute, which a body written here never has.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Refused, SERVICE, tell};
    use crate::bus::wire::Value;
    use crate::bus::{Bus, Message, marshal, unmarshal};

    /// The argument list the specification spells `susssasa{sv}i`, checked
    /// against what the marshaller would put in the header — because a body that
    /// disagrees with its own signature is a call every daemon refuses and a
    /// mistake no unit test sees until one is running.
    #[test]
    fn the_call_matches_the_specifications_signature() {
        let call = Message::call(
            SERVICE,
            "/org/freedesktop/Notifications",
            SERVICE,
            "Notify",
            vec![
                Value::Str("Skerry".to_owned()),
                Value::U32(0),
                Value::Str("dev.skerry.Skerry".to_owned()),
                Value::Str("Copied 12 of 12".to_owned()),
                Value::Str("/home/tester/Pictures".to_owned()),
                Value::Empty("s".to_owned()),
                Value::Dict(vec![(
                    "desktop-entry".to_owned(),
                    Value::Str("dev.skerry.Skerry".to_owned()),
                )]),
                Value::I32(-1),
            ],
        );
        let signature: String = call.body.iter().map(Value::signature).collect();
        assert_eq!(signature, "susssasa{sv}i");

        // And again from the other end of the wire, because an empty `as` and a
        // dictionary are the two arguments a signature is easiest to be wrong
        // about — an empty array reads back as an array of nothing, which is the
        // same eight characters and a different `Value`.
        let (read, _) = unmarshal(&marshal(&call)).whole().expect("a whole message");
        let back: String = read.body.iter().map(Value::signature).collect();
        assert_eq!(back, signature, "the body did not survive its own encoding");
        assert_eq!(
            read.body.get(6),
            Some(&Value::Dict(vec![(
                "desktop-entry".to_owned(),
                Value::Str("dev.skerry.Skerry".to_owned())
            )])),
            "the hint the daemon finds our icon by did not arrive"
        );
    }

    /// A daemon on the other end of a socket that says it can do `capabilities`,
    /// and hands back the `Notify` it was sent.
    fn daemon(capabilities: &'static [&'static str]) -> (Bus, std::thread::JoinHandle<Message>) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        for socket in [&ours, &theirs] {
            socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        }
        let serving = std::thread::spawn(move || {
            let mut daemon = Bus::over(theirs);
            let asked = daemon.receive().expect("no GetCapabilities");
            assert_eq!(asked.member.as_deref(), Some("GetCapabilities"));
            let named = capabilities.iter().map(|it| Value::Str((*it).to_owned())).collect();
            daemon.reply(&asked, vec![Value::Array(named)]).unwrap();
            let notify = daemon.receive().expect("no Notify");
            daemon.reply(&notify, vec![Value::U32(1)]).unwrap();
            notify
        });
        (Bus::over(ours), serving)
    }

    const FOLDER: &str = "/home/tester/<b>Tom & Jerry</b> <img src=\"x\"/>";

    /// **A path is text, so a daemon that reads markup gets it escaped.**
    ///
    /// The body is the folder an operation went to, and a folder can be named
    /// anything: under a daemon with `body-markup` an unescaped `<img src>` in a
    /// name is an image fetched and an `<a href>` a link, out of a sentence that
    /// was only meant to say where.
    #[test]
    fn a_daemon_that_reads_markup_is_sent_the_path_escaped() {
        let (mut bus, serving) = daemon(&["body", "body-markup", "actions"]);
        tell(&mut bus, "Skerry", "dev.skerry.Skerry", "dev.skerry.Skerry", "Copied", FOLDER)
            .unwrap();
        let sent = serving.join().unwrap();
        assert_eq!(
            sent.body.get(4).and_then(Value::text),
            Some("/home/tester/&lt;b&gt;Tom &amp; Jerry&lt;/b&gt; &lt;img src=\"x\"/&gt;")
        );
    }

    /// And one that does not is sent it as it is, because it would show an
    /// escape as the escape.
    #[test]
    fn a_daemon_that_does_not_read_markup_is_sent_the_path_as_it_is() {
        let (mut bus, serving) = daemon(&["body", "actions"]);
        tell(&mut bus, "Skerry", "dev.skerry.Skerry", "dev.skerry.Skerry", "Copied", FOLDER)
            .unwrap();
        assert_eq!(serving.join().unwrap().body.get(4).and_then(Value::text), Some(FOLDER));
    }

    /// No daemon is a sentence for the log and nothing for the window.
    #[test]
    fn every_refusal_has_a_detail_and_none_of_them_is_empty() {
        for refusal in [
            Refused::NoBus,
            Refused::NoDaemon,
            Refused::Failed("org.freedesktop.DBus.Error.NoReply".to_owned()),
        ] {
            assert!(!refusal.detail().is_empty(), "{refusal:?} says nothing to the log");
        }
    }
}
