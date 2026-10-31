//! MD5, for RTSP digest authentication and nothing else.
//!
//! Cameras ask for digest and digest is specified on MD5, so there is no
//! choice to make here; it secures nothing on its own.

/// How far each round rotates.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `floor(2^32 * abs(sin(i + 1)))`, the table from RFC 1321.
const SINES: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// The digest of `parts` joined with `:`, as every digest field is built.
pub fn joined(parts: &[&str]) -> String {
    hex(parts.join(":").as_bytes())
}

pub fn hex(data: &[u8]) -> String {
    digest(data).iter().fold(String::new(), |mut text, byte| {
        text.push_str(&format!("{byte:02x}"));
        text
    })
}

fn digest(data: &[u8]) -> [u8; 16] {
    let mut state = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];

    // The length in bits closes the message, after a one bit and zeroes.
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();

    message.push(0x80);

    while message.len() % 64 != 56 {
        message.push(0);
    }

    message.extend_from_slice(&bits.to_le_bytes());

    for block in message.chunks(64) {
        let mut words = [0u32; 16];

        for (at, word) in words.iter_mut().enumerate() {
            let bytes = block[at * 4..at * 4 + 4].try_into().expect("four bytes");
            *word = u32::from_le_bytes(bytes);
        }

        let [mut a, mut b, mut c, mut d] = state;

        for round in 0..64 {
            let (mixed, word) = match round / 16 {
                0 => ((b & c) | (!b & d), round),
                1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                2 => (b ^ c ^ d, (3 * round + 5) % 16),
                _ => (c ^ (b | !d), (7 * round) % 16),
            };

            let sum = a
                .wrapping_add(mixed)
                .wrapping_add(SINES[round])
                .wrapping_add(words[word]);

            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(sum.rotate_left(SHIFTS[round]));
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut digest = [0u8; 16];

    for (at, word) in state.iter().enumerate() {
        digest[at * 4..at * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }

    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test suite from RFC 1321, which a wrong table or shift fails.
    #[test]
    fn the_digests_from_the_rfc_come_out() {
        for (text, digest) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(hex(text.as_bytes()), digest, "the digest of {text:?}");
        }
    }

    /// Where the padding no longer fits beside the length and spills into a
    /// block of its own.
    #[test]
    fn a_message_that_fills_a_block_is_padded_into_the_next() {
        assert_eq!(hex(&[b'a'; 55]), "ef1772b6dff9a122358552954ad0df65");
        assert_eq!(hex(&[b'a'; 56]), "3b0c8ac703f828b04c6c197006d17218");
        assert_eq!(hex(&[b'a'; 64]), "014842d480b571495a4a0363793f7367");
    }

    #[test]
    fn the_parts_of_a_digest_are_joined_with_colons() {
        assert_eq!(joined(&["a", "b"]), hex(b"a:b"));
    }
}
