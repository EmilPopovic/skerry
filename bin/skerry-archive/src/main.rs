// SPDX-License-Identifier: Apache-2.0
//! `skerry-archive`: what is inside an archive, and unpacking one into a
//! directory it is the only thing allowed to write.
//!
//! **[ADR 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md).**
//! One request on `stdin`, one answer on `stdout`, and then this process is over
//! — unlike the two preview helpers, which stay alive because a grid of
//! thumbnails is dozens of requests. An archive is one `Space` press or one
//! extraction, so the process is spawned for the destination it is about and its
//! confinement is chosen for that destination.
//!
//! # Where the decoding is
//!
//! The containers are here: [`zip`] reads an index and [`tar`] reads headers,
//! which between them is 550 lines of arithmetic and no dependency. The
//! compressors are the machine's — `xz`, `zstd` and `bzip2` as `-dc` filters —
//! except gzip, which is `flate2`, already in this workspace's helper trees
//! through `png`.

mod detect;
mod place;
mod tar;
mod zip;

use std::io::Write;
use std::path::Path;

use sk_preview::archive::{
    Ask, Entry, Kind, Listing, MOST_ENTRIES, MOST_TABLE, Progress, Record, Refusal, Request,
    Unpacked,
};
use sk_preview::sandbox::{self, Limits};

use detect::Opened;
use place::Place;

/// How often an unpack says where it has got to.
///
/// Every sixteen entries or four megabytes, whichever comes first. The host
/// throttles again to ten a second before anything reaches a frame; this is the
/// cheaper end of the same decision, and its job is to keep a 32-byte record off
/// the pipe once per empty file in an archive of a million of them.
const REPORT_EVERY: (u64, u64) = (16, 4 * 1024 * 1024);

/// How much of the wire's table ceiling a listing will fill.
///
/// Seven eighths of it, so the reply is inside [`MOST_TABLE`] with room for the
/// header and the note rather than exactly at a limit the host would refuse.
const ROOM: usize = MOST_TABLE - MOST_TABLE / 8;

/// The most a symbolic link's target may be, in bytes.
///
/// Four kilobytes, which is `PATH_MAX`. An entry that says its link target is
/// larger than any path Linux can hold is an entry making a claim about how much
/// this should allocate, and the answer is to skip it.
const MOST_LINK: u64 = 4096;

fn main() {
    if std::env::args().any(|it| it == "--probe") {
        probe();
        return;
    }
    let mut input = std::io::stdin().lock();
    let Ok(Some(request)) = Request::read_from(&mut input) else {
        eprintln!("skerry-archive: not a request this build speaks");
        return;
    };
    // **Before a byte of the archive is opened**, which is ADR 0006's rule, and
    // the reason the read and execute sets are the root rather than the one file:
    // a filter is a dynamically linked program, so it needs its loader and every
    // library it maps, and which filter — or whether one is needed at all — is a
    // question only the archive's first bytes can answer. What the confinement is
    // *for* is the other set: an unpack may write inside its destination and
    // nowhere else, and a listing may write nothing anywhere.
    let writable: Vec<&Path> =
        if request.ask == Ask::Unpack { vec![&request.into] } else { Vec::new() };
    let limits = if request.ask == Ask::Unpack { Limits::UNPACK } else { Limits::PREVIEW };
    let root = [Path::new("/")];
    let report = sandbox::confine(limits, &root, &root, &writable);
    eprintln!("skerry-archive: {}", report.line());

    let mut out = std::io::stdout().lock();
    let wrote = match request.ask {
        Ask::List => list(&request.archive).write_to(&mut out),
        Ask::Unpack => {
            let unpacked = unpack(&request, &mut out);
            Record::Done(Box::new(unpacked)).write_to(&mut out)
        }
    };
    if wrote.is_err() {
        // The host closed the pipe: a panel that was closed, or a cancelled
        // unpack. Not a failure and not worth a line in anybody's log.
        std::process::exit(0);
    }
}

/// What `--probe` prints: whether the filters this machine has cover the formats.
fn probe() {
    let root = [Path::new("/")];
    let report = sandbox::confine(Limits::PREVIEW, &root, &root, &[]);
    println!("skerry-archive: {}", report.line());
    for filter in [
        sk_preview::archive::Filter::Xz,
        sk_preview::archive::Filter::Zstd,
        sk_preview::archive::Filter::Bzip2,
    ] {
        let Some(program) = filter.program() else { continue };
        // **Through pipes, not `/dev/null`.** The first version of this line
        // redirected to `/dev/null` and reported every filter missing on a
        // machine that has all three: opening it needs *write* access, and the
        // confinement this probe applies on purpose grants none. A probe that
        // lies about the thing it exists to check is worse than no probe.
        let found = std::process::Command::new(program).arg("--version").output().is_ok();
        println!("skerry-archive: {program}: {}", if found { "yes" } else { "MISSING" });
    }
    for path in std::env::args().skip(1).filter(|it| it != "--probe") {
        let listing = list(Path::new(&path));
        println!(
            "skerry-archive: {path}: {:?} {:?} {} entries, {} bytes, {} listed",
            listing.format.map(sk_preview::archive::Format::name),
            listing.refusal,
            listing.count,
            listing.bytes,
            listing.entries.len(),
        );
    }
}

/// What is inside `archive`.
fn list(archive: &Path) -> Listing {
    let opened = match detect::open(archive) {
        Ok(opened) => opened,
        Err(refusal) => return Listing::refused(refusal, String::new()),
    };
    match opened {
        Opened::Zip(file) => match zip::Zip::read(file) {
            Ok(zip) => {
                let all: Vec<Entry> = zip
                    .entries
                    .into_iter()
                    .map(|central| central.entry)
                    .filter(|entry| !entry.path.is_empty())
                    .collect();
                let bytes = all.iter().map(|entry| entry.size).sum();
                let count = all.len() as u64;
                // **Counted whole and sent bounded**, which is the same bargain
                // the tar walk makes: what a panel draws is forty rows, and what
                // a person is told is how many there really are.
                let mut table = 0usize;
                let entries = all
                    .into_iter()
                    .take(MOST_ENTRIES)
                    .take_while(|entry| {
                        table += 21 + entry.path.len();
                        table < ROOM
                    })
                    .collect();
                Listing {
                    format: Some(sk_preview::archive::Format {
                        container: sk_preview::archive::Container::Zip,
                        filter: sk_preview::archive::Filter::Store,
                    }),
                    count,
                    bytes,
                    entries,
                    refusal: Refusal::None,
                    note: String::new(),
                }
            }
            Err(refusal) => Listing::refused(refusal, String::new()),
        },
        Opened::Tar { format, bytes } => walk_tar(format, bytes),
        Opened::Single { format, name, bytes } => single(archive, format, name, bytes),
        Opened::Elsewhere(format) => Listing::refused(Refusal::Elsewhere, format.to_owned()),
        Opened::Nothing => Listing::refused(Refusal::NotAnArchive, String::new()),
    }
}

/// Reads a tar to the end, keeping the first [`MOST_ENTRIES`] of it.
///
/// **Counted past what is kept** — ADR 0032. The walk costs the same either way
/// because a tar has to be read to its end to be known at all, so the count and
/// the total are the real ones and only the table is bounded.
fn walk_tar(format: sk_preview::archive::Format, bytes: detect::Bytes) -> Listing {
    let mut tar = tar::Tar::over(bytes);
    let mut table = 0usize;
    let mut listing = Listing {
        format: Some(format),
        entries: Vec::new(),
        count: 0,
        bytes: 0,
        refusal: Refusal::None,
        note: String::new(),
    };
    loop {
        match tar.next() {
            // The archive's own root — `./` — which is not an entry and must not
            // be counted as one: a person sees two rows and a footer saying three.
            Ok(Some(item)) if item.path.is_empty() => {}
            Ok(Some(item)) => {
                listing.count += 1;
                listing.bytes += item.size;
                if listing.entries.len() < MOST_ENTRIES && table < ROOM {
                    // Twenty-one bytes of record and the path, which is what
                    // `Listing::write_to` puts on the wire for one entry.
                    table += 21 + item.path.len();
                    listing.entries.push(Entry {
                        path: item.path,
                        size: item.size,
                        mtime: item.mtime,
                        kind: item.kind,
                    });
                }
            }
            Ok(None) => return listing,
            // **What was read is still shown.** A tar that was cut off half way
            // through downloading still says what its first thousand entries
            // were, and the refusal beside them says the rest is missing.
            Err(refusal) => {
                listing.refusal = refusal;
                return listing;
            }
        }
    }
}

/// A single compressed file, whose one entry's size is what it decodes to.
fn single(
    archive: &Path,
    format: sk_preview::archive::Format,
    name: String,
    mut bytes: detect::Bytes,
) -> Listing {
    use std::io::Read;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        match bytes.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => total += read as u64,
            Err(_) => return Listing::refused(Refusal::Undecodable, String::new()),
        }
    }
    let mtime = std::fs::metadata(archive)
        .and_then(|facts| facts.modified())
        .ok()
        .and_then(|when| when.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs().cast_signed());
    Listing {
        format: Some(format),
        entries: vec![Entry { path: name, size: total, mtime, kind: Kind::File }],
        count: 1,
        bytes: total,
        refusal: Refusal::None,
        note: String::new(),
    }
}

/// Unpacks what the request names, reporting on `out` as it goes.
fn unpack(request: &Request, out: &mut impl Write) -> Unpacked {
    let refused =
        |refusal: Refusal, note: String| Unpacked { refusal, note, progress: Progress::default() };
    let opened = match detect::open(&request.archive) {
        Ok(opened) => opened,
        Err(refusal) => return refused(refusal, String::new()),
    };
    let mut place = match Place::under(&request.into) {
        Ok(place) => place,
        Err(refusal) => return refused(refusal, String::new()),
    };
    let mut said = Progress::default();
    // The child's half of the throttle: a record when either figure has moved
    // enough to be worth a wake-up in the window.
    let mut tell = |place: &Place, out: &mut dyn Write| {
        let (entries, bytes) = (place.written.entries, place.written.bytes);
        if entries < said.entries + REPORT_EVERY.0 && bytes < said.bytes + REPORT_EVERY.1 {
            return Ok(());
        }
        said = place.written;
        Record::Went(place.written).write_to(out)
    };
    let outcome = match opened {
        Opened::Zip(file) => unpack_zip(file, request, &mut place, out, &mut tell),
        Opened::Tar { bytes, .. } => unpack_tar(bytes, request, &mut place, out, &mut tell),
        Opened::Single { name, bytes, .. } => unpack_single(&name, bytes, &mut place),
        Opened::Elsewhere(format) => Err((Refusal::Elsewhere, format.to_owned())),
        Opened::Nothing => Err((Refusal::NotAnArchive, String::new())),
    };
    match outcome {
        Ok(()) if place.skipped > 0 => Unpacked {
            refusal: Refusal::Partial,
            note: format!("{} entries skipped", place.skipped),
            progress: place.written,
        },
        Ok(()) => Unpacked { refusal: Refusal::None, note: String::new(), progress: place.written },
        Err((refusal, note)) => Unpacked { refusal, note, progress: place.written },
    }
}

/// What an unpack failed at: the reason, and the entry that caused it.
type Failed = (Refusal, String);

/// Whether this entry is one the request asked for.
///
/// **A directory names its contents.** Selecting a folder in the panel and
/// pressing Extract means the folder, which is every path under it — so the
/// match is the name itself or the name and a separator.
fn wanted(path: &str, only: &[String]) -> bool {
    only.is_empty() || only.iter().any(|want| path == want || path.starts_with(&format!("{want}/")))
}

fn unpack_zip(
    file: std::fs::File,
    request: &Request,
    place: &mut Place,
    out: &mut impl Write,
    tell: &mut dyn FnMut(&Place, &mut dyn Write) -> std::io::Result<()>,
) -> Result<(), Failed> {
    let mut zip = zip::Zip::read(file).map_err(|refusal| (refusal, String::new()))?;
    for which in 0..zip.entries.len() {
        let (path, kind, mtime, mode, size) = {
            let central = &zip.entries[which];
            (
                central.entry.path.clone(),
                central.entry.kind,
                central.entry.mtime,
                central.mode,
                central.uncompressed,
            )
        };
        if path.is_empty() || !wanted(&path, &request.only) {
            continue;
        }
        match kind {
            Kind::Directory => place.directory(&path).map_err(|it| (it, path.clone()))?,
            // **A zip keeps a symlink's target in the entry's own data**, which
            // is why this is not the one line the tar path is: the target has to
            // be decompressed before there is anything to point at. Bounded,
            // because a `size` off a central directory is a claim and a path is
            // at most `PATH_MAX`.
            Kind::Link if size <= MOST_LINK => {
                let mut target = Vec::new();
                zip.extract(which, &mut target).map_err(|it| (it, path.clone()))?;
                let target = String::from_utf8_lossy(&target).into_owned();
                place.link(&path, &target).map_err(|it| (it, path.clone()))?;
            }
            Kind::Link | Kind::Other => place.refused(),
            Kind::File => {
                let Some(file) =
                    place.create(&path, mode & 0o111 != 0).map_err(|it| (it, path.clone()))?
                else {
                    continue;
                };
                let mut output = place.writer(file);
                let written = zip.extract(which, &mut output).map_err(|it| (it, path.clone()))?;
                drop(output);
                place.wrote(&path, mtime, written);
            }
        }
        let _ = tell(place, out);
    }
    Ok(())
}

fn unpack_tar(
    bytes: detect::Bytes,
    request: &Request,
    place: &mut Place,
    out: &mut impl Write,
    tell: &mut dyn FnMut(&Place, &mut dyn Write) -> std::io::Result<()>,
) -> Result<(), Failed> {
    let mut tar = tar::Tar::over(bytes);
    loop {
        let item = match tar.next() {
            Ok(Some(item)) => item,
            Ok(None) => return Ok(()),
            Err(refusal) => return Err((refusal, String::new())),
        };
        if item.path.is_empty() || !wanted(&item.path, &request.only) {
            continue;
        }
        let named = |refusal| (refusal, item.path.clone());
        match item.kind {
            Kind::Directory => place.directory(&item.path).map_err(named)?,
            Kind::Link => place.link(&item.path, &item.link).map_err(named)?,
            Kind::Other if item.hard => {
                place.hardlink(&item.path, &item.link).map_err(named)?;
            }
            // A device node, a fifo or a socket. Counted, never created.
            Kind::Other => place.refused(),
            Kind::File => match place.create(&item.path, item.mode & 0o111 != 0).map_err(named)? {
                Some(file) => {
                    let mut output = place.writer(file);
                    let written = tar
                        .data(&mut |chunk| output.write_all(chunk).map_err(|_| Refusal::NoRoom))
                        .map_err(named)?;
                    drop(output);
                    place.wrote(&item.path, item.mtime, written);
                }
                None => continue,
            },
        }
        let _ = tell(place, out);
    }
}

fn unpack_single(name: &str, mut bytes: detect::Bytes, place: &mut Place) -> Result<(), Failed> {
    let named = |refusal| (refusal, name.to_owned());
    let Some(file) = place.create(name, false).map_err(named)? else {
        return Ok(());
    };
    let mut file = place.writer(file);
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        use std::io::Read;
        let read = bytes.read(&mut buffer).map_err(|_| (Refusal::Undecodable, name.to_owned()))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|_| (Refusal::NoRoom, name.to_owned()))?;
        total += read as u64;
    }
    drop(file);
    place.wrote(name, 0, total);
    Ok(())
}
