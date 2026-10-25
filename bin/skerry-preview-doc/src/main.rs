// SPDX-License-Identifier: Apache-2.0
//! The document helper: one page of a PDF and its words, through the tools on
//! `PATH`.
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

mod words;

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use sk_preview::wire::{Refusal, Reply, Request};

fn main() {
    if std::env::args().any(|arg| arg == "--version") {
        println!("skerry-preview-doc {} ({})", env!("CARGO_PKG_VERSION"), env!("SKERRY_COMMIT"));
        return;
    }
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
            // **And whether its words can be found**, which is the other half of
            // what a page answer carries and the one a person will ask about:
            // selection is absent on a machine without `pdftotext` and on a
            // document that is a scan, and those look the same from the window.
            match words::of(Path::new(&path), 1, rotation(tool, Path::new(&path), 1)) {
                Some(layer) => println!(
                    "skerry-preview-doc: words: {} on a {:.0}×{:.0} pt page, first {:?}",
                    layer.words.len(),
                    layer.page.0,
                    layer.page.1,
                    layer.words.first().map(|word| word.text.as_str()).unwrap_or_default()
                ),
                None => println!("skerry-preview-doc: words: none — no text layer on this page"),
            }
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
    let root = [Path::new("/")];
    let tools = sk_preview::sandbox::executables(&["pdftoppm", "pdftotext", "pdfinfo", "mutool"]);
    let executable: Vec<_> = tools.iter().map(std::path::PathBuf::as_path).collect();
    let report =
        sk_preview::sandbox::confine(sk_preview::sandbox::Role::Pages, &root, &executable, &[]);
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
            words: Vec::new(),
        };
    }
    match render(tool, &path, request.within, page) {
        // **No delay, because a page is not timed.** A document is shown until
        // somebody turns it; the field is a GIF's frame and answers zero here.
        //
        // **And the page's words beside its pixels** — ADR 0024 — so the panel
        // can select a line out of it the way it already can out of a text file.
        // Its absence is a page with no selection and not a failure: a scan has
        // no words, and a machine with `mutool` alone cannot say where any are.
        Ok((pixels, size)) => Reply {
            refusal: Refusal::None,
            size,
            pages,
            delay_ms: 0,
            pixels,
            words: words::of(&path, page, rotation(tool, &path, page))
                .map(|layer| layer.pack())
                .unwrap_or_default(),
        },
        Err(refusal) => Reply {
            refusal,
            size: (0, 0),
            pages,
            delay_ms: 0,
            pixels: Vec::new(),
            words: Vec::new(),
        },
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
/// What one page is: how big it is in points, and how far it is turned.
///
/// **One `pdfinfo` call for both**, because both come off the same three lines of
/// its output and a second spawn per render is a tenth of what the render costs.
#[derive(Clone, Copy, Default)]
struct Facts {
    /// The `CropBox`, in points, **unrotated** — which is what `pdfinfo` prints as
    /// a page's size, and what every renderer here is told to draw.
    size: Option<(f32, f32)>,
    /// Degrees clockwise, 0, 90, 180 or 270.
    rotation: u32,
}

impl Facts {
    /// The page's size as it will be *drawn*, which is the turned one.
    fn drawn(self) -> Option<(f32, f32)> {
        let (width, height) = self.size?;
        Some(if self.rotation % 180 == 90 { (height, width) } else { (width, height) })
    }
}

/// What `pdfinfo` says about one page.
fn facts(path: &Path, page: u32) -> Facts {
    let page = page.max(1).to_string();
    let Ok(done) = Command::new("pdfinfo").args(["-f", &page, "-l", &page]).arg(path).output()
    else {
        return Facts::default();
    };
    let text = String::from_utf8_lossy(&done.stdout);
    let rotation = text
        .lines()
        .find_map(|line| line.split_once("rot:").map(|(_, rest)| rest.trim().to_owned()))
        .and_then(|rest| rest.parse::<i32>().ok())
        .map_or(0, |angle| angle.rem_euclid(360).unsigned_abs());
    // `Page    1 size:  420 x 595 pts (A5)` — the numbers between the colon and
    // the unit, which is the whole of the parse.
    let size = text.lines().find_map(|line| {
        let rest = line.split_once("size:")?.1.trim();
        let (width, rest) = rest.split_once(" x ")?;
        let height = rest.split_whitespace().next()?;
        Some((width.trim().parse().ok()?, height.parse().ok()?))
    });
    Facts { size, rotation }
}

/// How far the page is turned, for the text layer.
///
/// **A `/Rotate 90` page is where poppler's own two answers disagree**: its text
/// layer reports the unrotated page box as the page's size and the boxes in the
/// rotated space, and its renderer draws the rotated image. Zero where nothing
/// can say, which is the same as no rotation and is what every page that is not
/// turned reports anyway.
fn rotation(tool: Tool, path: &Path, page: u32) -> u32 {
    if tool != Tool::PdfToPpm {
        // mupdf's structured text is already in the rotated space, so the angle
        // is not a question that arm has to ask.
        return 0;
    }
    facts(path, page).rotation
}

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
    // **The box, not its long edge.** `-scale-to` sets the *page's* long edge, so
    // a portrait page asked for a landscape box was rendered to the box's width —
    // 709×1004 for a panel wanting 417×590, which is 2.9× the pixels, downscaled
    // again on the way to the screen. Worse, the ceiling below then bit at a size
    // the panel had not reached: the render came back smaller than the box, and a
    // page drawn at one to one inside a panel it does not fill is the "the
    // placeholder is bigger than the picture" this fixes.
    //
    // Four thousand a side is the wire's own ceiling, and no lower: a preview
    // whose long edge is capped below the panel's is a preview that gets *worse*
    // on a better screen.
    //
    // **And one pixel is the floor, not sixty-four.** The host refuses a page
    // larger than the box it asked for, and a floor above the box is exactly
    // that: measured, a draft box 44 pixels wide came back 64 wide from mupdf
    // and 65 from poppler.
    let within = sk_preview::wire::bounded(within);
    let page_number = page.to_string();
    let mut command = Command::new(match tool {
        Tool::PdfToPpm => "pdftoppm",
        Tool::MuTool => "mutool",
    });
    match tool {
        Tool::PdfToPpm => {
            // **Poppler distorts when given both axes** — `-scale-to-x 1000
            // -scale-to-y 200` answers 1000×200 for an A5 page — so the fit is
            // computed here from what `pdfinfo` says the page is, and one long
            // edge is passed.
            //
            // **Then cropped to the box**, because poppler rounds the edge it
            // derives up: measured over 120 boxes on three documents, 21 came back
            // one pixel wider than the box with the long edge exactly right.
            // `-W`/`-H` take that pixel, which is margin, off a page the host
            // would otherwise refuse.
            //
            // **And drawn from the `CropBox`**, which is the box `pdfinfo` sized
            // the fit from and the one mupdf draws. poppler's default is the
            // `MediaBox`: a 612×792 page cropped to 612×400, in a 1000×600 box,
            // was drawn whole at the crop's scale and came back 710×600 — the
            // top of a page that was not the one asked for.
            let long = fitted_long_edge(path, page, within);
            let (width, height) = (within.0.to_string(), within.1.to_string());
            command
                .args(["-png", "-cropbox", "-f", &page_number, "-l", &page_number])
                .args(["-scale-to", &long, "-W", &width, "-H", &height])
                .arg(path);
        }
        Tool::MuTool => {
            // mupdf fits inside both when given both — measured, 142×200 for the
            // same box poppler stretched — so it is handed the box itself.
            let (width, height) = (within.0.to_string(), within.1.to_string());
            command
                .args(["draw", "-F", "png", "-o", "-", "-w", &width, "-h", &height])
                .arg(path)
                .arg(&page_number);
        }
    }
    let done = command.output().map_err(|_| Refusal::NoRenderer)?;
    if !done.status.success() || done.stdout.is_empty() {
        return Err(Refusal::Undecodable);
    }
    decode_png(&done.stdout, within)
}

/// The long edge to render at, so the page lands inside `within`.
///
/// The page's own size decides which axis binds. Where `pdfinfo` says nothing —
/// an encrypted document, a page it will not describe — the box's short edge is
/// the answer: a page that long fits the box whichever way it is turned, where
/// the box's long edge, which this used to answer, is a page the crop cuts.
fn fitted_long_edge(path: &Path, page: u32, within: (u32, u32)) -> String {
    let fallback = within.0.min(within.1);
    let Some((width, height)) = facts(path, page).drawn() else { return fallback.to_string() };
    if width <= 0.0 || height <= 0.0 {
        return fallback.to_string();
    }
    let scale = (within.0 as f32 / width).min(within.1 as f32 / height);
    let long = (width.max(height) * scale).round();
    // Clamped to the same ceiling the box is, because a page thirty times as tall
    // as it is wide would otherwise ask for a render the wire cannot carry.
    (long.clamp(1.0, sk_preview::wire::MOST_SIDE as f32) as u32).to_string()
}

/// The renderer's PNG, as straight `RGBA8`, if it fits inside `within`.
///
/// **The size is the renderer's claim, and it is checked before a pixel is
/// decoded.** The renderer is a program off `PATH` reading a file out of a
/// download directory: a PNG of 65,536 × 256 is exactly the wire's byte ceiling,
/// and relaying it reached a `create_texture` past the device's limit, which
/// `wgpu` answers with a panic. Both renderers are asked for a page inside
/// `within`, so a page past it is not a page this asked for.
fn decode_png(bytes: &[u8], within: (u32, u32)) -> Result<(Vec<u8>, (u32, u32)), Refusal> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reading = decoder.read_info().map_err(|_| Refusal::Undecodable)?;
    let (width, height) = (reading.info().width, reading.info().height);
    if width > within.0 || height > within.1 {
        return Err(Refusal::TooLarge);
    }
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

    use super::{Tool, decode_png, fitted_long_edge, looks_like_pdf, on_path, render};
    use sk_preview::Refusal;
    use std::io::Write;
    use std::path::Path;

    /// A one-page PDF whose page dictionary carries `boxes` and which draws
    /// `content` in Helvetica, written with a correct cross-reference table so
    /// neither renderer has to repair it.
    fn one_page_pdf(path: &Path, boxes: &str, content: &str) {
        let page = format!(
            "<< /Type /Page /Parent 2 0 R {boxes} /Resources << /Font << /F1 5 0 R >> >> \
             /Contents 4 0 R >>"
        );
        let stream = format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len() + 1);
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            page.as_str(),
            stream.as_str(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj {object} endobj\n", index + 1).as_bytes());
        }
        let xref = bytes.len();
        bytes.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        let trailer = format!("trailer << /Size {} /Root 1 0 R >>\n", objects.len() + 1);
        bytes.extend_from_slice(trailer.as_bytes());
        bytes.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    /// **An honest render fits the box it was asked into, to the pixel**, because
    /// the host now refuses one that does not. Each box is one that came back too
    /// large before: poppler's derived edge a pixel over, and a box under the old
    /// sixty-four-pixel floor raised to it.
    #[test]
    fn a_rendered_page_lands_inside_its_box() {
        let dir = std::env::temp_dir().join(format!("skerry-doc-fit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("a4.pdf");
        one_page_pdf(&pdf, "/MediaBox [0 0 595 842]", "");
        for (tool, program) in [(Tool::PdfToPpm, "pdftoppm"), (Tool::MuTool, "mutool")] {
            if !on_path(program) {
                eprintln!("skerry-preview-doc: no {program} on this machine — skipped");
                continue;
            }
            for within in [(828, 1353), (118, 168), (141, 1178), (44, 1013), (1000, 30)] {
                let (_, size) = render(tool, &pdf, within, 1).unwrap();
                assert!(
                    size.0 <= within.0 && size.1 <= within.1,
                    "{program}: {size:?} in {within:?}"
                );
                // And it still fills the edge that binds, or the fix is a page
                // that got smaller to fit.
                let filled = (size.0 as f32 / within.0 as f32).max(size.1 as f32 / within.1 as f32);
                assert!(filled > 0.97, "{program}: {size:?} leaves {within:?} unfilled");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A cropped page is drawn as cropped, whole, with its words where it is
    /// drawn.** `pdfinfo` sizes a page by its `CropBox` and poppler draws the
    /// `MediaBox` unless told otherwise, so the fit was computed for one box and
    /// drawn for another — and the crop to the panel then cut it.
    #[test]
    fn a_cropped_page_is_drawn_whole_from_its_crop_box() {
        let dir = std::env::temp_dir().join(format!("skerry-doc-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("cropped.pdf");
        // A black quarter in the crop's bottom left, a word inside the crop and
        // one above it, on the part of the `MediaBox` the crop leaves out.
        let content = "0 0 0 rg 0 0 306 200 re f\n\
                       BT /F1 24 Tf 400 300 Td (Inside) Tj ET\n\
                       BT /F1 24 Tf 400 700 Td (Outside) Tj ET";
        one_page_pdf(&pdf, "/MediaBox [0 0 612 792] /CropBox [0 0 612 400]", content);
        let dark = |pixels: &[u8], width: u32, at: (u32, u32)| {
            pixels[((at.1 * width + at.0) * 4) as usize] < 64
        };
        for (tool, program) in [(Tool::PdfToPpm, "pdftoppm"), (Tool::MuTool, "mutool")] {
            if !on_path(program) {
                eprintln!("skerry-preview-doc: no {program} on this machine — skipped");
                continue;
            }
            let (pixels, size) = render(tool, &pdf, (1000, 600), 1).unwrap();
            assert_eq!(size, (918, 600), "{program} drew some other box");
            assert!(dark(&pixels, size.0, (100, 500)), "{program}: the crop's corner is missing");
            assert!(!dark(&pixels, size.0, (100, 100)), "{program}: the corner is not where it is");
        }
        if on_path("pdftotext") {
            let layer = super::words::of(&pdf, 1, 0).unwrap();
            assert_eq!(layer.page, (612.0, 400.0), "the words are measured on another page");
            let said: Vec<&str> = layer.words.iter().map(|word| word.text.as_str()).collect();
            assert_eq!(said, ["Inside"], "a word outside the crop was offered for selection");
            assert!(layer.words[0].at.1 < 400.0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where nothing says how big the page is, the box's short edge is the long
    /// edge asked for, because that page fits the box whichever way it is turned.
    #[test]
    fn a_page_of_unknown_size_is_fitted_to_the_short_edge() {
        let nowhere = Path::new("/nonexistent/skerry/page.pdf");
        assert_eq!(fitted_long_edge(nowhere, 1, (1000, 500)), "500");
        assert_eq!(fitted_long_edge(nowhere, 1, (300, 900)), "300");
    }

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
        let within = (800, 600);
        assert_eq!(decode_png(b"", within).err(), Some(Refusal::Undecodable));
        assert_eq!(
            decode_png(b"\x89PNG\r\n\x1a\nand nothing", within).err(),
            Some(Refusal::Undecodable)
        );
    }

    /// A one-bit grey PNG of `size`, which is how a picture this large stays a
    /// fixture of a few kilobytes.
    fn blank_png(size: (u32, u32)) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, size.0, size.1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        let mut writer = encoder.write_header().unwrap();
        let row = size.0.div_ceil(8) as usize;
        writer.write_image_data(&vec![0u8; row * size.1 as usize]).unwrap();
        writer.finish().unwrap();
        bytes
    }

    /// **A renderer's page is no larger than the box it was asked for.** The
    /// 65,536 × 256 page is what a replaced `pdftoppm` would answer with: inside
    /// the wire's byte ceiling, and sixteen times the side Vulkan guarantees.
    #[test]
    fn a_page_past_the_box_it_was_asked_for_is_refused() {
        let hostile = blank_png((65_536, 256));
        assert_eq!(decode_png(&hostile, (4096, 4096)).err(), Some(Refusal::TooLarge));

        // The box itself is fine, and one pixel past it on either side is not.
        let page = blank_png((3, 2));
        assert_eq!(decode_png(&page, (3, 2)).map(|(_, size)| size), Ok((3, 2)));
        assert_eq!(decode_png(&page, (2, 2)).err(), Some(Refusal::TooLarge));
        assert_eq!(decode_png(&page, (3, 1)).err(), Some(Refusal::TooLarge));
    }
}
