//! The document helper: one page of a PDF, through a renderer on `PATH`.
//!
//! **[ADR 0006](../../../docs/adr/0006-out-of-process-decoders.md)** provides for
//! exactly this — "plus external tools such as `ffmpeg` discovered on `PATH` at
//! first use" — and it is the honest answer for PDF specifically. Every pure-Rust
//! PDF *renderer* is a research project; every complete one is a C library, and
//! [06-portability.md](../../../docs/06-portability.md) forbids a C toolchain for
//! anything shipped by default. What is left is to drive a renderer the machine
//! already has, which is `pdftoppm` on any box with poppler and `mutool` on any
//! box with mupdf.
//!
//! **A machine with neither gets `Refusal::NoRenderer` and a sentence saying so.**
//! That is ADR 0006's "a missing helper degrades exactly one preview kind", one
//! level further down: the helper is here, and what is missing is the tool it
//! drives.
//!
//! Text is **not** here. Reading a text file is not decoding — `sk_fs::text`
//! already does it in the host for *Copy the text inside* — so a text preview
//! needs no child, no sandbox and no round trip. Putting it here would be paying
//! for a process to do a `read`.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use sk_preview::wire::{Refusal, Reply, Request};

fn main() {
    if std::env::args().any(|it| it == "--probe") {
        // **The same confinement the served process gets**, or the probe answers
        // a question nobody asked: its first version granted no execute right, so
        // it reported `pages: None` on a machine where `pdfinfo` works perfectly
        // — a diagnostic that lied about the thing it exists to diagnose.
        confine();
        println!("skerry-preview-doc: renderer: {:?}", renderer());
        if let (Some(tool), Some(path)) =
            (renderer(), std::env::args().skip(1).find(|it| it != "--probe"))
        {
            println!("skerry-preview-doc: pages: {:?}", count(tool, Path::new(&path)));
        }
        return;
    }
    confine();
    serve();
}

/// Locks down, and says what is in force.
///
/// **Read-only, everywhere, and nothing writable at all.** This helper writes no
/// cache: a document page is a preview, and ADR 0018 does not cache previews. The
/// renderer it spawns inherits the restriction, which is the point of applying it
/// here rather than around each spawn.
fn confine() {
    // **Execute at the root, and nothing writable at all.**
    //
    // The precise version — execute on the directories in `PATH` and nowhere else
    // — does not work, and the reason is worth the four lines: running a
    // dynamically linked program needs Landlock's execute right on its *loader and
    // every shared library it maps*, not only on the binary. `ld-linux` lives in
    // `/lib64` and poppler's libraries in `/usr/lib`, neither of which is on
    // `PATH`, so the exact grant refused every PDF on a machine with poppler
    // installed — measured, as `Refusal::NoRenderer` from a helper whose `--probe`
    // said the renderer was there.
    //
    // Chasing the library paths would mean tracking one distribution's layout, so
    // the grant is the root. What is actually load-bearing here is the *write*
    // restriction, and that is unchanged: this process cannot write a byte
    // anywhere, which was checked rather than assumed.
    let root = [Path::new("/")];
    let report = sk_preview::sandbox::confine(&root, &root, &[]);
    let _ = writeln!(std::io::stderr(), "skerry-preview-doc: {}", report.line());
}

fn serve() {
    let mut input = BufReader::new(std::io::stdin());
    let mut output = BufWriter::new(std::io::stdout());
    while let Ok(request) = Request::read_from(&mut input) {
        if answer(&request).write_to(&mut output).is_err() {
            return;
        }
    }
}

fn answer(request: &Request) -> Reply {
    let path = PathBuf::from(std::ffi::OsString::from(unsafe {
        // SAFETY: the bytes came from `OsStr::as_encoded_bytes` in the host — the
        // same encoding, no round trip through text. `sk_preview::wire` documents
        // it as the contract of the `path` field.
        std::ffi::OsStr::from_encoded_bytes_unchecked(&request.path)
    }));
    if !looks_like_pdf(&path) {
        return Reply::refused(Refusal::NotItsKind);
    }
    let Some(tool) = renderer() else {
        return Reply::refused(Refusal::NoRenderer);
    };
    // **The count first, and its failure is not the request's.** A machine with
    // the renderer but not the tool that counts pages still previews the document
    // — ADR 0019 — so zero travels back as "unknown" and the panel shows a page
    // number without a total rather than inventing one.
    let pages = count(tool, &path).unwrap_or(0);
    let page = request.page.max(1);
    // A page past the end is not a broken document: it is a `↓` at the last page,
    // which the host answers by staying where it is. Refusing here rather than
    // spawning a renderer that will fail costs nothing and says something precise.
    if pages > 0 && page > pages {
        return Reply {
            refusal: Refusal::NoSuchPage,
            size: (0, 0),
            pages,
            delay_ms: 0,
            pixels: Vec::new(),
        };
    }
    match render(tool, &path, request.within, page) {
        // **No delay, because a page is not timed.** A document is shown until
        // somebody turns it; the field is a GIF's frame and answers zero here.
        Ok((pixels, size)) => Reply { refusal: Refusal::None, size, pages, delay_ms: 0, pixels },
        Err(refusal) => Reply { refusal, size: (0, 0), pages, delay_ms: 0, pixels: Vec::new() },
    }
}

/// Whether the file starts with a PDF header.
///
/// **The bytes, not the extension.** The specification allows the header a little
/// junk in front of it, which real files have, so this looks in the first
/// kilobyte — the same rule every other reader uses.
fn looks_like_pdf(path: &Path) -> bool {
    let Ok(head) = std::fs::read(path) else { return false };
    head.iter()
        .take(1024)
        .collect::<Vec<_>>()
        .windows(5)
        .any(|window| window.iter().map(|byte| **byte).collect::<Vec<u8>>() == b"%PDF-")
}

/// Which renderers this build knows how to drive, in the order it prefers them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    /// poppler's, which is on most desktops and writes PNG to `stdout`.
    PdfToPpm,
    /// mupdf's, which is on the rest.
    MuTool,
}

/// The first renderer on `PATH`, looked for once per request.
///
/// Once per request rather than once per process, and that is deliberate: a child
/// lives for the session, and a machine that gains poppler while the window is
/// open should stop saying it has no renderer. The cost is two `access` calls on a
/// path that is about to spawn a process anyway.
fn renderer() -> Option<Tool> {
    [(Tool::PdfToPpm, "pdftoppm"), (Tool::MuTool, "mutool")]
        .into_iter()
        .find(|(_, program)| on_path(program))
        .map(|(tool, _)| tool)
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
}

/// How many pages the document has, if the machine can say.
///
/// **A second program, and a cheap one**: `pdfinfo` costs 0.01 s against the
/// 0.09–0.30 s the render itself takes, measured in
/// [ADR 0019](../../../docs/adr/0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md).
/// It ships in the same package as `pdftoppm`, so in practice it is there
/// whenever the renderer is — and "in practice" is why the answer is an `Option`
/// rather than a number.
fn count(tool: Tool, path: &Path) -> Option<u32> {
    let mut command = Command::new(match tool {
        Tool::PdfToPpm => "pdfinfo",
        Tool::MuTool => "mutool",
    });
    if tool == Tool::MuTool {
        command.arg("info");
    }
    let done = command.arg(path).output().ok()?;
    if !done.status.success() {
        return None;
    }
    // Both print a `Pages: N` line, poppler once and mupdf once per document.
    String::from_utf8_lossy(&done.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Pages:"))
        .and_then(|rest| rest.trim().parse().ok())
        .filter(|pages: &u32| *pages > 0)
}

/// Renders one page and decodes what came back.
///
/// **Both tools are asked for PNG on `stdout`**, so what is between them and this
/// process is a pipe rather than a temporary file — which matters more than it
/// looks: a temporary file would be a write, and this helper is confined with
/// nothing writable.
///
/// **A spawn per page, which ADR 0019 measured before choosing it**: reaching page
/// 400 of a 1200-page manual costs what reaching page 1 costs, because PDF has a
/// cross-reference table and poppler uses it.
fn render(
    tool: Tool,
    path: &Path,
    within: (u32, u32),
    page: u32,
) -> Result<(Vec<u8>, (u32, u32)), Refusal> {
    let long = within.0.max(within.1).clamp(64, 2048).to_string();
    let page = page.to_string();
    let mut command = Command::new(match tool {
        Tool::PdfToPpm => "pdftoppm",
        Tool::MuTool => "mutool",
    });
    match tool {
        // `-r` is a resolution and `-scale-to` is a long edge, which is what a box
        // wants: a page is rendered to fit rather than at a guessed DPI.
        Tool::PdfToPpm => {
            command.args(["-png", "-f", &page, "-l", &page, "-scale-to", &long]).arg(path);
        }
        Tool::MuTool => {
            command.args(["draw", "-F", "png", "-o", "-", "-w", &long]).arg(path).arg(&page);
        }
    }
    let done = command.output().map_err(|_| Refusal::NoRenderer)?;
    if !done.status.success() || done.stdout.is_empty() {
        return Err(Refusal::Undecodable);
    }
    decode_png(&done.stdout)
}

/// The renderer's PNG, as straight `RGBA8`.
fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, (u32, u32)), Refusal> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reading = decoder.read_info().map_err(|_| Refusal::Undecodable)?;
    let (width, height) = (reading.info().width, reading.info().height);
    let mut raw = vec![0u8; reading.output_buffer_size().ok_or(Refusal::TooLarge)?];
    let frame = reading.next_frame(&mut raw).map_err(|_| Refusal::Undecodable)?;
    let raw = &raw[..frame.buffer_size()];
    let widen = |stride: usize, each: fn(&[u8]) -> [u8; 4]| {
        if !raw.len().is_multiple_of(stride) {
            return Err(Refusal::Undecodable);
        }
        Ok(raw.chunks_exact(stride).flat_map(each).collect::<Vec<u8>>())
    };
    let pixels = match frame.color_type {
        png::ColorType::Rgba => raw.to_vec(),
        png::ColorType::Rgb => widen(3, |it| [it[0], it[1], it[2], 255])?,
        png::ColorType::Grayscale => widen(1, |it| [it[0], it[0], it[0], 255])?,
        png::ColorType::GrayscaleAlpha => widen(2, |it| [it[0], it[0], it[0], it[1]])?,
        png::ColorType::Indexed => return Err(Refusal::Undecodable),
    };
    Ok((pixels, (width, height)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{decode_png, looks_like_pdf, on_path};
    use sk_preview::Refusal;
    use std::io::Write;

    #[test]
    fn a_pdf_is_recognised_by_its_bytes_and_not_its_name() {
        let dir = std::env::temp_dir().join(format!("skerry-doc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Named `.txt`, and a PDF.
        let disguised = dir.join("actually.txt");
        std::fs::File::create(&disguised).unwrap().write_all(b"%PDF-1.7\n...").unwrap();
        assert!(looks_like_pdf(&disguised));

        // Named `.pdf`, and not one.
        let pretender = dir.join("not-really.pdf");
        std::fs::File::create(&pretender).unwrap().write_all(b"just text").unwrap();
        assert!(!looks_like_pdf(&pretender));

        // The header may have junk in front of it, which real files have — and
        // not a kilobyte of it.
        let late = dir.join("late.pdf");
        let mut bytes = vec![b' '; 200];
        bytes.extend_from_slice(b"%PDF-1.4");
        std::fs::File::create(&late).unwrap().write_all(&bytes).unwrap();
        assert!(looks_like_pdf(&late));

        // A file that is not there is not a PDF, and is not a panic.
        assert!(!looks_like_pdf(&dir.join("absent.pdf")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renderer_is_looked_for_on_the_path_that_is_set() {
        assert!(on_path("sh") || on_path("ls"), "no ordinary tool is on PATH");
        assert!(!on_path("a-program-nobody-has-installed-here"));
    }

    #[test]
    fn something_that_is_not_a_png_is_undecodable_rather_than_a_panic() {
        assert_eq!(decode_png(b"").err(), Some(Refusal::Undecodable));
        assert_eq!(decode_png(b"\x89PNG\r\n\x1a\nand nothing").err(), Some(Refusal::Undecodable));
    }
}
