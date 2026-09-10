//! Payload types and their codecs.
//!
//! Every multi-byte field is little-endian. Strings are fixed-width NUL-padded ASCII arrays,
//! never length-prefixed, so a firmware encoder can memcpy them into a static buffer.
//!
//! Bulk payloads (`CURRENT_SAMPLES`, `EVENT`, `GPIO_EVENT`) are exposed as borrowing blocks
//! with iterator access rather than owned vectors: at 50 ksps the sample path must not
//! allocate.

use crate::{DEVICE_MAGIC, MAX_PAYLOAD};

/// A payload was malformed: wrong length, bad magic, or an impossible record count.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PayloadError {
    /// The payload was shorter or longer than this frame type allows.
    Length { expected: usize, got: usize },
    /// `DEVICE_INFO` did not start with the expected magic.
    BadMagic(u32),
    /// A batched payload declared a record count or size inconsistent with its length.
    BadRecordLayout,
    /// The destination buffer was too small to encode into.
    DestTooSmall,
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

#[inline]
fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn i32le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn u64le(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

#[inline]
fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_i32(b: &mut [u8], off: usize, v: i32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn exact(payload: &[u8], expected: usize) -> Result<(), PayloadError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(PayloadError::Length {
            expected,
            got: payload.len(),
        })
    }
}

/// Copy a NUL-padded fixed-width ASCII field out of a payload.
fn ascii16(b: &[u8], off: usize) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&b[off..off + 16]);
    out
}

/// Interpret a NUL-padded fixed-width ASCII field as a `&str`, stopping at the first NUL and
/// at the first non-ASCII byte.
pub fn ascii_str(field: &[u8]) -> &str {
    let end = field
        .iter()
        .position(|&b| b == 0 || !b.is_ascii())
        .unwrap_or(field.len());
    // SAFETY-free: every byte in `..end` was checked to be ASCII, hence valid UTF-8.
    core::str::from_utf8(&field[..end]).unwrap_or("")
}

// ---------------------------------------------------------------------------
// HELLO
// ---------------------------------------------------------------------------

/// `HELLO`, 8 bytes. Host -> device handshake request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Hello {
    /// Protocol version the host speaks, `major << 8 | minor`.
    pub proto_version: u16,
    /// Host-chosen nonce, echoed nowhere; present so a device may seed a session id.
    pub nonce: u16,
    /// Reserved, must be zero in v1.0.
    pub reserved: u32,
}

impl Hello {
    pub const LEN: usize = 8;

    pub fn decode(p: &[u8]) -> Result<Self, PayloadError> {
        exact(p, Self::LEN)?;
        Ok(Hello {
            proto_version: u16le(p, 0),
            nonce: u16le(p, 2),
            reserved: u32le(p, 4),
        })
    }

    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, PayloadError> {
        if dst.len() < Self::LEN {
            return Err(PayloadError::DestTooSmall);
        }
        put_u16(dst, 0, self.proto_version);
        put_u16(dst, 2, self.nonce);
        put_u32(dst, 4, self.reserved);
        Ok(Self::LEN)
    }
}

// ---------------------------------------------------------------------------
// DEVICE_INFO
// ---------------------------------------------------------------------------

/// Device capability bits in [`DeviceInfo::caps`].
pub struct Caps;

impl Caps {
    /// Device can emit delta-encoded sample blocks.
    pub const DELTA_SAMPLES: u16 = 1 << 0;
    /// Device captures GPIO edges.
    pub const GPIO: u16 = 1 << 1;
    /// Device timestamps host-injected markers.
    pub const MARKERS: u16 = 1 << 2;
    /// Device reports an explicit timer wrap count in `SYNC`.
    pub const WRAP_COUNT: u16 = 1 << 3;
    /// Device has a pre-trigger ring buffer.
    pub const PRE_TRIGGER: u16 = 1 << 4;
}

/// `DEVICE_INFO`, 72 bytes. Device identity, timing base, and capabilities.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub proto_version: u16,
    pub device_type: u16,
    /// NUL-padded ASCII.
    pub serial: [u8; 16],
    /// NUL-padded ASCII.
    pub fw_version: [u8; 16],
    pub fw_build_id: u64,
    /// Ticks per second of the one hardware timer that stamps samples, events, and edges.
    pub timer_hz: u32,
    pub max_sample_rate_hz: u32,
    pub shunt_micro_ohm: i32,
    pub adc_full_scale_ua: u32,
    pub channel_count: u16,
    pub gpio_count: u16,
    /// Bitfield of [`Caps`].
    pub caps: u16,
}

impl DeviceInfo {
    pub const LEN: usize = 72;

    pub fn decode(p: &[u8]) -> Result<Self, PayloadError> {
        exact(p, Self::LEN)?;
        let magic = u32le(p, 0);
        if magic != DEVICE_MAGIC {
            return Err(PayloadError::BadMagic(magic));
        }
        Ok(DeviceInfo {
            proto_version: u16le(p, 4),
            device_type: u16le(p, 6),
            serial: ascii16(p, 8),
            fw_version: ascii16(p, 24),
            fw_build_id: u64le(p, 40),
            timer_hz: u32le(p, 48),
            max_sample_rate_hz: u32le(p, 52),
            shunt_micro_ohm: i32le(p, 56),
            adc_full_scale_ua: u32le(p, 60),
            channel_count: u16le(p, 64),
            gpio_count: u16le(p, 66),
            caps: u16le(p, 68),
            // bytes 70..72 reserved
        })
    }

    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, PayloadError> {
        if dst.len() < Self::LEN {
            return Err(PayloadError::DestTooSmall);
        }
        let d = &mut dst[..Self::LEN];
        d.fill(0);
        put_u32(d, 0, DEVICE_MAGIC);
        put_u16(d, 4, self.proto_version);
        put_u16(d, 6, self.device_type);
        d[8..24].copy_from_slice(&self.serial);
        d[24..40].copy_from_slice(&self.fw_version);
        put_u64(d, 40, self.fw_build_id);
        put_u32(d, 48, self.timer_hz);
        put_u32(d, 52, self.max_sample_rate_hz);
        put_i32(d, 56, self.shunt_micro_ohm);
        put_u32(d, 60, self.adc_full_scale_ua);
        put_u16(d, 64, self.channel_count);
        put_u16(d, 66, self.gpio_count);
        put_u16(d, 68, self.caps);
        Ok(Self::LEN)
    }

    /// The serial number as a string, NUL padding removed.
    pub fn serial_str(&self) -> &str {
        ascii_str(&self.serial)
    }

    /// The firmware version as a string, NUL padding removed.
    pub fn fw_version_str(&self) -> &str {
        ascii_str(&self.fw_version)
    }

    /// Build a NUL-padded 16-byte ASCII field from a string, truncating if necessary.
    pub fn ascii_field(s: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (dst, src) in out.iter_mut().zip(s.bytes().filter(u8::is_ascii)) {
            *dst = src;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// CONFIG
// ---------------------------------------------------------------------------

/// `CONFIG`, 24 bytes. Host -> device acquisition settings.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub sample_rate_hz: u32,
    /// Hardware averaging factor, device-specific; 0 or 1 means none.
    pub averaging: u16,
    /// ADC conversion-time code, device-specific.
    pub conv_time_code: u16,
    /// Which GPIO pins to capture edges on.
    pub gpio_mask: u32,
    pub shunt_micro_ohm: i32,
    pub flags: u32,
    pub reserved: u32,
}

impl Config {
    pub const LEN: usize = 24;

    pub fn decode(p: &[u8]) -> Result<Self, PayloadError> {
        exact(p, Self::LEN)?;
        Ok(Config {
            sample_rate_hz: u32le(p, 0),
            averaging: u16le(p, 4),
            conv_time_code: u16le(p, 6),
            gpio_mask: u32le(p, 8),
            shunt_micro_ohm: i32le(p, 12),
            flags: u32le(p, 16),
            reserved: u32le(p, 20),
        })
    }

    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, PayloadError> {
        if dst.len() < Self::LEN {
            return Err(PayloadError::DestTooSmall);
        }
        put_u32(dst, 0, self.sample_rate_hz);
        put_u16(dst, 4, self.averaging);
        put_u16(dst, 6, self.conv_time_code);
        put_u32(dst, 8, self.gpio_mask);
        put_i32(dst, 12, self.shunt_micro_ohm);
        put_u32(dst, 16, self.flags);
        put_u32(dst, 20, self.reserved);
        Ok(Self::LEN)
    }
}

// ---------------------------------------------------------------------------
// CURRENT_SAMPLES
// ---------------------------------------------------------------------------

/// One acquired sample, in device time.
///
/// This is the canonical decoded form. In the default wire mode the timestamp is
/// reconstructed as `t0 + i * period`; in `EXPLICIT_TS` mode it is read from the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Device timer ticks, still `u32` and still wrappable. Unwrapping happens exactly once,
    /// in the host's decode path, before anything above the session sees a timestamp.
    pub timestamp: u32,
    pub current_ua: i32,
    pub voltage_uv: u32,
}

/// Flag bits in a `CURRENT_SAMPLES` block header.
pub struct SampleFlags;

impl SampleFlags {
    /// Each record carries its own `u32` timestamp; `period_ticks` is ignored.
    pub const EXPLICIT_TS: u16 = 1 << 0;
    /// Records omit the voltage field; the reader substitutes the configured supply.
    pub const NO_VOLTAGE: u16 = 1 << 1;
    /// The device dropped samples immediately before this block.
    pub const OVERFLOW_BEFORE: u16 = 1 << 2;
    /// Reserved for `i16` current deltas. Spec'd, bit-reserved, not implemented in v1.
    pub const DELTA16: u16 = 1 << 3;
}

/// Fixed part of a `CURRENT_SAMPLES` payload, before the records.
pub const SAMPLE_BLOCK_HEADER: usize = 12;

/// A borrowed view of one `CURRENT_SAMPLES` payload.
#[derive(Copy, Clone, Debug)]
pub struct SampleBlock<'a> {
    t0: u32,
    period_ticks: u32,
    count: u16,
    flags: u16,
    records: &'a [u8],
}

impl<'a> SampleBlock<'a> {
    pub fn decode(p: &'a [u8]) -> Result<Self, PayloadError> {
        if p.len() < SAMPLE_BLOCK_HEADER {
            return Err(PayloadError::Length {
                expected: SAMPLE_BLOCK_HEADER,
                got: p.len(),
            });
        }
        let t0 = u32le(p, 0);
        let period_ticks = u32le(p, 4);
        let count = u16le(p, 8);
        let flags = u16le(p, 10);
        if flags & SampleFlags::DELTA16 != 0 {
            // Reserved for a future minor version; a v1.0 decoder must not guess.
            return Err(PayloadError::BadRecordLayout);
        }
        let stride = Self::record_size(flags);
        let need = SAMPLE_BLOCK_HEADER + stride * count as usize;
        if p.len() != need {
            return Err(PayloadError::Length {
                expected: need,
                got: p.len(),
            });
        }
        Ok(SampleBlock {
            t0,
            period_ticks,
            count,
            flags,
            records: &p[SAMPLE_BLOCK_HEADER..],
        })
    }

    /// Bytes per record for a given flag set.
    #[inline]
    pub const fn record_size(flags: u16) -> usize {
        let ts = if flags & SampleFlags::EXPLICIT_TS != 0 {
            4
        } else {
            0
        };
        let v = if flags & SampleFlags::NO_VOLTAGE != 0 {
            0
        } else {
            4
        };
        ts + 4 + v
    }

    /// How many samples fit in a payload of at most `MAX_PAYLOAD` bytes with these flags.
    #[inline]
    pub const fn max_samples_per_block(flags: u16) -> usize {
        (MAX_PAYLOAD - SAMPLE_BLOCK_HEADER) / Self::record_size(flags)
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub const fn t0(&self) -> u32 {
        self.t0
    }

    #[inline]
    pub const fn period_ticks(&self) -> u32 {
        self.period_ticks
    }

    #[inline]
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// `true` if the device reported dropped samples immediately before this block.
    #[inline]
    pub const fn overflow_before(&self) -> bool {
        self.flags & SampleFlags::OVERFLOW_BEFORE != 0
    }

    /// Iterate the block's samples, reconstructing timestamps where they are implicit.
    pub fn iter(&self) -> SampleIter<'a> {
        SampleIter {
            records: self.records,
            stride: Self::record_size(self.flags),
            flags: self.flags,
            t0: self.t0,
            period: self.period_ticks,
            i: 0,
            count: self.count as usize,
        }
    }

    /// Encode a uniformly spaced block of `(current_ua, voltage_uv)` pairs.
    ///
    /// This is the default v1 mode: the per-sample timestamp is omitted and reconstructed as
    /// `t0 + i * period_ticks`, which is a third of the bandwidth at 50 ksps. A timing
    /// discontinuity must terminate the block and start a new one with a fresh `t0` rather
    /// than being papered over.
    pub fn encode_uniform(
        dst: &mut [u8],
        t0: u32,
        period_ticks: u32,
        samples: &[(i32, u32)],
    ) -> Result<usize, PayloadError> {
        let stride = Self::record_size(0);
        let need = SAMPLE_BLOCK_HEADER + stride * samples.len();
        if samples.len() > u16::MAX as usize || need > MAX_PAYLOAD {
            return Err(PayloadError::BadRecordLayout);
        }
        if dst.len() < need {
            return Err(PayloadError::DestTooSmall);
        }
        put_u32(dst, 0, t0);
        put_u32(dst, 4, period_ticks);
        put_u16(dst, 8, samples.len() as u16);
        put_u16(dst, 10, 0);
        let mut off = SAMPLE_BLOCK_HEADER;
        for &(i, v) in samples {
            put_i32(dst, off, i);
            put_u32(dst, off + 4, v);
            off += stride;
        }
        Ok(need)
    }

    /// Encode a block carrying an explicit timestamp per sample, for non-uniform acquisition.
    pub fn encode_explicit(dst: &mut [u8], samples: &[Sample]) -> Result<usize, PayloadError> {
        let flags = SampleFlags::EXPLICIT_TS;
        let stride = Self::record_size(flags);
        let need = SAMPLE_BLOCK_HEADER + stride * samples.len();
        if samples.len() > u16::MAX as usize || need > MAX_PAYLOAD {
            return Err(PayloadError::BadRecordLayout);
        }
        if dst.len() < need {
            return Err(PayloadError::DestTooSmall);
        }
        put_u32(dst, 0, samples.first().map_or(0, |s| s.timestamp));
        put_u32(dst, 4, 0);
        put_u16(dst, 8, samples.len() as u16);
        put_u16(dst, 10, flags);
        let mut off = SAMPLE_BLOCK_HEADER;
        for s in samples {
            put_u32(dst, off, s.timestamp);
            put_i32(dst, off + 4, s.current_ua);
            put_u32(dst, off + 8, s.voltage_uv);
            off += stride;
        }
        Ok(need)
    }
}

/// Iterator over the samples of a [`SampleBlock`].
#[derive(Clone, Debug)]
pub struct SampleIter<'a> {
    records: &'a [u8],
    stride: usize,
    flags: u16,
    t0: u32,
    period: u32,
    i: usize,
    count: usize,
}

impl Iterator for SampleIter<'_> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.i >= self.count {
            return None;
        }
        let off = self.i * self.stride;
        let r = &self.records[off..off + self.stride];
        let explicit = self.flags & SampleFlags::EXPLICIT_TS != 0;
        let (timestamp, mut cur) = if explicit {
            (u32le(r, 0), 4)
        } else {
            // Wrapping is correct and intended: the device timer is a free-running u32 and
            // the host unwraps it once, later, in `wattson-core::time`.
            (
                self.t0
                    .wrapping_add((self.i as u32).wrapping_mul(self.period)),
                0,
            )
        };
        let current_ua = i32le(r, cur);
        cur += 4;
        let voltage_uv = if self.flags & SampleFlags::NO_VOLTAGE != 0 {
            0
        } else {
            u32le(r, cur)
        };
        self.i += 1;
        Some(Sample {
            timestamp,
            current_ua,
            voltage_uv,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.count - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for SampleIter<'_> {}

// ---------------------------------------------------------------------------
// EVENT
// ---------------------------------------------------------------------------

/// An opaque firmware event identifier. Names live in the capture metadata, not on the wire.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EventId(pub u16);

/// One firmware event in device time.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EventRecord {
    pub timestamp: u32,
    pub id: EventId,
    /// Present only when the block's record size is 10.
    pub value: Option<u32>,
}

/// Fixed part of an `EVENT` payload, before the records.
pub const EVENT_BLOCK_HEADER: usize = 4;

/// A borrowed view of one `EVENT` payload.
///
/// Events are batched because a burst of instrumented calls must not cost one USB frame each.
#[derive(Copy, Clone, Debug)]
pub struct EventBlock<'a> {
    count: u16,
    record_size: u16,
    records: &'a [u8],
}

impl<'a> EventBlock<'a> {
    /// Record size without a value field.
    pub const RECORD_SMALL: u16 = 6;
    /// Record size with a `u32` value field.
    pub const RECORD_WITH_VALUE: u16 = 10;

    pub fn decode(p: &'a [u8]) -> Result<Self, PayloadError> {
        if p.len() < EVENT_BLOCK_HEADER {
            return Err(PayloadError::Length {
                expected: EVENT_BLOCK_HEADER,
                got: p.len(),
            });
        }
        let count = u16le(p, 0);
        let record_size = u16le(p, 2);
        if record_size != Self::RECORD_SMALL && record_size != Self::RECORD_WITH_VALUE {
            return Err(PayloadError::BadRecordLayout);
        }
        let need = EVENT_BLOCK_HEADER + record_size as usize * count as usize;
        if p.len() != need {
            return Err(PayloadError::Length {
                expected: need,
                got: p.len(),
            });
        }
        Ok(EventBlock {
            count,
            record_size,
            records: &p[EVENT_BLOCK_HEADER..],
        })
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub const fn has_values(&self) -> bool {
        self.record_size == Self::RECORD_WITH_VALUE
    }

    pub fn iter(&self) -> EventIter<'a> {
        EventIter {
            records: self.records,
            stride: self.record_size as usize,
            with_value: self.has_values(),
            i: 0,
            count: self.count as usize,
        }
    }

    /// Encode a batch. `with_value` selects the 6- or 10-byte record layout for the whole
    /// block; a `None` value in a 10-byte block is encoded as zero.
    pub fn encode(
        dst: &mut [u8],
        events: &[EventRecord],
        with_value: bool,
    ) -> Result<usize, PayloadError> {
        let record_size = if with_value {
            Self::RECORD_WITH_VALUE
        } else {
            Self::RECORD_SMALL
        };
        let need = EVENT_BLOCK_HEADER + record_size as usize * events.len();
        if events.len() > u16::MAX as usize || need > MAX_PAYLOAD {
            return Err(PayloadError::BadRecordLayout);
        }
        if dst.len() < need {
            return Err(PayloadError::DestTooSmall);
        }
        put_u16(dst, 0, events.len() as u16);
        put_u16(dst, 2, record_size);
        let mut off = EVENT_BLOCK_HEADER;
        for e in events {
            put_u32(dst, off, e.timestamp);
            put_u16(dst, off + 4, e.id.0);
            if with_value {
                put_u32(dst, off + 6, e.value.unwrap_or(0));
            }
            off += record_size as usize;
        }
        Ok(need)
    }

    /// How many events fit in one frame at a given record layout.
    #[inline]
    pub const fn max_events_per_block(with_value: bool) -> usize {
        let rs = if with_value {
            Self::RECORD_WITH_VALUE
        } else {
            Self::RECORD_SMALL
        } as usize;
        (MAX_PAYLOAD - EVENT_BLOCK_HEADER) / rs
    }
}

/// Iterator over the events of an [`EventBlock`].
#[derive(Clone, Debug)]
pub struct EventIter<'a> {
    records: &'a [u8],
    stride: usize,
    with_value: bool,
    i: usize,
    count: usize,
}

impl Iterator for EventIter<'_> {
    type Item = EventRecord;

    fn next(&mut self) -> Option<EventRecord> {
        if self.i >= self.count {
            return None;
        }
        let off = self.i * self.stride;
        let r = &self.records[off..off + self.stride];
        self.i += 1;
        Some(EventRecord {
            timestamp: u32le(r, 0),
            id: EventId(u16le(r, 4)),
            value: if self.with_value {
                Some(u32le(r, 6))
            } else {
                None
            },
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.count - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for EventIter<'_> {}

// ---------------------------------------------------------------------------
// GPIO_EVENT
// ---------------------------------------------------------------------------

/// One digital edge: the full pin-state snapshot at that instant.
///
/// A snapshot rather than a `(pin, level)` pair, at the same 6 bytes: any single pin's
/// waveform then reconstructs from a scan with no initial-state bookkeeping.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GpioRecord {
    pub timestamp: u32,
    pub state: u16,
}

/// Fixed part of a `GPIO_EVENT` payload, before the records.
pub const GPIO_BLOCK_HEADER: usize = 4;

/// A borrowed view of one `GPIO_EVENT` payload.
#[derive(Copy, Clone, Debug)]
pub struct GpioBlock<'a> {
    count: u16,
    records: &'a [u8],
}

impl<'a> GpioBlock<'a> {
    pub const RECORD_SIZE: usize = 6;

    pub fn decode(p: &'a [u8]) -> Result<Self, PayloadError> {
        if p.len() < GPIO_BLOCK_HEADER {
            return Err(PayloadError::Length {
                expected: GPIO_BLOCK_HEADER,
                got: p.len(),
            });
        }
        let count = u16le(p, 0);
        let need = GPIO_BLOCK_HEADER + Self::RECORD_SIZE * count as usize;
        if p.len() != need {
            return Err(PayloadError::Length {
                expected: need,
                got: p.len(),
            });
        }
        Ok(GpioBlock {
            count,
            records: &p[GPIO_BLOCK_HEADER..],
        })
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> GpioIter<'a> {
        GpioIter {
            records: self.records,
            i: 0,
            count: self.count as usize,
        }
    }

    pub fn encode(dst: &mut [u8], edges: &[GpioRecord]) -> Result<usize, PayloadError> {
        let need = GPIO_BLOCK_HEADER + Self::RECORD_SIZE * edges.len();
        if edges.len() > u16::MAX as usize || need > MAX_PAYLOAD {
            return Err(PayloadError::BadRecordLayout);
        }
        if dst.len() < need {
            return Err(PayloadError::DestTooSmall);
        }
        put_u16(dst, 0, edges.len() as u16);
        put_u16(dst, 2, 0);
        let mut off = GPIO_BLOCK_HEADER;
        for e in edges {
            put_u32(dst, off, e.timestamp);
            put_u16(dst, off + 4, e.state);
            off += Self::RECORD_SIZE;
        }
        Ok(need)
    }
}

/// Iterator over the edges of a [`GpioBlock`].
#[derive(Clone, Debug)]
pub struct GpioIter<'a> {
    records: &'a [u8],
    i: usize,
    count: usize,
}

impl Iterator for GpioIter<'_> {
    type Item = GpioRecord;

    fn next(&mut self) -> Option<GpioRecord> {
        if self.i >= self.count {
            return None;
        }
        let off = self.i * GpioBlock::RECORD_SIZE;
        let r = &self.records[off..off + GpioBlock::RECORD_SIZE];
        self.i += 1;
        Some(GpioRecord {
            timestamp: u32le(r, 0),
            state: u16le(r, 4),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.count - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for GpioIter<'_> {}

// ---------------------------------------------------------------------------
// SYNC
// ---------------------------------------------------------------------------

/// `SYNC`, 16 bytes. Clock correlation between host and device.
///
/// The host sends one with `host_time_ns` set to its send time and the device fields zero;
/// the device echoes it with `device_ticks` and `wrap_count` filled in. The host records its
/// own receive time, giving the `(send, recv, ticks)` triple used to fit the clocks.
///
/// `wrap_count` is what makes `u32` tick wraparound *verifiable* rather than merely inferred.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SyncFrame {
    pub device_ticks: u32,
    /// Number of times the device timer has wrapped, from its overflow ISR.
    pub wrap_count: u16,
    pub flags: u16,
    pub host_time_ns: u64,
}

impl SyncFrame {
    pub const LEN: usize = 16;

    pub fn decode(p: &[u8]) -> Result<Self, PayloadError> {
        exact(p, Self::LEN)?;
        Ok(SyncFrame {
            device_ticks: u32le(p, 0),
            wrap_count: u16le(p, 4),
            flags: u16le(p, 6),
            host_time_ns: u64le(p, 8),
        })
    }

    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, PayloadError> {
        if dst.len() < Self::LEN {
            return Err(PayloadError::DestTooSmall);
        }
        put_u32(dst, 0, self.device_ticks);
        put_u16(dst, 4, self.wrap_count);
        put_u16(dst, 6, self.flags);
        put_u64(dst, 8, self.host_time_ns);
        Ok(Self::LEN)
    }
}

// ---------------------------------------------------------------------------
// MARKER
// ---------------------------------------------------------------------------

/// `MARKER`, 12 bytes. A host-injected annotation, timestamped by the device on receipt so
/// it lands in the device time base like everything else.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Marker {
    /// Filled in by the device; zero when sent by the host.
    pub device_ticks: u32,
    pub marker_id: u32,
    pub value: u32,
}

impl Marker {
    pub const LEN: usize = 12;

    pub fn decode(p: &[u8]) -> Result<Self, PayloadError> {
        exact(p, Self::LEN)?;
        Ok(Marker {
            device_ticks: u32le(p, 0),
            marker_id: u32le(p, 4),
            value: u32le(p, 8),
        })
    }

    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, PayloadError> {
        if dst.len() < Self::LEN {
            return Err(PayloadError::DestTooSmall);
        }
        put_u32(dst, 0, self.device_ticks);
        put_u32(dst, 4, self.marker_id);
        put_u32(dst, 8, self.value);
        Ok(Self::LEN)
    }
}

// ---------------------------------------------------------------------------
// ERROR
// ---------------------------------------------------------------------------

/// Error codes carried in an `ERROR` frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ErrorCode {
    None = 0x0000,
    UnsupportedFrame = 0x0001,
    BadConfig = 0x0002,
    NotConfigured = 0x0003,
    AlreadyCapturing = 0x0004,
    NotCapturing = 0x0005,
    BufferOverflow = 0x0010,
    AdcFault = 0x0011,
    Overcurrent = 0x0012,
    Internal = 0x00FF,
    /// A code this version does not recognise.
    Other = 0xFFFF,
}

impl ErrorCode {
    pub const fn from_u16(v: u16) -> ErrorCode {
        match v {
            0x0000 => ErrorCode::None,
            0x0001 => ErrorCode::UnsupportedFrame,
            0x0002 => ErrorCode::BadConfig,
            0x0003 => ErrorCode::NotConfigured,
            0x0004 => ErrorCode::AlreadyCapturing,
            0x0005 => ErrorCode::NotCapturing,
            0x0010 => ErrorCode::BufferOverflow,
            0x0011 => ErrorCode::AdcFault,
            0x0012 => ErrorCode::Overcurrent,
            0x00FF => ErrorCode::Internal,
            _ => ErrorCode::Other,
        }
    }
}

/// `ERROR`, 16 bytes. A typed failure plus the loss counters that must never be hidden.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DeviceError {
    /// Raw code as received, so an unknown code survives round-tripping.
    pub code: u16,
    pub detail: u16,
    pub dropped_samples: u32,
    pub buffer_overflows: u32,
    pub context: u32,
}

impl DeviceError {
    pub const LEN: usize = 16;

    pub fn decode(p: &[u8]) -> Result<Self, PayloadError> {
        exact(p, Self::LEN)?;
        Ok(DeviceError {
            code: u16le(p, 0),
            detail: u16le(p, 2),
            dropped_samples: u32le(p, 4),
            buffer_overflows: u32le(p, 8),
            context: u32le(p, 12),
        })
    }

    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, PayloadError> {
        if dst.len() < Self::LEN {
            return Err(PayloadError::DestTooSmall);
        }
        put_u16(dst, 0, self.code);
        put_u16(dst, 2, self.detail);
        put_u32(dst, 4, self.dropped_samples);
        put_u32(dst, 8, self.buffer_overflows);
        put_u32(dst, 12, self.context);
        Ok(Self::LEN)
    }

    #[inline]
    pub const fn kind(&self) -> ErrorCode {
        ErrorCode::from_u16(self.code)
    }

    /// `true` if this error reports data actually lost, as opposed to a rejected command.
    #[inline]
    pub const fn lost_data(&self) -> bool {
        self.dropped_samples > 0 || self.buffer_overflows > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_roundtrip() {
        let info = DeviceInfo {
            proto_version: 0x0100,
            device_type: 1,
            serial: DeviceInfo::ascii_field("WS-0001"),
            fw_version: DeviceInfo::ascii_field("0.1.0"),
            fw_build_id: 0xDEAD_BEEF_CAFE_1234,
            timer_hz: 1_000_000,
            max_sample_rate_hz: 50_000,
            shunt_micro_ohm: 100_000,
            adc_full_scale_ua: 1_000_000,
            channel_count: 1,
            gpio_count: 4,
            caps: Caps::GPIO | Caps::MARKERS | Caps::WRAP_COUNT,
        };
        let mut buf = [0u8; DeviceInfo::LEN];
        assert_eq!(info.encode(&mut buf).unwrap(), DeviceInfo::LEN);
        assert_eq!(DeviceInfo::decode(&buf).unwrap(), info);
        assert_eq!(info.serial_str(), "WS-0001");
        assert_eq!(info.fw_version_str(), "0.1.0");
    }

    #[test]
    fn device_info_rejects_bad_magic() {
        let mut buf = [0u8; DeviceInfo::LEN];
        put_u32(&mut buf, 0, 0x1234_5678);
        assert_eq!(
            DeviceInfo::decode(&buf),
            Err(PayloadError::BadMagic(0x1234_5678))
        );
    }

    #[test]
    fn uniform_sample_block_reconstructs_timestamps() {
        let samples: [(i32, u32); 4] = [
            (3000, 3_300_000),
            (3100, 3_299_000),
            (78_000, 3_240_000),
            (2900, 3_301_000),
        ];
        let mut buf = [0u8; 256];
        let n = SampleBlock::encode_uniform(&mut buf, 1000, 20, &samples).unwrap();
        let block = SampleBlock::decode(&buf[..n]).unwrap();
        assert_eq!(block.len(), 4);
        let got: [Sample; 4] = core::array::from_fn(|i| block.iter().nth(i).unwrap());
        for (i, s) in got.iter().enumerate() {
            assert_eq!(s.timestamp, 1000 + 20 * i as u32);
            assert_eq!(s.current_ua, samples[i].0);
            assert_eq!(s.voltage_uv, samples[i].1);
        }
    }

    #[test]
    fn explicit_sample_block_roundtrips() {
        let samples = [
            Sample {
                timestamp: 7,
                current_ua: -50,
                voltage_uv: 3_300_000,
            },
            Sample {
                timestamp: 900,
                current_ua: 81_000,
                voltage_uv: 3_180_000,
            },
        ];
        let mut buf = [0u8; 256];
        let n = SampleBlock::encode_explicit(&mut buf, &samples).unwrap();
        let block = SampleBlock::decode(&buf[..n]).unwrap();
        let decoded: [Sample; 2] = core::array::from_fn(|i| block.iter().nth(i).unwrap());
        assert_eq!(decoded, samples);
    }

    /// A block with the reserved DELTA16 bit set must be rejected, not guessed at.
    #[test]
    fn delta16_is_reserved() {
        let mut buf = [0u8; SAMPLE_BLOCK_HEADER];
        put_u16(&mut buf, 10, SampleFlags::DELTA16);
        assert_eq!(
            SampleBlock::decode(&buf).unwrap_err(),
            PayloadError::BadRecordLayout
        );
    }

    #[test]
    fn event_block_roundtrips_both_layouts() {
        let events = [
            EventRecord {
                timestamp: 10,
                id: EventId(0x0101),
                value: None,
            },
            EventRecord {
                timestamp: 20,
                id: EventId(0x0110),
                value: None,
            },
        ];
        let mut buf = [0u8; 128];
        let n = EventBlock::encode(&mut buf, &events, false).unwrap();
        let b = EventBlock::decode(&buf[..n]).unwrap();
        assert!(!b.has_values());
        assert_eq!(b.iter().count(), 2);
        assert_eq!(b.iter().next().unwrap(), events[0]);

        let with_val = [EventRecord {
            timestamp: 30,
            id: EventId(0x0110),
            value: Some(64),
        }];
        let n = EventBlock::encode(&mut buf, &with_val, true).unwrap();
        let b = EventBlock::decode(&buf[..n]).unwrap();
        assert!(b.has_values());
        assert_eq!(b.iter().next().unwrap(), with_val[0]);
    }

    #[test]
    fn event_block_rejects_unknown_record_size() {
        let mut buf = [0u8; EVENT_BLOCK_HEADER];
        put_u16(&mut buf, 0, 0);
        put_u16(&mut buf, 2, 7);
        assert_eq!(
            EventBlock::decode(&buf).unwrap_err(),
            PayloadError::BadRecordLayout
        );
    }

    #[test]
    fn gpio_block_roundtrips() {
        let edges = [
            GpioRecord {
                timestamp: 5,
                state: 0b0001,
            },
            GpioRecord {
                timestamp: 9,
                state: 0b0011,
            },
        ];
        let mut buf = [0u8; 64];
        let n = GpioBlock::encode(&mut buf, &edges).unwrap();
        let b = GpioBlock::decode(&buf[..n]).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b.iter().nth(1).unwrap(), edges[1]);
    }

    #[test]
    fn fixed_size_payloads_roundtrip() {
        let mut buf = [0u8; 32];

        let h = Hello {
            proto_version: 0x0100,
            nonce: 0xBEEF,
            reserved: 0,
        };
        let n = h.encode(&mut buf).unwrap();
        assert_eq!(Hello::decode(&buf[..n]).unwrap(), h);

        let c = Config {
            sample_rate_hz: 50_000,
            averaging: 1,
            conv_time_code: 4,
            gpio_mask: 0x0F,
            shunt_micro_ohm: 100_000,
            flags: 0,
            reserved: 0,
        };
        let n = c.encode(&mut buf).unwrap();
        assert_eq!(Config::decode(&buf[..n]).unwrap(), c);

        let s = SyncFrame {
            device_ticks: 12345,
            wrap_count: 2,
            flags: 0,
            host_time_ns: 1 << 40,
        };
        let n = s.encode(&mut buf).unwrap();
        assert_eq!(SyncFrame::decode(&buf[..n]).unwrap(), s);

        let m = Marker {
            device_ticks: 99,
            marker_id: 7,
            value: 42,
        };
        let n = m.encode(&mut buf).unwrap();
        assert_eq!(Marker::decode(&buf[..n]).unwrap(), m);

        let e = DeviceError {
            code: ErrorCode::BufferOverflow as u16,
            detail: 0,
            dropped_samples: 128,
            buffer_overflows: 1,
            context: 0,
        };
        let n = e.encode(&mut buf).unwrap();
        let back = DeviceError::decode(&buf[..n]).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.kind(), ErrorCode::BufferOverflow);
        assert!(back.lost_data());
    }

    #[test]
    fn wrong_length_is_reported_not_panicked() {
        assert_eq!(
            Hello::decode(&[0u8; 7]),
            Err(PayloadError::Length {
                expected: 8,
                got: 7
            })
        );
        assert_eq!(
            Marker::decode(&[]),
            Err(PayloadError::Length {
                expected: 12,
                got: 0
            })
        );
    }

    #[test]
    fn block_capacities_fit_a_frame() {
        assert!(SampleBlock::max_samples_per_block(0) >= 126);
        assert!(EventBlock::max_events_per_block(true) >= 100);
    }
}
