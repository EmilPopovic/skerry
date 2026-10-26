// SPDX-License-Identifier: Apache-2.0
//! The image helper: PNG, JPEG, GIF and SVG, in a process of its own.
//!
//! **[ADR 0006](../../../docs/adr/0006-out-of-process-decoders.md).** A decoder is
//! the largest attack surface in a file manager and it runs on whatever landed in a
//! download directory, so it runs here — sandboxed, memory-capped, and holding
//! none of the window's state. A crash or a hang costs one preview.
//!
//! **[ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md).**
//! A long-lived filter: one length-prefixed request in, one length-prefixed reply
//! out, in order, until the pipe closes.
//!
//! It is also runnable by hand, which is how it is debugged and how the sandbox
//! report is read:
//!
//! ```text
//! skerry-thumbnailer --probe /path/to/picture.png
//! ```

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod cache;
mod decode;
mod exif;
mod fonts;
mod gif;
mod md5;

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use sk_preview::wire::{Ask, Refusal, Reply, Request};

fn main() {
    if std::env::args().any(|arg| arg == "--version") {
        println!("skerry-thumbnailer {} ({})", env!("CARGO_PKG_VERSION"), env!("SKERRY_COMMIT"));
        return;
    }
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(at) = arguments.iter().position(|it| it == "--write") {
        let (from, into) = (arguments.first().map(PathBuf::from), arguments.get(at + 1));
        match (from, into) {
            (Some(from), Some(into)) => {
                let into = Path::new(into);
                // **Confined, with the output's own directory as the one thing
                // writable.** The served process may write only the thumbnail
                // cache, and a diagnostic that could write only there is one
                // nobody can find the output of — but a decode outside the
                // sandbox would be the one path in this program where an
                // untrusted file is parsed unconfined, which is the whole of what
                // ADR 0006 is about.
                let out = into.parent().filter(|it| !it.as_os_str().is_empty());
                let writable: Vec<&Path> = out.into_iter().collect();
                let report = sk_preview::sandbox::confine_by_hand(
                    sk_preview::sandbox::Role::Thumbnails,
                    &[Path::new("/")],
                    &[],
                    &writable,
                );
                println!("skerry-thumbnailer: sandbox: {}", report.line());
                write_out(&from, into, cache::SIDE);
            }
            _ => println!("skerry-thumbnailer: usage: skerry-thumbnailer <file> --write <out.png>"),
        }
        return;
    }
    if let Some(at) = arguments.iter().position(|it| it == "--probe") {
        probe(arguments.get(at + 1).map(PathBuf::from).as_deref());
        return;
    }
    let cache = cache::folder();
    let report = confine(sk_preview::sandbox::confine, cache.as_deref());
    // Rule 7's shape: say what is in force, including what is not.
    let _ = writeln!(std::io::stderr(), "skerry-thumbnailer: {}", report.line());
    serve(cache.as_deref());
}

/// Locks the process down before a single byte of a request is read.
///
/// **Read where the requests will point and write only to the cache.** A
/// thumbnailer is asked about files anywhere, so "read" is the root — which sounds
/// like no restriction and is not: it is *read* only, so a decoder that is talked
/// into writing has nowhere to write but the thumbnail directory, and Landlock's
/// filesystem rights are the whole of what a decoder could reach.
fn confine(how: sk_preview::sandbox::Confine, cache: Option<&Path>) -> sk_preview::sandbox::Report {
    let writable: Vec<&Path> = cache.into_iter().collect();
    // The cache has to exist before it can be granted, because a rule is a
    // descriptor and there is no descriptor for a directory that is not there.
    for directory in &writable {
        cache::prepare(directory);
    }
    // **Nothing executable.** A thumbnailer decodes in-process and has no
    // reason to start a program, so it is granted no execute right anywhere —
    // which means a decoder talked into spawning something cannot.
    how(sk_preview::sandbox::Role::Thumbnails, &[Path::new("/")], &[], &writable)
}

/// Answers requests until the host closes the pipe.
fn serve(cache: Option<&Path>) {
    let mut input = BufReader::new(std::io::stdin());
    let mut output = BufWriter::new(std::io::stdout());
    while let Ok(request) = Request::read_from(&mut input) {
        let reply = answer(&request, cache);
        if reply.write_to(&mut output).is_err() {
            return;
        }
    }
}

/// One request, answered.
///
/// **The cache is consulted only for a thumbnail.** A preview is one image at a
/// time at whatever size the panel is, so caching it would be writing a file per
/// window size for a picture somebody is looking at once — see
/// [ADR 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md).
fn answer(request: &Request, cache: Option<&Path>) -> Reply {
    let path = PathBuf::from(std::ffi::OsString::from(unsafe {
        // SAFETY: the bytes came from `OsStr::as_encoded_bytes` in the host, which
        // is exactly the contract this function documents — the same process
        // family, the same encoding, no round trip through text.
        std::ffi::OsStr::from_encoded_bytes_unchecked(&request.path)
    }));
    let mtime = modified(&path);
    if request.ask == Ask::Thumbnail
        && let Some(cache) = cache
        && let Some((pixels, width, height)) = cache::read(cache, &path, mtime)
    {
        // **Zero pages, because an image is not paged.** ADR 0019's field is a
        // document's business and this helper never has one.
        return Reply {
            refusal: Refusal::None,
            size: (width, height),
            pages: 0,
            delay_ms: 0,
            pixels,
            // **No words, because a picture has none.** ADR 0024's field is a
            // document's, and a thumbnail is not selected out of.
            words: Vec::new(),
        };
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return Reply::refused(Refusal::Unreadable);
    };
    let within = match request.ask {
        // A thumbnail is the cache's size whatever was asked for, so what goes
        // into the shared cache is what the specification says goes into it.
        Ask::Thumbnail => (cache::SIDE, cache::SIDE),
        Ask::Preview => request.within,
    };
    // **A thumbnail is always the first frame.** A grid of animating cells would
    // be a window that never sleeps, and the cache holds one picture per file —
    // so the frame a request may name is a preview's business only.
    let frame = if request.ask == Ask::Thumbnail { 1 } else { request.page };
    match decode::decode(&bytes, within, frame) {
        Ok(decoded) => {
            let raster = decoded.raster;
            if request.ask == Ask::Thumbnail
                && let Some(cache) = cache
            {
                cache::write(cache, &path, mtime, &raster.pixels, raster.size);
            }
            Reply {
                refusal: Refusal::None,
                size: raster.size,
                pages: decoded.frames,
                delay_ms: decoded.delay_ms,
                pixels: raster.pixels,
                words: Vec::new(),
            }
        }
        Err(refusal) => Reply::refused(refusal),
    }
}

/// The file's modification time, or zero.
///
/// Zero rather than a refusal: a file whose time cannot be read is still a file
/// worth decoding, and a thumbnail keyed on zero is simply never a cache hit.
fn modified(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|when| when.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(0)
}

/// Decodes one file and says what happened, for a person at a terminal.
fn probe(path: Option<&Path>) {
    let report = sk_preview::sandbox::confine_by_hand(
        sk_preview::sandbox::Role::Thumbnails,
        &[Path::new("/")],
        &[],
        &[],
    );
    println!("skerry-thumbnailer: sandbox: {}", report.line());
    let Some(path) = path else {
        println!(
            "skerry-thumbnailer: skerry-thumbnailer: usage: skerry-thumbnailer --probe <file>"
        );
        return;
    };
    println!("skerry-thumbnailer: fonts: {}", fonts::shared().report);
    match std::fs::read(path) {
        Err(why) => println!("skerry-thumbnailer: unreadable: {why}"),
        Ok(bytes) => match decode::decode(&bytes, (cache::SIDE, cache::SIDE), 1) {
            Ok(decoded) => println!(
                "skerry-thumbnailer: decoded {}×{}, {} bytes, {} frames; cache at {:?}",
                decoded.raster.size.0,
                decoded.raster.size.1,
                decoded.raster.pixels.len(),
                decoded.frames,
                cache::folder().map(|folder| cache::place(&folder, path))
            ),
            Err(refusal) => println!("skerry-thumbnailer: refused: {refusal:?}"),
        },
    }
}

/// Writes what a decode produced, for a person who wants to look at it.
///
/// **Because four of M0's six defects were found by looking at a PNG.** A
/// thumbnail that is wrong in a way a size cannot express — an SVG whose text
/// vanished, a premultiplied fringe, a picture fitted to the wrong edge — is
/// invisible to every assertion this helper can make about its own answer.
fn write_out(path: &Path, into: &Path, side: u32) {
    let Ok(bytes) = std::fs::read(path) else {
        println!("skerry-thumbnailer: unreadable: {}", path.display());
        return;
    };
    match decode::decode(&bytes, (side, side), 1) {
        Err(refusal) => println!("skerry-thumbnailer: refused: {refusal:?}"),
        Ok(decoded) => {
            let (width, height) = decoded.raster.size;
            // **Replaced by unlinking and creating, never by `O_TRUNC`**, which
            // the filter denies below Landlock ABI 3 — ADR 0046. A diagnostic
            // that could not overwrite its own last output there is one nobody
            // could run twice.
            let _ = std::fs::remove_file(into);
            let created = std::fs::OpenOptions::new().write(true).create_new(true).open(into);
            let written = created.map_err(|error| error.to_string()).and_then(|file| {
                let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writing = encoder.write_header().map_err(|error| error.to_string())?;
                writing.write_image_data(&decoded.raster.pixels).map_err(|error| error.to_string())
            });
            let said = match written {
                Ok(()) => format!("wrote {width}×{height} to {}", into.display()),
                Err(why) => format!("could not write {}: {why}", into.display()),
            };
            println!("skerry-thumbnailer: {said}");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use sk_preview::sandbox::{Confine, confine_below};
    use sk_preview::wire::{Ask, Refusal, Request};
    use std::path::{Path, PathBuf};

    /// **A thumbnail is cached where Landlock cannot restrict truncation, and
    /// where there is no Landlock and the setting allows decoding** — ADR 0046.
    /// The filter takes truncation over below ABI 3, so a cache writer that asked
    /// for `O_TRUNC` cached nothing on kernels 5.13 to 6.1. This is served mode
    /// short of the pipe: the confinement `main` applies, at the ABI asked for,
    /// then [`super::answer`] on a thumbnail request and the cache read back.
    #[test]
    fn a_thumbnail_is_cached_below_landlock_abi_3_and_without_landlock() {
        const MARKER: &str = "SKERRY_THUMBNAIL_CEILING";
        const NAME: &str = "tests::a_thumbnail_is_cached_below_landlock_abi_3_and_without_landlock";
        const ALLOW: &str = sk_preview::sandbox::WITHOUT_LANDLOCK;
        if let Some(picture) = std::env::var_os(MARKER) {
            let picture = PathBuf::from(picture);
            let below: Confine = if std::env::var_os(ALLOW).is_some() {
                |role, readable, run, write| confine_below(0, role, readable, run, write)
            } else {
                |role, readable, run, write| confine_below(2, role, readable, run, write)
            };
            let cache = super::cache::folder();
            let report = super::confine(below, cache.as_deref());
            assert!(report.landlock.is_none_or(|abi| abi == 2), "{}", report.line());
            let mtime = super::modified(&picture);
            let request = Request {
                ask: Ask::Thumbnail,
                within: (128, 128),
                page: 0,
                path: picture.as_os_str().as_encoded_bytes().to_vec(),
            };
            assert_eq!(super::answer(&request, cache.as_deref()).refusal, Refusal::None);
            let cache = cache.expect("the parent named a cache folder");
            assert!(super::cache::read(&cache, &picture, mtime).is_some(), "nothing was cached");
            return;
        }
        let root = std::env::temp_dir().join(format!("skerry-ceiling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let picture = root.join("picture.png");
        picture_at(&picture);
        let mut cases = vec![("none", true)];
        if sk_preview::sandbox::landlock_abi().is_some_and(|abi| abi >= 2) {
            cases.push(("abi2", false));
        } else {
            eprintln!("skerry: skip: no Landlock ABI 2 here to cap at");
        }
        for (label, allow) in cases {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", NAME, "--nocapture"])
                .env(MARKER, &picture)
                .env("XDG_CACHE_HOME", root.join(label))
                .env_remove(ALLOW);
            if allow {
                command.env(ALLOW, "allow");
            }
            let output = command.output().unwrap();
            let said = String::from_utf8_lossy(&output.stdout).into_owned()
                + &String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && said.contains(" 1 passed"), "{label}: {said}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An eight-pixel PNG, written unconfined by the test runner.
    fn picture_at(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 8, 8);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&[200u8; 8 * 8 * 4]).unwrap();
    }
}
