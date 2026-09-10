//! On-disk header, chunk header, and footer layouts.
//!
//! Every field is little-endian. The file header is 128 bytes and the chunk header is 40, and
//! **both sizes are frozen**. Together with the rule that a reader skips unknown chunk kinds
//! using `stored_len`, that is what makes a minor-version bump safe: a v1.0 reader can walk a
//! v1.1 file it does not fully understand.

use std::path::Path;

use wattson_protocol::crc32::checksum;

use super::{
    CHUNK_HEADER_LEN, CHUNK_MAGIC, FILE_MAGIC, FOOTER_LEN, FOOTER_MAGIC, FORMAT_MAJOR,
    FORMAT_MINOR, HEADER_LEN,
};
use crate::error::CaptureError;

// ---------------------------------------------------------------------------
// small endian helpers
// ---------------------------------------------------------------------------

#[inline]
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[inline]
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[inline]
fn i32le(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[inline]
fn u64le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

#[inline]
fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn puti32(b: &mut [u8], o: usize, v: i32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// compression
// ---------------------------------------------------------------------------

/// How chunk payloads are stored.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
#[repr(u32)]
pub enum Compression {
    None = 0,
    /// zstd level 3, one independent frame per chunk.
    #[default]
    Zstd = 1,
    /// Pure-Rust fallback for builds without a C compiler.
    Lz4 = 2,
}

impl Compression {
    pub const fn from_u32(v: u32) -> Option<Compression> {
        Some(match v {
            0 => Compression::None,
            1 => Compression::Zstd,
            2 => Compression::Lz4,
            _ => return None,
        })
    }

    pub const fn name(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd => "zstd",
            Compression::Lz4 => "lz4",
        }
    }
}

impl std::str::FromStr for Compression {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Compression::None),
            "zstd" => Ok(Compression::Zstd),
            "lz4" => Ok(Compression::Lz4),
            other => Err(format!(
                "unknown compression {other:?}; expected none, zstd, or lz4"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// file header
// ---------------------------------------------------------------------------

/// Bit flags in [`CaptureHeader::flags`].
#[derive(Debug)]
pub struct HeaderFlags;

impl HeaderFlags {
    /// The writer completed cleanly: an index and footer are present.
    pub const FINALIZED: u32 = 1 << 0;
    pub const HAS_VOLTAGE: u32 = 1 << 1;
    pub const HAS_GPIO: u32 = 1 << 2;
    pub const HAS_EVENTS: u32 = 1 << 3;
    /// The samples came from a simulator, not from measurement hardware.
    ///
    /// Recorded so a synthetic capture can never be mistaken for a measurement of real
    /// hardware — which matters the moment someone pastes a number into a bug report.
    pub const SYNTHETIC: u32 = 1 << 4;
}

/// The 128-byte file header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureHeader {
    pub format_major: u16,
    pub format_minor: u16,
    /// Host wall clock at capture start, nanoseconds since the Unix epoch. For humans and
    /// for correlating with external logs — never for a duration.
    pub created_unix_ns: u64,
    /// Host monotonic reference at capture start.
    pub capture_start_mono_ns: u64,
    pub device_timer_hz: u32,
    /// The rate that was *requested*. What actually happened is derived from timestamps.
    pub sample_rate_hz: u32,
    pub flags: u32,
    pub compression: Compression,
    /// NUL-padded ASCII.
    pub device_serial: [u8; 16],
    /// NUL-padded ASCII.
    pub fw_version: [u8; 16],
    pub fw_build_id: u64,
    pub shunt_micro_ohm: i32,
    pub current_offset_ua: i32,
    pub current_gain_num: u32,
    pub current_gain_den: u32,
    /// Byte offset of the footer, or 0 while the capture is still open.
    pub footer_offset: u64,
    pub target_chunk_bytes: u32,
}

impl Default for CaptureHeader {
    fn default() -> Self {
        CaptureHeader {
            format_major: FORMAT_MAJOR,
            format_minor: FORMAT_MINOR,
            created_unix_ns: 0,
            capture_start_mono_ns: 0,
            device_timer_hz: wattson_protocol::DEFAULT_TIMER_HZ,
            sample_rate_hz: 0,
            flags: 0,
            compression: Compression::default(),
            device_serial: [0; 16],
            fw_version: [0; 16],
            fw_build_id: 0,
            shunt_micro_ohm: 0,
            current_offset_ua: 0,
            current_gain_num: 1,
            current_gain_den: 1,
            footer_offset: 0,
            target_chunk_bytes: super::DEFAULT_CHUNK_BYTES,
        }
    }
}

impl CaptureHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&FILE_MAGIC);
        put16(&mut b, 8, self.format_major);
        put16(&mut b, 10, self.format_minor);
        put32(&mut b, 12, HEADER_LEN as u32);
        put64(&mut b, 16, self.created_unix_ns);
        put64(&mut b, 24, self.capture_start_mono_ns);
        put32(&mut b, 32, self.device_timer_hz);
        put32(&mut b, 36, self.sample_rate_hz);
        put32(&mut b, 40, self.flags);
        put32(&mut b, 44, self.compression as u32);
        b[48..64].copy_from_slice(&self.device_serial);
        b[64..80].copy_from_slice(&self.fw_version);
        put64(&mut b, 80, self.fw_build_id);
        puti32(&mut b, 88, self.shunt_micro_ohm);
        puti32(&mut b, 92, self.current_offset_ua);
        put32(&mut b, 96, self.current_gain_num);
        put32(&mut b, 100, self.current_gain_den);
        put64(&mut b, 104, self.footer_offset);
        put32(&mut b, 112, self.target_chunk_bytes);
        // bytes 116..124 reserved, left zero for additive v1.x fields
        let crc = checksum(&b[..124]);
        put32(&mut b, 124, crc);
        b
    }

    pub fn decode(b: &[u8], path: &Path) -> Result<CaptureHeader, CaptureError> {
        if b.len() < HEADER_LEN || b[0..8] != FILE_MAGIC {
            return Err(CaptureError::NotACapture {
                path: path.to_path_buf(),
            });
        }
        let expected = u32le(b, 124);
        if checksum(&b[..124]) != expected {
            return Err(CaptureError::Corrupt {
                path: path.to_path_buf(),
                what: "file header",
            });
        }
        let format_major = u16le(b, 8);
        let format_minor = u16le(b, 10);
        if format_major != FORMAT_MAJOR {
            return Err(CaptureError::IncompatibleVersion {
                path: path.to_path_buf(),
                major: format_major,
                minor: format_minor,
                supported_major: FORMAT_MAJOR,
            });
        }
        let compression_raw = u32le(b, 44);
        let compression = Compression::from_u32(compression_raw)
            .ok_or(CaptureError::UnsupportedCompression(compression_raw))?;

        let mut device_serial = [0u8; 16];
        device_serial.copy_from_slice(&b[48..64]);
        let mut fw_version = [0u8; 16];
        fw_version.copy_from_slice(&b[64..80]);

        Ok(CaptureHeader {
            format_major,
            format_minor,
            created_unix_ns: u64le(b, 16),
            capture_start_mono_ns: u64le(b, 24),
            device_timer_hz: u32le(b, 32),
            sample_rate_hz: u32le(b, 36),
            flags: u32le(b, 40),
            compression,
            device_serial,
            fw_version,
            fw_build_id: u64le(b, 80),
            shunt_micro_ohm: i32le(b, 88),
            current_offset_ua: i32le(b, 92),
            current_gain_num: u32le(b, 96),
            current_gain_den: u32le(b, 100),
            footer_offset: u64le(b, 104),
            target_chunk_bytes: u32le(b, 112),
        })
    }

    #[inline]
    pub const fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    pub fn serial_str(&self) -> &str {
        wattson_protocol::types::ascii_str(&self.device_serial)
    }

    pub fn fw_version_str(&self) -> &str {
        wattson_protocol::types::ascii_str(&self.fw_version)
    }

    /// Apply the stored calibration to a raw reading.
    pub fn calibrate_ua(&self, raw_ua: i32) -> i32 {
        let num = self.current_gain_num.max(1) as i64;
        let den = self.current_gain_den.max(1) as i64;
        (((raw_ua as i64 - self.current_offset_ua as i64) * num) / den) as i32
    }
}

// ---------------------------------------------------------------------------
// chunks
// ---------------------------------------------------------------------------

/// What a chunk holds.
///
/// A reader **must** skip kinds it does not recognise, using `stored_len`. That rule plus the
/// frozen 40-byte header is the whole forward-compatibility story.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[repr(u16)]
pub enum ChunkKind {
    Samples = 0x0001,
    Events = 0x0002,
    Gpio = 0x0003,
    Markers = 0x0004,
    Sync = 0x0005,
    Metadata = 0x0010,
    Annotations = 0x0011,
    Summary = 0x0020,
    Index = 0x00F0,
    End = 0x00FF,
    /// A kind this build does not know. Carries its raw value so it can be skipped and
    /// reported rather than guessed at.
    Unknown(u16),
}

impl ChunkKind {
    pub const fn from_u16(v: u16) -> ChunkKind {
        match v {
            0x0001 => ChunkKind::Samples,
            0x0002 => ChunkKind::Events,
            0x0003 => ChunkKind::Gpio,
            0x0004 => ChunkKind::Markers,
            0x0005 => ChunkKind::Sync,
            0x0010 => ChunkKind::Metadata,
            0x0011 => ChunkKind::Annotations,
            0x0020 => ChunkKind::Summary,
            0x00F0 => ChunkKind::Index,
            0x00FF => ChunkKind::End,
            other => ChunkKind::Unknown(other),
        }
    }

    pub const fn as_u16(self) -> u16 {
        match self {
            ChunkKind::Samples => 0x0001,
            ChunkKind::Events => 0x0002,
            ChunkKind::Gpio => 0x0003,
            ChunkKind::Markers => 0x0004,
            ChunkKind::Sync => 0x0005,
            ChunkKind::Metadata => 0x0010,
            ChunkKind::Annotations => 0x0011,
            ChunkKind::Summary => 0x0020,
            ChunkKind::Index => 0x00F0,
            ChunkKind::End => 0x00FF,
            ChunkKind::Unknown(v) => v,
        }
    }

    /// `true` if this chunk carries time-ordered records, so its time bounds are meaningful.
    pub const fn is_time_bearing(self) -> bool {
        matches!(
            self,
            ChunkKind::Samples
                | ChunkKind::Events
                | ChunkKind::Gpio
                | ChunkKind::Markers
                | ChunkKind::Sync
        )
    }
}

/// Flag bits in a chunk header.
#[derive(Debug)]
pub struct ChunkFlags;

impl ChunkFlags {
    pub const COMPRESSED: u16 = 1 << 0;
}

/// Sentinel for a chunk that carries no time bounds.
pub const NO_TIME: u64 = u64::MAX;

/// The 40-byte chunk header. Frozen forever.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ChunkHeader {
    pub kind: ChunkKind,
    pub flags: u16,
    pub uncompressed_len: u32,
    /// Bytes following this header.
    pub stored_len: u32,
    /// Capture-relative nanoseconds of the first record, or [`NO_TIME`].
    pub first_time_ns: u64,
    /// Capture-relative nanoseconds of the last record, or [`NO_TIME`].
    pub last_time_ns: u64,
    pub payload_crc32: u32,
}

impl ChunkHeader {
    pub fn encode(&self) -> [u8; CHUNK_HEADER_LEN] {
        let mut b = [0u8; CHUNK_HEADER_LEN];
        put32(&mut b, 0, CHUNK_MAGIC);
        put16(&mut b, 4, self.kind.as_u16());
        put16(&mut b, 6, self.flags);
        put32(&mut b, 8, self.uncompressed_len);
        put32(&mut b, 12, self.stored_len);
        put64(&mut b, 16, self.first_time_ns);
        put64(&mut b, 24, self.last_time_ns);
        put32(&mut b, 32, self.payload_crc32);
        let crc = checksum(&b[..36]);
        put32(&mut b, 36, crc);
        b
    }

    /// Decode a chunk header, returning `None` if it is not a valid one.
    ///
    /// `None` rather than an error because the recovery scan calls this on arbitrary offsets
    /// and a failure there simply means "stop, this is where the file ends".
    pub fn decode(b: &[u8]) -> Option<ChunkHeader> {
        if b.len() < CHUNK_HEADER_LEN || u32le(b, 0) != CHUNK_MAGIC {
            return None;
        }
        if checksum(&b[..36]) != u32le(b, 36) {
            return None;
        }
        Some(ChunkHeader {
            kind: ChunkKind::from_u16(u16le(b, 4)),
            flags: u16le(b, 6),
            uncompressed_len: u32le(b, 8),
            stored_len: u32le(b, 12),
            first_time_ns: u64le(b, 16),
            last_time_ns: u64le(b, 24),
            payload_crc32: u32le(b, 32),
        })
    }

    #[inline]
    pub const fn is_compressed(&self) -> bool {
        self.flags & ChunkFlags::COMPRESSED != 0
    }

    /// Total bytes this chunk occupies, header included.
    #[inline]
    pub const fn total_len(&self) -> u64 {
        CHUNK_HEADER_LEN as u64 + self.stored_len as u64
    }
}

// ---------------------------------------------------------------------------
// footer
// ---------------------------------------------------------------------------

/// The 32-byte footer written at close.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Footer {
    pub index_offset: u64,
    pub index_len: u64,
    pub total_samples: u64,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut b = [0u8; FOOTER_LEN];
        put64(&mut b, 0, self.index_offset);
        put64(&mut b, 8, self.index_len);
        put64(&mut b, 16, self.total_samples);
        let crc = checksum(&b[..24]);
        put32(&mut b, 24, crc);
        put32(&mut b, 28, FOOTER_MAGIC);
        b
    }

    /// Decode a footer, returning `None` if it is absent or damaged.
    ///
    /// A missing footer is the normal state of a capture killed mid-write, not an error: the
    /// reader falls back to a forward scan.
    pub fn decode(b: &[u8]) -> Option<Footer> {
        if b.len() < FOOTER_LEN || u32le(b, 28) != FOOTER_MAGIC {
            return None;
        }
        if checksum(&b[..24]) != u32le(b, 24) {
            return None;
        }
        Some(Footer {
            index_offset: u64le(b, 0),
            index_len: u64le(b, 8),
            total_samples: u64le(b, 16),
        })
    }
}

/// One entry in the index chunk: 32 bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct IndexEntry {
    pub kind: ChunkKind,
    pub flags: u16,
    pub file_offset: u64,
    pub stored_len: u32,
    pub record_count: u32,
    pub first_time_ns: u64,
}

impl IndexEntry {
    pub const LEN: usize = 32;

    pub fn encode_into(&self, b: &mut [u8]) {
        put16(b, 0, self.kind.as_u16());
        put16(b, 2, self.flags);
        put64(b, 4, self.file_offset);
        put32(b, 12, self.stored_len);
        put32(b, 16, self.record_count);
        put64(b, 20, self.first_time_ns);
        // bytes 28..32 reserved
        put32(b, 28, 0);
    }

    pub fn decode(b: &[u8]) -> Option<IndexEntry> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(IndexEntry {
            kind: ChunkKind::from_u16(u16le(b, 0)),
            flags: u16le(b, 2),
            file_offset: u64le(b, 4),
            stored_len: u32le(b, 12),
            record_count: u32le(b, 16),
            first_time_ns: u64le(b, 20),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_header() -> CaptureHeader {
        CaptureHeader {
            created_unix_ns: 1_770_000_000_000_000_000,
            capture_start_mono_ns: 12_345,
            device_timer_hz: 1_000_000,
            sample_rate_hz: 50_000,
            flags: HeaderFlags::HAS_VOLTAGE | HeaderFlags::HAS_EVENTS,
            compression: Compression::Zstd,
            device_serial: *b"WS-0001\0\0\0\0\0\0\0\0\0",
            fw_version: *b"0.1.0\0\0\0\0\0\0\0\0\0\0\0",
            fw_build_id: 0xDEAD_BEEF,
            shunt_micro_ohm: 100_000,
            current_offset_ua: -12,
            current_gain_num: 1001,
            current_gain_den: 1000,
            footer_offset: 4096,
            target_chunk_bytes: 65_536,
            ..Default::default()
        }
    }

    #[test]
    fn file_header_round_trips() {
        let h = sample_header();
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        let back = CaptureHeader::decode(&bytes, Path::new("x.pprof")).unwrap();
        assert_eq!(back, h);
        assert_eq!(back.serial_str(), "WS-0001");
        assert_eq!(back.fw_version_str(), "0.1.0");
    }

    #[test]
    fn a_corrupt_header_is_reported_not_believed() {
        let mut bytes = sample_header().encode();
        bytes[36] ^= 0xFF;
        assert!(matches!(
            CaptureHeader::decode(&bytes, Path::new("x.pprof")),
            Err(CaptureError::Corrupt {
                what: "file header",
                ..
            })
        ));
    }

    #[test]
    fn a_non_capture_is_rejected_by_magic() {
        let bytes = [0u8; HEADER_LEN];
        assert!(matches!(
            CaptureHeader::decode(&bytes, Path::new("x.pprof")),
            Err(CaptureError::NotACapture { .. })
        ));
    }

    /// A future major version must be refused with a message naming the version that wrote
    /// it, not silently misread.
    #[test]
    fn a_future_major_version_is_refused_by_name() {
        let mut h = sample_header();
        h.format_major = 2;
        h.format_minor = 3;
        let bytes = h.encode();
        match CaptureHeader::decode(&bytes, Path::new("future.pprof")) {
            Err(CaptureError::IncompatibleVersion {
                major,
                minor,
                supported_major,
                path,
            }) => {
                assert_eq!((major, minor, supported_major), (2, 3, FORMAT_MAJOR));
                assert_eq!(path, PathBuf::from("future.pprof"));
            }
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
    }

    /// A *minor* bump must still open: that is the entire point of the split.
    #[test]
    fn a_future_minor_version_still_opens() {
        let mut h = sample_header();
        h.format_minor = 7;
        let bytes = h.encode();
        let back = CaptureHeader::decode(&bytes, Path::new("x.pprof")).unwrap();
        assert_eq!(back.format_minor, 7);
    }

    #[test]
    fn chunk_header_round_trips() {
        let c = ChunkHeader {
            kind: ChunkKind::Samples,
            flags: ChunkFlags::COMPRESSED,
            uncompressed_len: 65_536,
            stored_len: 12_345,
            first_time_ns: 1_000,
            last_time_ns: 165_000_000,
            payload_crc32: 0x1234_5678,
        };
        let bytes = c.encode();
        assert_eq!(bytes.len(), CHUNK_HEADER_LEN);
        assert_eq!(ChunkHeader::decode(&bytes), Some(c));
        assert!(c.is_compressed());
        assert_eq!(c.total_len(), CHUNK_HEADER_LEN as u64 + 12_345);
    }

    #[test]
    fn a_damaged_chunk_header_decodes_to_none_so_a_scan_can_stop() {
        let c = ChunkHeader {
            kind: ChunkKind::Events,
            flags: 0,
            uncompressed_len: 10,
            stored_len: 10,
            first_time_ns: NO_TIME,
            last_time_ns: NO_TIME,
            payload_crc32: 0,
        };
        let mut bytes = c.encode();
        bytes[8] ^= 0x01;
        assert_eq!(ChunkHeader::decode(&bytes), None);
        assert_eq!(ChunkHeader::decode(&[0u8; CHUNK_HEADER_LEN]), None);
        assert_eq!(ChunkHeader::decode(&[]), None);
    }

    /// The forward-compatibility contract, stated as a test.
    #[test]
    fn unknown_chunk_kinds_survive_a_round_trip_so_they_can_be_skipped() {
        let c = ChunkHeader {
            kind: ChunkKind::Unknown(0x0777),
            flags: 0,
            uncompressed_len: 4,
            stored_len: 4,
            first_time_ns: 5,
            last_time_ns: 9,
            payload_crc32: 7,
        };
        let back = ChunkHeader::decode(&c.encode()).unwrap();
        assert_eq!(back.kind, ChunkKind::Unknown(0x0777));
        assert_eq!(
            back.stored_len, 4,
            "stored_len is what lets a reader skip past it"
        );
    }

    #[test]
    fn every_known_chunk_kind_round_trips() {
        for k in [
            ChunkKind::Samples,
            ChunkKind::Events,
            ChunkKind::Gpio,
            ChunkKind::Markers,
            ChunkKind::Sync,
            ChunkKind::Metadata,
            ChunkKind::Annotations,
            ChunkKind::Summary,
            ChunkKind::Index,
            ChunkKind::End,
        ] {
            assert_eq!(ChunkKind::from_u16(k.as_u16()), k);
        }
    }

    #[test]
    fn footer_round_trips_and_rejects_damage() {
        let f = Footer {
            index_offset: 1024,
            index_len: 256,
            total_samples: 999_999,
        };
        let bytes = f.encode();
        assert_eq!(Footer::decode(&bytes), Some(f));

        let mut damaged = bytes;
        damaged[4] ^= 0xFF;
        assert_eq!(Footer::decode(&damaged), None);

        // A capture killed mid-write simply has no footer. That is not an error.
        assert_eq!(Footer::decode(&[0u8; FOOTER_LEN]), None);
    }

    #[test]
    fn index_entries_round_trip() {
        let e = IndexEntry {
            kind: ChunkKind::Samples,
            flags: 0,
            file_offset: 128,
            stored_len: 4096,
            record_count: 8192,
            first_time_ns: 0,
        };
        let mut b = [0u8; IndexEntry::LEN];
        e.encode_into(&mut b);
        assert_eq!(IndexEntry::decode(&b), Some(e));
    }

    #[test]
    fn calibration_is_applied_as_offset_then_gain() {
        let mut h = CaptureHeader {
            current_offset_ua: 100,
            ..Default::default()
        };
        h.current_gain_num = 2;
        h.current_gain_den = 1;
        assert_eq!(h.calibrate_ua(300), 400);
        // A zero denominator must not divide by zero.
        h.current_gain_den = 0;
        assert_eq!(h.calibrate_ua(300), 400);
    }

    #[test]
    fn compression_names_round_trip() {
        for c in [Compression::None, Compression::Zstd, Compression::Lz4] {
            assert_eq!(c.name().parse::<Compression>().unwrap(), c);
            assert_eq!(Compression::from_u32(c as u32), Some(c));
        }
        assert!("gzip".parse::<Compression>().is_err());
        assert_eq!(Compression::from_u32(99), None);
    }
}
