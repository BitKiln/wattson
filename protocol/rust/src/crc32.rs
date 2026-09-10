//! CRC-32/ISO-HDLC.
//!
//! Chosen over CRC-16 deliberately: the 2 extra bytes are 0.25% overhead at realistic frame
//! sizes, while both candidate profiler MCUs accelerate this exact polynomial in hardware —
//! the RP2040 DMA sniffer computes CRC-32 inline with the transfer at zero CPU cost, and the
//! STM32 CRC peripheral is hardwired to `0x04C11DB7`. A CRC-16 would force a software loop
//! on both.
//!
//! Parameters: poly `0x04C11DB7` reflected, init `0xFFFF_FFFF`, xorout `0xFFFF_FFFF`.
//! Check value over `b"123456789"` is `0xCBF4_3926`.

use crc::{CRC_32_ISO_HDLC, Crc};

const CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

/// CRC-32/ISO-HDLC over `data`.
#[inline]
pub fn checksum(data: &[u8]) -> u32 {
    CRC.checksum(data)
}

/// Incremental digest, for callers that build a frame in pieces.
pub struct Digest(crc::Digest<'static, u32>);

impl Digest {
    #[inline]
    pub fn new() -> Self {
        Digest(CRC.digest())
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    #[inline]
    pub fn finalize(self) -> u32 {
        self.0.finalize()
    }
}

impl Default for Digest {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published check value for CRC-32/ISO-HDLC. If this fails, every golden vector
    /// and every capture file in existence is wrong.
    #[test]
    fn check_value() {
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(checksum(b""), 0);
    }

    #[test]
    fn incremental_matches_oneshot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut d = Digest::new();
        d.update(&data[..10]);
        d.update(&data[10..]);
        assert_eq!(d.finalize(), checksum(data));
    }
}
