// SPDX-License-Identifier: Apache-2.0
//! D-Bus, spoken on its own socket.
//!
//! [ADR 0023](../../../../docs/adr/0023-d-bus-is-a-socket-we-speak-ourselves.md)
//! settles why this is ours rather than a crate's, and the shape it takes:
//! **serving is one thread that blocks on a read; calling is a short-lived
//! connection on a job.** What is here is a connection, the four steps that open
//! one, and messages; [`wire`] is the type system.
//!
//! Nothing here knows what a file manager is. What the messages *mean* —
//! `ShowFolders` is a navigation, a name already owned is a second instance — is
//! `sk-app`'s, because that is where the window is.

pub mod wire;

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;

pub use wire::Value;

/// What a message is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Somebody is asking for something.
    Call,
    /// An answer to a call.
    Reply,
    /// A refusal, with a name and usually a sentence.
    Error,
    /// A broadcast nobody replied to.
    Signal,
}

impl Kind {
    const fn code(self) -> u8 {
        match self {
            Self::Call => 1,
            Self::Reply => 2,
            Self::Error => 3,
            Self::Signal => 4,
        }
    }

    const fn of(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Call),
            2 => Some(Self::Reply),
            3 => Some(Self::Error),
            4 => Some(Self::Signal),
            _ => None,
        }
    }
}

/// One message, in or out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Message {
    /// Call, reply, error or signal.
    pub kind: Kind,
    /// Its own serial, which a reply quotes.
    pub serial: u32,
    /// The serial this answers, for a reply or an error.
    pub answers: Option<u32>,
    /// Where it is going, or where it came from.
    pub peer: Option<String>,
    /// The object, for a call or a signal.
    pub path: Option<String>,
    /// The interface it belongs to.
    pub interface: Option<String>,
    /// The method or the signal.
    pub member: Option<String>,
    /// The error's name, for an error.
    pub error: Option<String>,
    /// The arguments.
    pub body: Vec<Value>,
}

impl Message {
    /// A method call.
    #[must_use]
    pub fn call(to: &str, path: &str, interface: &str, member: &str, body: Vec<Value>) -> Self {
        Self {
            kind: Kind::Call,
            serial: 0,
            answers: None,
            peer: Some(to.to_owned()),
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            error: None,
            body,
        }
    }

    /// A broadcast.
    #[must_use]
    pub fn signal(path: &str, interface: &str, member: &str, body: Vec<Value>) -> Self {
        Self {
            kind: Kind::Signal,
            serial: 0,
            answers: None,
            peer: None,
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            error: None,
            body,
        }
    }

    /// The first argument as text, which most answers are.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.body.first().and_then(Value::text)
    }

    /// The signature of the body, as the header spells it.
    fn signature(&self) -> String {
        self.body.iter().map(Value::signature).collect()
    }
}

/// The header field codes, from the specification's table.
const PATH: u8 = 1;
const INTERFACE: u8 = 2;
const MEMBER: u8 = 3;
const ERROR_NAME: u8 = 4;
const REPLY_SERIAL: u8 = 5;
const DESTINATION: u8 = 6;
const SENDER: u8 = 7;
const SIGNATURE: u8 = 8;

/// The bus's own object, interface and name.
pub(crate) const BUS: (&str, &str, &str) =
    ("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus");

/// A connection to a bus.
pub struct Bus {
    socket: UnixStream,
    serial: u32,
    /// The name the bus gave us, which is this connection's identity.
    unique: String,
    /// Bytes read but not yet parsed, because a read can land mid-message.
    held: Vec<u8>,
    /// How much of a message larger than [`LARGEST`] is still to arrive, and
    /// is dropped as it does rather than held.
    discard: usize,
    /// The well-known names this connection owns, in the order they were taken.
    names: Vec<String>,
}

impl Bus {
    /// Connects to the session bus, authenticates and says hello.
    ///
    /// `None` where there is no bus, which is a session without one rather than
    /// an error — [06-portability.md](../../../../docs/06-portability.md)'s bare
    /// Sway case, where every feature that needs a bus is absent and says so.
    ///
    /// **Runs on a job**: a `connect` and four round trips.
    #[must_use]
    pub fn session() -> Option<Self> {
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()?;
        Self::connect(&address)
    }

    /// Connects to the system bus, where udisks2 and the like live.
    #[must_use]
    pub fn system() -> Option<Self> {
        let address = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
            .unwrap_or_else(|_| "unix:path=/var/run/dbus/system_bus_socket".to_owned());
        Self::connect(&address)
    }

    /// Opens one connection to the address a `DBUS_*_BUS_ADDRESS` names.
    ///
    /// Only `unix:path=` and `unix:abstract=`. A `tcp:` bus is a thing the
    /// specification allows and nothing on a desktop uses, and a transport nobody
    /// has is a transport whose bugs nobody finds.
    #[must_use]
    pub fn connect(address: &str) -> Option<Self> {
        let socket = address.split(';').find_map(open)?;
        let mut bus = Self {
            socket,
            serial: 0,
            unique: String::new(),
            held: Vec::new(),
            discard: 0,
            names: Vec::new(),
        };
        bus.authenticate()?;
        let hello = bus.call(Message::call(BUS.0, BUS.1, BUS.2, "Hello", Vec::new()))?;
        hello.text()?.clone_into(&mut bus.unique);
        Some(bus)
    }

    /// The name the bus gave this connection.
    #[must_use]
    pub fn unique(&self) -> &str {
        &self.unique
    }

    /// The socket, to wait on beside something else.
    ///
    /// **For a connection that outlives a call.** The accessibility bus is served
    /// and *emitted to* by one thread, so that thread has to wait on the socket
    /// and on its own eventfd at once rather than blocking in
    /// [`Self::receive`] — see [`crate::atspi`].
    #[must_use]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.socket.as_fd()
    }

    /// `EXTERNAL`, which is the uid in hexadecimal and nothing else.
    ///
    /// **No `NEGOTIATE_UNIX_FD`.** Nothing here passes a descriptor, and asking
    /// for a capability we would not honour is a lie told at connect time.
    fn authenticate(&mut self) -> Option<()> {
        let uid = rustix::process::getuid().as_raw();
        let mut hex = String::new();
        for byte in uid.to_string().bytes() {
            // The uid's *digits* in hexadecimal, not the number: `EXTERNAL`
            // authenticates with the decimal text of the uid, hex-encoded.
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
            hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        // The leading NUL is the protocol's, not a string terminator: the server
        // reads one byte before the SASL conversation starts.
        self.socket.write_all(&[0]).ok()?;
        self.socket.write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes()).ok()?;
        let reply = self.read_line()?;
        if !reply.starts_with("OK") {
            return None;
        }
        self.socket.write_all(b"BEGIN\r\n").ok()?;
        Some(())
    }

    /// One `\r\n`-terminated line of the SASL conversation.
    fn read_line(&mut self) -> Option<String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < 512 {
            self.socket.read_exact(&mut byte).ok()?;
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                line.push(byte[0]);
            }
        }
        String::from_utf8(line).ok()
    }

    /// Sends a message and waits for its reply.
    ///
    /// **Runs on a job.** `None` for an error reply as well as for a broken
    /// connection: a caller that needs the difference reads
    /// [`Bus::call_for`].
    pub fn call(&mut self, message: Message) -> Option<Message> {
        self.call_for(message).ok()
    }

    /// As [`Bus::call`], with the error's name where there was one.
    ///
    /// # Errors
    ///
    /// The error's name, or a sentence about the connection.
    pub fn call_for(&mut self, message: Message) -> Result<Message, String> {
        let serial = self.send(message)?;
        loop {
            let answer = self.receive().ok_or_else(|| "the bus stopped answering".to_owned())?;
            if answer.answers != Some(serial) {
                // Something else arriving between the call and its answer: a
                // signal, or a call to us on a connection that also serves. The
                // serving thread reads its own; this one is a call's own
                // connection and has nobody to hand it to.
                continue;
            }
            if answer.kind == Kind::Error {
                let name = answer.error.clone().unwrap_or_else(|| "unknown".to_owned());
                let detail = answer.text().unwrap_or_default().to_owned();
                return Err(if detail.is_empty() { name } else { format!("{name}: {detail}") });
            }
            return Ok(answer);
        }
    }

    /// Sends a message and returns the serial it went out with.
    ///
    /// # Errors
    ///
    /// A sentence, where the socket would not take it.
    pub fn send(&mut self, mut message: Message) -> Result<u32, String> {
        self.serial = self.serial.wrapping_add(1).max(1);
        message.serial = self.serial;
        let bytes = marshal(&message);
        self.socket.write_all(&bytes).map_err(|error| format!("d-bus write: {error}"))?;
        Ok(message.serial)
    }

    /// Reads the next whole message, blocking until one arrives.
    ///
    /// `None` when the connection has gone, which is the bus going away — or
    /// when the stream stopped making sense, which nothing after it can recover.
    ///
    /// **A message this cannot read is passed over, never waited on.** Its header
    /// still says how long it is, so the next one is found where it starts; a
    /// call is told so by name, and a reply that cannot be read arrives as the
    /// error it amounts to — see [`Bus::account_for`].
    pub fn receive(&mut self) -> Option<Message> {
        loop {
            if let Some(message) = self.buffered().ok()? {
                return Some(message);
            }
            self.fill()?;
        }
    }

    /// Every whole message already read or, where there is none, every whole
    /// message one read brings. `None` when the connection has gone.
    ///
    /// **For a thread that waits on the socket beside something else**, once
    /// `poll` has said a read will not block. [`Bus::receive`] reads again until
    /// it has a message, which after passing over one it could not read is a
    /// wait for a message that may not be coming — while the other descriptor
    /// goes unanswered.
    ///
    /// **What is held comes first**, and the socket is read only when nothing
    /// whole is held: a read lands where the kernel had bytes, so a call can
    /// arrive in the same read as the reply before it.
    pub fn receive_ready(&mut self) -> Option<Vec<Message>> {
        let held = self.held.len();
        let messages = self.received()?;
        if self.held.len() < held {
            return Some(messages);
        }
        self.fill()?;
        self.received()
    }

    /// Every whole message among the bytes already read, without reading.
    ///
    /// `None` for a stream out of step. **For a thread about to `poll`**, which cannot see bytes that are already
    /// off the socket: the registry's first question can arrive in the read that
    /// brought the answer to `Embed`, and then nothing more arrives to wake it.
    pub fn received(&mut self) -> Option<Vec<Message>> {
        let mut messages = Vec::new();
        while let Some(message) = self.buffered().ok()? {
            messages.push(message);
        }
        Some(messages)
    }

    /// The next whole message among the bytes already read, if there is one.
    ///
    /// `Err` for a stream out of step.
    fn buffered(&mut self) -> Result<Option<Message>, OutOfStep> {
        loop {
            match unmarshal(&self.held) {
                Front::Whole(message, used) => {
                    self.held.drain(..used);
                    return Ok(Some(message));
                }
                Front::Partial => return Ok(None),
                Front::Unread(unread) => {
                    let here = unread.size.min(self.held.len());
                    self.held.drain(..here);
                    self.discard = unread.size - here;
                    if let Some(message) = self.account_for(unread) {
                        return Ok(Some(message));
                    }
                }
                Front::Lost => return Err(OutOfStep),
            }
        }
    }

    /// One read from the socket into what is held. `None` when it has closed.
    fn fill(&mut self) -> Option<()> {
        let mut chunk = [0u8; 4096];
        let read = self.socket.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        let dropped = read.min(self.discard);
        self.discard -= dropped;
        self.held.extend_from_slice(&chunk[dropped..read]);
        Some(())
    }

    /// What a message this did not read is owed.
    ///
    /// **A call is refused**, because its sender is otherwise waiting out its
    /// own timeout for an answer that will never come — unless it said it wants
    /// none. **A reply or an error becomes an error answering the same serial**,
    /// because [`Bus::call_for`] is waiting on that serial, and a reply it cannot
    /// read is the call failing, not the call still running — named
    /// [`UNREADABLE`], which is ours. A signal is owed nothing.
    fn account_for(&mut self, unread: Unread) -> Option<Message> {
        let header = unread.header?;
        match header.kind {
            Kind::Call => {
                if !unread.quiet && header.peer.is_some() {
                    let _ = self.refuse(&header, unread.error, unread.why);
                }
                None
            }
            Kind::Reply | Kind::Error => Some(Message {
                kind: Kind::Error,
                error: Some(UNREADABLE.to_owned()),
                body: vec![Value::Str(unread.why.to_owned())],
                ..header
            }),
            Kind::Signal => None,
        }
    }

    /// Answers a call.
    ///
    /// # Errors
    ///
    /// A sentence, where the socket would not take it.
    pub fn reply(&mut self, to: &Message, body: Vec<Value>) -> Result<(), String> {
        self.send(Message {
            kind: Kind::Reply,
            serial: 0,
            answers: Some(to.serial),
            peer: to.peer.clone(),
            path: None,
            interface: None,
            member: None,
            error: None,
            body,
        })
        .map(|_| ())
    }

    /// Refuses one.
    ///
    /// # Errors
    ///
    /// A sentence, where the socket would not take it.
    pub fn refuse(&mut self, to: &Message, name: &str, text: &str) -> Result<(), String> {
        self.send(Message {
            kind: Kind::Error,
            serial: 0,
            answers: Some(to.serial),
            peer: to.peer.clone(),
            path: None,
            interface: None,
            member: None,
            error: Some(name.to_owned()),
            body: vec![Value::Str(text.to_owned())],
        })
        .map(|_| ())
    }

    /// Asks for a well-known name. What the bus said.
    ///
    /// **Never `REPLACE_EXISTING`.** Nautilus may hold `FileManager1`, and taking
    /// it from a running file manager because we started is not ours to do —
    /// ADR 0023.
    pub fn request_name(&mut self, name: &str) -> Named {
        // `DO_NOT_QUEUE`: a name we are queued for is a name we do not have, and
        // acquiring it later — when the other file manager exits — would make us
        // the handler for calls nobody meant to send us.
        let flags = Value::U32(4);
        let call = Message::call(
            BUS.0,
            BUS.1,
            BUS.2,
            "RequestName",
            vec![Value::Str(name.to_owned()), flags],
        );
        match self.call(call).and_then(|reply| reply.body.first().and_then(Value::number)) {
            Some(1 | 4) => {
                self.names.push(name.to_owned());
                Named::Ours
            }
            Some(3) => Named::Already,
            _ => Named::Refused,
        }
    }

    /// The well-known names this connection holds.
    ///
    /// A capability, in [ADR 0007](../../../../docs/adr/0007-runtime-capability-detection.md)'s
    /// sense: whether the desktop's `FileManager1` is ours is something only the
    /// bus can say, and the startup line says it back.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

/// What asking for a name got.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Named {
    /// It is ours, and calls to it arrive here.
    Ours,
    /// Somebody else has it — another file manager, or another instance of this
    /// one, which is what single instance is built on.
    Already,
    /// The bus said no, or said nothing this build understands.
    Refused,
}

/// Opens the socket one address names.
fn open(address: &str) -> Option<UnixStream> {
    for part in address.split(',') {
        if let Some(path) = part.strip_prefix("unix:path=").or_else(|| part.strip_prefix("path=")) {
            return UnixStream::connect(path).ok();
        }
        if let Some(name) =
            part.strip_prefix("unix:abstract=").or_else(|| part.strip_prefix("abstract="))
        {
            // An abstract socket's name starts with a NUL, which `UnixStream`
            // cannot express through a path — `SocketAddrExt` is where the
            // standard library puts it.
            use std::os::linux::net::SocketAddrExt as _;
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes());
            return address.and_then(|it| UnixStream::connect_addr(&it)).ok();
        }
    }
    None
}

/// A message as bytes.
pub(crate) fn marshal(message: &Message) -> Vec<u8> {
    let signature = message.signature();
    let mut fields: Vec<(u8, Value)> = Vec::new();
    if let Some(path) = &message.path {
        fields.push((PATH, Value::Path(path.clone())));
    }
    if let Some(interface) = &message.interface {
        fields.push((INTERFACE, Value::Str(interface.clone())));
    }
    if let Some(member) = &message.member {
        fields.push((MEMBER, Value::Str(member.clone())));
    }
    if let Some(error) = &message.error {
        fields.push((ERROR_NAME, Value::Str(error.clone())));
    }
    if let Some(answers) = message.answers {
        fields.push((REPLY_SERIAL, Value::U32(answers)));
    }
    if let Some(peer) = &message.peer {
        fields.push((DESTINATION, Value::Str(peer.clone())));
    }
    if !signature.is_empty() {
        fields.push((SIGNATURE, Value::Signature(signature)));
    }

    // The body first, marshalled at the offset it will sit at — which is not
    // known until the header is, so it is marshalled twice: once to measure the
    // header and once at the real offset. Cheap, and the alternative is an
    // offset threaded through every write.
    let header_length = header_bytes(message, &fields, 0).len();
    let body_at = header_length + (8 - header_length % 8) % 8;
    let mut writer = wire::Writer::at(body_at);
    for value in &message.body {
        writer.write(value);
    }
    let body = writer.body(body_at);
    let mut out = header_bytes(message, &fields, body.len());
    out.resize(body_at, 0);
    out.extend_from_slice(&body);
    out
}

/// The fixed header and its field array.
fn header_bytes(message: &Message, fields: &[(u8, Value)], body: usize) -> Vec<u8> {
    let mut writer = wire::Writer::at(0);
    writer.write(&Value::Byte(b'l'));
    writer.write(&Value::Byte(message.kind.code()));
    writer.write(&Value::Byte(0));
    writer.write(&Value::Byte(1));
    writer.write(&Value::U32(u32::try_from(body).unwrap_or(0)));
    writer.write(&Value::U32(message.serial));
    // `a(yv)`: an array of structs, which is the one struct this program writes —
    // so it is written by hand rather than given a `Value` variant nothing else
    // would use.
    let mut inner = wire::Writer::at(writer.so_far() + 4);
    for (code, value) in fields {
        // A struct aligns to eight.
        let pad = (8 - inner.so_far() % 8) % 8;
        for _ in 0..pad {
            inner.write(&Value::Byte(0));
        }
        inner.write(&Value::Byte(*code));
        inner.write(&Value::Variant(Box::new(value.clone())));
    }
    let array = inner.body(writer.so_far() + 4);
    writer.write(&Value::U32(u32::try_from(array.len()).unwrap_or(0)));
    let mut out = writer.into_bytes();
    out.extend_from_slice(&array);
    out
}

/// The header flag a sender sets when it wants no answer.
const NO_REPLY_EXPECTED: u8 = 1;

/// The standard error for a call whose arguments are not what was expected.
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

/// The standard error for a request past what a resource allows.
const LIMITS_EXCEEDED: &str = "org.freedesktop.DBus.Error.LimitsExceeded";

/// The error a reply this end could not read is handed to its caller as.
///
/// **Ours rather than a standard one**: `InvalidArgs` would put a log line on
/// the service that answered, for a reply this end did not read.
const UNREADABLE: &str = "dev.skerry.Error.Unreadable";

/// The largest message this reads, header and body together.
///
/// **256 KiB, against a largest honest message of about 12 KiB.** What arrives
/// is `FileManager1` calls, whose argument is a list of URIs of which the window
/// opens the first, or one tab each; a second `skerry` handing over one path;
/// and replies to this program's own calls. The largest reply is
/// `ListActivatableNames`, 3,585 bytes of body for 112 names — measured against a
/// private `dbus-daemon` reading this machine's service directories. The
/// largest URI is 12,292 bytes — `file://` and a `PATH_MAX` path with every
/// byte percent-encoded — and the ceiling holds twenty of those.
///
/// **What a body costs decoded is why there is a ceiling at all.** The session
/// bus here forwards messages of up to 1,000,000,000 bytes, and an `ay` decoded
/// at 48 bytes of memory per byte on the wire, measured with a counting
/// allocator. With `ay` read as bytes and the reader's budget of values, the
/// costliest of eight shapes measured — `ag` of empty signatures — peaked at
/// 3.0 MiB for a message of this size.
const LARGEST: usize = 256 * 1024;

/// What the front of a receive buffer holds.
pub(crate) enum Front {
    /// Not all of the next message has arrived.
    Partial,
    /// A message, and how many bytes it took.
    Whole(Message, usize),
    /// A message its header frames and this will not read.
    Unread(Unread),
    /// Bytes that are not a message header, after which no message can be
    /// found again.
    Lost,
}

/// A message passed over, with what is needed to answer for it.
pub(crate) struct Unread {
    /// Its whole length on the stream.
    size: usize,
    /// Its header with an empty body, where the header itself read.
    header: Option<Message>,
    /// Whether its sender said it wants no answer.
    quiet: bool,
    /// The standard error that says why.
    error: &'static str,
    /// And the sentence under it, for whoever reads the error.
    why: &'static str,
}

/// A stream whose next bytes are not a message header.
struct OutOfStep;

#[cfg(test)]
impl Bus {
    /// A connection over a socket whose other end is a test, with no daemon,
    /// no authentication and no `Hello` — for a test that plays the peer.
    pub(crate) const fn over(socket: UnixStream) -> Self {
        let unique = String::new();
        Self { socket, serial: 0, unique, held: Vec::new(), discard: 0, names: Vec::new() }
    }
}

#[cfg(test)]
impl Front {
    /// The message, where there is a whole one.
    pub(crate) fn whole(self) -> Option<(Message, usize)> {
        match self {
            Self::Whole(message, used) => Some((message, used)),
            _ => None,
        }
    }
}

/// The message at the front of a buffer.
///
/// **Not arrived and not readable are two answers.** They were one, `None`, and
/// the reader went on waiting for the rest of a message that had all arrived
/// and only failed to decode — so it never left the buffer and nothing behind it
/// was read again. The fixed sixteen bytes say how long the whole message is,
/// which is all it takes to pass over one whatever is inside it.
pub(crate) fn unmarshal(bytes: &[u8]) -> Front {
    let Some(fixed) = bytes.get(..16) else { return Front::Partial };
    let little = match fixed[0] {
        b'l' => true,
        b'B' => false,
        _ => return Front::Lost,
    };
    // "If the major protocol version of the receiving application does not
    // match … the D-Bus connection must be disconnected" — the specification's
    // header table.
    if fixed[3] != 1 {
        return Front::Lost;
    }
    let number = |at: usize| -> u32 {
        let raw: [u8; 4] = fixed[at..at + 4].try_into().unwrap_or([0; 4]);
        if little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) }
    };
    let Some(fields_end) = 16usize.checked_add(number(12) as usize) else { return Front::Lost };
    let Some(body_at) = fields_end.checked_next_multiple_of(8) else { return Front::Lost };
    let Some(end) = body_at.checked_add(number(4) as usize) else { return Front::Lost };
    let oversized = end > LARGEST;
    let (error, why) = if oversized {
        (LIMITS_EXCEEDED, "the message is larger than this program reads")
    } else {
        (INVALID_ARGS, "the message's arguments could not be read")
    };
    let quiet = fixed[2] & NO_REPLY_EXPECTED != 0;
    let pass_over = |header| Front::Unread(Unread { size: end, header, quiet, error, why });
    // **Held only as far as its header**, for a message larger than this reads:
    // the header names whom to refuse, and the body is dropped as it arrives.
    let needed = if oversized { fields_end } else { end };
    if needed > LARGEST {
        return pass_over(None);
    }
    if bytes.len() < needed {
        return Front::Partial;
    }
    // "Unknown types must be ignored", in the same table.
    let Some(kind) = Kind::of(fixed[1]) else { return pass_over(None) };
    let Some((mut message, signature)) = header(kind, number(8), &bytes[..fields_end], little)
    else {
        return pass_over(None);
    };
    if oversized {
        return pass_over(Some(message));
    }
    match body(&bytes[..end], body_at, &signature, little) {
        Some(body) => {
            message.body = body;
            Front::Whole(message, end)
        }
        None => pass_over(Some(message)),
    }
}

/// A message's header fields, and the signature of the body after them.
fn header(kind: Kind, serial: u32, fields: &[u8], little: bool) -> Option<(Message, String)> {
    let mut message = Message {
        kind,
        serial,
        answers: None,
        peer: None,
        path: None,
        interface: None,
        member: None,
        error: None,
        body: Vec::new(),
    };
    let mut signature = String::new();
    let mut reader = wire::Reader::new(fields, 16, little);
    while !reader.done() {
        // Each field is a struct, aligned to eight.
        let padding = (8 - reader.at() % 8) % 8;
        for _ in 0..padding {
            reader.read("y")?;
        }
        if reader.done() {
            break;
        }
        let Value::Byte(code) = reader.read("y")? else { return None };
        let value = reader.read("v")?;
        let text = value.text().unwrap_or_default().to_owned();
        match code {
            PATH => message.path = Some(text),
            INTERFACE => message.interface = Some(text),
            MEMBER => message.member = Some(text),
            ERROR_NAME => message.error = Some(text),
            REPLY_SERIAL => message.answers = value.number().and_then(|it| u32::try_from(it).ok()),
            DESTINATION | SENDER => {
                if code == SENDER {
                    message.peer = Some(text);
                }
            }
            SIGNATURE => signature = text,
            _ => {}
        }
    }
    Some((message, signature))
}

/// The values a body from `at` to the end of `bytes` holds, which must be
/// exactly what `signature` names.
fn body(bytes: &[u8], at: usize, signature: &str, little: bool) -> Option<Vec<Value>> {
    if !wire::valid_signature(signature, false) {
        return None;
    }
    let mut reader = wire::Reader::new(bytes, at, little);
    let values: Option<Vec<Value>> =
        wire::split(signature).iter().map(|kind| reader.read(kind)).collect();
    if reader.at() != bytes.len() {
        return None;
    }
    values
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::{
        Bus, DESTINATION, Front, INVALID_ARGS, Kind, LARGEST, LIMITS_EXCEEDED, Message, SENDER,
        SIGNATURE, UNREADABLE, Value, marshal, unmarshal,
    };

    /// A call goes out and comes back as itself.
    ///
    /// The round trip is over `marshal` and `unmarshal` rather than over a bus,
    /// because the bus is not this test's subject: what is, is that the header's
    /// field array, its padding to eight and the body's offset agree.
    #[test]
    fn a_call_marshals_and_unmarshals_as_itself() {
        let call = Message::call(
            "org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
            "ShowItems",
            vec![
                Value::Array(vec![Value::Str("file:///home/tester/a file.txt".to_owned())]),
                Value::Str(String::new()),
            ],
        );
        let mut sent = call.clone();
        sent.serial = 7;
        let bytes = marshal(&sent);
        assert_eq!(bytes[0], b'l');
        assert_eq!(bytes[1], Kind::Call.code());
        assert_eq!(bytes[3], 1, "the protocol version");
        // The *header* is padded to eight before the body; the message ends where
        // the body does, which is not the same claim and was the first thing this
        // test got wrong.
        assert_eq!(bytes.len() % 8, 5, "the body's own length is not a multiple of eight");

        let (read, used) = unmarshal(&bytes).whole().expect("it would not read back");
        assert_eq!(used, bytes.len());
        assert_eq!(read.member.as_deref(), Some("ShowItems"));
        assert_eq!(read.interface.as_deref(), Some("org.freedesktop.FileManager1"));
        assert_eq!(read.path.as_deref(), Some("/org/freedesktop/FileManager1"));
        assert_eq!(read.serial, 7);
        assert_eq!(read.body, sent.body);
    }

    /// A reply quotes the serial it answers and carries no member.
    #[test]
    fn a_reply_quotes_the_serial_it_answers() {
        let reply = Message {
            kind: Kind::Reply,
            serial: 3,
            answers: Some(9),
            peer: Some(":1.42".to_owned()),
            path: None,
            interface: None,
            member: None,
            error: None,
            body: vec![Value::Str(":1.7".to_owned())],
        };
        let (read, _) = unmarshal(&marshal(&reply)).whole().expect("it would not read back");
        assert_eq!(read.kind, Kind::Reply);
        assert_eq!(read.answers, Some(9));
        assert_eq!(read.text(), Some(":1.7"));
        assert_eq!(read.member, None);
    }

    /// A buffer with half a message in it yields nothing, and yields it again
    /// when the rest arrives.
    ///
    /// **The case a socket produces constantly**: a `read` lands where the kernel
    /// had bytes, not where a message ends.
    #[test]
    fn half_a_message_is_not_a_message() {
        let mut call = Message::call("a.b", "/a", "a.b", "C", vec![Value::Str("x".to_owned())]);
        call.serial = 1;
        let bytes = marshal(&call);
        for cut in 1..bytes.len() {
            assert!(
                matches!(unmarshal(&bytes[..cut]), Front::Partial),
                "{cut} bytes were not a message still arriving"
            );
        }
        assert!(unmarshal(&bytes).whole().is_some());

        // And two messages in one buffer are two messages.
        let mut both = bytes.clone();
        both.extend_from_slice(&bytes);
        let (_, used) = unmarshal(&both).whole().expect("the first");
        assert_eq!(used, bytes.len());
        assert!(unmarshal(&both[used..]).whole().is_some(), "the second");
    }

    /// Rubbish is rubbish, not a panic.
    #[test]
    fn a_message_that_makes_no_sense_is_refused() {
        assert!(unmarshal(&[0; 16]).whole().is_none(), "message type zero");
        let mut bytes = vec![b'l', 1, 0, 1];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(unmarshal(&bytes).whole().is_none(), "lengths that overflow the buffer");
    }

    /// A connection whose other end is the test, with no daemon between.
    ///
    /// **A read gives up after five seconds**, so a reader that waits on a
    /// message that has all arrived fails here instead of hanging the suite.
    fn pair() -> (Bus, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        for socket in [&ours, &theirs] {
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        let bus = Bus {
            socket: ours,
            serial: 0,
            unique: ":1.1".to_owned(),
            held: Vec::new(),
            discard: 0,
            names: Vec::new(),
        };
        (bus, theirs)
    }

    /// `message` as a bus daemon delivers it, from the peer it names.
    ///
    /// The marshaller writes the peer as the destination, and a daemon fills in
    /// the sender; both are an `s` in a `(yv)`, so the swap is one byte.
    fn delivered(mut message: Message, serial: u32) -> Vec<u8> {
        message.serial = serial;
        let mut bytes = marshal(&message);
        let at = bytes.windows(4).position(|it| it == [DESTINATION, 1, b's', 0]).unwrap();
        bytes[at] = SENDER;
        bytes
    }

    /// A message whose one `u` argument is relabelled `b` — and holds a two,
    /// which is neither boolean, so no reader may take it.
    fn mistyped(mut bytes: Vec<u8>) -> Vec<u8> {
        let at = bytes.windows(6).position(|it| it == [SIGNATURE, 1, b'g', 0, 1, b'u']).unwrap();
        bytes[at + 5] = b'b';
        bytes
    }

    fn show_folders(body: Vec<Value>) -> Message {
        let object = "/org/freedesktop/FileManager1";
        Message::call(":1.9", object, "org.freedesktop.FileManager1", "ShowFolders", body)
    }

    /// **A message that does not read is passed over, and its caller told.**
    ///
    /// The defect: the reader answered "not all here yet" for a message that had
    /// all arrived and only failed to decode, so it was never dropped and the
    /// call behind it was never read — one bad `ShowFolders` and the file
    /// manager stopped answering for good.
    #[test]
    fn a_call_that_does_not_read_is_refused_and_the_next_one_is_read() {
        let (mut bus, mut peer) = pair();
        let bad = mistyped(delivered(show_folders(vec![Value::U32(2)]), 5));
        let good = delivered(show_folders(vec![Value::Str("file:///tmp".to_owned())]), 6);
        peer.write_all(&[bad, good].concat()).unwrap();

        let next = bus.receive().expect("the call behind the bad one was never read");
        assert_eq!((next.serial, next.text()), (6, Some("file:///tmp")));
        assert!(bus.held.is_empty(), "{} bytes left over", bus.held.len());

        let mut answer = [0u8; 4096];
        let read = peer.read(&mut answer).unwrap();
        let (refusal, _) = unmarshal(&answer[..read]).whole().expect("a whole refusal");
        assert_eq!(refusal.kind, Kind::Error);
        assert_eq!(refusal.answers, Some(5), "it answers the call that did not read");
        assert_eq!(refusal.error.as_deref(), Some(INVALID_ARGS));
    }

    /// The finding's own reproduction: `ShowFolders` with a `double` in it,
    /// which nothing here reads — so it is refused, and the call behind it read.
    #[test]
    fn a_call_with_a_double_in_it_is_refused_and_passed_over() {
        let (mut bus, mut peer) = pair();
        // An `x` relabelled `d`: the same eight bytes, which is what a double is.
        let mut double = delivered(show_folders(vec![Value::I64(1)]), 2);
        let at = double.windows(6).position(|it| it == [SIGNATURE, 1, b'g', 0, 1, b'x']).unwrap();
        double[at + 5] = b'd';
        let good = delivered(show_folders(Vec::new()), 3);
        peer.write_all(&[double, good].concat()).unwrap();
        assert_eq!(bus.receive().map(|it| it.serial), Some(3), "a double stopped the reader");

        let mut answer = [0u8; 4096];
        let read = peer.read(&mut answer).unwrap();
        let (refusal, _) = unmarshal(&answer[..read]).whole().expect("a whole refusal");
        assert_eq!((refusal.answers, refusal.error.as_deref()), (Some(2), Some(INVALID_ARGS)));
    }

    /// A message from a big-endian peer, whole: the header's lengths, its
    /// serial and a `u` in the body all turn round.
    ///
    /// By hand: the fixed sixteen bytes, `MEMBER` "C" padded to eight,
    /// `SIGNATURE` "u", one byte to the body's eight, and 0x01020304.
    #[test]
    fn a_whole_message_from_a_big_endian_peer_reads() {
        let bytes: &[u8] = b"B\x01\x00\x01\x00\x00\x00\x04\x00\x00\x00\x09\x00\x00\x00\x17\
            \x03\x01s\x00\x00\x00\x00\x01C\x00\x00\x00\x00\x00\x00\x00\
            \x08\x01g\x00\x01u\x00\x00\
            \x01\x02\x03\x04";
        let (message, used) = unmarshal(bytes).whole().expect("a big-endian message");
        assert_eq!(used, bytes.len());
        assert_eq!((message.kind, message.serial), (Kind::Call, 9));
        assert_eq!(message.member.as_deref(), Some("C"));
        assert_eq!(message.body, [Value::U32(0x0102_0304)]);
    }

    /// **A call that arrives in the same read as a reply is not left waiting.**
    ///
    /// `call_for` takes the reply and leaves the call held; the socket then has
    /// nothing more to say, so a reader that went to it first would wait for a
    /// message that already arrived.
    #[test]
    fn a_call_in_the_same_read_as_a_reply_is_answered_without_a_read() {
        let (mut bus, mut peer) = pair();
        let reply = Message {
            kind: Kind::Reply,
            serial: 0,
            answers: Some(1),
            peer: Some(":1.9".to_owned()),
            path: None,
            interface: None,
            member: None,
            error: None,
            body: Vec::new(),
        };
        let call = delivered(show_folders(Vec::new()), 7);
        peer.write_all(&[delivered(reply, 6), call].concat()).unwrap();
        assert!(bus.call_for(show_folders(Vec::new())).is_ok(), "the reply");

        let at = std::time::Instant::now();
        let held = bus.receive_ready().expect("the connection is open");
        assert!(at.elapsed() < Duration::from_secs(1), "it read before answering what it held");
        assert_eq!(held.iter().map(|it| it.serial).collect::<Vec<_>>(), [7]);
    }

    /// A sender that asked for no reply gets none, even a refusal.
    #[test]
    fn a_call_that_wants_no_answer_is_passed_over_quietly() {
        let (mut bus, mut peer) = pair();
        let mut bad = mistyped(delivered(show_folders(vec![Value::U32(2)]), 5));
        bad[2] |= super::NO_REPLY_EXPECTED;
        let good = delivered(show_folders(Vec::new()), 6);
        peer.write_all(&[bad, good].concat()).unwrap();
        assert_eq!(bus.receive().map(|it| it.serial), Some(6));
        peer.set_nonblocking(true).unwrap();
        let mut answer = [0u8; 64];
        assert!(peer.read(&mut answer).is_err(), "something was sent back");
    }

    /// "Unknown types must be ignored" — and ignoring one is passing over it.
    #[test]
    fn a_message_of_an_unknown_type_is_ignored() {
        let (mut bus, mut peer) = pair();
        let mut unknown = delivered(show_folders(Vec::new()), 5);
        unknown[1] = 9;
        let good = delivered(show_folders(Vec::new()), 6);
        peer.write_all(&[unknown, good].concat()).unwrap();
        assert_eq!(bus.receive().map(|it| it.serial), Some(6));
    }

    /// A reply that does not read fails its call instead of leaving it waiting.
    #[test]
    fn a_reply_that_does_not_read_fails_the_call() {
        let (mut bus, mut peer) = pair();
        let reply = Message {
            kind: Kind::Reply,
            serial: 0,
            answers: Some(1),
            peer: Some(":1.9".to_owned()),
            path: None,
            interface: None,
            member: None,
            error: None,
            body: vec![Value::U32(2)],
        };
        peer.write_all(&mistyped(delivered(reply, 3))).unwrap();
        let failed = bus.call_for(show_folders(Vec::new())).expect_err("an unreadable reply");
        // Named as ours: the service answered, and this end could not read it.
        assert!(failed.starts_with(UNREADABLE), "{failed}");
    }

    /// Bytes that are no header are a connection to give up on, not to wait on.
    #[test]
    fn a_stream_out_of_step_is_a_closed_connection() {
        let mut bytes = delivered(show_folders(Vec::new()), 1);
        bytes[0] = b'x';
        assert!(matches!(unmarshal(&bytes), Front::Lost), "an endianness that is neither");
        bytes[0] = b'l';
        bytes[3] = 2;
        assert!(matches!(unmarshal(&bytes), Front::Lost), "a major version that is not ours");

        let (mut bus, mut peer) = pair();
        peer.write_all(&bytes).unwrap();
        assert!(bus.receive().is_none());
    }

    /// One read, every message it completed, and no second read.
    ///
    /// **The AT-SPI thread's shape**: it waits on this socket and on an eventfd,
    /// and a second read after passing over a message that did not read would
    /// sit there while the eventfd's signals went unsent.
    #[test]
    fn a_ready_socket_is_read_once() {
        let (mut bus, mut peer) = pair();
        let first = delivered(show_folders(Vec::new()), 1);
        let second = delivered(show_folders(Vec::new()), 2);
        peer.write_all(&[first, second].concat()).unwrap();
        let read = bus.receive_ready().expect("the connection is open");
        assert_eq!(read.iter().map(|it| it.serial).collect::<Vec<_>>(), [1, 2]);

        peer.write_all(&mistyped(delivered(show_folders(vec![Value::U32(2)]), 3))).unwrap();
        let at = std::time::Instant::now();
        assert_eq!(bus.receive_ready().map(|it| it.len()), Some(0));
        assert!(at.elapsed() < Duration::from_secs(1), "it read a second time");
    }

    /// A call whose body takes it past [`LARGEST`], as it would arrive.
    fn too_large(serial: u32) -> Vec<u8> {
        let body = vec![Value::Bytes(vec![7; LARGEST + 64 * 1024])];
        delivered(show_folders(body), serial)
    }

    /// **Refused from its header**, before a byte of its body has arrived.
    ///
    /// The header's lengths are the message's size; a reader that waited for
    /// the body before looking at them had already buffered all of it.
    #[test]
    fn a_message_past_the_ceiling_is_refused_from_its_header() {
        let bytes = too_large(5);
        let header = bytes.len() - (4 + LARGEST + 64 * 1024);
        let Front::Unread(unread) = unmarshal(&bytes[..header]) else {
            panic!("the header alone did not say it was too large");
        };
        assert_eq!(unread.size, bytes.len());
        assert_eq!(unread.error, LIMITS_EXCEEDED);
        assert_eq!(unread.header.map(|it| it.serial), Some(5), "nobody to refuse");
    }

    /// **Dropped as it arrives, never held**, and the call behind it is read.
    #[test]
    fn a_message_too_large_to_hold_is_dropped_as_it_arrives() {
        let (mut bus, peer) = pair();
        let mut writer = peer.try_clone().unwrap();
        let sending = std::thread::spawn(move || {
            let good = delivered(show_folders(Vec::new()), 6);
            writer.write_all(&[too_large(5), good].concat()).unwrap();
        });
        let next = bus.receive().expect("the call behind the large one was never read");
        sending.join().unwrap();
        assert_eq!(next.serial, 6);
        assert!(
            bus.held.capacity() < LARGEST,
            "{} bytes were buffered for a message that was refused",
            bus.held.capacity()
        );

        let mut peer = peer;
        let mut answer = [0u8; 4096];
        let read = peer.read(&mut answer).unwrap();
        let (refusal, _) = unmarshal(&answer[..read]).whole().expect("a whole refusal");
        assert_eq!(refusal.answers, Some(5));
        assert_eq!(refusal.error.as_deref(), Some(LIMITS_EXCEEDED));
    }

    /// A real connection to whatever bus this machine has.
    ///
    /// **Ignored by default**, because a machine with no session bus is a machine
    /// where this can only fail — and that machine is a supported one, which is
    /// the whole of ADR 0023's degradation clause. Run it deliberately:
    ///
    /// ```text
    /// cargo test -p sk-desktop --release talks_to_a_real_bus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a session bus; run it deliberately"]
    fn talks_to_a_real_bus() {
        #![allow(clippy::print_stdout)]
        let at = std::time::Instant::now();
        let Some(mut bus) = super::Bus::session() else {
            println!("skerry-bus no session bus");
            return;
        };
        let connected = at.elapsed();
        assert!(bus.unique().starts_with(':'), "the bus gave us {:?}", bus.unique());

        // `ListNames` is the one call every bus answers, and its reply is an
        // array of strings — which is the shape most of this marshaller exists
        // for.
        let at = std::time::Instant::now();
        let names = bus
            .call(Message::call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ListNames",
                Vec::new(),
            ))
            .expect("ListNames went unanswered");
        let called = at.elapsed();
        let listed = names.body.first().map(Value::items).unwrap_or_default();
        assert!(listed.len() > 1, "a bus with one name on it");
        assert!(
            listed.iter().any(|name| name.text() == Some("org.freedesktop.DBus")),
            "the bus does not list itself"
        );

        // An error is an error and not a reply, and it says which.
        let refused = bus.call_for(Message::call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NoSuchMethod",
            Vec::new(),
        ));
        assert!(refused.is_err(), "a method that does not exist was answered");

        println!("skerry-bus connect {:.2} ms", connected.as_secs_f64() * 1000.0);
        println!("skerry-bus call {:.2} ms", called.as_secs_f64() * 1000.0);
        println!("skerry-bus names {}", listed.len());
    }
}
