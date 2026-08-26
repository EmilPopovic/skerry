//! X11 checks. Everything here also passes through Xwayland and the compositor's
//! X11 window manager, so a failure has three possible owners and each check says
//! which one it can rule out.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _};

use super::sel::{self, Payload, INCR_THRESHOLD};
use super::{xdnd, X11};
use crate::report::Outcome;

pub fn window(x: &mut X11) {
    let aware = xdnd::aware_version(x, x.window);
    let dpi = xft_dpi(x);
    let (mm, px) = randr_size(x);
    // Xwayland tells an X11 client nothing about a fractional scale. This is not
    // our bug and it is not fixable from inside the client, so it is recorded as
    // the limitation it is: the X11 path is drawn at 1x and upscaled by the
    // compositor, and the fallback is to keep the X11 path for compatibility
    // rather than for quality.
    let evidence = format!(
        "window {}x{} mapped, XdndAware {:?}; Xft.dpi {}; RANDR reports {}x{} px over {}x{} mm \
         = {:.1} dpi",
        x.width,
        x.height,
        aware,
        dpi.map(|d| d.to_string()).unwrap_or_else(|| "unset".into()),
        px.0,
        px.1,
        mm.0,
        mm.1,
        if mm.0 > 0 { px.0 as f64 * 25.4 / mm.0 as f64 } else { f64::NAN }
    );
    if aware == Some(super::XDND_VERSION) {
        x.report.pass("x-window", evidence);
    } else {
        x.report.fail("x-window", evidence);
    }
}

/// Own a selection and have a *second process of ours* read it. This is the check
/// on our own ICCCM code, with no bridge and no toolkit in the path.
pub fn selection_out(x: &mut X11, name: &'static str, selection: Atom, primary: bool, uris: &[String]) {
    let uri_list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let payloads = vec![
        Payload { target: x.atoms.text_uri_list, bytes: uri_list.into_bytes() },
        Payload { target: x.atoms.text_plain_utf8, bytes: uris.join("\n").into_bytes() },
        Payload { target: x.atoms.UTF8_STRING, bytes: uris.join("\n").into_bytes() },
    ];
    let Some(mut owner) = sel::Owner::take(x, selection, payloads) else {
        x.report.fail(name, "set_selection_owner did not stick");
        return;
    };
    let which = if primary { "primary" } else { "clipboard" };
    let (targets, _) = read_while_serving(&mut owner, &["x-read", which, "TARGETS"]);
    let (data, served) = read_while_serving(&mut owner, &["x-read", which, "text/uri-list"]);
    let evidence = format!(
        "our own requestor read TARGETS as {} and the data as {}; {served} requests served, \
         asked for {:?}",
        one_line(&targets),
        one_line(&data),
        owner.requests
    );
    let outcome = if data.contains("file://") && targets.contains("text/uri-list") {
        Outcome::Pass
    } else {
        Outcome::Fail
    };
    owner.release();
    drop(owner);
    x.report.record(name, outcome, evidence);
}

/// Read a selection owned by **GTK**, which is the check on our requestor. GTK is
/// a separate process (see gtk-peer.py) and stays a banned dependency; what is
/// being measured is interop with it, which cannot be measured without it.
pub fn selection_in(x: &mut X11, name: &'static str, selection: Atom, primary: bool) {
    let marker = format!("spike-d-gtk-{}", if primary { "primary" } else { "clipboard" });
    let which = if primary { "primary" } else { "clipboard" };
    let mut peer = match Command::new("./gtk-peer.py")
        .args(["own", which, &marker, "20"])
        .env("GDK_BACKEND", "x11")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            x.report.owed(name, format!("no GTK peer available: {e}"));
            return;
        }
    };
    // Wait for the peer to say it owns the selection rather than sleeping a
    // guessed interval.
    let owned = wait_for_owner(x, selection, Duration::from_secs(6));
    if !owned {
        let _ = peer.kill();
        x.report.fail(name, "the GTK peer never took the selection");
        return;
    }
    let targets = sel::request(x, selection, x.atoms.TARGETS, Duration::from_secs(3));
    let text = sel::request(x, selection, x.atoms.UTF8_STRING, Duration::from_secs(3));
    let names = targets
        .map(|r| {
            r.bytes
                .chunks_exact(4)
                .map(|c| x.atom_name(u32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let got = text.as_ref().map(|r| String::from_utf8_lossy(&r.bytes).to_string()).unwrap_or_default();
    let evidence = format!(
        "GTK offered {} targets ({}); we read {} bytes = {:?} with a real server timestamp",
        names.split_whitespace().count(),
        one_line(&names),
        got.len(),
        one_line(&got)
    );
    let outcome = if got.trim() == marker { Outcome::Pass } else { Outcome::Fail };
    let _ = peer.kill();
    let _ = peer.wait();
    x.report.record(name, outcome, evidence);
}

/// The compositor's X11-to-Wayland clipboard bridge, in both directions, with a
/// GTK client as the control. If GTK fails the same way we do, the bridge is the
/// subject of the measurement and not our code — which is the difference between
/// a bug we can fix and a limitation we have to design around.
pub fn bridge(x: &mut X11, uris: &[String]) {
    // X11 owner -> Wayland reader.
    let uri_list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let payloads = vec![
        Payload { target: x.atoms.text_uri_list, bytes: uri_list.into_bytes() },
        Payload { target: x.atoms.UTF8_STRING, bytes: uris.join("\n").into_bytes() },
    ];
    let clipboard = x.atoms.CLIPBOARD;
    let ours = match sel::Owner::take(x, clipboard, payloads) {
        Some(mut owner) => {
            let (text, _) = read_while_serving(&mut owner, &["--wl-paste", "--list-types"]);
            owner.release();
            text
        }
        None => String::new(),
    };
    let gtk = gtk_owner_then_wl_paste();
    let evidence = format!(
        "X11 owner to Wayland reader: with our window as owner wl-paste saw {:?}; with a GTK \
         X11 window as owner it saw {:?}. Both of those are read by the same wl-paste that \
         reads a Wayland owner correctly",
        one_line(&ours),
        one_line(&gtk)
    );
    let outcome = if !ours.trim().is_empty() {
        Outcome::Pass
    } else if gtk.trim().is_empty() {
        // GTK cannot cross it either.
        Outcome::Limitation
    } else {
        Outcome::Fail
    };
    x.report.record("x-bridge-out", outcome, evidence);

    // Wayland owner -> X11 reader.
    let marker = format!("spike-d-bridge-{}", std::process::id());
    let ok = Command::new("wl-copy").arg(&marker).status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        x.report.owed("x-bridge-in", "wl-copy unavailable");
        return;
    }
    std::thread::sleep(Duration::from_millis(400));
    let mine = sel::request(x, clipboard, x.atoms.UTF8_STRING, Duration::from_secs(3))
        .map(|r| String::from_utf8_lossy(&r.bytes).to_string())
        .unwrap_or_else(|| "no SelectionNotify".into());
    let gtk_read = Command::new("./gtk-peer.py")
        .args(["read", "clipboard"])
        .env("GDK_BACKEND", "x11")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let gtk_got = gtk_read.lines().any(|l| l.contains(&marker));
    let evidence = format!(
        "Wayland owner to X11 reader: our requestor got {:?}; a GTK X11 client got {}. The same \
         requestor reads a GTK X11 owner correctly, which is what places the failure",
        one_line(&mine),
        if gtk_got { "the marker" } else { "nothing" }
    );
    let outcome = if mine.trim() == marker {
        Outcome::Pass
    } else if !gtk_got {
        Outcome::Limitation
    } else {
        Outcome::Fail
    };
    x.report.record("x-bridge-in", outcome, evidence);
    for p in ["wl-copy"] {
        let _ = Command::new("pkill").args(["-x", p]).status();
    }
}

/// A file manager reaches `INCR` with a few thousand files selected, so the size
/// here is chosen to be past the threshold rather than to be large.
pub fn incr(x: &mut X11) {
    let entries = 40_000;
    let body: String = (0..entries).map(|i| format!("file:///tmp/incr/{i:06}-name.txt\r\n")).collect();
    let total = body.len();
    let payloads = vec![Payload { target: x.atoms.text_uri_list, bytes: body.into_bytes() }];
    let Some(mut owner) = sel::Owner::take(x, x.atoms.CLIPBOARD, payloads) else {
        x.report.fail("x-incr", "set_selection_owner did not stick");
        return;
    };
    let (text, served) = read_while_serving(&mut owner, &["x-read", "clipboard", "text/uri-list"]);
    // `x-read` prints a summary line, so the byte count comes from it rather than
    // from a megabyte of output on a terminal.
    let reported: usize = text
        .split_whitespace()
        .skip_while(|w| *w != "bytes")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let chunks: usize = text
        .split_whitespace()
        .skip_while(|w| *w != "chunks")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let lines = reported / "file:///tmp/incr/000000-name.txt\r\n".len();
    let evidence = format!(
        "{total} bytes over the {INCR_THRESHOLD}-byte threshold, {entries} entries offered; our \
         requestor received {reported} bytes in {chunks} property chunks, which is {lines} \
         entries; the owner wrote {} chunks and served {served} requests",
        owner.incr_chunks
    );
    let outcome = if reported == total { Outcome::Pass } else { Outcome::Fail };
    owner.release();
    drop(owner);
    x.report.record("x-incr", outcome, evidence);
}

pub fn xdnd_source(x: &mut X11, label: &str, uris: &[String], action_link: bool) {
    let uri_list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let payloads = vec![
        Payload { target: x.atoms.text_uri_list, bytes: uri_list.into_bytes() },
        Payload { target: x.atoms.text_plain_utf8, bytes: uris.join("\n").into_bytes() },
    ];
    let action = if action_link { x.atoms.XdndActionLink } else { x.atoms.XdndActionCopy };
    let Some(target) = xdnd::target_under_pointer(x) else {
        x.report.fail(
            format!("xdnd-source/{label}"),
            "no XdndAware window under the pointer — nothing to drag onto",
        );
        return;
    };
    let tw = target.window;
    let tv = target.version;
    let Some(mut src) = xdnd::Source::new(x, payloads, action) else {
        x.report.fail(format!("xdnd-source/{label}"), "could not own XdndSelection");
        return;
    };
    let now = x.server_time();
    src.enter(target);
    let ptr = x.conn.query_pointer(x.root).ok().and_then(|c| c.reply().ok());
    let (px, py) = ptr.map(|p| (p.root_x, p.root_y)).unwrap_or((0, 0));
    for i in 0..8 {
        src.position(px, py + i, action, now + i as u32);
        src.pump(Duration::from_millis(40));
    }
    src.drop(x.server_time());
    src.pump(Duration::from_millis(1200));

    let r = &src.result;
    let evidence = format!(
        "target {tw:#x} speaks XDND v{tv}; {} XdndStatus messages, accepted {}, action {:?}; \
         asked us for {:?}; XdndFinished {} accepted {:?}",
        r.status_messages, r.accepted, r.action, r.data_requested, r.finished, r.finished_accepted
    );
    let outcome = if r.accepted && !r.data_requested.is_empty() {
        Outcome::Pass
    } else if r.status_messages > 0 {
        Outcome::Limitation
    } else {
        Outcome::Fail
    };
    x.report.record(format!("xdnd-source/{label}"), outcome, evidence);
}

pub fn xdnd_target(x: &mut X11, label: &str, wait: Duration) {
    let prefer = [x.atoms.text_uri_list, x.atoms.gnome_copied_files, x.atoms.text_plain_utf8];
    match xdnd::receive(x, &prefer, wait) {
        Some(got) => {
            let text = String::from_utf8_lossy(&got.bytes).to_string();
            let evidence = format!(
                "source {:?} speaks XDND v{}; offered {:?}; action {:?}; {} XdndPosition \
                 messages; took {:?} and read {} bytes = {:?}; worst loop gap {:.3} ms",
                got.source.map(|w| format!("{w:#x}")),
                got.version,
                got.types,
                got.action,
                got.position_messages,
                got.chosen,
                got.bytes.len(),
                one_line(&text),
                got.worst_gap_ms
            );
            x.report.record(
                format!("xdnd-target/{label}"),
                if got.bytes.is_empty() { Outcome::Fail } else { Outcome::Pass },
                evidence,
            );
        }
        None => x.report.fail(format!("xdnd-target/{label}"), format!("no XdndEnter within {wait:?}")),
    }
}

/// XDND has four actions plus private ones; Wayland has three and no link. The
/// number is the point: a "link here" menu entry cannot be built on a shared
/// abstraction, so `sk-plat` has to expose the union and let the policy above it
/// decide what to grey out.
pub fn action_vocabulary(x: &mut X11) {
    let xdnd = ["XdndActionCopy", "XdndActionMove", "XdndActionLink", "XdndActionAsk", "XdndActionPrivate"];
    let wayland = ["copy", "move", "ask"];
    x.report.record(
        "dnd-action-vocab",
        Outcome::Limitation,
        format!(
            "XDND offers {} actions ({}); wl_data_device_manager.dnd_action offers {} ({}) and has \
             no link. A link drop on Wayland has to be expressed as `ask` plus a menu of ours",
            xdnd.len(),
            xdnd.join(" "),
            wayland.len(),
            wayland.join(" ")
        ),
    );
}

// ---------------------------------------------------------------------------

/// A reader child cannot be waited on with `status()`: it asks the X11 owner —
/// us — for the bytes, so the owner has to keep serving while the child runs.
/// `--wl-paste` as the first argument runs `wl-paste` instead of this binary.
fn read_while_serving(owner: &mut sel::Owner, args: &[&str]) -> (String, usize) {
    let child = if args[0] == "--wl-paste" {
        Command::new("wl-paste").args(&args[1..]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    } else {
        Command::new(std::env::current_exe().unwrap_or_else(|_| "spike-d-platform".into()))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    let Ok(mut child) = child else { return (String::new(), 0) };
    let mut out = child.stdout.take().unwrap();
    let mut served = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        served += owner.serve(Duration::from_millis(10));
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
    }
    served += owner.serve(Duration::from_millis(50));
    let _ = child.kill();
    let _ = child.wait();
    let mut buf = String::new();
    out.read_to_string(&mut buf).ok();
    (buf, served)
}

/// Poll until somebody other than us owns the selection.
fn wait_for_owner(x: &X11, selection: Atom, timeout: Duration) -> bool {
    let end = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < end {
        if let Some(r) = x.conn.get_selection_owner(selection).ok().and_then(|c| c.reply().ok()) {
            if r.owner != 0 && r.owner != x.window {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Let GTK own an X11 selection and see whether the Wayland side ever hears about
/// it. The control for `x-bridge-out`.
fn gtk_owner_then_wl_paste() -> String {
    let peer = Command::new("./gtk-peer.py")
        .args(["own", "clipboard", "spike-d-gtk-bridge", "8"])
        .env("GDK_BACKEND", "x11")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut peer) = peer else { return String::new() };
    std::thread::sleep(Duration::from_millis(1200));
    let out = Command::new("wl-paste")
        .args(["--list-types"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let _ = peer.kill();
    let _ = peer.wait();
    out
}

fn xft_dpi(x: &X11) -> Option<u32> {
    let r = x
        .conn
        .get_property(false, x.root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16)
        .ok()?
        .reply()
        .ok()?;
    let text = String::from_utf8_lossy(&r.value);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Xft.dpi:") {
            return v.trim().parse().ok();
        }
    }
    None
}

fn randr_size(x: &X11) -> ((u16, u16), (u16, u16)) {
    let s = &x.conn.setup().roots[x.screen];
    ((s.width_in_millimeters, s.height_in_millimeters), (s.width_in_pixels, s.height_in_pixels))
}

fn one_line(s: &str) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() > 90 {
        format!("{}...", flat.chars().take(90).collect::<String>())
    } else {
        flat
    }
}
