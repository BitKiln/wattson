//! Streaming frame decoder.
//!
//! The decoder accumulates bytes until a `0x00` delimiter, COBS-decodes the accumulated
//! region into a second buffer, validates the length field and CRC, and hands the caller a
//! [`Frame`] borrowing that buffer.
//!
//! # Why a callback and not an iterator
//!
//! [`Frame`] borrows the decoder's internal buffer, so `fn feed(&mut self) -> impl
//! Iterator<Item = Frame<'_>>` would be a *lending* iterator, which stable Rust cannot
//! express. Hence [`Decoder::feed`] takes `&mut dyn FnMut`. One indirect call per frame — at
//! roughly 780 frames/s for a 50 ksps stream — costs nothing measurable. Reaching for the
//! iterator form is a half-day detour that ends back here.
//!
//! # Robustness
//!
//! This decoder consumes untrusted bytes from a USB device, so it must never panic and must
//! resynchronise on its own. Malformed input increments a counter in [`DecoderStats`], the
//! accumulation is discarded up to the next delimiter, and decoding continues.

use crate::{
    CRC_LEN, DELIMITER, HEADER_LEN, MAX_ENCODED, MAX_FRAME, MAX_PAYLOAD,
    cobs_frame::cobs_decode,
    crc32::checksum,
    frame::{Frame, FrameType},
    types::{
        Config, DeviceError, DeviceInfo, EventBlock, GpioBlock, Hello, Marker, PayloadError,
        SampleBlock, SyncFrame,
    },
};

/// Why a single frame failed to decode. Never fatal: the decoder resynchronises.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// The COBS-encoded region was malformed.
    Cobs,
    /// The decoded frame was shorter than a header plus CRC.
    Runt { len: usize },
    /// The `LEN` field disagreed with the actual decoded length.
    LengthMismatch { declared: usize, actual: usize },
    /// The declared payload length exceeded [`crate::MAX_PAYLOAD`].
    PayloadTooLarge { declared: usize },
    /// The CRC trailer did not match the computed CRC.
    BadCrc { expected: u32, got: u32 },
    /// Framing was valid but the payload was malformed for its frame type.
    Payload(PayloadError),
}

impl From<PayloadError> for DecodeError {
    fn from(e: PayloadError) -> Self {
        DecodeError::Payload(e)
    }
}

/// Counters describing everything the decoder has seen.
///
/// These are surfaced in capture integrity reports. Losses are recorded, never hidden — a
/// silently dropped frame becomes a silently wrong energy figure downstream.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DecoderStats {
    /// Frames delivered to the callback, including [`Frame::Unknown`].
    pub frames: u64,
    /// Frames rejected because the CRC did not match.
    pub crc_errors: u64,
    /// Frames rejected because COBS decoding failed.
    pub cobs_errors: u64,
    /// Frames rejected because the length field was inconsistent.
    pub length_errors: u64,
    /// Frames whose framing was valid but whose payload was malformed.
    pub payload_errors: u64,
    /// Times the accumulator overflowed and was discarded up to the next delimiter.
    pub oversize: u64,
    /// Times a valid frame followed a discarded region, i.e. a successful resync.
    pub resyncs: u64,
    /// Gaps observed in the sequence number of received frames.
    pub seq_gaps: u64,
    /// Total bytes fed in.
    pub bytes: u64,
}

impl DecoderStats {
    /// Total frames rejected for any reason.
    pub const fn errors(&self) -> u64 {
        self.crc_errors
            + self.cobs_errors
            + self.length_errors
            + self.payload_errors
            + self.oversize
    }
}

/// Streaming COBS frame decoder with fixed-size buffers and no allocation.
pub struct Decoder {
    /// Accumulates raw wire bytes between delimiters.
    raw: [u8; MAX_ENCODED],
    raw_len: usize,
    /// Holds the COBS-decoded logical frame that a yielded [`Frame`] borrows.
    ///
    /// Sized to `MAX_ENCODED`, not `MAX_FRAME`: COBS decoding is a strict contraction, but
    /// [`cobs_decode`] conservatively demands a destination as large as its source, and the
    /// source here is the *encoded* region. Anything that decodes to more than a legal frame
    /// is rejected below by the length checks.
    plain: [u8; MAX_ENCODED],
    /// Set when the current accumulation already overflowed, so the remainder is dropped.
    poisoned: bool,
    /// Set once something was discarded, so the next good frame counts as a resync.
    desynced: bool,
    last_seq: Option<u8>,
    stats: DecoderStats,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Decoder {
            raw: [0u8; MAX_ENCODED],
            raw_len: 0,
            plain: [0u8; MAX_ENCODED],
            poisoned: false,
            desynced: false,
            last_seq: None,
            stats: DecoderStats {
                frames: 0,
                crc_errors: 0,
                cobs_errors: 0,
                length_errors: 0,
                payload_errors: 0,
                oversize: 0,
                resyncs: 0,
                seq_gaps: 0,
                bytes: 0,
            },
        }
    }

    /// Counters accumulated so far.
    #[inline]
    pub const fn stats(&self) -> DecoderStats {
        self.stats
    }

    /// Discard buffered bytes and sequence tracking. Counters are preserved.
    pub fn reset(&mut self) {
        self.raw_len = 0;
        self.poisoned = false;
        self.desynced = false;
        self.last_seq = None;
    }

    /// Feed bytes from a transport. Every complete, valid frame is passed to `on_frame`
    /// with its sequence number. Invalid frames are counted and skipped.
    pub fn feed(&mut self, bytes: &[u8], on_frame: &mut dyn FnMut(u8, Frame<'_>)) {
        self.feed_with_errors(bytes, on_frame, &mut |_| {});
    }

    /// As [`Decoder::feed`], additionally reporting each rejected frame.
    pub fn feed_with_errors(
        &mut self,
        bytes: &[u8],
        on_frame: &mut dyn FnMut(u8, Frame<'_>),
        on_error: &mut dyn FnMut(DecodeError),
    ) {
        self.stats.bytes += bytes.len() as u64;

        for &b in bytes {
            if b != DELIMITER {
                if self.raw_len < self.raw.len() {
                    self.raw[self.raw_len] = b;
                    self.raw_len += 1;
                } else if !self.poisoned {
                    // A run longer than any legal frame is not a frame. Discard the whole
                    // accumulation and wait for the next delimiter.
                    self.poisoned = true;
                    self.desynced = true;
                    self.stats.oversize += 1;
                }
                continue;
            }

            let len = self.raw_len;
            let poisoned = self.poisoned;
            self.raw_len = 0;
            self.poisoned = false;
            // An empty run between two delimiters is idle line noise, not an error.
            if poisoned || len == 0 {
                continue;
            }

            // Split the borrow so the yielded frame can borrow `plain` while `stats` and the
            // sequence tracker are still mutable.
            let Decoder {
                raw,
                plain,
                stats,
                last_seq,
                desynced,
                ..
            } = self;
            match decode_one(&raw[..len], plain, stats, last_seq) {
                Ok((seq, frame)) => {
                    if *desynced {
                        *desynced = false;
                        stats.resyncs += 1;
                    }
                    stats.frames += 1;
                    on_frame(seq, frame);
                }
                Err(e) => {
                    *desynced = true;
                    on_error(e);
                }
            }
        }
    }

    /// Decode exactly one delimited region, for tests and low-rate call sites.
    ///
    /// `region` must not include the trailing delimiter.
    pub fn decode_single<'a>(&'a mut self, region: &[u8]) -> Result<(u8, Frame<'a>), DecodeError> {
        let Decoder {
            plain,
            stats,
            last_seq,
            ..
        } = self;
        decode_one(region, plain, stats, last_seq)
    }
}

/// Decode one delimited region into `plain`, returning a frame borrowing it.
///
/// Free function rather than a method so the caller can hold disjoint borrows of the
/// decoder's fields; `plain` is reborrowed as shared for the returned frame's lifetime.
fn decode_one<'p>(
    region: &[u8],
    plain: &'p mut [u8; MAX_ENCODED],
    stats: &mut DecoderStats,
    last_seq: &mut Option<u8>,
) -> Result<(u8, Frame<'p>), DecodeError> {
    let n = cobs_decode(region, plain).map_err(|_| {
        stats.cobs_errors += 1;
        DecodeError::Cobs
    })?;

    if n < HEADER_LEN + CRC_LEN {
        stats.length_errors += 1;
        return Err(DecodeError::Runt { len: n });
    }
    if n > MAX_FRAME {
        stats.length_errors += 1;
        return Err(DecodeError::PayloadTooLarge {
            declared: n - HEADER_LEN - CRC_LEN,
        });
    }
    let declared = u16::from_le_bytes([plain[1], plain[2]]) as usize;
    if declared > MAX_PAYLOAD {
        stats.length_errors += 1;
        return Err(DecodeError::PayloadTooLarge { declared });
    }
    if HEADER_LEN + declared + CRC_LEN != n {
        stats.length_errors += 1;
        return Err(DecodeError::LengthMismatch {
            declared,
            actual: n.saturating_sub(HEADER_LEN + CRC_LEN),
        });
    }

    let body = HEADER_LEN + declared;
    let expected = u32::from_le_bytes([
        plain[body],
        plain[body + 1],
        plain[body + 2],
        plain[body + 3],
    ]);
    let got = checksum(&plain[..body]);
    if expected != got {
        stats.crc_errors += 1;
        return Err(DecodeError::BadCrc { expected, got });
    }

    let ty = plain[0];
    let seq = plain[3];
    if let Some(prev) = *last_seq
        && seq != prev.wrapping_add(1)
    {
        stats.seq_gaps += 1;
    }
    *last_seq = Some(seq);

    // Release the mutable borrow: everything from here on is read-only, so the payload slice
    // may live as long as `'p`.
    let plain: &'p [u8] = &*plain;
    let payload = &plain[HEADER_LEN..body];

    let frame = parse_payload(ty, payload).inspect_err(|_| {
        stats.payload_errors += 1;
    })?;
    Ok((seq, frame))
}

fn parse_payload(ty: u8, payload: &[u8]) -> Result<Frame<'_>, DecodeError> {
    Ok(match FrameType::from_u8(ty) {
        Some(FrameType::Hello) => Frame::Hello(Hello::decode(payload)?),
        Some(FrameType::DeviceInfo) => Frame::DeviceInfo(DeviceInfo::decode(payload)?),
        Some(FrameType::Config) => Frame::Config(Config::decode(payload)?),
        Some(FrameType::StartCapture) => Frame::StartCapture,
        Some(FrameType::StopCapture) => Frame::StopCapture,
        Some(FrameType::CurrentSamples) => Frame::CurrentSamples(SampleBlock::decode(payload)?),
        Some(FrameType::Event) => Frame::Event(EventBlock::decode(payload)?),
        Some(FrameType::GpioEvent) => Frame::GpioEvent(GpioBlock::decode(payload)?),
        Some(FrameType::Sync) => Frame::Sync(SyncFrame::decode(payload)?),
        Some(FrameType::Marker) => Frame::Marker(Marker::decode(payload)?),
        Some(FrameType::Error) => Frame::Error(DeviceError::decode(payload)?),
        // Forward compatibility: an unrecognised type is data we cannot interpret, not a
        // protocol violation.
        None => Frame::Unknown { ty, payload },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode_frame;
    use crate::types::{EventId, EventRecord, Sample};

    fn framed(ty: FrameType, seq: u8, payload: &[u8]) -> ([u8; MAX_ENCODED], usize) {
        let mut buf = [0u8; MAX_ENCODED];
        let n = encode_frame(ty, seq, payload, &mut buf).unwrap();
        (buf, n)
    }

    #[test]
    fn roundtrip_hello() {
        let hello = Hello {
            proto_version: 0x0100,
            nonce: 0x1234,
            reserved: 0,
        };
        let mut p = [0u8; Hello::LEN];
        hello.encode(&mut p).unwrap();
        let (wire, n) = framed(FrameType::Hello, 3, &p);

        let mut dec = Decoder::new();
        let mut seen = 0;
        dec.feed(&wire[..n], &mut |seq, f| {
            assert_eq!(seq, 3);
            match f {
                Frame::Hello(h) => assert_eq!(h, hello),
                other => panic!("wrong frame: {other:?}"),
            }
            seen += 1;
        });
        assert_eq!(seen, 1);
        assert_eq!(dec.stats().frames, 1);
        assert_eq!(dec.stats().errors(), 0);
    }

    #[test]
    fn roundtrip_sample_block_preserves_every_sample() {
        let samples: [(i32, u32); 64] =
            core::array::from_fn(|i| (3000 + i as i32 * 17, 3_300_000 - i as u32 * 11));
        let mut p = [0u8; MAX_PAYLOAD];
        let plen = SampleBlock::encode_uniform(&mut p, 4_000_000, 20, &samples).unwrap();
        let (wire, n) = framed(FrameType::CurrentSamples, 0, &p[..plen]);

        let mut dec = Decoder::new();
        let mut got: [Sample; 64] = [Sample {
            timestamp: 0,
            current_ua: 0,
            voltage_uv: 0,
        }; 64];
        let mut count = 0usize;
        dec.feed(&wire[..n], &mut |_, f| {
            let Frame::CurrentSamples(b) = f else {
                panic!("wrong frame")
            };
            for (i, s) in b.iter().enumerate() {
                got[i] = s;
                count += 1;
            }
        });
        assert_eq!(count, 64);
        for (i, s) in got.iter().enumerate() {
            assert_eq!(s.timestamp, 4_000_000 + 20 * i as u32);
            assert_eq!(s.current_ua, samples[i].0);
        }
    }

    /// An unrecognised type byte is data we cannot interpret, not a protocol violation.
    /// This is the forward-compatibility escape hatch that lets a v1.1 device talk to a
    /// v1.0 host, so it must not be quietly turned into an error.
    #[test]
    fn unknown_type_is_delivered_not_rejected() {
        let payload = [9u8, 9, 9];
        for ty in [0x00u8, 0x04, 0x15, 0x40, 0x6F, 0x7E, 0x80, 0xFF] {
            match parse_payload(ty, &payload).unwrap() {
                Frame::Unknown {
                    ty: got,
                    payload: p,
                } => {
                    assert_eq!(got, ty);
                    assert_eq!(p, &payload);
                }
                other => panic!("0x{ty:02X} should decode as Unknown, got {other:?}"),
            }
        }
    }

    #[test]
    fn corrupt_crc_is_counted_and_skipped() {
        let (mut wire, n) = framed(FrameType::StartCapture, 0, &[]);
        // Flip a bit inside the encoded region, but not the delimiter.
        wire[1] ^= 0x40;
        let mut dec = Decoder::new();
        let mut delivered = 0;
        let mut errs = 0;
        dec.feed_with_errors(&wire[..n], &mut |_, _| delivered += 1, &mut |_| errs += 1);
        assert_eq!(delivered, 0);
        assert_eq!(errs, 1);
        assert_eq!(dec.stats().frames, 0);
        assert!(dec.stats().errors() >= 1);
    }

    #[test]
    fn resyncs_after_garbage_between_frames() {
        let a = {
            let mut p = [0u8; Hello::LEN];
            Hello {
                proto_version: 0x0100,
                nonce: 1,
                reserved: 0,
            }
            .encode(&mut p)
            .unwrap();
            framed(FrameType::Hello, 0, &p)
        };
        let b = {
            let mut p = [0u8; Marker::LEN];
            Marker {
                device_ticks: 5,
                marker_id: 6,
                value: 7,
            }
            .encode(&mut p)
            .unwrap();
            framed(FrameType::Marker, 1, &p)
        };

        let mut dec = Decoder::new();
        let mut kinds = [0u8; 4];
        let mut n_seen = 0usize;
        let mut push = |_seq: u8, f: Frame<'_>| {
            if n_seen < kinds.len() {
                kinds[n_seen] = f.type_byte();
            }
            n_seen += 1;
        };

        dec.feed(&[0xAA, 0xBB, 0xCC, DELIMITER], &mut push);
        dec.feed(&a.0[..a.1], &mut push);
        dec.feed(&[0x01, 0x02, DELIMITER, DELIMITER], &mut push);
        dec.feed(&b.0[..b.1], &mut push);

        assert_eq!(
            n_seen, 2,
            "both good frames must survive the garbage around them"
        );
        assert_eq!(kinds[0], FrameType::Hello.as_u8());
        assert_eq!(kinds[1], FrameType::Marker.as_u8());
        assert!(dec.stats().resyncs >= 1);
    }

    #[test]
    fn byte_at_a_time_feeding_works() {
        let mut p = [0u8; MAX_PAYLOAD];
        let events = [EventRecord {
            timestamp: 42,
            id: EventId(0x0101),
            value: Some(64),
        }];
        let plen = EventBlock::encode(&mut p, &events, true).unwrap();
        let (wire, n) = framed(FrameType::Event, 0, &p[..plen]);

        let mut dec = Decoder::new();
        let mut seen = 0;
        for i in 0..n {
            dec.feed(&wire[i..i + 1], &mut |_, f| {
                let Frame::Event(b) = f else {
                    panic!("wrong frame")
                };
                assert_eq!(b.iter().next().unwrap(), events[0]);
                seen += 1;
            });
        }
        assert_eq!(seen, 1);
    }

    #[test]
    fn sequence_gaps_are_counted() {
        let (a, na) = framed(FrameType::StartCapture, 0, &[]);
        let (b, nb) = framed(FrameType::StopCapture, 5, &[]);
        let mut dec = Decoder::new();
        dec.feed(&a[..na], &mut |_, _| {});
        dec.feed(&b[..nb], &mut |_, _| {});
        assert_eq!(dec.stats().seq_gaps, 1);
    }

    #[test]
    fn oversize_run_does_not_panic_and_is_counted() {
        let mut dec = Decoder::new();
        let junk = [0xAAu8; 512];
        for _ in 0..8 {
            dec.feed(&junk, &mut |_, _| panic!("no frame should decode"));
        }
        dec.feed(&[DELIMITER], &mut |_, _| panic!("no frame should decode"));
        assert!(dec.stats().oversize >= 1);
        assert_eq!(dec.stats().frames, 0);
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut dec = Decoder::new();
        let mut x: u32 = 0x1234_5678;
        let mut chunk = [0u8; 97];
        for _ in 0..200 {
            for b in chunk.iter_mut() {
                // xorshift; deterministic, no dependency
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
            dec.feed(&chunk, &mut |_, _| {});
        }
    }
}
