//! Wattson wire protocol.
//!
//! This crate is `no_std` and dependency-thin on purpose: phase-2 profiler firmware links
//! *this same crate* (Rust firmware) or is validated against the *same golden byte vectors*
//! (C firmware). Adding `std`, `serde`, an allocator, or an async runtime here is a tax paid
//! by every future firmware target, so don't.
//!
//! # Framing
//!
//! A logical frame is little-endian:
//!
//! ```text
//! off  size  field
//! 0    1     TYPE   u8
//! 1    2     LEN    u16   payload length, 0..=1024
//! 3    1     SEQ    u8    per-direction sequence, wraps 255 -> 0
//! 4    LEN   PAYLOAD
//! 4+L  4     CRC32  u32   CRC-32/ISO-HDLC over bytes [0 .. 4+LEN)
//! ```
//!
//! The whole logical frame is then COBS-encoded and a `0x00` delimiter appended. COBS
//! guarantees that `0x00` never occurs inside an encoded frame, which is what lets a host
//! resynchronise unambiguously after corruption or when attaching to an already-running
//! device mid-stream.
//!
//! See `protocol/spec/frames.md` for the normative specification.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

pub mod cobs_frame;
pub mod crc32;
pub mod decode;
pub mod encode;
pub mod frame;
pub mod ids;
pub mod types;

pub use cobs_frame::{cobs_decode, cobs_encode, max_encoded_len};
pub use crc32::checksum;
pub use decode::{DecodeError, Decoder, DecoderStats};
pub use encode::{EncodeError, encode_frame, encoded_len};
pub use frame::{Frame, FrameType};
pub use types::{
    Caps, Config, DeviceError, DeviceInfo, ErrorCode, EventBlock, EventId, EventRecord, GpioBlock,
    GpioRecord, Hello, Marker, Sample, SampleBlock, SampleFlags, SyncFrame,
};

/// Protocol version reported in `HELLO` / `DEVICE_INFO`, as `major << 8 | minor`.
pub const PROTOCOL_VERSION: u16 = 0x0100;

/// Maximum payload length a single frame may carry.
pub const MAX_PAYLOAD: usize = 1024;

/// Maximum logical (pre-COBS) frame length: header 4 + payload + CRC 4.
pub const MAX_FRAME: usize = MAX_PAYLOAD + 8;

/// Maximum wire length of one encoded frame including the trailing `0x00` delimiter.
pub const MAX_ENCODED: usize = MAX_FRAME + MAX_FRAME / 254 + 2;

/// Byte that delimits encoded frames. Never appears inside a COBS-encoded frame.
pub const DELIMITER: u8 = 0x00;

/// Size of the fixed frame header preceding the payload.
pub const HEADER_LEN: usize = 4;

/// Size of the CRC trailer following the payload.
pub const CRC_LEN: usize = 4;

/// Magic in `DEVICE_INFO`, ASCII `"PPRF"` read little-endian.
pub const DEVICE_MAGIC: u32 = 0x4652_5050;

/// `DEVICE_INFO.device_type` reported by a simulated device.
///
/// In the reserved high range on purpose. A host keys "is this synthetic?" off what the
/// device says it is, not off how the host happened to reach it: a simulator reached over TCP
/// is still a simulator, and a capture of one must never be mistaken for a measurement of real
/// hardware.
pub const DEVICE_TYPE_SIMULATOR: u16 = 0xFFFF;

/// Default device timer frequency when a device does not report one: 1 MHz, i.e. 1 tick = 1 µs.
pub const DEFAULT_TIMER_HZ: u32 = 1_000_000;
