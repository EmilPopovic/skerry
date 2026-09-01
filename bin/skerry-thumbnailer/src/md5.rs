//! MD5, because the thumbnail specification says MD5.
//!
//! **Not a hash anybody chose.** The XDG thumbnail specification names the file
//! `<md5 of the file's URI>.png`, and interoperating with every other file manager
//! on the machine means computing the same digest they do. It is used here as a
//! *name*, never as a check on anything: nothing is authenticated by it and
//! nothing is trusted because of it.
//!
//! Sixty lines rather than a dependency, for the reason
//! [07-dependencies.md](../../../docs/07-dependencies.md) gives about small,
//! well-specified things — and RFC 1321 has the test vectors, which is what makes
//! writing one defensible.

/// The digest of `bytes`, as the thirty-two lowercase hex characters a filename
/// wants.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(32);
    for byte in digest(bytes) {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
    }
    out
}

fn digest(bytes: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut message = bytes.to_vec();
    let bits = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_le_bytes());

    for block in message.chunks_exact(64) {
        let mut words = [0u32; 16];
        for (word, chunk) in words.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        let [mut a, mut b, mut c, mut d] = state;
        for step in 0..64usize {
            let (mixed, index) = match step / 16 {
                0 => ((b & c) | (!b & d), step),
                1 => ((d & b) | (!d & c), (5 * step + 1) % 16),
                2 => (b ^ c ^ d, (3 * step + 5) % 16),
                _ => (c ^ (b | !d), (7 * step) % 16),
            };
            let turned = a
                .wrapping_add(mixed)
                .wrapping_add(SINE[step])
                .wrapping_add(words[index])
                .rotate_left(SHIFT[(step / 16) * 4 + step % 4]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(turned);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = [0u8; 16];
    for (slot, word) in out.chunks_exact_mut(4).zip(state) {
        slot.copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// The per-round left rotations, four per round.
const SHIFT: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];

/// `floor(abs(sin(i + 1)) * 2^32)`, the constants RFC 1321 tabulates.
const SINE: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

#[cfg(test)]
mod tests {
    use super::hex;

    /// **RFC 1321's own test suite**, which is the whole reason writing this
    /// rather than taking a dependency is defensible: the answers are published
    /// and a wrong implementation cannot pass them by accident.
    #[test]
    fn the_rfc_1321_vectors_come_out_right() {
        for (input, wanted) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "123456789012345678901234567890123456789012345678901234567890123456\
                 78901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(hex(input.as_bytes()), wanted, "for {input:?}");
        }
    }

    /// The block boundary, which is where a padding mistake hides: 55, 56 and 64
    /// bytes are one block, one block plus padding that does not fit, and exactly
    /// two blocks.
    #[test]
    fn the_padding_is_right_on_both_sides_of_a_block() {
        let digests: Vec<String> =
            [55, 56, 63, 64, 65].iter().map(|len| hex(&vec![b'x'; *len])).collect();
        // Different lengths cannot share a digest, which is the property a
        // mis-padded implementation loses first.
        for (at, digest) in digests.iter().enumerate() {
            assert_eq!(digest.len(), 32);
            assert!(!digests[at + 1..].contains(digest), "two lengths hashed the same");
        }
        // And two of them against answers from somewhere else — `python3 -c
        // "import hashlib; hashlib.md5(b'x'*64).hexdigest()"` — so this is not
        // merely internally consistent. Both were taken *after* the first draft of
        // this test asserted a value nobody had computed and the implementation,
        // which was right, failed against it.
        assert_eq!(hex(&[b'x'; 64]), "c1bb4f81d892b2d57947682aeb252456");
        assert_eq!(hex(&[b'x'; 55]), "04364420e25c512fd958a70738aa8f72");
    }
}
