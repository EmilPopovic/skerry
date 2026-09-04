// SPDX-License-Identifier: MIT OR Apache-2.0
//! Which way up a JPEG is, and turning it that way.
//!
//! **A photograph from a phone is stored sideways.** The sensor reads out in one
//! orientation whatever the camera was doing, and the file says which of eight
//! transforms puts it right — EXIF tag `0x0112`, in the `APP1` segment. A viewer
//! that ignores it shows every portrait photograph on its side, which is the
//! defect this exists to fix; it was
//! [08-roadmap.md](../../../docs/08-roadmap.md#m5--3-wk--inspector-and-quick-look)'s
//! "images with EXIF orientation" and nothing had read a tag.
//!
//! **Ninety lines rather than a crate**, for [rule
//! 10](../../../docs/03-principles.md)'s reason: what is needed is one 16-bit
//! integer out of one IFD entry, and every EXIF crate reads a hundred tags into a
//! typed model. The parser below refuses everything it does not recognise and
//! answers [`Orientation::Upright`], because a photograph shown as it was stored
//! is the failure this had before.

/// One of the eight ways a picture can be stored relative to how it is seen.
///
/// The names are the operation that *undoes* the storage, which is what a viewer
/// applies. The numbers are EXIF's own.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Orientation {
    /// 1: already the right way up.
    #[default]
    Upright,
    /// 2: mirrored left to right.
    FlipHorizontal,
    /// 3: upside down.
    Half,
    /// 4: mirrored top to bottom.
    FlipVertical,
    /// 5: transposed — mirrored, then a quarter turn clockwise.
    Transpose,
    /// 6: a quarter turn clockwise puts it right.
    Clockwise,
    /// 7: transverse — mirrored, then a quarter turn anticlockwise.
    Transverse,
    /// 8: a quarter turn anticlockwise puts it right.
    Anticlockwise,
}

impl Orientation {
    /// The tag's value, as EXIF numbers them. Anything else is upright.
    const fn of(tag: u16) -> Self {
        match tag {
            2 => Self::FlipHorizontal,
            3 => Self::Half,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Clockwise,
            7 => Self::Transverse,
            8 => Self::Anticlockwise,
            _ => Self::Upright,
        }
    }

    /// Whether applying it swaps width and height.
    pub const fn turns(self) -> bool {
        matches!(self, Self::Transpose | Self::Clockwise | Self::Transverse | Self::Anticlockwise)
    }

    /// Whether there is anything to do at all.
    pub const fn is_upright(self) -> bool {
        matches!(self, Self::Upright)
    }
}

/// The orientation a JPEG's `APP1` segment declares, if it declares one.
///
/// **Only the first `APP1`, and only its first IFD.** A file with two of them is
/// a file with an opinion this does not need to resolve: the first is the one
/// every camera writes and every viewer reads.
#[must_use]
pub fn orientation(bytes: &[u8]) -> Orientation {
    let Some(exif) = app1(bytes) else { return Orientation::Upright };
    let Some(tiff) = exif.strip_prefix(b"Exif\0\0") else { return Orientation::Upright };
    let little = match tiff.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Orientation::Upright,
    };
    let short = |at: usize| -> Option<u16> {
        let pair: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if little { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) })
    };
    let long = |at: usize| -> Option<u32> {
        let quad: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if little { u32::from_le_bytes(quad) } else { u32::from_be_bytes(quad) })
    };
    // The magic 42 is the specification's, and checking it is what stops a
    // `II`-prefixed something-else from being read as an IFD offset.
    if short(2) != Some(42) {
        return Orientation::Upright;
    }
    let Some(first) = long(4).and_then(|it| usize::try_from(it).ok()) else {
        return Orientation::Upright;
    };
    let Some(entries) = short(first) else { return Orientation::Upright };
    for index in 0..usize::from(entries) {
        // Twelve bytes an entry, after the two-byte count.
        let at = first + 2 + index * 12;
        if short(at) != Some(0x0112) {
            continue;
        }
        // A `SHORT` value sits in the first two bytes of the twelve-byte entry's
        // value field, whichever end the file is written from.
        return short(at + 8).map_or(Orientation::Upright, Orientation::of);
    }
    Orientation::Upright
}

/// The bytes of the first `APP1` segment.
fn app1(bytes: &[u8]) -> Option<&[u8]> {
    let mut at = 2; // Past `FF D8`.
    while at + 4 <= bytes.len() {
        if bytes.get(at) != Some(&0xff) {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        // A start-of-scan means the metadata is behind us.
        if marker == 0xda {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([*bytes.get(at + 2)?, *bytes.get(at + 3)?]));
        if length < 2 {
            return None;
        }
        if marker == 0xe1 {
            return bytes.get(at + 4..at + 2 + length);
        }
        at += 2 + length;
    }
    None
}

/// Turns `pixels` — RGBA8, row-major, `size` — the way `how` says.
///
/// **A new buffer rather than a rotation in place.** In place is possible for the
/// half turn and not for the quarter turns, and one code path that is obviously
/// right beats two where one of them is clever: a thumbnail is at most a few
/// hundred pixels square by the time this runs.
#[must_use]
pub fn turn(pixels: &[u8], size: (u32, u32), how: Orientation) -> (Vec<u8>, (u32, u32)) {
    let (width, height) = (size.0 as usize, size.1 as usize);
    if how.is_upright() || pixels.len() < width * height * 4 {
        return (pixels.to_vec(), size);
    }
    let (out_width, out_height) = if how.turns() { (height, width) } else { (width, height) };
    let mut out = vec![0u8; out_width * out_height * 4];
    for y in 0..height {
        for x in 0..width {
            // Where this pixel goes, which is the whole of the transform.
            let (to_x, to_y) = match how {
                Orientation::Upright => (x, y),
                Orientation::FlipHorizontal => (width - 1 - x, y),
                Orientation::Half => (width - 1 - x, height - 1 - y),
                Orientation::FlipVertical => (x, height - 1 - y),
                Orientation::Transpose => (y, x),
                Orientation::Clockwise => (height - 1 - y, x),
                Orientation::Transverse => (height - 1 - y, width - 1 - x),
                Orientation::Anticlockwise => (y, width - 1 - x),
            };
            let from = (y * width + x) * 4;
            let into = (to_y * out_width + to_x) * 4;
            out[into..into + 4].copy_from_slice(&pixels[from..from + 4]);
        }
    }
    (out, (out_width as u32, out_height as u32))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Orientation, orientation, turn};

    /// A JPEG with an `APP1` saying "a quarter turn clockwise puts it right".
    ///
    /// Built by hand rather than committed as a fixture, because what is under
    /// test is the *parse* and a hand-built one can be read in the test.
    fn tagged(value: u16, little: bool) -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(if little { b"II" } else { b"MM" });
        let short = |number: u16| {
            if little { number.to_le_bytes() } else { number.to_be_bytes() }
        };
        let long = |number: u32| {
            if little { number.to_le_bytes() } else { number.to_be_bytes() }
        };
        tiff.extend_from_slice(&short(42));
        tiff.extend_from_slice(&long(8)); // The IFD begins right after the header.
        tiff.extend_from_slice(&short(1)); // One entry.
        tiff.extend_from_slice(&short(0x0112)); // Orientation.
        tiff.extend_from_slice(&short(3)); // SHORT.
        tiff.extend_from_slice(&long(1)); // One of them.
        tiff.extend_from_slice(&short(value));
        tiff.extend_from_slice(&short(0)); // The rest of the value field.

        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff);
        let mut out = vec![0xff, 0xd8];
        out.extend_from_slice(&[0xff, 0xe1]);
        let length = u16::try_from(exif.len() + 2).expect("a small segment");
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&exif);
        out.extend_from_slice(&[0xff, 0xda]); // A scan, which ends the metadata.
        out
    }

    /// Both byte orders, because a camera may write either and half of them do.
    #[test]
    fn the_tag_is_read_at_either_end() {
        for little in [true, false] {
            assert_eq!(orientation(&tagged(6, little)), Orientation::Clockwise, "{little}");
            assert_eq!(orientation(&tagged(1, little)), Orientation::Upright, "{little}");
            assert_eq!(orientation(&tagged(8, little)), Orientation::Anticlockwise, "{little}");
        }
    }

    /// Anything that is not a JPEG with a tag is upright, rather than an error.
    #[test]
    fn a_file_with_nothing_to_say_is_upright() {
        assert_eq!(orientation(&[]), Orientation::Upright);
        assert_eq!(orientation(b"\xff\xd8\xff\xda"), Orientation::Upright);
        assert_eq!(orientation(b"not a jpeg at all"), Orientation::Upright);
        // A truncated `APP1`, which is the shape a corrupt file has.
        let mut cut = tagged(6, true);
        cut.truncate(10);
        assert_eq!(orientation(&cut), Orientation::Upright);
        // An unknown value: the specification has eight and files carry others.
        assert_eq!(orientation(&tagged(99, true)), Orientation::Upright);
    }

    /// A quarter turn moves the corner pixel to the corner it belongs in.
    ///
    /// Two by one, so a transform that swapped the axes without swapping the
    /// *size* would land out of bounds rather than merely looking wrong.
    #[test]
    fn a_quarter_turn_swaps_the_axes_and_the_size() {
        // Two pixels side by side: red then green.
        let pixels = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let (turned, size) = turn(&pixels, (2, 1), Orientation::Clockwise);
        assert_eq!(size, (1, 2), "a portrait picture out of a landscape one");
        // Clockwise puts the left-hand pixel at the top.
        assert_eq!(&turned[0..4], &[255, 0, 0, 255]);
        assert_eq!(&turned[4..8], &[0, 255, 0, 255]);

        let (back, size) = turn(&turned, size, Orientation::Anticlockwise);
        assert_eq!(size, (2, 1));
        assert_eq!(back, pixels, "and the two turns are inverses");
    }

    /// A half turn and the mirrors keep the size and move the pixels.
    #[test]
    fn a_half_turn_and_a_mirror_keep_the_size() {
        let pixels = vec![1, 1, 1, 1, 2, 2, 2, 2];
        let (turned, size) = turn(&pixels, (2, 1), Orientation::Half);
        assert_eq!(size, (2, 1));
        assert_eq!(&turned[0..4], &[2, 2, 2, 2]);
        let (mirrored, _) = turn(&pixels, (2, 1), Orientation::FlipHorizontal);
        assert_eq!(&mirrored[0..4], &[2, 2, 2, 2]);
        let (same, _) = turn(&pixels, (2, 1), Orientation::FlipVertical);
        assert_eq!(same, pixels, "one row has nothing to flip vertically");
    }

    /// Upright is the identity and costs no arithmetic.
    #[test]
    fn upright_is_the_picture_it_was_given() {
        let pixels = vec![9; 16];
        let (out, size) = turn(&pixels, (2, 2), Orientation::Upright);
        assert_eq!((out, size), (pixels, (2, 2)));
    }
}
