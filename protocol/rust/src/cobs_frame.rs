//! COBS (Consistent Overhead Byte Stuffing) encode/decode over caller-provided slices.
//!
//! Wraps the `cobs` crate with slice-only, allocation-free helpers and the error type used
//! by the rest of this crate. Overhead is at most 1 byte per 254 bytes of input, plus the
//! leading code byte.

/// Failure decoding a COBS-encoded buffer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CobsError {
    /// The encoded data was malformed: a code byte pointed past the end of the buffer,
    /// or a `0x00` appeared inside the encoded region.
    Malformed,
    /// The destination slice was too small to hold the result.
    DestTooSmall,
}

/// Maximum COBS-encoded length for `n` bytes of input, excluding the trailing delimiter.
#[inline]
pub const fn max_encoded_len(n: usize) -> usize {
    n + n / 254 + 1
}

/// COBS-encode `src` into `dst`, returning the number of bytes written.
///
/// Does **not** append the trailing `0x00` delimiter; the caller owns framing.
pub fn cobs_encode(src: &[u8], dst: &mut [u8]) -> Result<usize, CobsError> {
    if dst.len() < max_encoded_len(src.len()) {
        return Err(CobsError::DestTooSmall);
    }
    Ok(cobs::encode(src, dst))
}

/// COBS-decode `src` (which must **not** include the trailing `0x00`) into `dst`.
pub fn cobs_decode(src: &[u8], dst: &mut [u8]) -> Result<usize, CobsError> {
    if dst.len() < src.len() {
        return Err(CobsError::DestTooSmall);
    }
    let report = cobs::decode(src, dst).map_err(|_| CobsError::Malformed)?;
    // A well-formed encoded region is consumed entirely; a short read means a code byte
    // pointed past the end, which the `cobs` crate tolerates but we must not.
    if report.parsed_size() != src.len() {
        return Err(CobsError::Malformed);
    }
    Ok(report.frame_size())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors from Cheshire & Baker, "Consistent Overhead Byte Stuffing" (SIGCOMM '97),
    /// table 1. Encoded forms here exclude the trailing delimiter.
    #[test]
    fn published_vectors() {
        let cases: &[(&[u8], &[u8])] = &[
            (&[0x00], &[0x01, 0x01]),
            (&[0x00, 0x00], &[0x01, 0x01, 0x01]),
            (&[0x11, 0x22, 0x00, 0x33], &[0x03, 0x11, 0x22, 0x02, 0x33]),
            (&[0x11, 0x22, 0x33, 0x44], &[0x05, 0x11, 0x22, 0x33, 0x44]),
            (&[0x11, 0x00, 0x00, 0x00], &[0x02, 0x11, 0x01, 0x01, 0x01]),
        ];
        let mut enc = [0u8; 64];
        let mut dec = [0u8; 64];
        for (plain, expected) in cases {
            let n = cobs_encode(plain, &mut enc).unwrap();
            assert_eq!(&enc[..n], *expected, "encoding {plain:02x?}");
            let m = cobs_decode(&enc[..n], &mut dec).unwrap();
            assert_eq!(&dec[..m], *plain, "decoding {expected:02x?}");
        }
    }

    #[test]
    fn encoded_never_contains_zero() {
        let mut enc = [0u8; 1024];
        for len in 0..300usize {
            let src: heapless_vec::Buf = heapless_vec::Buf::ramp(len);
            let n = cobs_encode(src.as_slice(), &mut enc).unwrap();
            assert!(
                !enc[..n].contains(&0x00),
                "len {len} produced an embedded zero"
            );
        }
    }

    /// Tiny fixed-capacity helper so this test needs no allocator.
    mod heapless_vec {
        pub struct Buf {
            data: [u8; 300],
            len: usize,
        }
        impl Buf {
            /// A buffer of `len` bytes cycling 0x00..0xFF, so it is full of delimiters.
            pub fn ramp(len: usize) -> Self {
                let mut data = [0u8; 300];
                for (i, b) in data.iter_mut().enumerate().take(len) {
                    *b = (i % 256) as u8;
                }
                Buf { data, len }
            }
            pub fn as_slice(&self) -> &[u8] {
                &self.data[..self.len]
            }
        }
    }

    #[test]
    fn dest_too_small_is_reported() {
        let mut tiny = [0u8; 2];
        assert_eq!(
            cobs_encode(&[1, 2, 3, 4], &mut tiny),
            Err(CobsError::DestTooSmall)
        );
    }
}
