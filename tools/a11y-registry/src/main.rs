// SPDX-License-Identifier: MIT OR Apache-2.0
//! A stand-in for `at-spi2-registryd`, so a scenario can test accessibility.
//!
//! **What it is for.** [ADR 0026](../../../docs/adr/0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md)
//! makes the window publish an AT-SPI tree, and a tree nobody has read is a
//! claim rather than a feature. Reading it needs a bus with a registry on it,
//! because an application that cannot `Embed` does not start its thread — and
//! the machine's own registry is the desktop's, which a test may not disturb.
//!
//! So: own `org.a11y.atspi.Registry`, answer `Embed` with a reference to
//! ourselves, and print what registered. The *reading* is deliberately not done
//! here — `gdbus` does it in the scenario, because a tree checked with this
//! program's own marshaller would agree with it and with nobody else.
//!
//! ```text
//! a11y-registry --address unix:path=/tmp/bus --for 30
//! ```

use std::time::{Duration, Instant};

use sk_desktop::bus::{Bus, Kind, Value};

fn main() -> std::process::ExitCode {
    let mut address = None;
    let mut seconds = 30u64;
    let mut probe = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--address" => address = arguments.next(),
            "--for" => seconds = arguments.next().and_then(|it| it.parse().ok()).unwrap_or(30),
            "--emit-probe" => probe = true,
            other => {
                eprintln!("a11y-registry: unknown option `{other}`");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    let Some(address) = address.or_else(|| std::env::var("AT_SPI_BUS_ADDRESS").ok()) else {
        eprintln!("a11y-registry: --address, or AT_SPI_BUS_ADDRESS in the environment");
        return std::process::ExitCode::FAILURE;
    };
    if probe {
        return emit_probe(&address);
    }
    let Some(mut bus) = Bus::connect(&address) else {
        eprintln!("a11y-registry: no bus at {address}");
        return std::process::ExitCode::FAILURE;
    };
    if !matches!(bus.request_name("org.a11y.atspi.Registry"), sk_desktop::bus::Named::Ours) {
        eprintln!("a11y-registry: somebody else is the registry on that bus");
        return std::process::ExitCode::FAILURE;
    }
    // The line a scenario waits for before it starts the window: a registry that
    // is not on the bus yet is a window that will not publish.
    println!("registry ready");

    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        let Some(message) = bus.receive() else { break };
        if message.kind != Kind::Call {
            continue;
        }
        let member = message.member.clone().unwrap_or_default();
        if member == "Embed" {
            let plug = message
                .body
                .first()
                .and_then(|value| match value {
                    Value::Struct(fields) => Some(fields.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let name = plug.first().and_then(Value::text).unwrap_or_default();
            let path = plug.get(1).and_then(Value::text).unwrap_or_default();
            println!("embedded {name} {path}");
            let desktop = Value::Struct(vec![
                Value::Str("org.a11y.atspi.Registry".to_owned()),
                Value::Path("/org/a11y/atspi/accessible/root".to_owned()),
            ]);
            let _ = bus.reply(&message, vec![desktop]);
            continue;
        }
        let _ = bus.refuse(
            &message,
            "org.freedesktop.DBus.Error.UnknownMethod",
            "this registry only answers Embed",
        );
    }
    std::process::ExitCode::SUCCESS
}

/// Sends one AT-SPI event and reports whether the bus kept the connection.
///
/// **The daemon is the judge.** A message this program can read back is a
/// message this program agrees with; a message `dbus-daemon` forwards is a
/// message that is on the wire correctly, and one it disconnects us over is not.
fn emit_probe(address: &str) -> std::process::ExitCode {
    use sk_desktop::bus::Message;
    let Some(mut bus) = Bus::connect(address) else {
        eprintln!("a11y-registry: no bus at {address}");
        return std::process::ExitCode::FAILURE;
    };
    let sender = Value::Struct(vec![
        Value::Str(bus.unique().to_owned()),
        Value::Path("/org/a11y/atspi/accessible/root".to_owned()),
    ]);
    let bodies: Vec<(&str, Vec<Value>)> = vec![
        ("empty", Vec::new()),
        ("string", vec![Value::Str("x".to_owned())]),
        ("two ints", vec![Value::Str(String::new()), Value::I32(0), Value::I32(0)]),
        (
            "with a variant",
            vec![
                Value::Str(String::new()),
                Value::I32(0),
                Value::I32(0),
                Value::Variant(Box::new(Value::Str(String::new()))),
            ],
        ),
        (
            "with an empty dict",
            vec![
                Value::Str(String::new()),
                Value::I32(0),
                Value::I32(0),
                Value::Variant(Box::new(Value::Str(String::new()))),
                Value::Dict(Vec::new()),
            ],
        ),
        (
            "the whole event",
            vec![
                Value::Str(String::new()),
                Value::I32(0),
                Value::I32(0),
                Value::Variant(Box::new(Value::Str(String::new()))),
                Value::Dict(vec![("sender".to_owned(), sender)]),
            ],
        ),
    ];
    for (what, body) in bodies {
        let message = Message::signal(
            "/org/a11y/atspi/accessible/1",
            "org.a11y.atspi.Event.Window",
            "Activate",
            body,
        );
        let signature = message.body.iter().map(Value::signature).collect::<String>();
        match bus.send(message) {
            Ok(serial) => println!("sent {what} ({signature}) as {serial}"),
            Err(problem) => {
                println!("refused {what}: {problem}");
                return std::process::ExitCode::FAILURE;
            }
        }
        // A ping the daemon must answer. If it has disconnected us over the
        // message above, this is where it shows.
        let ping = Message::call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.Peer",
            "Ping",
            Vec::new(),
        );
        if bus.call(ping).is_none() {
            println!("  ^ the bus dropped us after {what}");
            return std::process::ExitCode::FAILURE;
        }
        println!("  still connected");
    }
    std::process::ExitCode::SUCCESS
}
