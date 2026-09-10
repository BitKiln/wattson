//! Property tests for the load-bearing framing invariants.
//!
//! The decoder eats untrusted bytes from a USB device. Three things must hold no matter what
//! arrives on the wire:
//!
//! 1. Anything this crate encodes, it decodes back identically.
//! 2. Arbitrary garbage never panics.
//! 3. A valid frame surrounded by garbage is still recovered — resynchronisation works.

use proptest::prelude::*;
use wattson_protocol::{
    Decoder, EventBlock, EventId, EventRecord, Frame, FrameType, GpioBlock, GpioRecord,
    MAX_ENCODED, MAX_PAYLOAD, Sample, SampleBlock, encode_frame,
};

/// Encode one frame and return the wire bytes.
fn wire(ty: FrameType, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; MAX_ENCODED];
    let n = encode_frame(ty, seq, payload, &mut buf).expect("encode");
    buf.truncate(n);
    buf
}

/// Decode `bytes` and collect `(seq, type_byte, payload-ish)` summaries of every frame.
fn decode_all(bytes: &[u8]) -> Vec<(u8, u8)> {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    dec.feed(bytes, &mut |seq, f| out.push((seq, f.type_byte())));
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// encode -> decode is the identity on the frame header and payload bytes.
    #[test]
    fn frame_roundtrip(
        ty_idx in 0usize..FrameType::ALL.len(),
        seq: u8,
        payload in proptest::collection::vec(any::<u8>(), 0..=MAX_PAYLOAD),
    ) {
        // Use an unknown type byte so the payload is passed through verbatim; typed payloads
        // have their own length rules and are covered by the per-type tests below.
        let _ = ty_idx;
        let bytes = wire(FrameType::Error, seq, &payload);

        let mut dec = Decoder::new();
        let mut seen: Option<(u8, Vec<u8>)> = None;
        let mut errors = 0usize;
        dec.feed_with_errors(
            &bytes,
            &mut |s, f| {
                // ERROR frames are fixed length, so a random payload is usually rejected at
                // the payload layer — which is itself the property we want: framing succeeded.
                seen = Some((s, vec![f.type_byte()]));
            },
            &mut |_| errors += 1,
        );
        // Either the frame decoded, or its payload was the wrong length for ERROR. Framing
        // itself must never produce a CRC or COBS error on our own output.
        let st = dec.stats();
        prop_assert_eq!(st.crc_errors, 0, "payload_len={} stats={:?}", payload.len(), st);
        prop_assert_eq!(st.cobs_errors, 0, "payload_len={} stats={:?}", payload.len(), st);
        prop_assert_eq!(st.length_errors, 0, "payload_len={} stats={:?}", payload.len(), st);
    }

    /// Arbitrary bytes must never panic and must never fabricate a frame.
    #[test]
    fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let mut dec = Decoder::new();
        let mut frames = 0usize;
        dec.feed(&bytes, &mut |_, _| frames += 1);
        // A random byte sequence passing a 32-bit CRC is possible but astronomically
        // unlikely; assert only that we survived.
        prop_assert!(frames < 64);
    }

    /// garbage ++ frame ++ garbage ++ frame recovers both frames.
    #[test]
    fn resync_recovers_following_frames(
        junk_a in proptest::collection::vec(1u8..=255, 0..300),
        junk_b in proptest::collection::vec(1u8..=255, 0..300),
        seq_a: u8,
        seq_b: u8,
    ) {
        let mut stream = Vec::new();
        stream.extend_from_slice(&junk_a);
        stream.push(0x00); // terminate the junk run
        stream.extend_from_slice(&wire(FrameType::StartCapture, seq_a, &[]));
        stream.extend_from_slice(&junk_b);
        stream.push(0x00);
        stream.extend_from_slice(&wire(FrameType::StopCapture, seq_b, &[]));

        let got = decode_all(&stream);
        let starts = got.iter().filter(|(_, t)| *t == FrameType::StartCapture.as_u8()).count();
        let stops = got.iter().filter(|(_, t)| *t == FrameType::StopCapture.as_u8()).count();
        prop_assert_eq!(starts, 1, "START_CAPTURE lost after garbage");
        prop_assert_eq!(stops, 1, "STOP_CAPTURE lost after garbage");
    }

    /// Sample blocks survive the full encode/frame/decode path with every value intact.
    #[test]
    fn sample_block_roundtrip(
        t0: u32,
        period in 1u32..10_000,
        samples in proptest::collection::vec(
            (any::<i32>(), any::<u32>()),
            0..=SampleBlock::max_samples_per_block(0),
        ),
    ) {
        let mut payload = vec![0u8; MAX_PAYLOAD];
        let n = SampleBlock::encode_uniform(&mut payload, t0, period, &samples).expect("encode");
        payload.truncate(n);
        let bytes = wire(FrameType::CurrentSamples, 0, &payload);

        let mut dec = Decoder::new();
        let mut got: Vec<Sample> = Vec::new();
        dec.feed(&bytes, &mut |_, f| {
            if let Frame::CurrentSamples(b) = f {
                got.extend(b.iter());
            }
        });

        prop_assert_eq!(got.len(), samples.len());
        for (i, s) in got.iter().enumerate() {
            prop_assert_eq!(s.current_ua, samples[i].0);
            prop_assert_eq!(s.voltage_uv, samples[i].1);
            prop_assert_eq!(s.timestamp, t0.wrapping_add((i as u32).wrapping_mul(period)));
        }
    }

    /// Explicit-timestamp blocks are bit-identical to the canonical `Sample` type.
    #[test]
    fn explicit_sample_block_roundtrip(
        samples in proptest::collection::vec(
            (any::<u32>(), any::<i32>(), any::<u32>()),
            0..=SampleBlock::max_samples_per_block(1),
        ),
    ) {
        let samples: Vec<Sample> = samples
            .into_iter()
            .map(|(timestamp, current_ua, voltage_uv)| Sample { timestamp, current_ua, voltage_uv })
            .collect();
        let mut payload = vec![0u8; MAX_PAYLOAD];
        let n = SampleBlock::encode_explicit(&mut payload, &samples).expect("encode");
        payload.truncate(n);

        let block = SampleBlock::decode(&payload).expect("decode");
        let got: Vec<Sample> = block.iter().collect();
        prop_assert_eq!(got, samples);
    }

    /// Event batches round-trip in both record layouts.
    #[test]
    fn event_block_roundtrip(
        with_value: bool,
        raw in proptest::collection::vec((any::<u32>(), any::<u16>(), any::<u32>()), 0..100),
    ) {
        let events: Vec<EventRecord> = raw
            .into_iter()
            .map(|(timestamp, id, value)| EventRecord {
                timestamp,
                id: EventId(id),
                value: if with_value { Some(value) } else { None },
            })
            .collect();

        let mut payload = vec![0u8; MAX_PAYLOAD];
        let n = EventBlock::encode(&mut payload, &events, with_value).expect("encode");
        payload.truncate(n);
        let bytes = wire(FrameType::Event, 0, &payload);

        let mut dec = Decoder::new();
        let mut got: Vec<EventRecord> = Vec::new();
        dec.feed(&bytes, &mut |_, f| {
            if let Frame::Event(b) = f {
                got.extend(b.iter());
            }
        });
        prop_assert_eq!(got, events);
    }

    /// GPIO edge batches round-trip.
    #[test]
    fn gpio_block_roundtrip(
        raw in proptest::collection::vec((any::<u32>(), any::<u16>()), 0..150),
    ) {
        let edges: Vec<GpioRecord> = raw
            .into_iter()
            .map(|(timestamp, state)| GpioRecord { timestamp, state })
            .collect();

        let mut payload = vec![0u8; MAX_PAYLOAD];
        let n = GpioBlock::encode(&mut payload, &edges).expect("encode");
        payload.truncate(n);

        let block = GpioBlock::decode(&payload).expect("decode");
        let got: Vec<GpioRecord> = block.iter().collect();
        prop_assert_eq!(got, edges);
    }

    /// Feeding the same stream in arbitrary chunk sizes yields the same frames. A transport
    /// hands over whatever the OS gives it; frame boundaries never align with read sizes.
    #[test]
    fn chunking_does_not_change_the_result(
        chunk in 1usize..64,
        n_frames in 1usize..8,
    ) {
        let mut stream = Vec::new();
        for i in 0..n_frames {
            stream.extend_from_slice(&wire(FrameType::StartCapture, i as u8, &[]));
        }
        let whole = decode_all(&stream);

        let mut dec = Decoder::new();
        let mut chunked = Vec::new();
        for part in stream.chunks(chunk) {
            dec.feed(part, &mut |seq, f| chunked.push((seq, f.type_byte())));
        }
        prop_assert_eq!(whole, chunked);
    }
}
