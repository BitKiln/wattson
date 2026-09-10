//! The `.pprof` capture format.
//!
//! Design constraints, in priority order:
//!
//! 1. **Appendable while capturing at 50 ksps.** Nothing may require rewriting earlier bytes.
//! 2. **Seekable by time**, so a zoomable UI can jump to a span without reading the file.
//! 3. **Readable when truncated.** Ctrl-C, a crash, or a yanked USB cable must cost at most
//!    the chunk in flight — not the forty minutes before it.
//! 4. **Compressed**, because 50 ksps for an hour is 2 GB uncompressed.
//! 5. **Additively extensible**, so a v1.1 writer's files still open in a v1.0 reader.
//!
//! # Structure
//!
//! ```text
//! [ FileHeader   128 B, uncompressed, frozen ]
//! [ Chunk ]*                 appended during capture
//! [ INDEX chunk ]            written at close
//! [ Footer       32 B ]      written at close
//! ```
//!
//! # Why structure-of-arrays
//!
//! Sample chunks store all the timestamps, then all the currents, then all the voltages —
//! not interleaved records. Adjacent current readings in a real trace differ by tens of
//! microamps out of a 32-bit word, so column-major layout puts long runs of identical high
//! bytes next to each other. That is worth roughly 3-6x under zstd against about 1.5x for
//! interleaved.
//!
//! # Why one zstd frame per chunk
//!
//! No dictionary, no streaming state carried across chunks, so any chunk decodes standalone.
//! That is precisely what makes random seek possible; a single stream compressed end to end
//! would force a full scan to reach the last second of a capture.

use crate::error::CaptureError;

pub mod header;
pub mod payload;
pub mod reader;
pub mod summary;
pub mod writer;

pub use header::{CaptureHeader, ChunkHeader, ChunkKind, Compression, Footer, HeaderFlags};
pub use reader::{CaptureReader, IntegrityReport, ReadSample, StoredEvent, StoredGpio};
pub use summary::{Bucket, SummaryLevel, SummaryPyramid};
pub use writer::{CaptureWriter, WriterOptions};

/// Format version this build writes.
pub const FORMAT_MAJOR: u16 = 1;
/// Minor version this build writes. Bumped only for additive changes.
pub const FORMAT_MINOR: u16 = 0;

/// File magic: `PPROFCAP`.
pub const FILE_MAGIC: [u8; 8] = *b"PPROFCAP";
/// Chunk magic, `"CHNK"` little-endian.
pub const CHUNK_MAGIC: u32 = u32::from_le_bytes(*b"CHNK");
/// Footer magic, `"PEND"` little-endian.
pub const FOOTER_MAGIC: u32 = u32::from_le_bytes(*b"PEND");

/// Size of the file header. Frozen for the life of format v1.
pub const HEADER_LEN: usize = 128;
/// Size of a chunk header. Frozen forever — this plus "skip unknown kinds" is what makes
/// minor-version bumps safe.
pub const CHUNK_HEADER_LEN: usize = 40;
/// Size of the footer.
pub const FOOTER_LEN: usize = 32;

/// Default uncompressed bytes per chunk.
///
/// 64 KiB is about 8192 samples with voltage, or 164 ms at 50 ksps. That gives roughly six
/// file writes per second, zstd level 3 finishes well inside a millisecond, and seek
/// granularity is finer than one pixel at a whole-capture zoom.
///
/// This is an educated guess, which is exactly why it is stored in the file header rather
/// than hardcoded: it can be re-tuned after measurement without a format change.
pub const DEFAULT_CHUNK_BYTES: u32 = 64 * 1024;

/// A stretch of missing data.
///
/// Gaps are recorded explicitly and never silently absorbed. Integrating across a gap
/// produces a plausible-looking but wrong energy figure, which is the worst failure mode this
/// project has: a CI gate that passes because data went missing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Gap {
    pub start_ns: u64,
    pub end_ns: u64,
    pub cause: GapCause,
    /// Best estimate of how many samples were lost.
    pub lost_estimate: u64,
}

impl Gap {
    pub const fn duration_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }
}

/// Why data is missing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GapCause {
    /// The device reported a buffer overflow.
    DeviceOverflow,
    /// A frame sequence number skipped.
    SequenceGap,
    /// The host could not keep up and dropped a batch.
    HostOverrun,
    /// The device timestamp jumped further than one sample period.
    TimestampJump,
    /// The file was truncated at this point.
    Truncation,
}

impl GapCause {
    pub const fn describe(self) -> &'static str {
        match self {
            GapCause::DeviceOverflow => "device buffer overflow",
            GapCause::SequenceGap => "dropped frame",
            GapCause::HostOverrun => "host could not keep up",
            GapCause::TimestampJump => "timestamp discontinuity",
            GapCause::Truncation => "file truncated",
        }
    }
}

/// What to do when a statistics span crosses a gap.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum GapPolicy {
    /// Refuse to compute. The default for `assert`, because a CI gate must never pass by
    /// silently integrating over missing data.
    #[default]
    Error,
    /// Compute over the data that exists and report what was skipped.
    Skip,
    /// Linearly interpolate across the gap. Convenient for eyeballing, never for asserting.
    Interpolate,
}

/// Summary of a completed capture.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CaptureSummary {
    pub sample_count: u64,
    pub event_count: u64,
    pub gpio_count: u64,
    pub duration_ns: u64,
    /// Rate derived from actual timestamps, not the configured value.
    ///
    /// `CONFIG.sample_rate_hz` is what was asked for; this is what happened. They differ, and
    /// only one of them is safe to integrate with.
    pub effective_rate_hz: f64,
    pub gaps: Vec<Gap>,
    pub bytes_written: u64,
    /// Uncompressed payload bytes, so a compression ratio can be reported.
    pub bytes_uncompressed: u64,
}

impl CaptureSummary {
    pub fn compression_ratio(&self) -> f64 {
        if self.bytes_written == 0 {
            1.0
        } else {
            self.bytes_uncompressed as f64 / self.bytes_written as f64
        }
    }

    pub fn duration_s(&self) -> f64 {
        self.duration_ns as f64 / 1e9
    }

    /// Total time covered by gaps.
    pub fn lost_ns(&self) -> u64 {
        self.gaps.iter().map(Gap::duration_ns).sum()
    }
}

/// Compress a payload, or pass it through.
pub(crate) fn compress(
    compression: Compression,
    data: &[u8],
) -> Result<std::borrow::Cow<'_, [u8]>, CaptureError> {
    use std::borrow::Cow;
    match compression {
        Compression::None => Ok(Cow::Borrowed(data)),
        #[cfg(feature = "zstd")]
        Compression::Zstd => {
            zstd::bulk::compress(data, 3)
                .map(Cow::Owned)
                .map_err(|e| CaptureError::Compression {
                    op: "compress",
                    detail: e.to_string(),
                })
        }
        #[cfg(not(feature = "zstd"))]
        Compression::Zstd => Err(CaptureError::UnsupportedCompression(
            Compression::Zstd as u32,
        )),
        #[cfg(feature = "lz4")]
        Compression::Lz4 => Ok(Cow::Owned(lz4_flex::compress_prepend_size(data))),
        #[cfg(not(feature = "lz4"))]
        Compression::Lz4 => Err(CaptureError::UnsupportedCompression(
            Compression::Lz4 as u32,
        )),
    }
}

/// Decompress a payload whose uncompressed size is known from the chunk header.
pub(crate) fn decompress(
    compression: Compression,
    data: &[u8],
    uncompressed_len: usize,
) -> Result<std::borrow::Cow<'_, [u8]>, CaptureError> {
    use std::borrow::Cow;
    match compression {
        Compression::None => Ok(Cow::Borrowed(data)),
        #[cfg(feature = "zstd")]
        Compression::Zstd => zstd::bulk::decompress(data, uncompressed_len)
            .map(Cow::Owned)
            .map_err(|e| CaptureError::Compression {
                op: "decompress",
                detail: e.to_string(),
            }),
        #[cfg(not(feature = "zstd"))]
        Compression::Zstd => {
            let _ = uncompressed_len;
            Err(CaptureError::UnsupportedCompression(
                Compression::Zstd as u32,
            ))
        }
        #[cfg(feature = "lz4")]
        Compression::Lz4 => lz4_flex::decompress_size_prepended(data)
            .map(Cow::Owned)
            .map_err(|e| CaptureError::Compression {
                op: "decompress",
                detail: e.to_string(),
            }),
        #[cfg(not(feature = "lz4"))]
        Compression::Lz4 => {
            let _ = uncompressed_len;
            Err(CaptureError::UnsupportedCompression(
                Compression::Lz4 as u32,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magics_are_what_the_spec_says() {
        assert_eq!(&FILE_MAGIC, b"PPROFCAP");
        assert_eq!(CHUNK_MAGIC.to_le_bytes(), *b"CHNK");
        assert_eq!(FOOTER_MAGIC.to_le_bytes(), *b"PEND");
    }

    #[test]
    fn compression_round_trips() {
        let data: Vec<u8> = (0..10_000u32).flat_map(|i| (i / 7).to_le_bytes()).collect();
        for c in [Compression::None, Compression::Zstd] {
            let packed = compress(c, &data).expect("compress");
            let back = decompress(c, &packed, data.len()).expect("decompress");
            assert_eq!(back.as_ref(), data.as_slice(), "{c:?} did not round-trip");
        }
    }

    /// The reason for structure-of-arrays, measured rather than asserted by faith.
    #[test]
    fn column_major_layout_compresses_far_better_than_interleaved() {
        // A realistic-ish trace: slowly varying current, near-constant voltage.
        let n = 8192usize;
        let currents: Vec<i32> = (0..n)
            .map(|i| 3_000 + ((i as f64 / 50.0).sin() * 40.0) as i32)
            .collect();
        let voltages: Vec<u32> = (0..n).map(|i| 3_300_000 - (i % 17) as u32).collect();

        let mut soa = Vec::new();
        soa.extend(currents.iter().flat_map(|c| c.to_le_bytes()));
        soa.extend(voltages.iter().flat_map(|v| v.to_le_bytes()));

        let mut aos = Vec::new();
        for i in 0..n {
            aos.extend(currents[i].to_le_bytes());
            aos.extend(voltages[i].to_le_bytes());
        }
        assert_eq!(soa.len(), aos.len());

        let soa_packed = compress(Compression::Zstd, &soa).unwrap().len();
        let aos_packed = compress(Compression::Zstd, &aos).unwrap().len();
        assert!(
            soa_packed < aos_packed,
            "structure-of-arrays ({soa_packed} B) should beat interleaved ({aos_packed} B)"
        );
    }

    #[test]
    fn summary_reports_loss_and_ratio() {
        let s = CaptureSummary {
            sample_count: 1000,
            duration_ns: 1_000_000_000,
            gaps: vec![
                Gap {
                    start_ns: 10,
                    end_ns: 110,
                    cause: GapCause::SequenceGap,
                    lost_estimate: 5,
                },
                Gap {
                    start_ns: 500,
                    end_ns: 700,
                    cause: GapCause::DeviceOverflow,
                    lost_estimate: 10,
                },
            ],
            bytes_written: 250,
            bytes_uncompressed: 1000,
            ..Default::default()
        };
        assert_eq!(s.lost_ns(), 300);
        assert_eq!(s.compression_ratio(), 4.0);
        assert_eq!(s.duration_s(), 1.0);
    }

    /// The default must be the safe one: a CI gate that silently integrates across missing
    /// data is worse than one that fails.
    #[test]
    fn the_default_gap_policy_refuses_to_guess() {
        assert_eq!(GapPolicy::default(), GapPolicy::Error);
    }
}
