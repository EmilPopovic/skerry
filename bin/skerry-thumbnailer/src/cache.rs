// SPDX-License-Identifier: Apache-2.0
//! The shared thumbnail cache, read and written by the child.
//!
//! **The specification's directory, so every file manager on the machine shares
//! it.** `$XDG_CACHE_HOME/thumbnails/normal/<md5 of the file's URI>.png`, 128×128,
//! with `Thumb::URI` and `Thumb::MTime` in the PNG's text chunks — which is what
//! makes a thumbnail this program wrote usable by Nautilus and one Nautilus wrote
//! usable here.
//!
//! **The child owns it, not the host** — [ADR
//! 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md).
//! A cached thumbnail is a PNG somebody else wrote, so reading one in the host
//! would put a decoder back in the main process for exactly the files most likely
//! to have been planted.
//!
//! **`Thumb::MTime` is checked, not trusted for being present.** The specification
//! says a thumbnail is stale when it disagrees with the original's modification
//! time, and a cache that skipped that check would show yesterday's picture of a
//! file somebody re-exported this morning.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, StatxFlags};

/// The specification's `normal` size. `large` is 256 and is not written here —
/// [ADR 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)
/// has one slot size and this is it.
pub const SIDE: u32 = 128;

/// The folder `normal` thumbnails go in, or nothing if there is no cache
/// directory.
///
/// **Read from the environment once**, by `main`, and handed to everything after
/// it. A test asks [`folder_from`] instead of writing `$XDG_CACHE_HOME`, which is
/// process-wide and read by C in the same process.
pub fn folder() -> Option<PathBuf> {
    folder_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// [`folder`] over the two values it reads.
fn folder_from(cache_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = cache_home
        .map(PathBuf::from)
        .filter(|it| it.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("thumbnails").join("normal"))
}

/// Where a file's thumbnail lives in `folder`.
pub fn place(folder: &Path, path: &Path) -> PathBuf {
    folder.join(format!("{}.png", crate::md5::hex(uri(path).as_bytes())))
}

/// The `file://` URI of a path, percent-encoded as the specification requires.
///
/// **Every byte outside the unreserved set is escaped**, which is what makes the
/// digest agree with other implementations. A path is bytes rather than text, so
/// this encodes bytes: a filename that is not UTF-8 still has one right answer.
fn uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Reads a cached thumbnail, if there is a fresh one.
///
/// Returns the RGBA and its size. A thumbnail whose recorded `Thumb::MTime`
/// disagrees with `mtime` is stale and is treated as absent.
pub fn read(folder: &Path, path: &Path, mtime: i64) -> Option<(Vec<u8>, u32, u32)> {
    read_from(&place(folder, path), mtime)
}

/// As [`read`], from a named file — the seam a test uses.
///
/// **Nothing wider or taller than [`SIDE`] is decoded.** The pixel buffer below
/// is sized by what the header claims, the header was written by whoever wrote
/// the file, and a `normal` thumbnail is at most 128×128 by the specification.
fn read_from(file: &Path, mtime: i64) -> Option<(Vec<u8>, u32, u32)> {
    let file = open_regular(file).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reading = decoder.read_info().ok()?;
    if reading.info().width > SIDE || reading.info().height > SIDE {
        return None;
    }
    let fresh = reading
        .info()
        .uncompressed_latin1_text
        .iter()
        .find(|chunk| chunk.keyword == "Thumb::MTime")
        .and_then(|chunk| chunk.text.trim().parse::<i64>().ok())
        .is_some_and(|written| written == mtime);
    if !fresh {
        return None;
    }
    let mut raw = vec![0u8; reading.output_buffer_size()?];
    let frame = reading.next_frame(&mut raw).ok()?;
    let info = reading.info();
    let pixels = to_rgba(&raw[..frame.buffer_size()], frame.color_type)?;
    Some((pixels, info.width, info.height))
}

/// Writes a thumbnail where the specification says, with the two chunks that make
/// it shareable and checkable.
///
/// **Through a temporary and a rename**, which the specification requires and
/// which is the same three steps the session file takes: a reader sees a whole
/// thumbnail or none, never a torn one. Failure is silent on purpose — a cache
/// that cannot be written is a slower program, not a broken one.
pub fn write(folder: &Path, path: &Path, mtime: i64, pixels: &[u8], size: (u32, u32)) {
    let _ = write_to(&place(folder, path), path, mtime, pixels, size);
}

/// How a cache folder is opened, to write into or to close.
///
/// **Not through a link at its name**, and never waiting: `O_DIRECTORY` refuses a
/// FIFO swapped in for the folder before the open could block on it.
const FOLDER: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Opens a file to read, if what is at `file` is one.
///
/// **A regular file at that very name, or nothing.** Every file manager this
/// person runs writes here, so what is in it may have been planted: a link is not
/// followed, and a FIFO opens without waiting for a writer and is then refused,
/// rather than holding the child until the host's deadline.
fn open_regular(file: &Path) -> std::io::Result<std::fs::File> {
    let open = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::open(file, open, Mode::empty())?;
    let stat = rustix::fs::statx(&fd, "", AtFlags::EMPTY_PATH, StatxFlags::TYPE)?;
    if FileType::from_raw_mode(u32::from(stat.stx_mode)) != FileType::RegularFile {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(std::fs::File::from(fd))
}

/// Makes the cache's folder, or closes one that was left open.
///
/// **0700 for `thumbnails/normal` and the `thumbnails` above it**, as the
/// specification's "Permissions" section says: a thumbnail is a small copy of a
/// picture and its `Thumb::URI` is the picture's full path. An existing folder
/// is tightened rather than trusted, because the thumbnails an older version
/// wrote into it are 0644 and the folder is the one thing that closes all of
/// them at once. `$XDG_CACHE_HOME` itself is the person's and is not touched.
///
/// **Before confinement**, which is where it is called, because the folder has
/// to exist before Landlock can be given a descriptor for it.
pub fn prepare(directory: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    if std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory).is_err() {
        return;
    }
    for folder in [Some(directory), directory.parent()].into_iter().flatten() {
        seal(folder);
    }
}

/// Takes group and other access off a cache folder an older version left open.
///
/// A folder somebody else owns is left alone, so a child running as root with
/// another person's `$HOME` changes nothing of theirs.
fn seal(folder: &Path) {
    let Ok(fd) = rustix::fs::open(folder, FOLDER, Mode::empty()) else { return };
    let asked = StatxFlags::MODE | StatxFlags::UID;
    let Ok(stat) = rustix::fs::statx(&fd, "", AtFlags::EMPTY_PATH, asked) else { return };
    let mode = u32::from(stat.stx_mode);
    let shared = mode & 0o077;
    if shared != 0 && stat.stx_uid == rustix::process::geteuid().as_raw() {
        let _ = rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode & 0o700));
    }
}

/// As [`write`], to a named file, and saying why it did not.
///
/// Everything happens against one descriptor for the folder, so the thumbnail is
/// staged and published in the same place, and the rename replaces a link at
/// its name rather than following it. **One staging name per process**, because
/// the child answers one request at a time
/// ([ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md));
/// what is already there was left by a killed child with the same pid, and is
/// removed.
fn write_to(
    file: &Path,
    original: &Path,
    mtime: i64,
    pixels: &[u8],
    size: (u32, u32),
) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let (Some(directory), Some(name)) = (file.parent(), file.file_name()) else {
        return Err(std::io::Error::other("no folder"));
    };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    let folder = rustix::fs::open(directory, FOLDER, Mode::empty())?;
    let staging = Path::new(name).with_extension(format!("{}.tmp", std::process::id()));
    match rustix::fs::unlinkat(&folder, &staging, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let written = encode(stage(&folder, &staging)?, original, mtime, pixels, size)
        .map_err(std::io::Error::from)
        .and_then(|()| {
            rustix::fs::renameat(&folder, &staging, &folder, name).map_err(std::io::Error::from)
        });
    if written.is_err() {
        let _ = rustix::fs::unlinkat(&folder, &staging, AtFlags::empty());
    }
    written
}

/// Creates the staging file, 0600, refusing anything already at its name.
///
/// **`O_EXCL` is what makes the `unlinkat` before it safe.** A link planted again
/// in between is a name that exists, and the create fails "regardless of the
/// contents of the symbolic link" (POSIX `open`) rather than writing through it —
/// and there is no `O_TRUNC` for the sandbox to have to allow.
fn stage(folder: &impl rustix::fd::AsFd, staging: &Path) -> std::io::Result<std::fs::File> {
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let handle = rustix::fs::openat(folder, staging, create, Mode::from_raw_mode(0o600))?;
    Ok(std::fs::File::from(handle))
}

fn encode(
    to: std::fs::File,
    original: &Path,
    mtime: i64,
    pixels: &[u8],
    size: (u32, u32),
) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(to), size.0, size.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.add_text_chunk("Thumb::URI".to_owned(), uri(original))?;
    encoder.add_text_chunk("Thumb::MTime".to_owned(), mtime.to_string())?;
    let mut writing = encoder.write_header()?;
    writing.write_image_data(pixels)?;
    writing.finish()
}

/// Widens whatever the decoder produced into straight `RGBA8`.
///
/// `normalize_to_color8` has already expanded bit depths and palettes, so what is
/// left is the four channel counts — and a PNG with none of them is one this does
/// not read rather than one it guesses at.
fn to_rgba(raw: &[u8], colour: png::ColorType) -> Option<Vec<u8>> {
    let widen = |stride: usize, spread: fn(&[u8]) -> [u8; 4]| {
        raw.len()
            .is_multiple_of(stride)
            .then(|| raw.chunks_exact(stride).flat_map(spread).collect())
    };
    match colour {
        png::ColorType::Rgba => Some(raw.to_vec()),
        png::ColorType::Rgb => widen(3, |it| [it[0], it[1], it[2], 255]),
        png::ColorType::Grayscale => widen(1, |it| [it[0], it[0], it[0], 255]),
        png::ColorType::GrayscaleAlpha => widen(2, |it| [it[0], it[0], it[0], it[1]]),
        png::ColorType::Indexed => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        SIDE, folder_from, place, prepare, read_from, seal, stage, to_rgba, uri, write_to,
    };
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("skerry-thumbnail-cache-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn mode(path: &Path) -> String {
        format!("{:o}", std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777)
    }

    fn open_up(path: &Path) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs `work`, failing the test rather than hanging it if it blocks.
    fn promptly<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(work()));
        receiver.recv_timeout(std::time::Duration::from_secs(10)).expect("it blocked")
    }

    fn fifo(at: &Path) {
        rustix::fs::mkfifoat(rustix::fs::CWD, at, rustix::fs::Mode::from_raw_mode(0o644)).unwrap();
    }

    /// **0600 in 0700, which is the specification's "Permissions" section.** A
    /// thumbnail is a small copy of the picture and its `Thumb::URI` chunk is the
    /// picture's full path, so one anybody could read publishes both.
    #[test]
    fn a_thumbnail_is_readable_by_its_owner_alone() {
        let root = scratch("private");
        let folder = root.join("thumbnails").join("normal");
        let file = folder.join("0123.png");
        write_to(&file, Path::new("/home/a/private.png"), 7, &[255; 4], (1, 1)).unwrap();
        assert_eq!(mode(&file), "600", "the thumbnail");
        assert_eq!(mode(&folder), "700", "thumbnails/normal");
        assert_eq!(mode(&root.join("thumbnails")), "700", "thumbnails");
        let staging = file.with_extension(format!("{}.tmp", std::process::id()));
        assert!(!staging.exists(), "the staging file was left beside the thumbnail");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **A folder an older version left open is closed**, which is the only thing
    /// that reaches the thumbnails it already wrote 0644 — and the folder above
    /// `thumbnails` is the person's, so it is left as it was.
    #[test]
    fn an_open_cache_folder_is_closed_and_the_one_above_it_is_not() {
        let root = scratch("tighten");
        let thumbnails = root.join("thumbnails");
        let normal = thumbnails.join("normal");
        std::fs::create_dir_all(&normal).unwrap();
        for folder in [&root, &thumbnails, &normal] {
            open_up(folder);
        }
        prepare(&normal);
        assert_eq!(mode(&normal), "700", "thumbnails/normal");
        assert_eq!(mode(&thumbnails), "700", "thumbnails");
        assert_eq!(mode(&root), "755", "the cache home was changed");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **A link in the cache is followed by neither the write nor the close**, and
    /// a FIFO in a folder's place is not waited on. The staging link is removed
    /// rather than opened, and the thumbnail's own name is replaced rather than
    /// followed.
    #[test]
    fn a_link_in_the_cache_is_not_written_or_closed_through() {
        let root = scratch("links");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        open_up(&elsewhere);
        std::fs::create_dir_all(root.join("thumbnails")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("thumbnails").join("normal")).unwrap();
        prepare(&root.join("thumbnails").join("normal"));
        assert_eq!(mode(&elsewhere), "755", "the close went through a link");
        let pipe = root.join("pipe");
        fifo(&pipe);
        let held = pipe.clone();
        promptly(move || seal(&held));
        assert_eq!(mode(&pipe), "644", "a FIFO was changed");

        let folder = root.join("real");
        std::fs::create_dir_all(&folder).unwrap();
        let (staged, named) = (root.join("staged-victim"), root.join("named-victim"));
        std::fs::write(&staged, "unrelated").unwrap();
        std::fs::write(&named, "unrelated").unwrap();
        let file = folder.join("0123.png");
        let staging = file.with_extension(format!("{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&staged, &staging).unwrap();
        std::os::unix::fs::symlink(&named, &file).unwrap();
        write_to(&file, Path::new("/a.png"), 7, &[255; 4], (1, 1)).unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), b"unrelated", "the staging link");
        assert_eq!(std::fs::read(&named).unwrap(), b"unrelated", "the thumbnail's own name");
        assert_eq!(mode(&file), "600", "the thumbnail is not the file the write made");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **The staging file is created, never opened**: a link put back at its name
    /// between the write's `unlinkat` and its create is refused, and the file it
    /// points at is untouched. The write cannot be made to lose that race on
    /// demand, so this asks the create directly.
    #[test]
    fn a_link_at_the_staging_name_is_refused_by_the_create() {
        use rustix::fs::{Mode, OFlags};
        let root = scratch("stage");
        let victim = root.join("victim");
        std::fs::write(&victim, "unrelated").unwrap();
        std::os::unix::fs::symlink(&victim, root.join("0123.1.tmp")).unwrap();
        let open = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = rustix::fs::open(&root, open, Mode::empty()).unwrap();
        let refused = stage(&fd, Path::new("0123.1.tmp")).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&victim).unwrap(), b"unrelated");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **Only a regular file at the thumbnail's own name is read.** A link is not
    /// followed, and a FIFO is refused at once rather than holding the child
    /// until the host's deadline.
    #[test]
    fn a_link_or_a_fifo_at_the_thumbnail_name_is_not_read() {
        let root = scratch("planted");
        let file = root.join("0123.png");
        write_to(&file, Path::new("/a.png"), 7, &[255; 4], (1, 1)).unwrap();
        assert!(read_from(&file, 7).is_some(), "the thumbnail itself was refused");
        let linked = root.join("linked.png");
        std::os::unix::fs::symlink(&file, &linked).unwrap();
        assert!(read_from(&linked, 7).is_none(), "a thumbnail was read through a link");
        let pipe = root.join("pipe.png");
        fifo(&pipe);
        assert!(promptly(move || read_from(&pipe, 7).is_none()), "a FIFO was read");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **A thumbnail larger than the specification's is not decoded**, because the
    /// buffer it decodes into is whatever size its header claims.
    #[test]
    fn a_thumbnail_larger_than_the_specification_allows_is_not_decoded() {
        let root = scratch("oversized");
        for (side, kept) in [(SIDE, true), (SIDE + 1, false)] {
            let file = root.join(format!("{side}.png"));
            let pixels = vec![255; side as usize * 4];
            write_to(&file, Path::new("/a.png"), 7, &pixels, (side, 1)).unwrap();
            assert_eq!(read_from(&file, 7).is_some(), kept, "{side} pixels wide");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **The digest has to match other implementations**, so the URI it is taken
    /// of has to be encoded the way they encode it.
    #[test]
    fn a_uri_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(uri(Path::new("/a/b.png")), "file:///a/b.png");
        assert_eq!(uri(Path::new("/a/my file.png")), "file:///a/my%20file.png");
        assert_eq!(uri(Path::new("/a/ä.png")), "file:///a/%C3%A4.png");
        // A path that is not UTF-8 still has one answer rather than a lossy one.
        let messy = Path::new(std::ffi::OsStr::from_bytes(b"/a/\xff.png"));
        assert_eq!(uri(messy), "file:///a/%FF.png");
    }

    /// The name is the digest of the URI, and the directory is the specification's.
    ///
    /// **From two values, never from the process.** Writing `$XDG_CACHE_HOME`
    /// here raced every other test in the binary that reads the environment.
    #[test]
    fn a_thumbnail_lands_where_the_specification_says() {
        let folder = folder_from(Some("/tmp/cache-for-a-test".into()), Some("/h".into())).unwrap();
        let at = place(&folder, Path::new("/home/a/photo.png"));
        assert!(at.starts_with("/tmp/cache-for-a-test/thumbnails/normal"), "{at:?}");
        assert_eq!(
            at.file_name().unwrap().to_string_lossy(),
            format!("{}.png", crate::md5::hex(b"file:///home/a/photo.png"))
        );
        // A relative cache home is invalid by the base directory specification,
        // so the one under the home directory is used instead.
        let fallback = folder_from(Some("cache".into()), Some("/h".into())).unwrap();
        assert_eq!(fallback, Path::new("/h/.cache/thumbnails/normal"));
        assert_eq!(folder_from(None, None), None);
    }

    #[test]
    fn every_colour_type_widens_to_four_channels_or_says_it_cannot() {
        assert_eq!(to_rgba(&[1, 2, 3, 4], png::ColorType::Rgba).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(to_rgba(&[1, 2, 3], png::ColorType::Rgb).unwrap(), vec![1, 2, 3, 255]);
        assert_eq!(to_rgba(&[9], png::ColorType::Grayscale).unwrap(), vec![9, 9, 9, 255]);
        assert_eq!(to_rgba(&[9, 4], png::ColorType::GrayscaleAlpha).unwrap(), vec![9, 9, 9, 4]);
        assert!(to_rgba(&[1], png::ColorType::Indexed).is_none());
        // A buffer that is not a whole number of pixels is refused rather than
        // truncated into an image with a torn last row.
        assert!(to_rgba(&[1, 2, 3, 4], png::ColorType::Rgb).is_none());
    }
}
