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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The specification's `normal` size. `large` is 256 and is not written here —
/// [ADR 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md)
/// has one slot size and this is it.
pub const SIDE: u32 = 128;

/// Where a file's thumbnail lives, or nothing if there is no cache directory.
pub fn place(path: &Path) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|it| it.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    let name = format!("{}.png", crate::md5::hex(uri(path).as_bytes()));
    Some(base.join("thumbnails").join("normal").join(name))
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
pub fn read(path: &Path, mtime: i64) -> Option<(Vec<u8>, u32, u32)> {
    let file = std::fs::File::open(place(path)?).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reading = decoder.read_info().ok()?;
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
pub fn write(path: &Path, mtime: i64, pixels: &[u8], size: (u32, u32)) {
    if let Some(file) = place(path) {
        let _ = write_to(&file, path, mtime, pixels, size);
    }
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
/// **Before confinement**, which is where it is called, so the sandbox never has
/// to allow changing a mode.
pub fn prepare(directory: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    if std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory).is_err() {
        return;
    }
    for folder in [Some(directory), directory.parent()].into_iter().flatten() {
        let _ = tighten(folder);
    }
}

/// Takes group and other access off a folder, unless the name is a link.
///
/// **The descriptor is what is changed, and it is checked against the name.** A
/// path checked and then changed by path can be swapped for a link in between,
/// and the change then lands wherever the link points.
fn tighten(folder: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let named = std::fs::symlink_metadata(folder)?;
    let shared = named.mode() & 0o077;
    if !named.is_dir() || shared == 0 {
        return Ok(());
    }
    let handle = std::fs::File::open(folder)?;
    let held = handle.metadata()?;
    if (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Ok(());
    }
    handle.set_permissions(std::fs::Permissions::from_mode(held.mode() & 0o700))
}

/// As [`write`], to a named file, and saying why it did not.
///
/// The seam a test uses, because `$XDG_CACHE_HOME` is process-global.
///
/// **The staging file is created, never opened.** `O_CREAT | O_EXCL` fails on a
/// name that exists, "regardless of the contents of the symbolic link" (POSIX
/// `open`), so a link planted at the staging name is refused rather than written
/// through — and there is no `O_TRUNC` for the sandbox to have to allow. One
/// name per process is enough because the child answers one request at a time
/// ([ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)).
fn write_to(
    file: &Path,
    original: &Path,
    mtime: i64,
    pixels: &[u8],
    size: (u32, u32),
) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let directory = file.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    let staging = file.with_extension(format!("{}.tmp", std::process::id()));
    let handle =
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&staging)?;
    let written = encode(handle, original, mtime, pixels, size)
        .map_err(std::io::Error::from)
        .and_then(|()| std::fs::rename(&staging, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
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

    use super::{place, prepare, to_rgba, uri, write_to};
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

    /// **A link in the cache is followed by neither the write nor the close.** The
    /// staging name is the one this helper has always used, so it is the one
    /// somebody could have planted something at.
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

        let folder = root.join("real");
        std::fs::create_dir_all(&folder).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "unrelated").unwrap();
        let file = folder.join("0123.png");
        let staging = file.with_extension(format!("{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &staging).unwrap();
        assert!(write_to(&file, Path::new("/a.png"), 7, &[255; 4], (1, 1)).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "unrelated");
        assert!(!file.exists(), "a thumbnail was published through somebody else's file");
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
        let messy =
            Path::new(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b"/a/\xff.png") });
        assert_eq!(uri(messy), "file:///a/%FF.png");
    }

    /// The name is the digest of the URI, and the directory is the specification's.
    #[test]
    fn a_thumbnail_lands_where_the_specification_says() {
        // SAFETY: single-threaded test, restored below.
        let saved = std::env::var_os("XDG_CACHE_HOME");
        unsafe { std::env::set_var("XDG_CACHE_HOME", "/tmp/cache-for-a-test") };
        let at = place(Path::new("/home/a/photo.png")).unwrap();
        assert!(at.starts_with("/tmp/cache-for-a-test/thumbnails/normal"), "{at:?}");
        assert_eq!(
            at.file_name().unwrap().to_string_lossy(),
            format!("{}.png", crate::md5::hex(b"file:///home/a/photo.png"))
        );
        match saved {
            // SAFETY: as above.
            Some(had) => unsafe { std::env::set_var("XDG_CACHE_HOME", had) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
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
