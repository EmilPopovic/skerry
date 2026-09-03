//! GIF, decoded here rather than by a crate.
//!
//! **Ours because it is small and frozen.** The format is a header, a palette, a
//! block loop and LZW — the specification has not moved since 1990, and the
//! alternative was another crate in the tree
//! ([07-dependencies.md](../../../docs/07-dependencies.md) tier 3) for four
//! hundred lines of arithmetic. Every parse that does not understand something
//! answers a [`Refusal`] rather than guessing, which is the same bargain the
//! sniffing in [`crate::decode`] makes.
//!
//! # A GIF is a stack of frames, and a frame is a patch
//!
//! Only the first frame is usually the whole picture. The rest are patches: a
//! rectangle somewhere inside the logical screen, drawn over what was there,
//! with a disposal method that says what to do with the region afterwards. So a
//! frame cannot be decoded on its own — reaching frame 40 means composing forty
//! of them, which is why this takes the frame it is asked for and walks there.
//!
//! That is the one place this disagrees with [ADR
//! 0019](../../../docs/adr/0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md):
//! a PDF's page 400 costs what page 1 costs, and a GIF's frame 40 costs forty
//! frames. The child is still stateless — it holds nothing between requests —
//! and the cost is bounded by [`MOST_FRAMES`], which is what keeps a
//! pathological file from turning one request into a minute of work.

use sk_preview::Refusal;

use crate::decode::Raster;

/// The most frames this will walk to.
///
/// **Two hundred and fifty-six**, which is longer than any animation somebody
/// puts in a directory and short enough that the worst request is bounded:
/// reaching frame *n* composes *n* frames, so an unbounded count would make one
/// request cost the whole file however large it is. Past this the animation is
/// truncated and the panel plays what there is — the same bargain the text
/// preview makes at four hundred lines.
pub const MOST_FRAMES: u32 = 256;

/// What a decode found: the frame asked for, and what the file holds.
pub struct Frames {
    /// The frame, composed onto the logical screen.
    pub raster: Raster,
    /// How many frames the file has, capped at [`MOST_FRAMES`].
    pub count: u32,
    /// How long this frame is shown, in milliseconds. Zero where it did not say.
    pub delay_ms: u32,
}

/// Whether these bytes are a GIF at all.
#[must_use]
pub fn is_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

/// Decodes the `wanted`th frame, one-based; zero means the first.
///
/// # Errors
///
/// [`Refusal::NotItsKind`] where the header is not a GIF's, and
/// [`Refusal::Undecodable`] where anything after it does not parse — a truncated
/// file, a code stream that runs off the end, a palette index with no colour
/// behind it.
pub fn decode(bytes: &[u8], wanted: u32) -> Result<Frames, Refusal> {
    if !is_gif(bytes) {
        return Err(Refusal::NotItsKind);
    }
    let count = count_frames(bytes)?;
    let mut reader = Reader { bytes, at: 6 };
    let screen = (u32::from(reader.u16()?), u32::from(reader.u16()?));
    crate::decode::too_big(u64::from(screen.0) * u64::from(screen.1))?;
    if screen.0 == 0 || screen.1 == 0 {
        return Err(Refusal::Undecodable);
    }
    let packed = reader.byte()?;
    let _background = reader.byte()?;
    let _aspect = reader.byte()?;
    let global = if packed & 0x80 == 0 { Vec::new() } else { reader.palette(packed)? };

    let stride = screen.0 as usize * 4;
    let mut canvas = vec![0u8; stride * screen.1 as usize];
    let mut before: Option<Vec<u8>> = None;
    // The graphic control extension that applies to the *next* image, which is
    // where a GIF keeps the delay and which colour is transparent.
    let mut control = Control::default();
    let mut at = 0u32;
    let target = wanted.max(1).min(count.max(1));
    loop {
        match reader.byte()? {
            EXTENSION => {
                let label = reader.byte()?;
                let blocks = reader.sub_blocks()?;
                if label == GRAPHIC_CONTROL {
                    control = Control::of(&blocks);
                }
            }
            IMAGE => {
                let frame = reader.image(&global, packed)?;
                if control.disposal == RESTORE_PREVIOUS {
                    before = Some(canvas.clone());
                }
                compose(&mut canvas, screen, &frame, control.transparent);
                at += 1;
                if at >= target {
                    return Ok(Frames {
                        raster: Raster { pixels: canvas, size: screen },
                        count,
                        delay_ms: control.delay_ms,
                    });
                }
                dispose(&mut canvas, screen, &frame, control.disposal, before.take());
                control = Control::default();
            }
            // The trailer, and anything that is not a block we know: a byte that
            // is neither says the stream has gone wrong, and stopping with what
            // has been composed so far is a picture rather than a lie.
            _ => {
                return Ok(Frames {
                    raster: Raster { pixels: canvas, size: screen },
                    count,
                    delay_ms: control.delay_ms,
                });
            }
        }
    }
}

/// How many frames the file holds, by walking the blocks without decoding them.
///
/// **A skip rather than a decode.** The count is needed before the first frame is
/// drawn — the panel says `3 / 14` and rule 7 forbids inventing the 14 — and
/// walking sub-block lengths is a pass over the file's structure rather than over
/// its pixels.
fn count_frames(bytes: &[u8]) -> Result<u32, Refusal> {
    let mut reader = Reader { bytes, at: 6 };
    let _screen = (reader.u16()?, reader.u16()?);
    let packed = reader.byte()?;
    let _background = reader.byte()?;
    let _aspect = reader.byte()?;
    if packed & 0x80 != 0 {
        reader.palette(packed)?;
    }
    let mut count = 0;
    while count < MOST_FRAMES {
        match reader.byte()? {
            EXTENSION => {
                let _label = reader.byte()?;
                reader.sub_blocks()?;
            }
            IMAGE => {
                let _left = reader.u16()?;
                let _top = reader.u16()?;
                let _width = reader.u16()?;
                let _height = reader.u16()?;
                let local = reader.byte()?;
                if local & 0x80 != 0 {
                    reader.palette(local)?;
                }
                let _min_code = reader.byte()?;
                reader.sub_blocks()?;
                count += 1;
            }
            _ => break,
        }
    }
    Ok(count)
}

/// The block markers, from the specification's table.
const EXTENSION: u8 = 0x21;
const IMAGE: u8 = 0x2c;
const GRAPHIC_CONTROL: u8 = 0xf9;

/// The disposal methods this cares about: what to do with a frame's region once
/// the next frame arrives.
const RESTORE_BACKGROUND: u8 = 2;
const RESTORE_PREVIOUS: u8 = 3;

/// What a graphic control extension said about the frame after it.
#[derive(Clone, Copy, Default)]
struct Control {
    /// Which palette index is transparent, where one is.
    transparent: Option<u8>,
    /// How long the frame is shown, in milliseconds.
    delay_ms: u32,
    /// What to do with the frame's region afterwards.
    disposal: u8,
}

impl Control {
    fn of(blocks: &[u8]) -> Self {
        if blocks.len() < 4 {
            return Self::default();
        }
        let flags = blocks[0];
        // **Hundredths of a second on the wire.** A GIF's delay is in centiseconds
        // and every browser treats a zero — and, historically, a one — as "as fast
        // as is reasonable", which is 100 ms. Ten milliseconds a frame is not a
        // rate anything actually plays at.
        let centiseconds = u32::from(u16::from_le_bytes([blocks[1], blocks[2]]));
        let delay_ms = if centiseconds <= 1 { 100 } else { centiseconds * 10 };
        Self {
            transparent: (flags & 1 != 0).then_some(blocks[3]),
            delay_ms,
            disposal: (flags >> 2) & 7,
        }
    }
}

/// One frame's pixels and where they go on the logical screen.
struct Frame {
    at: (u32, u32),
    size: (u32, u32),
    /// One palette entry per pixel, row-major, already de-interlaced.
    indices: Vec<u8>,
    /// The palette this frame's indices are into: its own, or the global one.
    palette: Vec<[u8; 3]>,
}

/// Draws a frame over the canvas, honouring its transparent index.
fn compose(canvas: &mut [u8], screen: (u32, u32), frame: &Frame, transparent: Option<u8>) {
    let stride = screen.0 as usize * 4;
    for row in 0..frame.size.1 {
        let y = frame.at.1 + row;
        if y >= screen.1 {
            break;
        }
        for column in 0..frame.size.0 {
            let x = frame.at.0 + column;
            if x >= screen.0 {
                break;
            }
            let Some(&index) = frame.indices.get((row * frame.size.0 + column) as usize) else {
                continue;
            };
            if transparent == Some(index) {
                continue;
            }
            let Some(colour) = frame.palette.get(index as usize) else { continue };
            let at = y as usize * stride + x as usize * 4;
            let Some(pixel) = canvas.get_mut(at..at + 4) else { continue };
            pixel.copy_from_slice(&[colour[0], colour[1], colour[2], 0xff]);
        }
    }
}

/// Does what a frame's disposal method says, before the next frame is drawn.
fn dispose(
    canvas: &mut [u8],
    screen: (u32, u32),
    frame: &Frame,
    disposal: u8,
    before: Option<Vec<u8>>,
) {
    match disposal {
        RESTORE_BACKGROUND => {
            let stride = screen.0 as usize * 4;
            for row in 0..frame.size.1 {
                let y = frame.at.1 + row;
                if y >= screen.1 {
                    break;
                }
                for column in 0..frame.size.0 {
                    let x = frame.at.0 + column;
                    if x >= screen.0 {
                        break;
                    }
                    let at = y as usize * stride + x as usize * 4;
                    if let Some(pixel) = canvas.get_mut(at..at + 4) {
                        pixel.fill(0);
                    }
                }
            }
        }
        // **The canvas as it was before this frame**, which is what the method
        // says and what a bouncing sprite over a fixed background needs. Where the
        // copy is missing — a stream that says "restore" on its first frame — the
        // canvas is left alone, which is the other method and is a picture.
        RESTORE_PREVIOUS => {
            if let Some(previous) = before {
                canvas.copy_from_slice(&previous);
            }
        }
        _ => {}
    }
}

/// A cursor over the file's bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, Refusal> {
        let byte = *self.bytes.get(self.at).ok_or(Refusal::Undecodable)?;
        self.at += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, Refusal> {
        Ok(u16::from_le_bytes([self.byte()?, self.byte()?]))
    }

    fn take(&mut self, count: usize) -> Result<&'_ [u8], Refusal> {
        let end = self.at.checked_add(count).ok_or(Refusal::Undecodable)?;
        let slice = self.bytes.get(self.at..end).ok_or(Refusal::Undecodable)?;
        self.at = end;
        Ok(slice)
    }

    /// A colour table, whose length is three bits of a packed byte.
    fn palette(&mut self, packed: u8) -> Result<Vec<[u8; 3]>, Refusal> {
        let entries = 1usize << ((packed & 7) + 1);
        Ok(self.take(entries * 3)?.chunks_exact(3).map(|it| [it[0], it[1], it[2]]).collect())
    }

    /// The sub-block chain at the cursor, concatenated.
    fn sub_blocks(&mut self) -> Result<Vec<u8>, Refusal> {
        let mut out = Vec::new();
        loop {
            let len = self.byte()?;
            if len == 0 {
                return Ok(out);
            }
            out.extend_from_slice(self.take(len as usize)?);
        }
    }

    /// One image descriptor and the frame behind it.
    fn image(&mut self, global: &[[u8; 3]], screen: u8) -> Result<Frame, Refusal> {
        let at = (u32::from(self.u16()?), u32::from(self.u16()?));
        let size = (u32::from(self.u16()?), u32::from(self.u16()?));
        let packed = self.byte()?;
        let palette = if packed & 0x80 == 0 { global.to_vec() } else { self.palette(packed)? };
        if palette.is_empty() {
            // A frame with no palette anywhere: the global table was absent and
            // this frame declined to bring one.
            let _ = screen;
            return Err(Refusal::Undecodable);
        }
        let min_code = self.byte()?;
        let data = self.sub_blocks()?;
        let pixels = (size.0 as usize).checked_mul(size.1 as usize).ok_or(Refusal::TooLarge)?;
        crate::decode::too_big(pixels as u64)?;
        let mut indices = lzw(min_code, &data, pixels)?;
        if packed & 0x40 != 0 {
            indices = deinterlace(&indices, size);
        }
        Ok(Frame { at, size, indices, palette })
    }
}

/// LZW, as the GIF specification spells it.
///
/// **Little-endian bit packing and a variable code width.** Codes start one bit
/// wider than the minimum code size, grow when the dictionary fills, and reset on
/// a clear code — which a GIF may send at any point and not only at the start.
fn lzw(min_code: u8, data: &[u8], pixels: usize) -> Result<Vec<u8>, Refusal> {
    if !(2..=11).contains(&min_code) {
        return Err(Refusal::Undecodable);
    }
    let clear = 1u16 << min_code;
    let end = clear + 1;
    // The dictionary as two flat arrays: the code this entry extends, and the
    // byte it adds. Walking a chain backwards and reversing is what makes an
    // entry cost two words instead of a `Vec` each.
    let mut prefix = vec![u16::MAX; 4096];
    let mut suffix = vec![0u8; 4096];
    for code in 0..clear {
        suffix[code as usize] = code as u8;
    }
    let mut next = end + 1;
    let mut width = u32::from(min_code) + 1;
    let mut previous: Option<u16> = None;
    let mut out: Vec<u8> = Vec::with_capacity(pixels);
    let mut stack: Vec<u8> = Vec::with_capacity(4096);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for &byte in data {
        accumulator |= u32::from(byte) << bits;
        bits += 8;
        while bits >= width {
            let code = (accumulator & ((1 << width) - 1)) as u16;
            accumulator >>= width;
            bits -= width;
            if code == clear {
                next = end + 1;
                width = u32::from(min_code) + 1;
                previous = None;
                continue;
            }
            if code == end {
                return Ok(finish(out, pixels));
            }
            // **The one case that is not a lookup.** A code equal to the next free
            // entry is the encoder referring to the entry it is about to add,
            // which is the string it last emitted plus that string's own first
            // byte — the trick that makes a run of one byte cost two codes.
            let known = code < next && (code < clear || prefix[code as usize] != u16::MAX);
            let first = if known {
                expand(code, &prefix, &suffix, &mut stack, &mut out)?
            } else {
                let Some(previous) = previous else { return Err(Refusal::Undecodable) };
                let first = expand(previous, &prefix, &suffix, &mut stack, &mut out)?;
                out.push(first);
                first
            };
            if let Some(previous) = previous
                && next < 4096
            {
                prefix[next as usize] = previous;
                suffix[next as usize] = first;
                next += 1;
                // **The width grows the moment the next code would not fit**, so
                // the encoder and the decoder count entries in lockstep: a
                // decoder that widened an entry late reads the following code at
                // the wrong width, and everything after it is noise that still
                // decodes to pixels.
                if next == 1 << width && width < 12 {
                    width += 1;
                }
            }
            previous = Some(code);
            if out.len() >= pixels {
                return Ok(finish(out, pixels));
            }
        }
    }
    Ok(finish(out, pixels))
}

/// Pads or trims a decoded frame to the pixel count its descriptor promised.
///
/// **A short stream is a picture with a hole in it, not a refusal.** Half-written
/// GIFs are common — a download that stopped — and every viewer shows what
/// arrived. What is missing is left as palette index zero.
fn finish(mut out: Vec<u8>, pixels: usize) -> Vec<u8> {
    out.resize(pixels, 0);
    out
}

/// Writes the string a code stands for and answers its first byte.
fn expand(
    code: u16,
    prefix: &[u16],
    suffix: &[u8],
    stack: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<u8, Refusal> {
    stack.clear();
    let mut walk = code;
    // Bounded by the dictionary's size: a cycle in the chain would otherwise be
    // an infinite loop inside a decoder, which is a hang rather than a refusal.
    for _ in 0..=4096 {
        stack.push(*suffix.get(walk as usize).ok_or(Refusal::Undecodable)?);
        match prefix.get(walk as usize) {
            Some(&u16::MAX) | None => {
                let first = *stack.last().ok_or(Refusal::Undecodable)?;
                out.extend(stack.iter().rev());
                return Ok(first);
            }
            Some(&next) => walk = next,
        }
    }
    Err(Refusal::Undecodable)
}

/// Puts an interlaced frame's rows back in order.
///
/// Four passes — every eighth row from 0, every eighth from 4, every fourth from
/// 2, every second from 1 — which is what made a GIF appear in stages over a
/// modem and is still in files today.
fn deinterlace(indices: &[u8], size: (u32, u32)) -> Vec<u8> {
    let (width, height) = (size.0 as usize, size.1 as usize);
    let mut out = vec![0u8; width * height];
    let mut source = indices.chunks_exact(width);
    for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
        let mut y = start;
        while y < height {
            if let Some(row) = source.next() {
                out[y * width..(y + 1) * width].copy_from_slice(row);
            }
            y += step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{decode, is_gif};

    /// Writes a GIF whose frames are literal codes and no dictionary.
    ///
    /// **An encoder rather than a byte blob**, because a hand-computed bit stream
    /// is a fixture nobody can change: adding a second frame to
    /// `[0x44, 0x82, 0x02]` means recomputing eighteen bits by hand, and getting
    /// it wrong looks exactly like a decoder bug. Literal codes only — every
    /// pixel is one code, no dictionary entries are ever used — which is a
    /// legitimate encoding that any decoder must handle.
    struct Writer {
        bytes: Vec<u8>,
    }

    impl Writer {
        /// A file with a two-colour global palette: black and white.
        fn new(size: (u16, u16)) -> Self {
            let mut bytes = b"GIF89a".to_vec();
            bytes.extend_from_slice(&size.0.to_le_bytes());
            bytes.extend_from_slice(&size.1.to_le_bytes());
            // Global table, two entries: `0x80 | (size bits = 0)` is 2 colours.
            bytes.push(0x80);
            bytes.push(0);
            bytes.push(0);
            bytes.extend_from_slice(&[0, 0, 0]);
            bytes.extend_from_slice(&[255, 255, 255]);
            Self { bytes }
        }

        /// A graphic control extension: a delay in centiseconds, and a disposal.
        fn control(
            &mut self,
            centiseconds: u16,
            disposal: u8,
            transparent: Option<u8>,
        ) -> &mut Self {
            self.bytes.extend_from_slice(&[super::EXTENSION, super::GRAPHIC_CONTROL, 4]);
            let flags = (disposal << 2) | u8::from(transparent.is_some());
            self.bytes.push(flags);
            self.bytes.extend_from_slice(&centiseconds.to_le_bytes());
            self.bytes.push(transparent.unwrap_or(0));
            self.bytes.push(0);
            self
        }

        /// One frame at `at`, `size` big, whose pixels are palette indices.
        fn frame(&mut self, at: (u16, u16), size: (u16, u16), pixels: &[u8]) -> &mut Self {
            self.bytes.push(super::IMAGE);
            for value in [at.0, at.1, size.0, size.1] {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
            self.bytes.push(0);
            // The smallest code size a GIF allows, which gives codes 0 and 1 for
            // the two colours, 4 for clear and 5 for end.
            let min_code = 2u8;
            self.bytes.push(min_code);
            let mut codes = vec![1u16 << min_code];
            codes.extend(pixels.iter().map(|index| u16::from(*index)));
            codes.push((1u16 << min_code) + 1);
            let data = pack(&codes, min_code);
            for chunk in data.chunks(255) {
                self.bytes.push(chunk.len() as u8);
                self.bytes.extend_from_slice(chunk);
            }
            self.bytes.push(0);
            self
        }

        fn done(&mut self) -> Vec<u8> {
            let mut bytes = self.bytes.clone();
            bytes.push(0x3b);
            bytes
        }
    }

    /// Packs codes little-endian, widening exactly where the decoder does.
    ///
    /// **A literal-only stream still grows the dictionary.** The decoder adds an
    /// entry for every code after a clear whether or not the encoder ever refers
    /// to one, so the code width grows on a schedule the encoder has to keep to —
    /// which is what the first version of this got wrong, and it read as a
    /// decoder that lost the last pixel of a four-pixel image.
    fn pack(codes: &[u16], min_code: u8) -> Vec<u8> {
        let clear = 1u16 << min_code;
        let end = clear + 1;
        let mut next = end + 1;
        let mut width = u32::from(min_code) + 1;
        let mut after_clear = true;
        let mut out = Vec::new();
        let mut accumulator = 0u32;
        let mut bits = 0u32;
        for code in codes {
            accumulator |= u32::from(*code) << bits;
            bits += width;
            while bits >= 8 {
                out.push((accumulator & 0xff) as u8);
                accumulator >>= 8;
                bits -= 8;
            }
            if *code == clear {
                next = end + 1;
                width = u32::from(min_code) + 1;
                after_clear = true;
            } else if *code != end {
                if !after_clear {
                    next += 1;
                    if next == 1 << width && width < 12 {
                        width += 1;
                    }
                }
                after_clear = false;
            }
        }
        if bits > 0 {
            out.push((accumulator & 0xff) as u8);
        }
        out
    }

    /// A still GIF decodes to its pixels, in straight RGBA.
    #[test]
    fn a_still_gif_is_its_own_pixels() {
        let bytes = Writer::new((2, 2)).frame((0, 0), (2, 2), &[0, 1, 1, 0]).done();
        assert!(is_gif(&bytes));
        let frames = decode(&bytes, 1).expect("a frame");
        assert_eq!(frames.count, 1);
        assert_eq!(frames.raster.size, (2, 2));
        assert_eq!(
            frames.raster.pixels,
            [
                0, 0, 0, 255, 255, 255, 255, 255, //
                255, 255, 255, 255, 0, 0, 0, 255,
            ]
        );
    }

    /// A frame is a patch over what came before it, and the count is known before
    /// any of them is drawn.
    #[test]
    fn a_later_frame_is_composed_over_the_ones_before_it() {
        let bytes = Writer::new((2, 2))
            .frame((0, 0), (2, 2), &[0, 0, 0, 0])
            .control(5, 0, None)
            .frame((1, 0), (1, 1), &[1])
            .done();
        // The count is a walk of the blocks, so it is the same from either frame.
        assert_eq!(decode(&bytes, 1).expect("first").count, 2);
        let first = decode(&bytes, 1).expect("first").raster.pixels;
        assert_eq!(&first[4..8], [0, 0, 0, 255], "the first frame is all black");

        let second = decode(&bytes, 2).expect("second");
        assert_eq!(second.delay_ms, 50, "five centiseconds is fifty milliseconds");
        assert_eq!(&second.raster.pixels[4..8], [255, 255, 255, 255], "the patch landed");
        assert_eq!(&second.raster.pixels[0..4], [0, 0, 0, 255], "and nothing else moved");
    }

    /// A transparent index leaves what is under it, and `restore to background`
    /// clears the patch's own region rather than the whole screen.
    #[test]
    fn transparency_and_disposal_do_what_they_say() {
        let bytes = Writer::new((2, 1))
            .frame((0, 0), (2, 1), &[1, 1])
            .control(0, super::RESTORE_BACKGROUND, Some(0))
            .frame((0, 0), (1, 1), &[0])
            .control(0, 0, None)
            .frame((1, 0), (1, 1), &[1])
            .done();
        // Frame 2 paints index 0, which is transparent, so the white beneath it
        // stays: a transparent pixel is not a black one.
        let second = decode(&bytes, 2).expect("second");
        assert_eq!(&second.raster.pixels[0..4], [255, 255, 255, 255]);
        // Frame 3 comes after a `restore to background`, which cleared the first
        // pixel's region — so it is transparent, and the second is painted.
        let third = decode(&bytes, 3).expect("third");
        assert_eq!(&third.raster.pixels[0..4], [0, 0, 0, 0], "the region was cleared");
        assert_eq!(&third.raster.pixels[4..8], [255, 255, 255, 255]);
    }

    /// An interlaced frame's rows come back in order.
    #[test]
    fn an_interlaced_frame_is_put_back_in_order() {
        // Eight rows of one pixel each, written in interlace order: rows 0, 4, 2,
        // 6, then 1, 3, 5, 7 — pass one is every eighth from 0, and with a height
        // of eight that is row 0 alone.
        let order = [0usize, 4, 2, 6, 1, 3, 5, 7];
        let mut pixels = [0u8; 8];
        for (source, target) in order.iter().enumerate() {
            // A one in the rows that are odd, so the answer is checkable by shape.
            pixels[source] = u8::from(target % 2 == 1);
        }
        let mut writer = Writer::new((1, 8));
        writer.bytes.push(super::IMAGE);
        for value in [0u16, 0, 1, 8] {
            writer.bytes.extend_from_slice(&value.to_le_bytes());
        }
        writer.bytes.push(0x40); // interlaced, no local palette
        writer.bytes.push(2);
        let mut codes = vec![4u16];
        codes.extend(pixels.iter().map(|index| u16::from(*index)));
        codes.push(5);
        let data = pack(&codes, 2);
        writer.bytes.push(data.len() as u8);
        writer.bytes.extend_from_slice(&data);
        writer.bytes.push(0);
        let bytes = writer.done();

        let frames = decode(&bytes, 1).expect("a frame");
        let rows: Vec<u8> = frames.raster.pixels.chunks_exact(4).map(|it| it[0]).collect();
        assert_eq!(rows, [0, 255, 0, 255, 0, 255, 0, 255], "every odd row is white");
    }

    /// Anything that is not a GIF is not one, and a truncated one refuses rather
    /// than reading off the end.
    #[test]
    fn a_broken_file_refuses_and_does_not_read_past_the_end() {
        assert!(!is_gif(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(decode(b"not a gif at all", 1).err(), Some(sk_preview::Refusal::NotItsKind));
        let whole = Writer::new((2, 2)).frame((0, 0), (2, 2), &[0, 1, 1, 0]).done();
        for cut in 6..whole.len() {
            // Every truncation is either a refusal or a picture, and never a panic
            // — which is the property that matters for a decoder fed a download
            // that stopped halfway.
            let _ = decode(&whole[..cut], 1);
        }
        // **A frame whose code stream stops early is a picture with a hole in
        // it**, not a refusal: the descriptor promises sixteen pixels and four
        // arrive, which is what a download that stopped looks like. The rest is
        // palette index zero.
        let short = Writer::new((4, 4)).frame((0, 0), (4, 4), &[1, 1, 1, 1]).done();
        let frames = decode(&short, 1).expect("what arrived");
        assert_eq!(frames.raster.size, (4, 4));
        assert_eq!(&frames.raster.pixels[0..4], [255, 255, 255, 255], "the four that arrived");
        assert_eq!(&frames.raster.pixels[60..64], [0, 0, 0, 255], "and index zero for the rest");
    }
}
