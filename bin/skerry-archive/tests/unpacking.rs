// SPDX-License-Identifier: Apache-2.0
//! The helper, driven the way the window drives it.
//!
//! **Against the binary and not the functions inside it**, because half of what
//! is being tested is the sandbox — and a sandbox applied by a library call in a
//! test process would confine the test runner. `CARGO_BIN_EXE_skerry-archive` is
//! the binary this build produced, so the protocol, the confinement and the
//! decoders are all the ones that ship.
//!
//! **The archives are somebody else's**, for the reason the GIF fixture in
//! `skerry-thumbnailer` is Pillow's: an archive written by the same code that
//! reads it proves the two agree, not that either is right. `zip` and `tar` write
//! these. The two that climb out of their destination are ours, because no
//! archiver will write one on purpose.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sk_preview::archive::{
    Ask, Container, Entry, Format, Kind, Listing, Record, Refusal, Request, Unpacked,
};

/// A directory of this test's own, named so a failure leaves it to look at.
fn scratch(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("skerry-archive-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Whether a program is on this machine, so a missing archiver skips rather than
/// fails: the same rule `tools/gui-test` applies to `grim`.
fn have(program: &str) -> bool {
    Command::new(program).arg("--help").output().is_ok()
}

/// The tree every fixture is made from.
fn tree(at: &Path) -> PathBuf {
    let tree = at.join("tree");
    std::fs::create_dir_all(tree.join("deep/inner")).unwrap();
    std::fs::write(tree.join("hello.txt"), b"hello archive\n").unwrap();
    std::fs::write(tree.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(tree.join("run.sh"), perms(0o755)).unwrap();
    // **Incompressible, and deliberately.** A blob of one repeated byte deflates
    // to nothing, so the archive is mostly headers — and the corruption test
    // below, which flips a byte in the middle of the file, was flipping a
    // filename and watching the unpack succeed.
    std::fs::write(tree.join("deep/blob.bin"), noise(200_000)).unwrap();
    std::fs::write(tree.join("deep/inner/note.md"), b"nested\n").unwrap();
    std::os::unix::fs::symlink("hello.txt", tree.join("link.txt")).unwrap();
    tree
}

/// Bytes no compressor can do anything with, from a sequence that is the same
/// every run: a fixture has to be reproducible to be worth failing on.
fn noise(length: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// A file's mode, which is what says whether the execute bit survived.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode()
}

fn perms(mode: u32) -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(mode)
}

/// Asks the helper what is inside `archive`.
fn listing_of(archive: &Path) -> Listing {
    let mut child = Command::new(env!("CARGO_BIN_EXE_skerry-archive"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let request = Request {
        ask: Ask::List,
        archive: archive.to_path_buf(),
        into: PathBuf::new(),
        only: Vec::new(),
    };
    let mut input = child.stdin.take().unwrap();
    request.write_to(&mut input).unwrap();
    drop(input);
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let listing = Listing::read_from(&mut output).unwrap();
    let _ = child.wait();
    listing
}

/// Unpacks `archive` into `into`, and says how many progress records came first.
fn unpacked(archive: &Path, into: &Path, only: &[String]) -> (Unpacked, usize) {
    std::fs::create_dir_all(into).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_skerry-archive"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let request = Request {
        ask: Ask::Unpack,
        archive: archive.to_path_buf(),
        into: into.to_path_buf(),
        only: only.to_vec(),
    };
    let mut input = child.stdin.take().unwrap();
    request.write_to(&mut input).unwrap();
    drop(input);
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut records = 0;
    loop {
        match Record::read_from(&mut output).unwrap() {
            Some(Record::Went(_)) => records += 1,
            Some(Record::Done(done)) => {
                let _ = child.wait();
                return (*done, records);
            }
            None => panic!("the helper stopped without saying it had finished"),
        }
    }
}

/// The files a listing holds, as name and size, so two formats can be compared.
fn files(listing: &Listing) -> Vec<(String, u64)> {
    let mut found: Vec<(String, u64)> = listing
        .entries
        .iter()
        .filter(|entry| entry.kind == Kind::File)
        .map(|entry| (entry.path.clone(), entry.size))
        .collect();
    found.sort();
    found
}

/// One tar header block, checksummed the way the format defines.
///
/// **Ours, because no archiver writes the fixtures below.** `tar` refuses to
/// store a member called `../escape` and strips a leading separator, which is
/// exactly why an extractor cannot be tested with archives `tar` produced.
fn header(name: &str, size: u64, flag: u8, link: &str) -> [u8; 512] {
    let mut block = [0u8; 512];
    block[..name.len()].copy_from_slice(name.as_bytes());
    block[100..107].copy_from_slice(format!("{:07o}", 0o644).as_bytes());
    block[108..115].copy_from_slice(b"0000000");
    block[116..123].copy_from_slice(b"0000000");
    block[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    block[136..147].copy_from_slice(format!("{:011o}", 0).as_bytes());
    block[156] = flag;
    block[157..157 + link.len()].copy_from_slice(link.as_bytes());
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    // The checksum counts its own field as spaces, which is the rule that makes
    // it computable at all.
    block[148..156].copy_from_slice(b"        ");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    let text = format!("{sum:06o}\0 ");
    block[148..156].copy_from_slice(text.as_bytes());
    block
}

/// A tar of the entries given, as `(name, flag, link, body)`.
fn tar_of(path: &Path, entries: &[(&str, u8, &str, &[u8])]) {
    let mut bytes = Vec::new();
    for (name, flag, link, body) in entries {
        bytes.extend_from_slice(&header(name, body.len() as u64, *flag, link));
        bytes.extend_from_slice(body);
        let padding = (512 - body.len() % 512) % 512;
        bytes.extend(std::iter::repeat_n(0u8, padding));
    }
    bytes.extend(std::iter::repeat_n(0u8, 1024));
    std::fs::write(path, bytes).unwrap();
}

/// Five formats, one tree, and the same answer about it.
///
/// **The test that would have caught the container name.** A single compressed
/// file was reported as a `tar.gz` for an afternoon, because the format is a
/// container *and* a filter and only one of them had been thought about.
#[test]
fn every_format_says_the_same_thing_about_the_same_tree() {
    if !have("tar") || !have("zip") {
        eprintln!("skerry-archive: no tar or no zip on this machine — skipped");
        return;
    }
    let at = scratch("formats");
    tree(&at);
    let archive_of = |args: &[&str]| {
        assert!(
            Command::new(args[0]).args(&args[1..]).current_dir(&at).status().unwrap().success()
        );
    };
    archive_of(&["zip", "-qry", "t.zip", "tree"]);
    archive_of(&["tar", "-cf", "t.tar", "tree"]);
    archive_of(&["tar", "-czf", "t.tar.gz", "tree"]);

    let plain = listing_of(&at.join("t.tar"));
    let wanted = vec![
        ("tree/deep/blob.bin".to_owned(), 200_000),
        ("tree/deep/inner/note.md".to_owned(), 7),
        ("tree/hello.txt".to_owned(), 14),
        ("tree/run.sh".to_owned(), 18),
    ];
    assert_eq!(files(&plain), wanted, "a plain tar");
    assert_eq!(plain.refusal, Refusal::None);
    assert_eq!(plain.format.map(Format::name), Some("tar"));

    for (name, called) in [("t.zip", "zip"), ("t.tar.gz", "tar.gz")] {
        let listing = listing_of(&at.join(name));
        assert_eq!(listing.refusal, Refusal::None, "{name}");
        assert_eq!(listing.format.map(Format::name), Some(called), "{name}");
        assert_eq!(files(&listing), wanted, "{name} disagreed with the tar");
        // The symlink is a link in both, which is the one kind an extractor can
        // get wrong in a way that matters.
        let link = listing.entries.iter().find(|entry| entry.path == "tree/link.txt");
        assert_eq!(link.map(|entry| entry.kind), Some(Kind::Link), "{name}");
    }

    // The compressors that are somebody else's program, where the machine has
    // them. Skipped rather than failed: a build machine without `xz` is a build
    // machine whose *users* may still have one.
    for (flag, name, called) in
        [("-J", "t.txz", "tar.xz"), ("--zstd", "t.tzst", "tar.zst"), ("-j", "t.tbz", "tar.bz2")]
    {
        let made = Command::new("tar")
            .args([flag, "-cf", name, "tree"])
            .current_dir(&at)
            .status()
            .is_ok_and(|it| it.success());
        if !made {
            eprintln!("skerry-archive: this machine cannot write a {called} — skipped");
            continue;
        }
        let listing = listing_of(&at.join(name));
        assert_eq!(listing.refusal, Refusal::None, "{called}");
        assert_eq!(listing.format.map(Format::name), Some(called));
        assert_eq!(files(&listing), wanted, "{called} disagreed with the tar");
    }
    std::fs::remove_dir_all(&at).unwrap();
}

/// What went in is what comes out — bytes, the execute bit, and the link.
#[test]
fn an_unpack_puts_back_what_went_in() {
    if !have("tar") {
        return;
    }
    let at = scratch("roundtrip");
    let source = tree(&at);
    assert!(
        Command::new("tar")
            .args(["-czf", "t.tar.gz", "tree"])
            .current_dir(&at)
            .status()
            .unwrap()
            .success()
    );
    let into = at.join("out");
    let (done, _) = unpacked(&at.join("t.tar.gz"), &into, &[]);
    assert_eq!(done.refusal, Refusal::None, "{done:?}");
    assert_eq!(done.progress.bytes, 200_039);

    let there = into.join("tree");
    for name in ["hello.txt", "run.sh", "deep/blob.bin", "deep/inner/note.md"] {
        assert_eq!(
            std::fs::read(source.join(name)).unwrap(),
            std::fs::read(there.join(name)).unwrap(),
            "{name} came back different"
        );
    }
    let mode = mode_of(&there.join("run.sh"));
    assert_eq!(mode & 0o111, 0o111, "the execute bit did not come with it");
    let plain = mode_of(&there.join("hello.txt"));
    assert_eq!(plain & 0o111, 0, "a text file arrived executable");
    assert_eq!(
        std::fs::read_link(there.join("link.txt")).unwrap(),
        Path::new("hello.txt"),
        "the symlink is not a symlink"
    );
    std::fs::remove_dir_all(&at).unwrap();
}

/// Zip-slip, in the two shapes a string check is for.
#[test]
fn a_path_that_climbs_out_lands_nowhere() {
    let at = scratch("escape");
    let into = at.join("into");
    tar_of(
        &at.join("evil.tar"),
        &[
            ("../escaped.txt", b'0', "", b"out\n"),
            ("/absolute.txt", b'0', "", b"out\n"),
            ("deep/../../also.txt", b'0', "", b"out\n"),
            ("kept.txt", b'0', "", b"in\n"),
        ],
    );
    let (done, _) = unpacked(&at.join("evil.tar"), &into, &[]);
    assert_eq!(done.refusal, Refusal::Partial, "skipped entries must be reported");
    assert_eq!(done.progress.entries, 1, "more than the one honest entry was written");
    assert_eq!(std::fs::read(into.join("kept.txt")).unwrap(), b"in\n");
    for escaped in ["escaped.txt", "also.txt"] {
        assert!(!at.join(escaped).exists(), "{escaped} landed beside the destination");
    }
    assert!(!Path::new("/absolute.txt").exists(), "an absolute path was honoured");
    std::fs::remove_dir_all(&at).unwrap();
}

/// The other half of zip-slip, which no `..` appears in.
///
/// An archive holds a symlink out of the destination and then a file *underneath*
/// it. Every path here is innocent as a string; what stops it is that the walk to
/// the parent directory refuses to follow a link.
#[test]
fn nothing_is_written_through_a_symlink_the_archive_planted() {
    let at = scratch("through");
    let (into, outside) = (at.join("into"), at.join("outside"));
    std::fs::create_dir_all(&outside).unwrap();
    tar_of(
        &at.join("door.tar"),
        &[
            ("door", b'2', outside.to_str().unwrap(), b""),
            ("door/passwd", b'0', "", b"owned\n"),
            ("fine.txt", b'0', "", b"fine\n"),
        ],
    );
    let (done, _) = unpacked(&at.join("door.tar"), &into, &[]);
    assert_eq!(done.refusal, Refusal::Partial);
    assert!(!outside.join("passwd").exists(), "a file was written through the symlink");
    // The link itself is there — a symlink is a string, and writing one puts
    // nothing anywhere — and the honest entry after it still arrived.
    assert!(into.join("door").is_symlink());
    assert_eq!(std::fs::read(into.join("fine.txt")).unwrap(), b"fine\n");
    std::fs::remove_dir_all(&at).unwrap();
}

/// **`tar czf x.tgz .` — the commonest tarball there is.**
///
/// Every entry in one is `./something`, and left alone that is a folder called
/// `.` with the whole archive inside it *and* an unpack that writes nothing,
/// because a `.` component is refused and should be. Both halves are asserted
/// here because the listing looking right is not the same claim as the files
/// arriving.
#[test]
fn a_tarball_made_of_a_dot_is_not_a_folder_called_dot() {
    if !have("tar") {
        return;
    }
    let at = scratch("dot");
    let source = tree(&at);
    assert!(
        Command::new("tar")
            .args(["-czf", "../dot.tgz", "."])
            .current_dir(&source)
            .status()
            .unwrap()
            .success()
    );
    let archive = at.join("dot.tgz");

    let listing = listing_of(&archive);
    assert_eq!(listing.refusal, Refusal::None);
    for entry in &listing.entries {
        assert!(!entry.path.starts_with("./"), "{:?} kept its prefix", entry.path);
        assert!(!entry.path.is_empty(), "the archive's own root is a row");
    }
    assert_eq!(
        files(&listing),
        vec![
            ("deep/blob.bin".to_owned(), 200_000),
            ("deep/inner/note.md".to_owned(), 7),
            ("hello.txt".to_owned(), 14),
            ("run.sh".to_owned(), 18),
        ]
    );

    let into = at.join("out");
    let (done, _) = unpacked(&archive, &into, &[]);
    assert_eq!(done.refusal, Refusal::None, "{done:?}");
    assert_eq!(
        std::fs::read(into.join("deep/inner/note.md")).unwrap(),
        b"nested\n",
        "the entries were refused rather than written"
    );
    // At the top of the destination, which is where `./hello.txt` means.
    assert!(into.join("hello.txt").is_file(), "the entries landed somewhere else");
    assert_eq!(mode_of(&into.join("run.sh")) & 0o111, 0o111);
    std::fs::remove_dir_all(&at).unwrap();
}

/// **A zip with something in front of it is still a zip.**
///
/// A self-extracting archive is a program with a zip stapled to its end, and a
/// download can arrive with a prefix for less interesting reasons. The format is
/// defined from its *end*, so the file does not begin with `PK` and every offset
/// it records is short by the length of whatever is in front — which is what
/// makes this two claims: that it is recognised, and that its entries are found.
///
/// Go's own `archive/zip` corpus keeps a file for this, and it was the only one
/// of its thirty that this refused. The fixture here is built rather than
/// borrowed, because a test that needs Go installed is a test that runs nowhere.
#[test]
fn a_zip_behind_a_prefix_is_found_by_its_index() {
    if !have("zip") {
        return;
    }
    let at = scratch("prefix");
    tree(&at);
    assert!(
        Command::new("zip")
            .args(["-qry", "plain.zip", "tree"])
            .current_dir(&at)
            .status()
            .unwrap()
            .success()
    );
    let mut stapled = b"#!/bin/sh\n# a stub, and then a zip\n".to_vec();
    stapled.extend_from_slice(&std::fs::read(at.join("plain.zip")).unwrap());
    let archive = at.join("sfx.bin");
    std::fs::write(&archive, stapled).unwrap();

    let listing = listing_of(&archive);
    assert_eq!(listing.refusal, Refusal::None, "a prefixed zip was refused");
    assert_eq!(files(&listing), files(&listing_of(&at.join("plain.zip"))));

    // And the entries are read from where they actually are, not from where the
    // directory says: those offsets are all short by the prefix.
    let into = at.join("out");
    let (done, _) = unpacked(&archive, &into, &[]);
    assert_eq!(done.refusal, Refusal::None, "{done:?}");
    assert_eq!(std::fs::read(into.join("tree/hello.txt")).unwrap(), b"hello archive\n");
    std::fs::remove_dir_all(&at).unwrap();
}

/// A zip's symlink comes out as a symlink, the same as a tar's.
///
/// **A different mechanism, and that is the point.** A tar records a link target
/// in its header; a zip records it as the entry's *data*, so a reader that
/// treated the two the same would drop every link a zip holds — which is what
/// this one did until a test asked.
#[test]
fn a_zip_keeps_its_links() {
    if !have("zip") {
        return;
    }
    let at = scratch("ziplinks");
    let source = tree(&at);
    // `-y` or `zip` stores what the link points at instead of the link.
    assert!(
        Command::new("zip")
            .args(["-qry", "t.zip", "tree"])
            .current_dir(&at)
            .status()
            .unwrap()
            .success()
    );
    let into = at.join("out");
    let (done, _) = unpacked(&at.join("t.zip"), &into, &[]);
    assert_eq!(done.refusal, Refusal::None, "{done:?}");
    let link = into.join("tree/link.txt");
    assert!(link.is_symlink(), "the link came out as something else");
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("hello.txt"));
    // And the file it points at is there too, so the link resolves.
    assert_eq!(std::fs::read(source.join("hello.txt")).unwrap(), std::fs::read(link).unwrap());
    std::fs::remove_dir_all(&at).unwrap();
}

/// Extracting one folder out of an archive writes that folder and nothing else.
#[test]
fn a_selection_is_the_only_thing_unpacked() {
    if !have("tar") {
        return;
    }
    let at = scratch("selection");
    tree(&at);
    assert!(
        Command::new("tar")
            .args(["-cf", "t.tar", "tree"])
            .current_dir(&at)
            .status()
            .unwrap()
            .success()
    );
    let into = at.join("out");
    let only = vec!["tree/deep/inner".to_owned()];
    let (done, _) = unpacked(&at.join("t.tar"), &into, &only);
    assert_eq!(done.refusal, Refusal::None);
    assert!(into.join("tree/deep/inner/note.md").exists(), "the selection is missing");
    assert!(!into.join("tree/hello.txt").exists(), "something outside the selection arrived");
    std::fs::remove_dir_all(&at).unwrap();
}

/// A 7z is named by its format rather than called "not an archive".
///
/// **And not by a program either.** Saying "7z is not installed" is false on a
/// machine that has 7-Zip on it, and the truth — that this program does not read
/// the format — is a different sentence with different advice.
#[test]
fn an_archive_this_does_not_read_is_named_by_its_format() {
    let at = scratch("elsewhere");
    // The magic is the format; the rest of the file need not be one, because
    // nothing here will read it.
    std::fs::write(at.join("thing.7z"), b"7z\xbc\xaf\x27\x1c\x00\x04").unwrap();
    let listing = listing_of(&at.join("thing.7z"));
    assert_eq!(listing.refusal, Refusal::Elsewhere);
    assert_eq!(listing.note, "7z");
    assert_eq!(listing.refusal.message(), Some("archive.elsewhere"));

    // A rar the same way, so the two do not come back as one word.
    std::fs::write(at.join("thing.rar"), b"Rar!\x1a\x07\x01\x00").unwrap();
    assert_eq!(listing_of(&at.join("thing.rar")).note, "rar");
    std::fs::remove_dir_all(&at).unwrap();
}

/// Something that is not an archive at all is not an archive, and says so with
/// no message, because a text file having no listing is not news.
#[test]
fn a_file_that_is_not_an_archive_says_so_quietly() {
    let at = scratch("plain");
    std::fs::write(at.join("notes.txt"), b"just some words\n").unwrap();
    let listing = listing_of(&at.join("notes.txt"));
    assert_eq!(listing.refusal, Refusal::NotAnArchive);
    assert_eq!(listing.refusal.message(), None);
    std::fs::remove_dir_all(&at).unwrap();
}

/// A zip whose bytes were changed after it was written stops with the entry named.
#[test]
fn a_broken_zip_stops_and_says_which_entry() {
    if !have("zip") {
        return;
    }
    let at = scratch("corrupt");
    tree(&at);
    assert!(
        Command::new("zip")
            .args(["-qry", "t.zip", "tree"])
            .current_dir(&at)
            .status()
            .unwrap()
            .success()
    );
    // The blob is 200 kB of one byte, so the middle of the file is certainly
    // inside its deflated data rather than in a header.
    let mut bytes = std::fs::read(at.join("t.zip")).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    std::fs::write(at.join("t.zip"), &bytes).unwrap();

    let (done, _) = unpacked(&at.join("t.zip"), &at.join("out"), &[]);
    assert_eq!(done.refusal, Refusal::Undecodable, "a corrupt zip unpacked cleanly");
    assert!(done.note.contains("blob.bin"), "which entry? {:?}", done.note);
    std::fs::remove_dir_all(&at).unwrap();
}

/// An unpack says where it has got to before it has got to the end.
#[test]
fn an_unpack_reports_before_it_finishes() {
    let at = scratch("progress");
    // Enough entries to pass the child's own throttle twice over.
    let bodies: Vec<Vec<u8>> = (0..40).map(|n| format!("file {n}\n").into_bytes()).collect();
    let names: Vec<String> = (0..40).map(|n| format!("f{n:03}.txt")).collect();
    let entries: Vec<(&str, u8, &str, &[u8])> = names
        .iter()
        .zip(&bodies)
        .map(|(name, body)| (name.as_str(), b'0', "", body.as_slice()))
        .collect();
    tar_of(&at.join("many.tar"), &entries);
    let (done, records) = unpacked(&at.join("many.tar"), &at.join("out"), &[]);
    assert_eq!(done.refusal, Refusal::None);
    assert_eq!(done.progress.entries, 40);
    assert!(records >= 2, "a forty-entry unpack reported {records} times");
    std::fs::remove_dir_all(&at).unwrap();
}

/// A device node in an archive is counted and never made.
#[test]
fn a_device_node_is_not_something_an_archive_can_make() {
    let at = scratch("devices");
    tar_of(
        &at.join("nodes.tar"),
        &[("null", b'3', "", b""), ("pipe", b'6', "", b""), ("real.txt", b'0', "", b"real\n")],
    );
    let into = at.join("into");
    let (done, _) = unpacked(&at.join("nodes.tar"), &into, &[]);
    assert_eq!(done.progress.entries, 1);
    assert!(!into.join("null").exists());
    assert!(!into.join("pipe").exists());
    assert!(into.join("real.txt").exists());
    std::fs::remove_dir_all(&at).unwrap();
}

/// **An archive with nothing in it cannot be recognised**, and this is the test
/// that says so out loud.
///
/// A tar's end is 1,024 zero bytes, so an empty one is *only* that — no `ustar`
/// magic and no header to checksum, because there is no header. It is
/// indistinguishable by content from any other file of zeros, and detection here
/// is by content on purpose. The answer is the quiet one: not an archive, no
/// message, the panel shows the file as what it otherwise is.
#[test]
fn an_archive_with_nothing_in_it_cannot_be_recognised() {
    let at = scratch("empty");
    tar_of(&at.join("nothing.tar"), &[]);
    let listing = listing_of(&at.join("nothing.tar"));
    assert_eq!(listing.refusal, Refusal::NotAnArchive);
    assert_eq!(listing.refusal.message(), None);

    // One entry is enough to be recognisable, and then the count is honest.
    tar_of(&at.join("one.tar"), &[("only.txt", b'0', "", b"x\n")]);
    let listing = listing_of(&at.join("one.tar"));
    assert_eq!(listing.refusal, Refusal::None);
    assert_eq!(listing.count, 1);
    assert!(!listing.truncated());
    assert_eq!(listing.format.map(|format| format.container), Some(Container::Tar));
    let _: Vec<Entry> = listing.entries;
    std::fs::remove_dir_all(&at).unwrap();
}

/// The confinement is in force in the process that does the work.
///
/// **The line, not an inference.** Everything above this test proves that *our*
/// checks refuse an escape; this proves the kernel was asked as well, in the
/// child that did the unpacking, which is the half that still holds when one of
/// those checks turns out to be wrong.
#[test]
fn the_child_that_unpacks_says_what_is_holding_it() {
    let at = scratch("confined");
    tar_of(&at.join("one.tar"), &[("only.txt", b'0', "", b"x\n")]);
    let into = at.join("into");
    std::fs::create_dir_all(&into).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_skerry-archive"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = Request {
        ask: Ask::Unpack,
        archive: at.join("one.tar"),
        into: into.clone(),
        only: Vec::new(),
    };
    request.write_to(&mut child.stdin.take().unwrap()).unwrap();
    let done = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&done.stderr);
    assert!(said.contains("no-new-privs"), "{said}");
    assert!(!said.contains("FAILED"), "a layer did not take: {said}");
    // Where the kernel has no Landlock the child refuses rather than unpacking —
    // ADR 0046, which superseded 0006's "reduced isolation is logged" — and the
    // line says why.
    assert!(said.contains("landlock"), "{said}");
    if said.contains("landlock ABSENT") {
        let refused = sk_preview::sandbox::REFUSED_WITHOUT_LANDLOCK;
        assert_eq!(done.status.code(), Some(refused), "{said}");
    } else {
        assert!(into.join("only.txt").exists());
    }
    std::fs::remove_dir_all(&at).unwrap();
}

/// **A probe's report reaches a file**, which is how one reaches a bug report.
///
/// The served confinement's file ceiling is zero for a listing, so a probe under
/// it ended at its first `println!` with a panic and status 101.
#[test]
fn a_probe_sent_to_a_file_says_everything() {
    let at = scratch("probe");
    let report = std::fs::File::create(at.join("probe.txt")).unwrap();
    let probed = Command::new(env!("CARGO_BIN_EXE_skerry-archive"))
        .arg("--probe")
        .env(sk_preview::sandbox::WITHOUT_LANDLOCK, "allow")
        .stdout(report)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let said = std::fs::read_to_string(at.join("probe.txt")).unwrap();
    std::fs::remove_dir_all(&at).unwrap();
    assert!(probed.success(), "{probed:?}: {said}");
    assert!(said.contains("no-new-privs") && said.contains("xz:"), "{said}");
}
