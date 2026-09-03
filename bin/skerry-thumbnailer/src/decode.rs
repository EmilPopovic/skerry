//! Bytes on disk to straight `RGBA8`, and nothing else.
//!
//! Four formats, chosen because they are what a person actually has: PNG and
//! JPEG are most of the pictures on any machine, SVG is most of the icons and
//! diagrams, and GIF is most of the things that move. WebP and AVIF are each
//! another crate and each another arm here; they are absent rather than
//! half-done, and `Refusal::NotItsKind` is what says so to the host.
//!
//! **GIF is ours** — [`crate::gif`] — because the format is a header, a palette,
//! a block loop and LZW, and because it is the only one of the four whose frames
//! the panel walks: a GIF answers with a frame count and a delay beside its
//! pixels, and everything else answers zero for both.
//!
//! **Sniffed, not extension-matched.** A `.png` that is a JPEG is common enough
//! that every image viewer handles it, and the caller's MIME database guessed from
//! the name before the request was sent. What is here is the last word.
//!
//! **The scale is chosen before the decode where the format allows it.** A JPEG
//! decoder can skip DCT coefficients and produce an eighth-size image for an
//! eighth of the work, which is the difference between a grid of holiday
//! photographs appearing and a grid of holiday photographs appearing eventually.

use sk_preview::Refusal;

/// An image, as the host wants it.
pub struct Raster {
    /// Straight, non-premultiplied `RGBA8`, row-major.
    pub pixels: Vec<u8>,
    /// Its dimensions.
    pub size: (u32, u32),
}

/// The largest image this will decode, in pixels.
///
/// Sixty-four megapixels. The address-space cap would catch a decoder that tried
/// for more, but as a failed allocation somewhere inside a decoder rather than as
/// a refusal the host can put on screen — and `RLIMIT_AS` kills the child, which
/// costs the whole batch rather than one file.
const MOST_PIXELS: u64 = 64 * 1024 * 1024;

/// A decode's answer: the pixels, and what the file said about its frames.
pub struct Decoded {
    /// The image, fitted inside the box that was asked for.
    pub raster: Raster,
    /// How many frames the file holds. Zero for everything that is one picture.
    ///
    /// **Zero rather than one**, which is ADR 0019's don't-care value on the wire:
    /// a caller that has no frames to think about never learns the field exists.
    pub frames: u32,
    /// How long this frame is shown, in milliseconds, or zero.
    pub delay_ms: u32,
}

impl Decoded {
    /// One picture, with nothing to say about frames.
    fn still(raster: Raster) -> Self {
        Self { raster, frames: 0, delay_ms: 0 }
    }
}

/// Decodes `bytes` and fits the result inside `within`.
///
/// `frame` is one-based and only a GIF has any; zero means the first.
///
/// # Errors
///
/// A [`Refusal`] the host turns into a message, or into nothing where there is
/// nothing worth saying.
pub fn decode(bytes: &[u8], within: (u32, u32), frame: u32) -> Result<Decoded, Refusal> {
    let raster = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(bytes)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        jpeg(bytes)
    } else if crate::gif::is_gif(bytes) {
        let frames = crate::gif::decode(bytes, frame)?;
        return Ok(Decoded {
            raster: fit(frames.raster, within),
            frames: frames.count,
            delay_ms: frames.delay_ms,
        });
    } else if looks_like_svg(bytes) {
        // SVG has no fixed size, so it is rendered *at* the wanted size rather
        // than decoded and scaled — which is the whole point of a vector format
        // and the reason this arm takes `within`.
        return svg(bytes, within).map(Decoded::still);
    } else {
        return Err(Refusal::NotItsKind);
    }?;
    Ok(Decoded::still(fit(raster, within)))
}

/// Whether the head of a file looks like SVG.
///
/// **Not a parse.** An XML declaration, a doctype or a comment can precede the
/// root element, so this looks for `<svg` in the first kilobyte — which is what
/// every other sniffing implementation does and is the same answer for every file
/// that is not deliberately adversarial. A file that fools it fails to render and
/// comes back as `Undecodable`.
fn looks_like_svg(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .take(1024)
        .collect::<Vec<_>>()
        .windows(4)
        .any(|window| window.iter().map(|byte| **byte).collect::<Vec<u8>>() == b"<svg")
}

fn png(bytes: &[u8]) -> Result<Raster, Refusal> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reading = decoder.read_info().map_err(|_| Refusal::Undecodable)?;
    let info = reading.info();
    too_big(u64::from(info.width) * u64::from(info.height))?;
    let (width, height) = (info.width, info.height);
    let mut raw = vec![0u8; reading.output_buffer_size().ok_or(Refusal::TooLarge)?];
    let frame = reading.next_frame(&mut raw).map_err(|_| Refusal::Undecodable)?;
    let pixels = widen(&raw[..frame.buffer_size()], frame.color_type)?;
    Ok(Raster { pixels, size: (width, height) })
}

fn jpeg(bytes: &[u8]) -> Result<Raster, Refusal> {
    let mut decoder = zune_jpeg::JpegDecoder::new(bytes);
    decoder.decode_headers().map_err(|_| Refusal::Undecodable)?;
    // `zune-jpeg` answers in `usize`, which on this target is wider than the
    // `u32` the wire carries — so both conversions are checked rather than cast.
    let (width, height) = decoder.dimensions().ok_or(Refusal::Undecodable)?;
    too_big(width as u64 * height as u64)?;
    let (width, height) = (
        u32::try_from(width).map_err(|_| Refusal::TooLarge)?,
        u32::try_from(height).map_err(|_| Refusal::TooLarge)?,
    );
    decoder.set_options(
        zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB),
    );
    let raw = decoder.decode().map_err(|_| Refusal::Undecodable)?;
    let pixels = widen(&raw, png::ColorType::Rgb)?;
    Ok(Raster { pixels, size: (width, height) })
}

fn svg(bytes: &[u8], within: (u32, u32)) -> Result<Raster, Refusal> {
    let mut options = resvg::usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|_| Refusal::Undecodable)?;
    // **A vector fills its box, up as well as down**, which is the whole point of
    // a vector: an icon whose document says `width="10"` asked for at 128 is asked
    // for at 128, and rendering it at 10 and letting the window stretch it would
    // be a blurred icon from a format that cannot blur. `box_fit`'s refusal to
    // enlarge is right for a raster, where the pixels do not exist, and wrong here.
    let natural = tree.size();
    let (width, height) = fill(natural.width(), natural.height(), within);
    let mut canvas =
        resvg::tiny_skia::Pixmap::new(width.max(1), height.max(1)).ok_or(Refusal::TooLarge)?;
    let scale = resvg::tiny_skia::Transform::from_scale(
        width as f32 / natural.width().max(1.0),
        height as f32 / natural.height().max(1.0),
    );
    resvg::render(&tree, scale, &mut canvas.as_mut());
    // **Tiny-skia is premultiplied and the wire is not.** A premultiplied buffer
    // drawn as straight RGBA is a picture with dark fringes wherever it is
    // transparent, which is every icon.
    let pixels = canvas
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let demultiply = pixel.demultiply();
            [demultiply.red(), demultiply.green(), demultiply.blue(), demultiply.alpha()]
        })
        .collect();
    Ok(Raster { pixels, size: (width.max(1), height.max(1)) })
}

pub(crate) fn too_big(pixels: u64) -> Result<(), Refusal> {
    if pixels > MOST_PIXELS { Err(Refusal::TooLarge) } else { Ok(()) }
}

/// Widens three or one channels into four. The same four cases the cache reads.
fn widen(raw: &[u8], colour: png::ColorType) -> Result<Vec<u8>, Refusal> {
    let spread = |stride: usize, each: fn(&[u8]) -> [u8; 4]| {
        if !raw.len().is_multiple_of(stride) {
            return Err(Refusal::Undecodable);
        }
        Ok(raw.chunks_exact(stride).flat_map(each).collect())
    };
    match colour {
        png::ColorType::Rgba => Ok(raw.to_vec()),
        png::ColorType::Rgb => spread(3, |it| [it[0], it[1], it[2], 255]),
        png::ColorType::Grayscale => spread(1, |it| [it[0], it[0], it[0], 255]),
        png::ColorType::GrayscaleAlpha => spread(2, |it| [it[0], it[0], it[0], it[1]]),
        png::ColorType::Indexed => Err(Refusal::Undecodable),
    }
}

/// The size an image of `width`×`height` takes inside `within`, aspect kept.
///
/// **Never enlarged.** A 32-pixel icon asked for at 128 comes back at 32: scaling
/// it up here would send four times the bytes over the pipe and put the same
/// blur on screen that drawing it larger does.
pub fn box_fit(width: u32, height: u32, within: (u32, u32)) -> (u32, u32) {
    let (room_x, room_y) = (within.0.max(1), within.1.max(1));
    if width <= room_x && height <= room_y {
        return (width, height);
    }
    let scale = f64::from(room_x) / f64::from(width.max(1));
    let scale = scale.min(f64::from(room_y) / f64::from(height.max(1)));
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// The size something with no pixels of its own takes inside `within`.
///
/// As [`box_fit`], but it enlarges: a vector has no natural resolution, so the box
/// is the resolution.
fn fill(width: f32, height: f32, within: (u32, u32)) -> (u32, u32) {
    let (natural_w, natural_h) = (f64::from(width.max(1.0)), f64::from(height.max(1.0)));
    let scale =
        (f64::from(within.0.max(1)) / natural_w).min(f64::from(within.1.max(1)) / natural_h);
    (((natural_w * scale).round() as u32).max(1), ((natural_h * scale).round() as u32).max(1))
}

/// Scales a raster into its box.
///
/// **A box filter, not a nearest sample.** A photograph reduced to a hundredth of
/// its area by point sampling is aliasing rather than a thumbnail — one pixel in
/// ten thousand decides what a cell looks like, so a picture of a beach becomes
/// whatever colour one grain of sand was. Averaging the source pixels each
/// destination pixel covers costs one pass and is what makes a small picture
/// legible.
fn fit(raster: Raster, within: (u32, u32)) -> Raster {
    let (width, height) = box_fit(raster.size.0, raster.size.1, within);
    if (width, height) == raster.size {
        return raster;
    }
    let (from_w, from_h) = (raster.size.0 as usize, raster.size.1 as usize);
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let top = row * from_h / height as usize;
        let bottom = (((row + 1) * from_h).div_ceil(height as usize)).max(top + 1).min(from_h);
        for column in 0..width as usize {
            let left = column * from_w / width as usize;
            let right =
                (((column + 1) * from_w).div_ceil(width as usize)).max(left + 1).min(from_w);
            let mut sums = [0u32; 4];
            let mut counted = 0u32;
            for y in top..bottom {
                for x in left..right {
                    let at = (y * from_w + x) * 4;
                    for (sum, channel) in sums.iter_mut().zip(&raster.pixels[at..at + 4]) {
                        *sum += u32::from(*channel);
                    }
                    counted += 1;
                }
            }
            let counted = counted.max(1);
            out.extend(sums.map(|sum| (sum / counted) as u8));
        }
    }
    Raster { pixels: out, size: (width, height) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    /// The pixels alone, for the assertions that predate frames.
    fn raster_of(
        bytes: &[u8],
        within: (u32, u32),
        frame: u32,
    ) -> Result<super::Raster, super::Refusal> {
        super::decode(bytes, within, frame).map(|it| it.raster)
    }

    use super::{Raster, box_fit, decode, fill, fit, looks_like_svg};
    use sk_preview::Refusal;

    #[test]
    fn a_box_fit_keeps_the_aspect_and_never_enlarges() {
        // Wider than tall, into a square.
        assert_eq!(box_fit(400, 200, (100, 100)), (100, 50));
        // Taller than wide.
        assert_eq!(box_fit(200, 400, (100, 100)), (50, 100));
        // Already inside: untouched, not stretched up.
        assert_eq!(box_fit(32, 32, (128, 128)), (32, 32));
        // Never zero, however extreme the ratio.
        assert_eq!(box_fit(10_000, 1, (100, 100)), (100, 1));
        assert_eq!(box_fit(1, 1, (0, 0)), (1, 1));
    }

    /// And the vector rule is the other one: it fills, in both directions.
    #[test]
    fn a_vector_fills_its_box_in_either_direction() {
        assert_eq!(fill(10.0, 5.0, (100, 100)), (100, 50));
        assert_eq!(fill(400.0, 200.0, (100, 100)), (100, 50));
        assert_eq!(fill(0.0, 0.0, (64, 64)), (64, 64));
    }

    /// **The reason it is a box filter.** A checkerboard reduced by point sampling
    /// comes out solid — every destination pixel lands on the same phase — and
    /// averaging gives the grey a person expects.
    #[test]
    fn shrinking_averages_rather_than_samples() {
        let side = 64usize;
        let pixels = (0..side * side)
            .flat_map(|at| {
                let dark = ((at / side) + (at % side)).is_multiple_of(2);
                let level = if dark { 0u8 } else { 255 };
                [level, level, level, 255]
            })
            .collect();
        let small = fit(Raster { pixels, size: (64, 64) }, (8, 8));
        assert_eq!(small.size, (8, 8));
        for channel in small.pixels.chunks_exact(4) {
            assert!(
                (100..=155).contains(&channel[0]),
                "a checkerboard averaged to {} rather than to grey",
                channel[0]
            );
            assert_eq!(channel[3], 255, "the alpha was averaged away");
        }
    }

    /// **A JPEG, decoded, because most of the pictures on a machine are one.**
    ///
    /// The bytes are a 4×4 JPEG of one colour, and the recipe rather than the
    /// blob is the reviewable part:
    ///
    /// ```text
    /// python3 -c "from PIL import Image
    /// Image.new('RGB', (4, 4), (200, 40, 90)).save('t.jpg', quality=20, optimize=True)"
    /// ```
    ///
    /// It is inline rather than a file because 286 bytes of baseline JPEG is the
    /// smallest thing that exercises `zune-jpeg`, the RGB-to-RGBA widening and
    /// the opaque alpha this decoder has to supply — a JPEG has no alpha channel
    /// and a raster drawn with a zero one is invisible.
    #[test]
    fn a_jpeg_decodes_to_opaque_rgba() {
        const TINY: &[u8] = &[
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x28, 0x1c, 0x1e,
            0x23, 0x1e, 0x19, 0x28, 0x23, 0x21, 0x23, 0x2d, 0x2b, 0x28, 0x30, 0x3c, 0x64, 0x41,
            0x3c, 0x37, 0x37, 0x3c, 0x7b, 0x58, 0x5d, 0x49, 0x64, 0x91, 0x80, 0x99, 0x96, 0x8f,
            0x80, 0x8c, 0x8a, 0xa0, 0xb4, 0xe6, 0xc3, 0xa0, 0xaa, 0xda, 0xad, 0x8a, 0x8c, 0xc8,
            0xff, 0xcb, 0xda, 0xee, 0xf5, 0xff, 0xff, 0xff, 0x9b, 0xc1, 0xff, 0xff, 0xff, 0xfa,
            0xff, 0xe6, 0xfd, 0xff, 0xf8, 0xff, 0xdb, 0x00, 0x43, 0x01, 0x2b, 0x2d, 0x2d, 0x3c,
            0x35, 0x3c, 0x76, 0x41, 0x41, 0x76, 0xf8, 0xa5, 0x8c, 0xa5, 0xf8, 0xf8, 0xf8, 0xf8,
            0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
            0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
            0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
            0xf8, 0xf8, 0xf8, 0xf8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x04, 0x00, 0x04, 0x03,
            0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00, 0x15, 0x00,
            0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x03, 0xff, 0xc4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xc4,
            0x00, 0x15, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0xff, 0xc4, 0x00, 0x14, 0x11, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f,
            0x00, 0x80, 0x02, 0xe7, 0xff, 0xd9,
        ];
        let raster = raster_of(TINY, (128, 128), 1).expect("a valid JPEG was refused");
        assert_eq!(raster.size, (4, 4), "a 4×4 JPEG was not 4×4");
        assert_eq!(raster.pixels.len(), 4 * 4 * 4);
        for pixel in raster.pixels.chunks_exact(4) {
            assert_eq!(pixel[3], 255, "a JPEG has no alpha and this one is not opaque");
            assert!(pixel[0] > pixel[1] && pixel[0] > pixel[2], "the colour is not the one saved");
        }
    }

    #[test]
    fn something_that_is_not_an_image_is_not_its_kind() {
        assert_eq!(decode(b"just some text", (128, 128), 1).err(), Some(Refusal::NotItsKind));
        assert_eq!(decode(&[], (128, 128), 1).err(), Some(Refusal::NotItsKind));
        // A PNG signature with nothing behind it is that kind, and broken.
        let stub = b"\x89PNG\r\n\x1a\n and then nothing";
        assert_eq!(decode(stub, (128, 128), 1).err(), Some(Refusal::Undecodable));
    }

    #[test]
    fn svg_is_sniffed_through_whatever_precedes_the_root() {
        assert!(looks_like_svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"));
        assert!(looks_like_svg(b"<?xml version=\"1.0\"?>\n<!-- a note -->\n<svg/>"));
        assert!(!looks_like_svg(b"<html><body>no</body></html>"));
        // Past the first kilobyte is not sniffed, which bounds the scan.
        let mut late = vec![b' '; 2000];
        late.extend_from_slice(b"<svg/>");
        assert!(!looks_like_svg(&late));
    }

    /// **A vector is rendered at the size asked for, not at the size it declares.**
    /// The first draft used `box_fit` here and a 10×5 icon asked for at 100 came
    /// back at 10×5 — correct for a photograph, whose pixels do not exist above
    /// their own resolution, and wrong for the one format that has no resolution.
    #[test]
    fn an_svg_is_rendered_at_the_size_it_was_asked_for() {
        let source = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="5">
             <rect width="10" height="5" fill="#ff0000"/></svg>"##;
        let raster = raster_of(source, (100, 100), 1).expect("a rectangle would not render");
        // Fitted into the box, aspect kept: 10×5 into 100×100 is 100×50.
        assert_eq!(raster.size, (100, 50));
        assert_eq!(raster.pixels.len(), 100 * 50 * 4);
        // And it is red, straight rather than premultiplied.
        assert_eq!(&raster.pixels[..4], &[255, 0, 0, 255]);
    }

    /// **Premultiplication, which is the bug this catches.** A half-transparent
    /// white square read as straight RGBA would come back grey.
    #[test]
    fn a_transparent_svg_comes_back_straight_and_not_premultiplied() {
        let source = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
             <rect width="4" height="4" fill="#ffffff" fill-opacity="0.5"/></svg>"##;
        let raster = raster_of(source, (4, 4), 1).expect("a rectangle would not render");
        let pixel = &raster.pixels[..4];
        assert!(pixel[3] > 100 && pixel[3] < 160, "alpha is {}", pixel[3]);
        assert!(pixel[0] > 240, "white came back at {} — premultiplied", pixel[0]);
    }
}
