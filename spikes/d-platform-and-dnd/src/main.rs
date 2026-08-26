//! Spike D — platform layer and drag-and-drop.
//!
//! Question, bar and method are in [README.md](README.md). This binary is the
//! instrument: `wayland` runs every check that needs nobody else, and the
//! `dnd-*` modes are the halves of a pair, so that a drag against Nautilus and a
//! drag against a second copy of ourselves are the same command with a different
//! argument.

mod report;
mod wl;
mod x11;

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("wayland");

    match mode {
        "probe" => probe(),
        "wayland" => wayland(&args[1..]),
        "dnd-source" => std::process::exit(i32::from(dnd_source(&args[1..]))),
        "dnd-target" => dnd_target(&args[1..]),
        "hang-source" => hang_source(&args[1..]),
        "read-selection" => read_selection(&args[1..]),
        "own-primary" => own_primary(&args[1..]),
        "input-method" => input_method(&args[1..]),
        "dnd-pair" => dnd_pair(&args[1..]),
        "dnd-in" => dnd_in(&args[1..]),
        "x11" => x11_arm(&args[1..]),
        "xdnd-target" => xdnd_target(&args[1..]),
        "xdnd-source" => xdnd_source(&args[1..]),
        "x-own" => x_own(&args[1..]),
        "x-read" => x_read(&args[1..]),
        other => {
            eprintln!(
                "modes: probe | wayland | dnd-source X Y [label] | dnd-target [seconds] [label] \
                 | hang-source [seconds]"
            );
            eprintln!("unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

fn header(title: &str) {
    println!("Spike D — {title}");
    println!("  compositor   {}", compositor_id());
    println!("  session      {}", std::env::var("XDG_SESSION_TYPE").unwrap_or_default());
    println!("  load         {:.2}", report::loadavg());
    println!();
}

fn compositor_id() -> String {
    std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn probe() {
    let (_conn, _queue, app) = wl::App::connect();
    header("advertised interfaces");
    let mut list = app.globals.advertised.clone();
    list.sort();
    for (name, version) in list {
        println!("  v{version:<3} {name}");
    }
}

fn wayland(rest: &[String]) {
    let hold = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let (conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    header("Wayland arm");

    wl::checks::globals(&mut app);

    // The data devices come first, before the surface is ever drawn. A device
    // created after the first keyboard enter is never told what the current
    // selection is — measured, not assumed: the same reader that returned
    // `no-offer` with the device created late reads the selection correctly with
    // it created here.
    if let Some(ddm) = app.globals.ddm.clone() {
        app.data.device = Some(ddm.get_data_device(&app.globals.seat, &qh, ()));
    }
    if let Some(psm) = app.globals.psm.clone() {
        app.data.primary_device = Some(psm.get_device(&app.globals.seat, &qh, ()));
    }

    // A window that has never been drawn has no scale to negotiate and no
    // keyboard focus to own a selection with, so this comes before every check.
    app.wait_for(&mut queue, Duration::from_secs(2), |a| a.configured);
    app.redraw(&qh);
    app.pump(&mut queue, Duration::from_millis(400));
    // The scale can arrive with the first configure or just after it; redrawing
    // once more costs nothing and removes the race from the measurement.
    app.redraw(&qh);
    app.pump(&mut queue, Duration::from_millis(200));

    let fx = wl::checks::Fixture::make();
    app.pump(&mut queue, Duration::from_millis(100));

    wl::checks::fractional_scale(&mut app, &mut queue);
    wl::checks::checkerboard(&mut app);
    wl::checks::keyboard(&mut app, &mut queue);

    wl::checks::clipboard_out(&mut app, &mut queue, &qh, &fx);
    wl::checks::clipboard_in(&mut app, &mut queue, &conn, false);
    wl::checks::primary_out(&mut app, &mut queue, &qh, &fx, output_extent());
    wl::checks::clipboard_in(&mut app, &mut queue, &conn, true);
    wl::checks::slow_transfer(&mut app, &mut queue, &conn);
    wl::checks::text_input(&mut app, &mut queue, &qh);

    println!();
    println!("  {}", app.report.summary());
    println!("  fixture      {}", fx.dir.display());

    if hold > 0 {
        println!("  holding the window open for {hold} s");
        app.pump(&mut queue, Duration::from_secs(hold));
    }
    std::process::exit(i32::from(app.report.failed()));
}

fn dnd_source(rest: &[String]) -> bool {
    // Either an explicit point, or `pid:N` — which is resolved *after* our own
    // window is up, because mapping it re-tiles the workspace and moves the peer.
    let spec = rest.first().cloned().unwrap_or_default();
    let target_pid = spec.strip_prefix("pid:").and_then(|p| p.parse::<u32>().ok());
    let target_addr = spec.strip_prefix("addr:").map(str::to_owned);
    let keyed = target_pid.is_some() || target_addr.is_some();
    let explicit = if keyed {
        None
    } else {
        Some((
            spec.parse::<u32>().unwrap_or(0),
            rest.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0),
        ))
    };
    let label = rest.get(if keyed { 1 } else { 2 }).cloned().unwrap_or_else(|| "unnamed".into());
    let (_conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    header(&format!("drag to {spec} — {label}"));

    let ddm = app.globals.ddm.clone().expect("wl_data_device_manager");
    app.data.device = Some(ddm.get_data_device(&app.globals.seat, &qh, ()));
    app.wait_for(&mut queue, Duration::from_secs(2), |a| a.configured);
    app.redraw(&qh);
    app.pump(&mut queue, Duration::from_millis(600));

    let target = match (&explicit, target_pid, &target_addr) {
        (Some(p), _, _) => Some(*p),
        (None, Some(pid), _) => wl::checks::drop_point_of(pid),
        (None, None, Some(a)) => wl::checks::drop_point_of_addr(a),
        _ => None,
    };
    let Some((x, y)) = target else {
        eprintln!("no drop point: {spec} has no window");
        std::process::exit(1);
    };
    println!("  drop point   {x},{y}");
    let fx = wl::checks::Fixture::make();
    let extent = output_extent();
    wl::checks::dnd_source(&mut app, &mut queue, &qh, &fx, &label, (x, y), extent);
    wl::checks::dnd_action_report(&mut app, false);
    println!();
    println!("  {}", app.report.summary());
    println!("  fixture      {}", fx.dir.display());
    app.report.failed()
}

fn dnd_target(rest: &[String]) {
    let secs = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(30);
    let label = rest.get(1).cloned().unwrap_or_else(|| "unnamed".into());
    let (conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    header(&format!("waiting {secs} s for a drop — {label}"));

    app.wait_for(&mut queue, Duration::from_secs(2), |a| a.configured);
    app.redraw(&qh);
    app.pump(&mut queue, Duration::from_millis(200));
    let ddm = app.globals.ddm.clone().expect("wl_data_device_manager");
    app.data.device = Some(ddm.get_data_device(&app.globals.seat, &qh, ()));

    // The target announces where it is, so the source side of the pair does not
    // have to be told a coordinate by hand.
    if let Some((x, y, w, h)) = wl::checks::window_geometry() {
        println!("  target-rect  {x} {y} {w} {h}");
        println!("  target-drop  {} {}", x + w / 2, y + h / 2);
    }
    wl::checks::dnd_target(&mut app, &mut queue, &conn, &label, Duration::from_secs(secs));
    println!();
    println!("  {}", app.report.summary());
    std::process::exit(i32::from(app.report.failed()));
}

/// The control for the whole drag matrix: one copy of this binary drags onto
/// another. It removes every peer's bugs from the measurement, so a failure
/// against Nautilus can be attributed rather than guessed at.
fn dnd_pair(rest: &[String]) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(&exe)
        .args(["dnd-target", "40", "spike-d"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn target");
    let stdout = child.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let target_pid = child.id();
    let mut transcript = String::new();
    // Wait until the target has a window, then hand the *pid* to the source: the
    // rectangle it prints here is already stale by the time our own window maps.
    let mut ready = false;
    for _ in 0..40 {
        let mut line = String::new();
        use std::io::BufRead;
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        transcript.push_str(&line);
        if line.trim().starts_with("target-drop ") {
            ready = true;
            break;
        }
    }
    if !ready {
        eprintln!("the target never announced a rectangle:\n{transcript}");
        let _ = child.kill();
        std::process::exit(1);
    }
    let label = rest.first().cloned().unwrap_or_else(|| "spike-d".into());
    let failed = dnd_source(&[format!("pid:{target_pid}"), label]);

    use std::io::Read as _;
    let mut tail = String::new();
    let _ = reader.read_to_string(&mut tail);
    let _ = child.wait();
    println!();
    println!("  --- the target's own report ---");
    for line in transcript.lines().chain(tail.lines()) {
        println!("  {line}");
    }
    std::process::exit(i32::from(failed || !tail.contains("PASS")));
}

/// The other direction: a drag that starts inside a peer's window and ends in
/// ours. There is no protocol for "pick up that peer's first item", so the press
/// lands at a position inside the peer's content area and the peer decides what
/// that means. Fragile by nature, which is why the offer contents are reported
/// rather than assumed — a drag of the wrong item still proves the target half.
fn dnd_in(rest: &[String]) {
    let spec = rest.first().cloned().unwrap_or_default();
    let peer_pid = spec.strip_prefix("pid:").and_then(|p| p.parse::<u32>().ok());
    let label = rest.get(1).cloned().unwrap_or_else(|| "unnamed".into());
    let (conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    header(&format!("drag from {spec} into us — {label}"));

    let ddm = app.globals.ddm.clone().expect("wl_data_device_manager");
    app.data.device = Some(ddm.get_data_device(&app.globals.seat, &qh, ()));
    app.wait_for(&mut queue, Duration::from_secs(2), |a| a.configured);
    app.redraw(&qh);
    app.pump(&mut queue, Duration::from_millis(600));

    let Some(pid) = peer_pid else {
        eprintln!("expected pid:N");
        std::process::exit(2);
    };
    let Some((px, py, pw, ph)) = wl::checks::window_geometry_of(pid) else {
        eprintln!("pid {pid} has no window");
        std::process::exit(1);
    };
    let Some((ux, uy, uw, uh)) = wl::checks::window_geometry() else {
        eprintln!("we have no window");
        std::process::exit(1);
    };
    let land = ((ux + uw / 2).max(0) as u32, (uy + uh / 2).max(0) as u32);
    println!("  peer         {px},{py} {pw}x{ph}");
    println!("  us           {ux},{uy} {uw}x{uh}; landing at {},{}", land.0, land.1);

    let extent = output_extent();
    let Some(pointer) = wl::vpointer::Pointer::new(&app, &qh, extent) else {
        eprintln!("no virtual pointer");
        std::process::exit(3);
    };
    app.data.accept_order =
        vec![wl::data::URI_LIST.into(), wl::data::GNOME_COPIED.into(), wl::data::TEXT.into()];

    // There is no protocol request for "pick up that peer's first item", and a
    // single guessed coordinate is a coin flip against an unknown view layout. So
    // the harness searches: a handful of plausible item positions, each tried
    // until a drag actually enters our surface. Which one worked is reported,
    // because it is the only part of this that is not reproducible reasoning.
    let candidates: Vec<(i64, i64)> = vec![
        (70, 130),
        (110, 170),
        (70, 200),
        (160, 150),
        (110, 260),
        (250, 170),
    ];
    let mut used = None;
    for (dx, dy) in candidates {
        let grab = ((px + dx).max(0) as u32, (py + dy).max(0) as u32);
        // Click to select, then press and drag: a view starts a drag from an item,
        // and pressing on empty space starts a rubber band instead.
        pointer.move_to(grab.0, grab.1);
        app.pump(&mut queue, Duration::from_millis(150));
        pointer.press(wl::vpointer::BTN_LEFT);
        app.pump(&mut queue, Duration::from_millis(80));
        pointer.release(wl::vpointer::BTN_LEFT);
        // Longer than any double-click interval. A press 250 ms after a click at
        // the same position is a double click, and a double click on a file opens
        // it — which is how the first version of this searched six positions,
        // opened two text editors, and started no drags at all.
        app.pump(&mut queue, Duration::from_millis(700));

        pointer.press(wl::vpointer::BTN_LEFT);
        app.pump(&mut queue, Duration::from_millis(120));
        let steps = 20;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let x = grab.0 as f64 + (land.0 as f64 - grab.0 as f64) * t;
            let y = grab.1 as f64 + (land.1 as f64 - grab.1 as f64) * t;
            pointer.move_to(x.round() as u32, y.round() as u32);
            app.pump(&mut queue, Duration::from_millis(15));
        }
        app.pump(&mut queue, Duration::from_millis(400));
        if app.data.incoming.entered {
            used = Some(grab);
            pointer.release(wl::vpointer::BTN_LEFT);
            app.pump(&mut queue, Duration::from_millis(300));
            break;
        }
        // Nothing came: put the button down where it started so the peer does not
        // keep a half-finished gesture, and try the next candidate.
        pointer.release(wl::vpointer::BTN_LEFT);
        app.pump(&mut queue, Duration::from_millis(200));
        pointer.move_to(land.0, land.1);
        app.pump(&mut queue, Duration::from_millis(100));
    }
    pointer.destroy();
    match used {
        Some((gx, gy)) => println!("  grabbed at   {gx},{gy}"),
        None => println!("  grabbed at   nothing — no candidate position started a drag"),
    }

    // The drag has already been released, so the target half only has to collect
    // what arrived. Waiting for another one would report "no drag entered" for a
    // drag that worked.
    wl::checks::collect_drop(&mut app, &mut queue, &conn, &label);
    println!();
    println!("  {}", app.report.summary());
    std::process::exit(i32::from(app.report.failed()));
}

fn x11_arm(_rest: &[String]) {
    let Some(mut x) = x11::X11::open("skerry spike D (X11)") else {
        eprintln!("no X11 display");
        std::process::exit(3);
    };
    header("X11 arm");
    let fx = wl::checks::Fixture::make();
    x11::checks::window(&mut x);
    let clipboard = x.atoms.CLIPBOARD;
    let primary = u32::from(x11rb::protocol::xproto::AtomEnum::PRIMARY);
    x11::checks::selection_out(&mut x, "x-clipboard-out", clipboard, false, &fx.uris);
    x11::checks::selection_in(&mut x, "x-clipboard-in", clipboard, false);
    x11::checks::selection_out(&mut x, "x-primary-out", primary, true, &fx.uris);
    x11::checks::selection_in(&mut x, "x-primary-in", primary, true);
    x11::checks::incr(&mut x);
    x11::checks::bridge(&mut x, &fx.uris);
    x11::checks::action_vocabulary(&mut x);
    println!();
    println!("  {}", x.report.summary());
    println!("  fixture      {}", fx.dir.display());
    std::process::exit(i32::from(x.report.failed()));
}

/// Own an X11 selection and serve it for a while. Paired with `x-read`, this
/// isolates our ICCCM handling from the compositor's X11-to-Wayland bridge — the
/// two are otherwise indistinguishable, and both are plausible suspects.
fn x_own(rest: &[String]) {
    let which = rest.first().map(String::as_str).unwrap_or("clipboard");
    let secs = rest.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(20);
    let big = rest.get(2).map(String::as_str) == Some("big");
    let Some(x) = x11::X11::open("skerry spike D (X11 owner)") else {
        eprintln!("no X11 display");
        std::process::exit(3);
    };
    let selection = if which == "primary" {
        u32::from(x11rb::protocol::xproto::AtomEnum::PRIMARY)
    } else {
        x.atoms.CLIPBOARD
    };
    let bytes = if big {
        (0..40_000).map(|i| format!("file:///tmp/incr/{i:06}-name.txt\r\n")).collect::<String>()
    } else {
        "file:///tmp/spike-d-x11-owner.txt\r\n".to_string()
    };
    let payloads = vec![
        x11::sel::Payload { target: x.atoms.text_uri_list, bytes: bytes.clone().into_bytes() },
        x11::sel::Payload { target: x.atoms.text_plain_utf8, bytes: bytes.clone().into_bytes() },
        x11::sel::Payload { target: x.atoms.UTF8_STRING, bytes: bytes.into_bytes() },
    ];
    let Some(mut owner) = x11::sel::Owner::take(&x, selection, payloads) else {
        println!("owner-failed");
        std::process::exit(1);
    };
    println!("owning {which} for {secs} s, window {:#x}", x.window);
    let served = owner.serve(Duration::from_secs(secs));
    println!("served {served} requests: {:?}; INCR chunks {}", owner.requests, owner.incr_chunks);
    println!("refused: {:?}", owner.refused);
}

/// Read an X11 selection with our own requestor, including the `INCR` path.
fn x_read(rest: &[String]) {
    let which = rest.first().map(String::as_str).unwrap_or("clipboard");
    let target = rest.get(1).map(String::as_str).unwrap_or("text/uri-list");
    let Some(x) = x11::X11::open("skerry spike D (X11 reader)") else {
        eprintln!("no X11 display");
        std::process::exit(3);
    };
    let selection = if which == "primary" {
        u32::from(x11rb::protocol::xproto::AtomEnum::PRIMARY)
    } else {
        x.atoms.CLIPBOARD
    };
    let target_atom = match target {
        "text/uri-list" => x.atoms.text_uri_list,
        "UTF8_STRING" => x.atoms.UTF8_STRING,
        "TARGETS" => x.atoms.TARGETS,
        other => {
            eprintln!("unknown target {other}");
            std::process::exit(2);
        }
    };
    use x11rb::protocol::xproto::ConnectionExt as _;
    let holder = x.conn.get_selection_owner(selection).ok().and_then(|c| c.reply().ok());
    println!("owner {:?}", holder.map(|r| format!("{:#x}", r.owner)));
    match x11::sel::request(&x, selection, target_atom, Duration::from_secs(5)) {
        Some(r) if target_atom == x.atoms.TARGETS => {
            let atoms: Vec<u32> = r.bytes.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
            let names: Vec<String> = atoms.iter().map(|a| x.atom_name(*a)).collect();
            println!("targets {names:?}");
        }
        Some(r) => println!(
            "bytes {} incr {} chunks {} worst-gap {:.3} ms first {:?}",
            r.bytes.len(),
            r.incr,
            r.chunks,
            r.worst_gap_ms,
            String::from_utf8_lossy(&r.bytes[..r.bytes.len().min(60)]).to_string()
        ),
        None => println!("no SelectionNotify"),
    }
}

fn xdnd_target(rest: &[String]) {
    let secs = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(30);
    let label = rest.get(1).cloned().unwrap_or_else(|| "unnamed".into());
    let Some(mut x) = x11::X11::open("skerry spike D (X11 target)") else {
        eprintln!("no X11 display");
        std::process::exit(3);
    };
    header(&format!("X11 waiting {secs} s for a drop — {label}"));
    if let Some((wx, wy, w, h)) = x11_geometry(&x) {
        println!("  target-rect  {wx} {wy} {w} {h}");
        println!("  target-drop  {} {}", wx + w / 2, wy + h / 2);
    }
    x11::checks::xdnd_target(&mut x, &label, Duration::from_secs(secs));
    println!();
    println!("  {}", x.report.summary());
    std::process::exit(i32::from(x.report.failed()));
}

fn xdnd_source(rest: &[String]) {
    let spec = rest.first().cloned().unwrap_or_default();
    let target_pid = spec.strip_prefix("pid:").and_then(|p| p.parse::<u32>().ok());
    let mut args = rest.iter().skip(if target_pid.is_some() { 1 } else { 2 });
    let label = args.next().cloned().unwrap_or_else(|| "unnamed".into());
    let link = args.next().map(String::as_str) == Some("link");
    let Some(mut x) = x11::X11::open("skerry spike D (X11 source)") else {
        eprintln!("no X11 display");
        std::process::exit(3);
    };
    header(&format!("XDND drag to {spec} — {label}"));
    let fx = wl::checks::Fixture::make();
    let (x_pos, y_pos) = match target_pid {
        Some(pid) => match wl::checks::drop_point_of(pid) {
            Some(p) => p,
            None => {
                eprintln!("pid {pid} has no window");
                std::process::exit(1);
            }
        },
        None => (
            spec.parse::<u32>().unwrap_or(0),
            rest.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0),
        ),
    };
    println!("  drop point   {x_pos},{y_pos}");
    // The pointer has to be over the target before the source looks for one, so
    // the virtual pointer is driven from a separate connection to the compositor.
    move_pointer(x_pos, y_pos);
    std::thread::sleep(Duration::from_millis(250));
    x11::checks::xdnd_source(&mut x, &label, &fx.uris, link);
    println!();
    println!("  {}", x.report.summary());
    println!("  fixture      {}", fx.dir.display());
    std::process::exit(i32::from(x.report.failed()));
}

/// The window rectangle, in the X server's pixels rather than the compositor's
/// logical units — Xwayland reports what it renders.
fn x11_geometry(x: &x11::X11) -> Option<(i16, i16, i16, i16)> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let g = x.conn.get_geometry(x.window).ok()?.reply().ok()?;
    let t = x.conn.translate_coordinates(x.window, x.root, 0, 0).ok()?.reply().ok()?;
    let scale = hypr_scale();
    // grim and hyprctl speak logical units and the virtual pointer takes logical
    // extents, so an X11 rectangle has to be divided by the scale to be pointed at.
    Some((
        (t.dst_x as f64 / scale) as i16,
        (t.dst_y as f64 / scale) as i16,
        (g.width as f64 / scale) as i16,
        (g.height as f64 / scale) as i16,
    ))
}

fn hypr_scale() -> f64 {
    std::process::Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()
        .and_then(|o| field_f(&String::from_utf8_lossy(&o.stdout), "\"scale\": "))
        .unwrap_or(1.0)
}

/// Park the pointer using a throwaway Wayland connection, so the X11 arm does not
/// have to know about `zwlr_virtual_pointer_v1`.
fn move_pointer(x: u32, y: u32) {
    let (_conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    let extent = output_extent();
    if let Some(p) = wl::vpointer::Pointer::new(&app, &qh, extent) {
        p.move_to(x, y);
        app.pump(&mut queue, Duration::from_millis(150));
        p.destroy();
    }
    app.pump(&mut queue, Duration::from_millis(50));
}

/// Act as an input method and nothing else, so that another *process* — arm 2, on
/// `winit` — can be the application under test. Without this the winit arm's IME
/// claim would rest on reading its source rather than on a composition arriving.
fn input_method(rest: &[String]) {
    let secs = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(30);
    let (_conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    let Some(imm) = app.globals.imm.clone() else {
        eprintln!("no zwp_input_method_manager_v2");
        std::process::exit(3);
    };
    let seat = app.globals.seat.clone();
    app.ime.bind_method(&imm, &seat, &qh);
    queue.flush().ok();
    app.pump(&mut queue, Duration::from_millis(200));
    if app.ime.method_unavailable {
        eprintln!("another input method already holds the seat");
        std::process::exit(3);
    }
    println!("input method bound; composing into whatever takes focus for {secs} s");
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut composed = 0u32;
    while std::time::Instant::now() < deadline {
        app.pump(&mut queue, Duration::from_millis(250));
        if app.ime.method_active && composed == 0 {
            // A preedit, a pause long enough to be visible, then the commit that
            // replaces it — the shape of a real composition.
            app.ime.compose();
            queue.flush().ok();
            app.pump(&mut queue, Duration::from_millis(600));
            app.ime.finish();
            queue.flush().ok();
            composed += 1;
            println!("composed {:?} then committed {:?}", wl::ime::PREEDIT, wl::ime::COMMIT);
        }
        if !app.ime.method_active {
            composed = 0;
        }
    }
}

/// Isolation for the `primary-out` failure: own the primary selection and hold
/// it, printing every serial involved, so an external reader can be pointed at it
/// by hand.
fn own_primary(rest: &[String]) {
    let secs = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(20);
    let (_conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    app.wait_for(&mut queue, Duration::from_secs(2), |a| a.configured);
    app.redraw(&qh);
    app.pump(&mut queue, Duration::from_millis(400));
    let psm = app.globals.psm.clone().expect("primary selection manager");
    app.data.primary_device = Some(psm.get_device(&app.globals.seat, &qh, ()));
    let ddm = app.globals.ddm.clone().expect("data device manager");
    app.data.device = Some(ddm.get_data_device(&app.globals.seat, &qh, ()));
    app.pump(&mut queue, Duration::from_millis(100));

    let extent = output_extent();
    let clicked = wl::checks::click_self(&mut app, &mut queue, &qh, extent);
    println!("  focus            {}", app.seat_state.keyboard_focus);
    println!("  kbd enter serial {:?}", app.seat_state.keyboard_enter_serial);
    println!("  button serial    {:?}", app.seat_state.last_button_serial);
    println!("  click landed     {clicked}");

    app.data.serve(&["file:///tmp/spike-d-own-primary.txt"]);
    let serial = app
        .seat_state
        .last_button_serial
        .or(app.seat_state.keyboard_enter_serial)
        .unwrap_or(0);
    let source = psm.create_source(&qh, ());
    source.offer(wl::data::TEXT.into());
    source.offer(wl::data::URI_LIST.into());
    app.data.primary_device.as_ref().unwrap().set_selection(Some(&source), serial);
    queue.flush().ok();
    println!("  set_selection    serial {serial}");
    app.pump(&mut queue, Duration::from_millis(500));
    println!("  cancelled        {}", app.data.source_cancelled);
    println!("  primary events   {}", app.data.primary_events);
    println!("  holding {secs} s — try: wl-paste --primary --list-types");
    app.pump(&mut queue, Duration::from_secs(secs));
    println!("  requests served  {:?}", app.data.outgoing.requested);
}

/// A reader that uses `wl_data_device` and `zwp_primary_selection_device_v1` — the
/// protocols under test — rather than a data-control manager. That distinction is
/// the whole point: when a third-party tool sees nothing, this says whether the
/// selection exists at all.
///
/// It takes keyboard focus by mapping a window, which is what makes the offer
/// arrive; the process that owns the selection keeps owning it after it loses
/// focus.
fn read_selection(rest: &[String]) {
    let primary = rest.first().map(String::as_str) == Some("primary");
    let (conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    // The data devices are created before the surface is ever drawn. A device
    // created after the first keyboard enter may never be told what the current
    // selection is, because the compositor sends `selection` on focus change and
    // not on device creation.
    let ddm = app.globals.ddm.clone().expect("wl_data_device_manager");
    app.data.device = Some(ddm.get_data_device(&app.globals.seat, &qh, ()));
    if let Some(psm) = app.globals.psm.clone() {
        app.data.primary_device = Some(psm.get_device(&app.globals.seat, &qh, ()));
    }
    app.wait_for(&mut queue, Duration::from_secs(2), |a| a.configured);
    app.redraw(&qh);
    let got = app.wait_for(&mut queue, Duration::from_secs(3), move |a| {
        if primary {
            a.data.primary_events > 0
        } else {
            a.data.selection_events > 0
        }
    });
    if !got {
        println!("no-offer");
        return;
    }
    let mimes = if primary { app.data.primary_mimes.clone() } else { app.data.selection_mimes.clone() };
    println!("types {}", mimes.join(" "));
    let mut t = match (primary, app.data.primary_selection.clone(), app.data.selection.clone()) {
        (true, Some(o), _) => wl::data::Transfer::primary(&conn, &o, wl::data::TEXT),
        (false, _, Some(o)) => wl::data::Transfer::offer(&conn, &o, wl::data::TEXT),
        _ => {
            println!("offer-null");
            return;
        }
    };
    let (done, _) = wl::data::drain(&mut app, &mut queue, &mut t, Duration::from_secs(3));
    println!("done {done} bytes {} text {}", t.bytes.len(), t.text().replace('\n', " "));
}

/// The other half of the `slow-transfer` check. No window, no focus, no surface:
/// it owns the selection through a data-control manager and then does nothing
/// when asked for the bytes.
fn hang_source(rest: &[String]) {
    let secs = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(20);
    let (_conn, mut queue, mut app) = wl::App::connect();
    let qh = queue.handle();
    if !wl::hang::own_selection(&mut app, &qh, "text/plain;charset=utf-8") {
        eprintln!("no zwlr_data_control_manager_v1; cannot play the hung peer");
        std::process::exit(3);
    }
    app.pump(&mut queue, Duration::from_secs(secs));
    println!("held {} unanswered requests", app.hang.held.len());
}

/// The virtual pointer's absolute motion is in output extents, so it needs the
/// output's logical size rather than its resolution.
fn output_extent() -> (u32, u32) {
    let out = std::process::Command::new("hyprctl").args(["monitors", "-j"]).output();
    if let Ok(out) = out {
        let s = String::from_utf8_lossy(&out.stdout);
        let (w, h, scale) = (
            field(&s, "\"width\": "),
            field(&s, "\"height\": "),
            field_f(&s, "\"scale\": "),
        );
        if let (Some(w), Some(h), Some(scale)) = (w, h, scale) {
            if scale > 0.0 {
                return ((w as f64 / scale) as u32, (h as f64 / scale) as u32);
            }
        }
    }
    (1920, 1080)
}

fn field(s: &str, key: &str) -> Option<u64> {
    let rest = &s[s.find(key)? + key.len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

fn field_f(s: &str, key: &str) -> Option<f64> {
    let rest = &s[s.find(key)? + key.len()..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    rest[..end].parse().ok()
}
