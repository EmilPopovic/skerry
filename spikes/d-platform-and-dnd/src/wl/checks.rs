// SPDX-License-Identifier: MIT OR Apache-2.0
//! The check bodies, in the order a run executes them. Every one either records
//! a number or records why there is not one; none of them prints a verdict,
//! because the verdict is the conjunction and belongs to the caller.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wayland_client::{Connection, EventQueue, QueueHandle};

use super::data::{self, Transfer, GNOME_COPIED, TEXT, URI_LIST};
use super::{ime, paint, vpointer, App, LOGICAL};
use crate::report::{Outcome, Samples};

/// Two files in a real directory, so that a peer which acts on the drop — and
/// Nautilus does — acts on something harmless.
pub struct Fixture {
    pub dir: std::path::PathBuf,
    pub uris: Vec<String>,
}

impl Fixture {
    pub fn make() -> Self {
        // `SPIKE_D_FIXTURE_DIR` exists because a sandboxed peer cannot read
        // `/tmp` at all — a snap gets a private one — so a drag of a `/tmp` path
        // to a confined target is refused for a reason that has nothing to do
        // with the protocol.
        let base = std::env::var_os("SPIKE_D_FIXTURE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("skerry-spike-d-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("dest")).expect("fixture");
        let mut uris = Vec::new();
        for name in ["dragged-one.txt", "dragged-two.txt"] {
            let p = dir.join(name);
            std::fs::write(&p, format!("spike D fixture: {name}\n")).expect("write");
            uris.push(format!("file://{}", p.display()));
        }
        Self { dir, uris }
    }

    pub fn uri_refs(&self) -> Vec<&str> {
        self.uris.iter().map(|s| s.as_str()).collect()
    }
}

pub fn globals(app: &mut App) {
    let mut wanted = vec![
        ("wl_compositor", true),
        ("wl_shm", true),
        ("wl_seat", true),
        ("xdg_wm_base", true),
        ("wp_viewporter", app.globals.viewporter.is_some()),
        ("wp_fractional_scale_manager_v1", app.globals.frac.is_some()),
        ("wl_data_device_manager", app.globals.ddm.is_some()),
        ("zwp_primary_selection_device_manager_v1", app.globals.psm.is_some()),
        ("zwp_text_input_manager_v3", app.globals.tim.is_some()),
    ];
    // Instruments, not requirements: their absence changes what this spike can
    // automate, not what the product can do.
    let instruments = [
        ("zwlr_virtual_pointer_manager_v1", app.globals.vpm.is_some()),
        ("zwp_input_method_manager_v2", app.globals.imm.is_some()),
    ];
    let missing: Vec<_> = wanted.iter().filter(|(_, ok)| !ok).map(|(n, _)| *n).collect();
    wanted.retain(|(_, ok)| *ok);
    let evidence = format!(
        "{} interfaces advertised; required present: {}; instruments: {}",
        app.globals.advertised.len(),
        wanted.len(),
        instruments.iter().filter(|(_, ok)| *ok).count(),
    );
    if missing.is_empty() {
        app.report.pass("globals", evidence);
    } else {
        app.report.fail("globals", format!("{evidence}; missing {}", missing.join(", ")));
    }
    for (n, ok) in instruments {
        if !ok {
            app.report.owed(
                "instrument",
                format!("{n} absent — the drag matrix cannot be automated on this compositor"),
            );
        }
    }
}

/// The condition Spike A added. Three separate claims, and the third is the one
/// that a golden frame could not see.
pub fn fractional_scale(app: &mut App, queue: &mut EventQueue<App>) {
    if app.globals.frac.is_none() || app.globals.viewporter.is_none() {
        app.report.fail(
            "frac-scale",
            "wp_fractional_scale_v1 or wp_viewporter absent — a fractional scale cannot be \
             expressed, and the fallback is an integer scale with a resampled surface",
        );
        return;
    }
    let got = app.wait_for(queue, Duration::from_millis(500), |a| a.scale_events > 0);
    if !got {
        app.report.fail("frac-scale", "no preferred_scale event within 500 ms");
        return;
    }
    let expect_w = (LOGICAL.0 * app.scale_120).div_ceil(120);
    let expect_h = (LOGICAL.1 * app.scale_120).div_ceil(120);
    let ok = app.buffer_px == (expect_w, expect_h);
    let evidence = format!(
        "preferred_scale {}/120 = {:.4}x; buffer {}x{} px for a {}x{} logical window; \
         viewport destination {}x{}",
        app.scale_120,
        app.scale_120 as f64 / 120.0,
        app.buffer_px.0,
        app.buffer_px.1,
        LOGICAL.0,
        LOGICAL.1,
        LOGICAL.0,
        LOGICAL.1
    );
    app.report.record(
        "frac-scale",
        if ok { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
}

/// The compositor's own screenshot is the only witness that the buffer was not
/// resampled. Spike A's assertion said 1:1 while the screen said otherwise, so
/// this check does not ask our own bookkeeping anything: it asks the framebuffer
/// whether a one-physical-pixel checkerboard is still a checkerboard.
///
/// It locates itself. Scanning the whole output for the longest run of strict
/// alternation between our two known colours avoids having to convert a logical
/// window position into a physical one — which is arithmetic that could be wrong
/// in the same direction as the bug.
pub fn checkerboard(app: &mut App) {
    let Some(cap) = capture_output() else {
        app.report.owed("checker-1:1", "grim produced no capture");
        return;
    };
    let run = longest_alternating_run(&cap);
    let expect = paint::CHECKER_SIZE.0 as usize;
    let geom = window_geometry()
        .map(|(x, y, w, h)| format!("window {w}x{h} logical at {x},{y}"))
        .unwrap_or_else(|| "window geometry unavailable".into());
    let evidence = format!(
        "{geom}; captured {}x{} px; longest run of 1-px alternation between {:#04x} and {:#04x} \
         is {run} px, {expect} expected",
        cap.width, cap.height, paint::CHECKER_DARK & 0xff, paint::CHECKER_LIGHT & 0xff
    );
    // A resample turns the alternation into a gradient, and the longest surviving
    // run collapses. Allowing one pixel of slack at each edge is the only
    // tolerance here.
    app.report.record(
        "checker-1:1",
        if run + 2 >= expect { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
}

pub fn keyboard(app: &mut App, queue: &mut EventQueue<App>) {
    app.wait_for(queue, Duration::from_millis(500), |a| a.seat_state.keymap.is_some());
    let s = &app.seat_state;
    if s.keymap.is_none() {
        app.report.fail("keyboard", "no xkb keymap arrived within 500 ms");
        return;
    }
    let evidence = format!(
        "keymap {} bytes compiled in {} us, layout \"{}\"; focus {}",
        s.keymap_bytes,
        s.keymap_compile_us,
        s.layout_name,
        if s.keyboard_focus { "held" } else { "not held" }
    );
    app.report.pass("keyboard", evidence);
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// A selection we own, read by a process that is not us. `wl-paste` is a real
/// third-party client, which is the point — a self-read would prove only that our
/// own bookkeeping is consistent.
pub fn clipboard_out(app: &mut App, queue: &mut EventQueue<App>, qh: &QueueHandle<App>, fx: &Fixture) {
    let Some(ddm) = app.globals.ddm.clone() else {
        app.report.fail("clipboard-out", "no wl_data_device_manager");
        return;
    };
    let Some(serial) = app.seat_state.keyboard_enter_serial else {
        app.report
            .fail("clipboard-out", "no input serial — the window never held keyboard focus");
        return;
    };
    app.data.serve(&fx.uri_refs());
    app.data.outgoing = Default::default();
    let source = ddm.create_data_source(qh, ());
    for mime in [URI_LIST, TEXT, GNOME_COPIED] {
        source.offer(mime.into());
    }
    app.data.device.as_ref().unwrap().set_selection(Some(&source), serial);
    queue.flush().ok();

    for (label, args) in [
        ("text/uri-list", vec!["--no-newline", "--type", URI_LIST]),
        ("text/plain", vec!["--no-newline", "--type", TEXT]),
        ("mime list", vec!["--list-types"]),
    ] {
        match paste(app, queue, &args, false) {
            Some(text) if !text.is_empty() => {
                app.report.pass("clipboard-out", format!("wl-paste read {label}: {}", one_line(&text)))
            }
            _ => app.report.fail("clipboard-out", format!("wl-paste read nothing for {label}")),
        }
    }
    if app.data.outgoing.short_writes > 0 {
        app.report.fail(
            "clipboard-out",
            format!("{} short writes — a real implementation needs a write queue", app.data.outgoing.short_writes),
        );
    }
}

pub fn clipboard_in(app: &mut App, queue: &mut EventQueue<App>, conn: &Connection, primary: bool) {
    let name = if primary { "primary-in" } else { "clipboard-in" };
    let marker = format!("spike-d-{}-{}", if primary { "primary" } else { "clipboard" }, std::process::id());
    let epoch = if primary { app.data.primary_events } else { app.data.selection_events };
    let mut cmd = Command::new("wl-copy");
    if primary {
        cmd.arg("--primary");
    }
    let ok = cmd.arg(&marker).status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        app.report.owed(name, "wl-copy unavailable");
        return;
    }
    // Waiting for `is_some()` would return instantly on the selection we set
    // ourselves two checks ago. The epoch is the difference between "a selection
    // exists" and "a new one arrived".
    let arrived = app.wait_for(queue, Duration::from_secs(3), move |a| {
        if primary {
            a.data.primary_events > epoch
        } else {
            a.data.selection_events > epoch
        }
    });
    if !arrived {
        app.report.fail(name, format!("no new selection event within 3 s (epoch {epoch})"));
        return;
    }
    let (mimes, mut t) = if primary {
        let o = app.data.primary_selection.clone().unwrap();
        (app.data.primary_mimes.clone(), Transfer::primary(conn, &o, TEXT))
    } else {
        let o = app.data.selection.clone().unwrap();
        (app.data.selection_mimes.clone(), Transfer::offer(conn, &o, TEXT))
    };
    let (done, worst_gap) = data::drain(app, queue, &mut t, Duration::from_secs(2));
    let text = t.text();
    let evidence = format!(
        "{} MIME types offered ({}); read {} bytes = {:?}; worst dispatch gap during the \
         transfer {:.3} ms",
        mimes.len(),
        one_line(&mimes.join(" ")),
        t.bytes.len(),
        one_line(&text),
        worst_gap
    );
    app.report.record(
        name,
        if done && text.trim() == marker { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
}

pub fn primary_out(
    app: &mut App,
    queue: &mut EventQueue<App>,
    qh: &QueueHandle<App>,
    fx: &Fixture,
    extent: (u32, u32),
) {
    let Some(psm) = app.globals.psm.clone() else {
        app.report.fail("primary-out", "no zwp_primary_selection_device_manager_v1");
        return;
    };
    // A *fresh* serial. wlroots validates the serial on
    // zwp_primary_selection_device_v1.set_selection against a ring of recent
    // ones and silently returns if it does not match, while its
    // wl_data_device.set_selection does not check at all — so a stale serial
    // produces a working clipboard and a primary selection that never exists,
    // with no error on either side. Synthesizing a click is how this check gets
    // one it can trust.
    let serial = match fresh_serial(app, queue, qh, extent) {
        Some(s) => s,
        None => match app.seat_state.keyboard_enter_serial {
            Some(s) => s,
            None => {
                app.report.fail("primary-out", "no input serial at all");
                return;
            }
        },
    };
    app.data.serve(&fx.uri_refs());
    app.data.outgoing = Default::default();
    let cancelled_before = app.data.source_cancelled;
    let source = psm.create_source(qh, ());
    for mime in [URI_LIST, TEXT] {
        source.offer(mime.into());
    }
    app.data.primary_device.as_ref().unwrap().set_selection(Some(&source), serial);
    queue.flush().ok();
    app.pump(queue, Duration::from_millis(150));
    let types = paste(app, queue, &["--primary", "--list-types"], true).unwrap_or_default();
    // A second instance of ourselves, using the protocol under test rather than a
    // data-control manager, so a third-party tool seeing nothing does not get to
    // be the last word on whether the selection exists.
    let peer = peer_read(app, queue, "primary");
    match paste(app, queue, &["--primary", "--no-newline", "--type", TEXT], true) {
        Some(text) if !text.is_empty() => app.report.pass(
            "primary-out",
            format!("wl-paste --primary read {}; our own reader saw {}", one_line(&text), one_line(&peer)),
        ),
        _ => {
            // Hyprland discards a `zwp_primary_selection_device_v1.set_selection`
            // while `misc:middle_click_paste` is off: no protocol error, no
            // `cancelled`, no offer. Proved by toggling the option with nothing
            // else changed. The bar admits a documented compositor limitation,
            // and this one has no client-side fallback at all — which is the part
            // that matters for the product.
            let gated = hypr_option("misc:middle_click_paste") == Some(0);
            let evidence = format!(
                "wl-paste --primary listed no types ({:?}); a second instance reading over \
                 zwp_primary_selection_device_v1 saw: {}; requests served {:?}; sources \
                 cancelled since set_selection {}; serial {serial}; \
                 misc:middle_click_paste = {:?}. Toggling that option to 1 and changing nothing \
                 else makes the same code readable by wl-paste, so the selection is being \
                 discarded by the compositor. There is no fallback: the request produces no \
                 error, so a primary-selection feature cannot even be greyed out",
                one_line(&types),
                one_line(&peer),
                app.data.outgoing.requested,
                app.data.source_cancelled - cancelled_before,
                hypr_option("misc:middle_click_paste"),
            );
            app.report.record(
                "primary-out",
                if gated { Outcome::Limitation } else { Outcome::Fail },
                evidence,
            );
        }
    }
}

/// Rule 1 for the clipboard. A peer that offers a type and then never writes must
/// cost us nothing but a pending read, and the number that proves it is the
/// dispatch gap while the read is outstanding.
///
/// The hung peer is a second copy of this binary rather than a mock, because the
/// failure this guards against is a real process on the other end of a real pipe.
pub fn slow_transfer(app: &mut App, queue: &mut EventQueue<App>, conn: &Connection) {
    let exe = std::env::current_exe().expect("current_exe");
    let epoch = app.data.selection_events;
    let child = Command::new(exe)
        .args(["hang-source", "20"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        app.report.owed("slow-transfer", "could not spawn the hung peer");
        return;
    };
    let arrived = app.wait_for(queue, Duration::from_secs(3), move |a| a.data.selection_events > epoch);
    if !arrived {
        app.report.owed("slow-transfer", "the hung peer never took the selection");
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    let offer = app.data.selection.clone().unwrap();
    let mut t = Transfer::offer(conn, &offer, TEXT);
    let mut gaps = Vec::new();
    let mut last = Instant::now();
    let end = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < end {
        queue.flush().ok();
        queue.dispatch_pending(app).ok();
        let now = Instant::now();
        gaps.push((now - last).as_secs_f64() * 1000.0);
        last = now;
        t.poll();
        std::thread::sleep(Duration::from_micros(200));
    }
    let s = Samples(gaps);
    let worst = s.0.iter().cloned().fold(0.0f64, f64::max);
    let evidence = format!(
        "peer offered {} and never wrote: {} bytes after 1200 ms, transfer still open ({}); \
         dispatch interval {}; worst {worst:.3} ms against rule 1's 8 ms",
        TEXT,
        t.bytes.len(),
        !t.done,
        s.describe(" ms"),
    );
    // Both halves matter. A loop that stayed responsive because the transfer
    // secretly completed has measured nothing — which is what the first version
    // of this check did.
    app.report.record(
        "slow-transfer",
        if worst < 8.0 && !t.done && t.bytes.is_empty() { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Drag and drop
// ---------------------------------------------------------------------------

/// A drag from us to a window at `target`, with the negotiated action recorded.
/// `expect_read` says whether the peer is one we know reads the data, which
/// separates "the handshake worked" from "the peer did something".
pub fn dnd_source(
    app: &mut App,
    queue: &mut EventQueue<App>,
    qh: &QueueHandle<App>,
    fx: &Fixture,
    label: &str,
    target: (u32, u32),
    extent: (u32, u32),
) {
    let Some(ddm) = app.globals.ddm.clone() else {
        app.report.fail("dnd-source", "no wl_data_device_manager");
        return;
    };
    let Some(pointer) = vpointer::Pointer::new(app, qh, extent) else {
        app.report.owed("dnd-source", "no virtual pointer to drive the drag");
        return;
    };
    app.data.serve(&fx.uri_refs());
    app.data.outgoing = Default::default();

    let source = ddm.create_data_source(qh, ());
    for mime in [URI_LIST, TEXT, GNOME_COPIED] {
        source.offer(mime.into());
    }
    source.set_actions(data::offered_actions());
    let (icon, icon_canvas) = super::seat::drag_icon(app, qh);

    // The grab has to come from a press inside our own surface, so the drag starts
    // over the checkerboard and travels to the peer.
    let Some(origin) = window_centre() else {
        app.report.owed("dnd-source", "hyprctl reported no window for this pid");
        return;
    };
    pointer.move_to(origin.0, origin.1);
    app.pump(queue, Duration::from_millis(120));
    pointer.press(vpointer::BTN_LEFT);
    app.pump(queue, Duration::from_millis(120));
    let Some(serial) = app.seat_state.last_button_serial else {
        app.report.fail("dnd-source", "no button serial after a synthetic press inside our surface");
        pointer.release(vpointer::BTN_LEFT);
        return;
    };
    data::start_drag(app, &source, Some(&icon), serial);
    queue.flush().ok();

    let steps = 24;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let x = origin.0 as f64 + (target.0 as f64 - origin.0 as f64) * t;
        let y = origin.1 as f64 + (target.1 as f64 - origin.1 as f64) * t;
        pointer.move_to(x.round() as u32, y.round() as u32);
        app.pump(queue, Duration::from_millis(12));
    }
    app.pump(queue, Duration::from_millis(400));
    let accepted_before_drop = app.data.outgoing.target_accepted.clone();
    let action_before_drop = app.data.outgoing.action;
    pointer.release(vpointer::BTN_LEFT);
    // A source has to keep dispatching after the drop: the target's request for
    // the bytes arrives *afterwards*, and a source that stops here leaves the
    // target reading a pipe nobody will write to. Measured — the first version of
    // this check pumped for 800 ms and the two-process pair read zero bytes.
    app.wait_for(queue, Duration::from_secs(4), |a| {
        a.data.outgoing.finished && !a.data.outgoing.requested.is_empty()
    });
    app.pump(queue, Duration::from_millis(200));

    let o = &app.data.outgoing;
    let evidence = format!(
        "target accepted {:?} and asked for {:?}; action {:?}; drop_performed {} finished {} \
         cancelled {}",
        accepted_before_drop,
        o.requested,
        action_before_drop.or(o.action).map(|a| format!("{a:?}")),
        o.dropped,
        o.finished,
        o.cancelled
    );
    // The observable outcome of a drag is that the peer asked for the bytes.
    // Whether the *source* is then told the drop finished is a separate question
    // with its own consequence, recorded below rather than folded in here.
    let outcome = if !o.requested.is_empty() {
        Outcome::Pass
    } else if o.cancelled {
        Outcome::Limitation
    } else {
        Outcome::Fail
    };
    let (dropped, finished) = (o.dropped, o.finished);
    let asked = !o.requested.is_empty();
    app.report.record(format!("dnd-source/{label}"), outcome, evidence);

    // A move is "copy, then delete the originals once the drop is confirmed".
    // The confirmation is `dnd_finished`, and a source that never receives it can
    // never safely delete anything — so this is not bookkeeping, it is the
    // difference between move being implementable and not.
    if asked {
        let e = format!(
            "the peer read the data; dnd_drop_performed {dropped}, dnd_finished {finished}. \
             Without dnd_finished a move cannot delete the originals, so the fallback is to \
             treat every unconfirmed drop as a copy"
        );
        app.report.record(
            format!("dnd-finish/{label}"),
            if finished { Outcome::Pass } else { Outcome::Limitation },
            e,
        );
    }

    icon_canvas.destroy();
    icon.destroy();
    source.destroy();
    pointer.destroy();
}

/// Whether the compositor ever reports a *negotiated* action. The roadmap's bar
/// names copy, move and link; a target that is never told which one was chosen
/// cannot implement any of them differently.
pub fn dnd_action_report(app: &mut App, requested_single: bool) {
    let o = &app.data.outgoing;
    let evidence = format!(
        "source offered {}; the compositor reported source_actions to the target but sent \
         wl_data_source.action {} and wl_data_offer.action {} to us; target accepted {:?}",
        if requested_single { "copy alone" } else { "copy|move|ask" },
        o.action.map(|a| format!("{a:?}")).unwrap_or_else(|| "never".into()),
        app.data.incoming.action.map(|a| format!("{a:?}")).unwrap_or_else(|| "never".into()),
        o.target_accepted,
    );
    let outcome = if o.action.is_some() { Outcome::Pass } else { Outcome::Limitation };
    app.report.record("dnd-actions", outcome, evidence);
}

/// Sit still and accept whatever is dragged in. Used by the peer-to-us direction
/// and by the two-instance control.
pub fn dnd_target(
    app: &mut App,
    queue: &mut EventQueue<App>,
    conn: &Connection,
    label: &str,
    wait: Duration,
) {
    app.data.accept_order = vec![URI_LIST.into(), GNOME_COPIED.into(), TEXT.into()];
    let entered = app.wait_for(queue, wait, |a| a.data.incoming.entered);
    if !entered {
        app.report.fail(format!("dnd-target/{label}"), format!("no drag entered within {wait:?}"));
        return;
    }
    let dropped = app.wait_for(queue, Duration::from_secs(20), |a| a.data.incoming.dropped);
    if !dropped {
        let mimes = app.data.incoming.mimes.clone();
        app.report.record(
            format!("dnd-target/{label}"),
            Outcome::Limitation,
            format!(
                "entered with {} types ({}) but never dropped",
                mimes.len(),
                one_line(&mimes.join(" "))
            ),
        );
        return;
    }
    collect_drop(app, queue, conn, label);
}

/// The half that reads the data, for a drag that has already been released. Kept
/// separate because the caller that *drives* the drag cannot then wait for one to
/// arrive — by the time it looks, the `leave` has already cleared `entered`, and
/// waiting again reports "no drag entered" for a drag that worked.
pub fn collect_drop(app: &mut App, queue: &mut EventQueue<App>, conn: &Connection, label: &str) {
    let mimes = app.data.incoming.mimes.clone();
    let source_actions = app.data.incoming.source_actions;
    if !app.data.incoming.dropped {
        app.report.record(
            format!("dnd-target/{label}"),
            if mimes.is_empty() { Outcome::Fail } else { Outcome::Limitation },
            format!(
                "{} types were offered ({}) and no drop followed",
                mimes.len(),
                one_line(&mimes.join(" "))
            ),
        );
        return;
    }
    let offer = app.data.dnd_offer.clone();
    let action = app.data.incoming.action;
    let (bytes, text) = match &offer {
        Some(o) => {
            let mime = app
                .data
                .accept_order
                .iter()
                .find(|m| mimes.iter().any(|x| x == *m))
                .cloned()
                .unwrap_or_else(|| TEXT.into());
            let mut t = Transfer::offer(conn, o, &mime);
            let (_, _gap) = data::drain(app, queue, &mut t, Duration::from_secs(3));
            // finish() then destroy() is the order the protocol requires, and
            // skipping finish() is why some sources never report a move as done.
            o.finish();
            o.destroy();
            (t.bytes.len(), t.text())
        }
        None => (0, String::new()),
    };
    let evidence = format!(
        "{} types offered ({}); source_actions {:?}; negotiated {:?}; read {bytes} bytes = {}",
        mimes.len(),
        one_line(&mimes.join(" ")),
        source_actions.map(|a| format!("{a:?}")),
        action.map(|a| format!("{a:?}")),
        one_line(&text)
    );
    app.report.record(
        format!("dnd-target/{label}"),
        if bytes > 0 { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
}

// ---------------------------------------------------------------------------
// IME
// ---------------------------------------------------------------------------

pub fn text_input(app: &mut App, queue: &mut EventQueue<App>, qh: &QueueHandle<App>) {
    let Some(tim) = app.globals.tim.clone() else {
        app.report.fail("text-input-v3", "zwp_text_input_manager_v3 absent");
        return;
    };
    let seat = app.globals.seat.clone();
    if let Some(imm) = app.globals.imm.clone() {
        app.ime.bind_method(&imm, &seat, qh);
        app.pump(queue, Duration::from_millis(100));
        if app.ime.method_unavailable {
            app.report.owed(
                "text-input-v3",
                "another input method already holds the seat — this machine has none, so this \
                 is a message from a future run",
            );
            return;
        }
    } else {
        app.report.owed(
            "text-input-v3",
            "no zwp_input_method_manager_v2 and no input method installed: the client half \
             cannot be driven from this machine",
        );
        return;
    }

    app.ime.enable(&tim, &seat, qh);
    queue.flush().ok();
    let active = app.wait_for(queue, Duration::from_secs(2), |a| a.ime.method_active && a.ime.entered);
    if !active {
        app.report.fail(
            "text-input-v3",
            format!(
                "enable() did not activate the input method within 2 s (entered {}, active {})",
                app.ime.entered, app.ime.method_active
            ),
        );
        return;
    }

    let t0 = Instant::now();
    app.ime.compose();
    queue.flush().ok();
    let got_preedit = app.wait_for(queue, Duration::from_secs(2), |a| a.ime.preedit == ime::PREEDIT);
    let preedit_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // The commit below replaces the preedit, so what it was has to be captured
    // here or the evidence describes the wrong moment.
    let observed_preedit = app.ime.preedit.clone();
    let observed_cursor = app.ime.preedit_cursor;

    let t1 = Instant::now();
    app.ime.finish();
    queue.flush().ok();
    let got_commit = app.wait_for(queue, Duration::from_secs(2), |a| a.ime.committed == ime::COMMIT);
    let commit_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let evidence = format!(
        "preedit {:?} arrived in {preedit_ms:.3} ms with cursor {:?}; commit {:?} in \
         {commit_ms:.3} ms replaced it; the field holds {} chars and the preedit is now {:?}",
        observed_preedit,
        observed_cursor,
        app.ime.committed,
        app.ime.committed.chars().count(),
        app.ime.preedit,
    );
    app.report.record(
        "text-input-v3",
        if got_preedit && got_commit { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
}

// ---------------------------------------------------------------------------
// Helpers that shell out. Kept together so the shape of what is not measured
// in-process is visible in one place.
// ---------------------------------------------------------------------------

/// `wl-paste` asks us for the data, so our event loop has to be running while it
/// does. Spawning it and then pumping is the whole trick.
fn paste(app: &mut App, queue: &mut EventQueue<App>, args: &[&str], primary: bool) -> Option<String> {
    let _ = primary;
    let mut child = Command::new("wl-paste")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut status = None;
    while Instant::now() < deadline {
        queue.flush().ok();
        queue.dispatch_pending(app).ok();
        if let Ok(Some(s)) = child.try_wait() {
            status = Some(s);
            break;
        }
        if let Some(g) = queue.prepare_read() {
            g.read().ok();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if status.is_none() {
        let _ = child.kill();
    }
    let mut buf = String::new();
    out.read_to_string(&mut buf).ok();
    Some(buf)
}

/// Spawn a second copy of this binary as a reader and collect what it printed.
/// It takes keyboard focus, which is what makes the offer arrive; selection
/// *ownership* survives losing focus, so the owner is still us.
fn peer_read(app: &mut App, queue: &mut EventQueue<App>, which: &str) -> String {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return format!("no exe: {e}"),
    };
    let mut child = match Command::new(exe)
        .args(["read-selection", which])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("spawn failed: {e}"),
    };
    let mut out = child.stdout.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        queue.flush().ok();
        queue.dispatch_pending(app).ok();
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if let Some(g) = queue.prepare_read() {
            g.read().ok();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut buf = String::new();
    out.read_to_string(&mut buf).ok();
    buf.trim().replace('\n', " | ")
}

/// Click on our own window and return the serial the compositor issued for it.
/// Without a virtual pointer there is no way to manufacture one, and the check
/// falls back to whatever serial it has.
fn fresh_serial(
    app: &mut App,
    queue: &mut EventQueue<App>,
    qh: &QueueHandle<App>,
    extent: (u32, u32),
) -> Option<u32> {
    click_self(app, queue, qh, extent).then(|| app.seat_state.last_button_serial).flatten()
}

/// Click the middle of our own window with the virtual pointer. Returns whether
/// the button event came back to us, which is the difference between a serial we
/// can use and a click that landed on somebody else.
pub fn click_self(
    app: &mut App,
    queue: &mut EventQueue<App>,
    qh: &QueueHandle<App>,
    extent: (u32, u32),
) -> bool {
    let Some(pointer) = vpointer::Pointer::new(app, qh, extent) else { return false };
    let Some((x, y)) = window_centre() else { return false };
    let before = app.seat_state.last_button_serial;
    pointer.move_to(x, y);
    app.pump(queue, Duration::from_millis(120));
    pointer.press(vpointer::BTN_LEFT);
    app.pump(queue, Duration::from_millis(120));
    pointer.release(vpointer::BTN_LEFT);
    app.pump(queue, Duration::from_millis(120));
    pointer.destroy();
    app.seat_state.last_button_serial != before && app.seat_state.last_button_serial.is_some()
}

/// One integer out of `hyprctl getoption`. Used to tell a compositor policy
/// choice apart from a bug in our own protocol handling.
fn hypr_option(name: &str) -> Option<i64> {
    let out = Command::new("hyprctl").args(["getoption", name]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .find_map(|l| l.strip_prefix("int: "))
        .and_then(|v| v.trim().parse().ok())
}

/// The rectangle of somebody else's window, by pid. Reading a peer's position
/// *before* our own window exists gives a stale answer under a tiling
/// compositor: mapping our surface re-tiles the workspace and moves the peer.
/// Every drag therefore resolves its target after both windows are up.
pub fn window_geometry_of(pid: u32) -> Option<(i64, i64, i64, i64)> {
    let json = hyprctl_clients()?;
    for chunk in json.split("\"address\"") {
        if !chunk.contains(&format!("\"pid\": {pid}")) {
            continue;
        }
        let at = numbers_after(chunk, "\"at\": [")?;
        let size = numbers_after(chunk, "\"size\": [")?;
        return Some((at.0, at.1, size.0, size.1));
    }
    None
}

/// The same lookup keyed by the compositor's window address. Needed for a peer
/// that reuses an already-running process — `firefox --new-window` opens a window
/// belonging to a pid that was there before, so the pid identifies the browser
/// and not the window.
pub fn window_geometry_of_addr(addr: &str) -> Option<(i64, i64, i64, i64)> {
    let json = hyprctl_clients()?;
    for chunk in json.split("\"address\"") {
        if !chunk.trim_start().starts_with(&format!(": \"{addr}\"")) {
            continue;
        }
        let at = numbers_after(chunk, "\"at\": [")?;
        let size = numbers_after(chunk, "\"size\": [")?;
        return Some((at.0, at.1, size.0, size.1));
    }
    None
}

/// The middle of a peer's content area, pushed below a header bar or tab strip.
pub fn drop_point_of(pid: u32) -> Option<(u32, u32)> {
    content_middle(window_geometry_of(pid)?)
}

pub fn drop_point_of_addr(addr: &str) -> Option<(u32, u32)> {
    content_middle(window_geometry_of_addr(addr)?)
}

fn content_middle((x, y, w, h): (i64, i64, i64, i64)) -> Option<(u32, u32)> {
    Some(((x + w / 2).max(0) as u32, (y + h / 2 + h / 8).max(0) as u32))
}

fn hyprctl_clients() -> Option<String> {
    let out = Command::new("hyprctl").args(["clients", "-j"]).output().ok()?;
    String::from_utf8(out.stdout).ok()
}

/// The window rectangle in logical coordinates, found by pid so that a second
/// instance does not report the first one's geometry.
pub fn window_geometry() -> Option<(i64, i64, i64, i64)> {
    let json = hyprctl_clients()?;
    let pid = std::process::id() as i64;
    // Deliberately not a JSON dependency: one field lookup in a spike is not
    // worth a crate, and `hyprctl clients -j` prints one object per client with
    // `pid`, `at` and `size`.
    for chunk in json.split("\"address\"") {
        if !chunk.contains(&format!("\"pid\": {pid}")) {
            continue;
        }
        let at = numbers_after(chunk, "\"at\": [")?;
        let size = numbers_after(chunk, "\"size\": [")?;
        return Some((at.0, at.1, size.0, size.1));
    }
    None
}

pub fn window_centre() -> Option<(u32, u32)> {
    let (x, y, w, h) = window_geometry()?;
    Some(((x + w / 2).max(0) as u32, (y + h / 3).max(0) as u32))
}

fn numbers_after(s: &str, key: &str) -> Option<(i64, i64)> {
    let rest = &s[s.find(key)? + key.len()..];
    let end = rest.find(']')?;
    let mut it = rest[..end].split(',').map(|p| p.trim().parse::<i64>());
    Some((it.next()?.ok()?, it.next()?.ok()?))
}

pub struct Capture {
    pub width: usize,
    pub height: usize,
    /// One luminance byte per pixel. The checkerboard is grey on purpose.
    pub luma: Vec<u8>,
}

/// `grim -t ppm` writes the output at its native resolution with no scaling in
/// the path, which is the only reason this measurement can be trusted. A cropped
/// capture would go through grim's own scaler, and an instrument that resamples
/// cannot detect a resample.
fn capture_output() -> Option<Capture> {
    let out = Command::new("grim").args(["-t", "ppm", "-"]).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ppm(&out.stdout)
}

fn parse_ppm(raw: &[u8]) -> Option<Capture> {
    if !raw.starts_with(b"P6") {
        return None;
    }
    let mut fields = Vec::new();
    let mut i = 2;
    while fields.len() < 3 && i < raw.len() {
        while i < raw.len() && raw[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < raw.len() && !raw[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(std::str::from_utf8(&raw[start..i]).ok()?.parse::<usize>().ok()?);
    }
    i += 1;
    let (width, height) = (fields[0], fields[1]);
    let body = raw.get(i..)?;
    if body.len() < width * height * 3 {
        return None;
    }
    // Rec. 601 is unnecessary: the pattern is neutral grey, so the green channel
    // is the luminance.
    let luma = (0..width * height).map(|p| body[p * 3 + 1]).collect();
    Some(Capture { width, height, luma })
}

/// The longest horizontal run in which consecutive pixels alternate between the
/// two checkerboard values. Resampling destroys this and nothing else in a
/// desktop does.
fn longest_alternating_run(cap: &Capture) -> usize {
    let dark = (paint::CHECKER_DARK & 0xff) as u8;
    let light = (paint::CHECKER_LIGHT & 0xff) as u8;
    let is = |v: u8| v == dark || v == light;
    let mut best = 0usize;
    for y in 0..cap.height {
        let row = &cap.luma[y * cap.width..(y + 1) * cap.width];
        let mut run = 0usize;
        for x in 0..cap.width {
            let v = row[x];
            let ok = is(v) && (run == 0 || row[x - 1] != v);
            run = if ok { run + 1 } else { usize::from(is(v)) };
            best = best.max(run);
        }
    }
    best
}

fn one_line(s: &str) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() > 90 {
        format!("{}...", flat.chars().take(90).collect::<String>())
    } else {
        flat
    }
}
