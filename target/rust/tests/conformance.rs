//! The target-side C library, verified against the host decoder on a host.
//!
//! Section 5 of `protocol/spec/frames.md` says an independent implementation is conformant
//! when it reproduces the golden byte vectors exactly and accepts them on decode. The C
//! instrumentation library is that independent implementation, and this file is where it is
//! held to it — no device, no toolchain for a device, just `cargo test`.
//!
//! Every test takes [`wattson_target::lock`] first: the C library keeps its state in one
//! file-scope struct, so these cannot run concurrently.

use wattson_protocol::{Decoder, EventBlock, EventId, EventRecord, Frame};
use wattson_target as pp;

/// Decode a byte stream the way the host does, returning `(seq, records)` per EVENT frame.
///
/// Anything that is not a well-formed EVENT frame fails the test rather than being skipped —
/// the C library must not emit anything else, and a decoder error here is exactly the bug this
/// file exists to catch.
fn decode_events(bytes: &[u8]) -> Vec<(u8, Vec<EventRecord>)> {
    let mut decoder = Decoder::new();
    let mut out = Vec::new();
    let mut unexpected = Vec::new();
    decoder.feed(bytes, &mut |seq, frame| match frame {
        Frame::Event(block) => out.push((seq, block.iter().collect::<Vec<_>>())),
        other => unexpected.push(format!("{other:?}")),
    });
    assert!(
        unexpected.is_empty(),
        "the instrumentation library emitted non-EVENT frames: {unexpected:?}"
    );
    let stats = decoder.stats();
    assert_eq!(
        stats.errors(),
        0,
        "the host decoder rejected a frame: {stats:?}"
    );
    assert_eq!(
        stats.seq_gaps, 0,
        "sequence numbers are not contiguous: {stats:?}"
    );
    out
}

/// Flatten to just the records, for the many tests that do not care about batching.
fn decode_records(bytes: &[u8]) -> Vec<EventRecord> {
    decode_events(bytes)
        .into_iter()
        .flat_map(|(_, r)| r)
        .collect()
}

fn rec(timestamp: u32, id: u16, value: Option<u32>) -> EventRecord {
    EventRecord {
        timestamp,
        id: EventId(id),
        value,
    }
}

// ---------------------------------------------------------------------------
// The wire format
// ---------------------------------------------------------------------------

/// The bytes the C emits are the bytes the Rust encoder emits, for the same records.
///
/// This is the strongest statement available without hardware: two independent
/// implementations of the same spec section, byte for byte.
#[test]
fn c_frames_are_byte_identical_to_the_rust_encoder() {
    let _guard = pp::lock();

    let timestamps = [0u32, 1_000, 71_582, 0xFFFF_FFFF];
    let ids = [0x0101u16, 0x0102, 0x0201, 0xFF01];

    for with_values in [false, true] {
        let values = [7u32, 0, 0xDEAD_BEEF, 1];
        let from_c =
            pp::encode_event_frame(0x2A, &timestamps, &ids, with_values.then_some(&values[..]))
                .expect("the C encoder accepted the records");

        let records: Vec<EventRecord> = (0..ids.len())
            .map(|i| rec(timestamps[i], ids[i], with_values.then_some(values[i])))
            .collect();

        let mut payload = [0u8; wattson_protocol::MAX_PAYLOAD];
        let n = EventBlock::encode(&mut payload, &records, with_values).expect("rust encode");
        let mut framed = [0u8; wattson_protocol::MAX_ENCODED];
        let m = wattson_protocol::encode_frame(
            wattson_protocol::FrameType::Event,
            0x2A,
            &payload[..n],
            &mut framed,
        )
        .expect("rust frame");

        assert_eq!(
            from_c,
            &framed[..m],
            "C and Rust disagree on the wire format (with_values = {with_values})"
        );
    }
}

/// The frame the C emits round-trips through the host decoder unchanged.
#[test]
fn events_survive_the_round_trip() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(1_000);
    pp::event(0x0101);
    pp::advance(950);
    pp::event(0x0102);
    pp::flush();

    assert_eq!(
        decode_records(&pp::sink()),
        vec![rec(1_000, 0x0101, None), rec(1_950, 0x0102, None)]
    );
}

/// A timestamp taken right before the u32 timer wraps survives as the raw value it was.
///
/// The library must not attempt to unwrap anything: unwrapping happens exactly once, on the
/// host, in the decode path. A device that also tried would produce two epochs that disagree.
#[test]
fn timestamps_are_raw_and_wrap_untouched() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(0xFFFF_FFF0);
    pp::event(0x0101);
    pp::advance(0x20); // wraps to 0x10
    pp::event(0x0102);
    pp::flush();

    assert_eq!(
        decode_records(&pp::sink()),
        vec![rec(0xFFFF_FFF0, 0x0101, None), rec(0x10, 0x0102, None)]
    );
}

/// Values ride along, and a block that carries them uses 10-byte records.
#[test]
fn valued_events_carry_their_value() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(500);
    pp::event_u32(0x0110, 244);
    pp::flush();

    let frames = decode_events(&pp::sink());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1, vec![rec(500, 0x0110, Some(244))]);
}

/// Mixed valued and unvalued events split into separate blocks, in order.
///
/// The wire format has one `record_size` per block, so a mixed queue must become several
/// frames. Reordering to save a frame would reorder the timeline, and the timeline is the
/// product.
#[test]
fn mixed_record_sizes_split_into_frames_without_reordering() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(0);
    pp::event(0x0101); // 6-byte run
    pp::advance(10);
    pp::event(0x0102);
    pp::advance(10);
    pp::event_u32(0x0110, 64); // 10-byte run
    pp::advance(10);
    pp::event(0x0201); // back to 6-byte
    pp::flush();

    let frames = decode_events(&pp::sink());
    assert_eq!(frames.len(), 3, "one frame per run of equal record size");

    // Sequence numbers advance by one per frame, and start at zero after pp_init.
    assert_eq!(
        frames.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    assert_eq!(
        frames
            .iter()
            .flat_map(|(_, r)| r)
            .copied()
            .collect::<Vec<_>>(),
        vec![
            rec(0, 0x0101, None),
            rec(10, 0x0102, None),
            rec(20, 0x0110, Some(64)),
            rec(30, 0x0201, None),
        ]
    );
}

/// A batch larger than one frame splits at the block limit and loses nothing.
#[test]
fn large_batches_split_across_frames() {
    let _guard = pp::lock();
    pp::reset();

    let n = pp::ring_capacity();
    for i in 0..n {
        pp::set_time(i as u32 * 100);
        pp::event(0x0100 + (i as u16 % 8));
    }
    pp::flush();

    let frames = decode_events(&pp::sink());
    assert!(frames.len() > 1, "{n} events should not fit in one frame");

    let records: Vec<EventRecord> = frames.into_iter().flat_map(|(_, r)| r).collect();
    assert_eq!(records.len(), n);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r.timestamp, i as u32 * 100, "record {i} is out of order");
    }
    assert_eq!(pp::stats().events_sent, n as u32);
    assert_eq!(pp::stats().events_dropped, 0);
}

// ---------------------------------------------------------------------------
// The macros
// ---------------------------------------------------------------------------

/// `PP_SCOPE` emits start and stop, deriving the stop id as start + 1.
#[test]
fn scope_macro_brackets_the_block() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(100);
    pp::macro_scope(0x0200, 2_000);
    pp::flush();

    assert_eq!(
        decode_records(&pp::sink()),
        vec![rec(100, 0x0200, None), rec(2_100, 0x0201, None)]
    );
}

/// Nested occurrences of one scope produce properly ordered start/start/stop/stop.
///
/// This is what a re-entrant instrumented function actually emits, and it is the case the
/// host's stack-based pairing exists to handle.
#[test]
fn nested_scopes_nest() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(0);
    pp::macro_scope_nested(0x0200, 10);
    pp::flush();

    assert_eq!(
        decode_records(&pp::sink()),
        vec![
            rec(0, 0x0200, None),
            rec(10, 0x0200, None),
            rec(20, 0x0201, None),
            rec(30, 0x0201, None),
        ]
    );
}

/// An explicit start/stop pair, for ids that are not adjacent.
#[test]
fn scope_id_macro_uses_the_given_stop() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(0);
    pp::macro_scope_explicit(0x1000, 0x2000, 5);
    pp::flush();

    assert_eq!(
        decode_records(&pp::sink()),
        vec![rec(0, 0x1000, None), rec(5, 0x2000, None)]
    );
}

/// Returning out of a scope still closes it, wherever the compiler supports it.
///
/// GCC and Clang have `__attribute__((cleanup))`, so every toolchain that matters for embedded
/// work gets this. Elsewhere the scope is left open on purpose, and the host reports it as an
/// unterminated occurrence rather than inventing a duration — so the fallback is documented,
/// not silently wrong.
#[test]
fn early_return_closes_the_scope_where_the_compiler_allows_it() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(0);
    assert_eq!(pp::macro_scope_early_return(0x0200, 40), 1);
    pp::flush();

    let records = decode_records(&pp::sink());
    if pp::scope_is_safe() {
        assert_eq!(
            records,
            vec![rec(0, 0x0200, None), rec(40, 0x0201, None)],
            "cleanup attribute is available, so the stop must be emitted"
        );
    } else {
        assert_eq!(
            records,
            vec![rec(0, 0x0200, None)],
            "without the cleanup attribute the start is left unterminated, by design"
        );
    }
}

/// The macros and the functions produce identical records.
#[test]
fn macros_and_functions_agree() {
    let _guard = pp::lock();

    pp::reset();
    pp::set_time(7);
    pp::macro_event(0x0101);
    pp::macro_event_u32(0x0110, 9);
    pp::flush();
    let from_macros = decode_records(&pp::sink());

    pp::reset();
    pp::set_time(7);
    pp::event(0x0101);
    pp::event_u32(0x0110, 9);
    pp::flush();
    let from_functions = decode_records(&pp::sink());

    assert_eq!(from_macros, from_functions);
}

// ---------------------------------------------------------------------------
// Loss is loud
// ---------------------------------------------------------------------------

/// A full ring drops the newest events and counts every one.
///
/// Overwriting the oldest instead would corrupt already-recorded history to make room for the
/// present, turning a burst into a timeline with a hole nobody can see.
#[test]
fn a_full_ring_drops_loudly_and_keeps_what_it_has() {
    let _guard = pp::lock();
    pp::reset();

    let capacity = pp::ring_capacity();
    let overflow = 20;
    for i in 0..capacity + overflow {
        pp::set_time(i as u32);
        pp::event(0x0101);
    }

    assert_eq!(pp::pending(), capacity as u32);
    assert_eq!(pp::stats().events_dropped, overflow as u32);
    assert_eq!(pp::stats().events_recorded, capacity as u32);

    pp::flush();
    let records = decode_records(&pp::sink());
    assert_eq!(records.len(), capacity, "everything recorded is delivered");
    for (i, r) in records.iter().enumerate() {
        assert_eq!(
            r.timestamp, i as u32,
            "the events kept are the oldest ones, in order"
        );
    }
}

/// Event id 0 is reserved as invalid by the wire format, so it is refused and counted.
#[test]
fn event_id_zero_is_refused() {
    let _guard = pp::lock();
    pp::reset();

    pp::event(0);
    pp::event_u32(0, 5);

    assert_eq!(pp::pending(), 0);
    assert_eq!(pp::stats().events_dropped, 2);
    assert_eq!(pp::flush(), 0);
    assert!(pp::sink().is_empty());
}

/// A short write loses the whole frame, and says so.
///
/// The host discards a partial frame at the next delimiter, so every record in it is gone.
/// Counting them as sent would be a lie the capture could not detect.
#[test]
fn a_short_write_counts_the_whole_frame_as_lost() {
    let _guard = pp::lock();
    pp::reset();
    pp::set_write_limit(4);

    pp::set_time(0);
    pp::event(0x0101);
    pp::event(0x0102);
    pp::flush();

    let stats = pp::stats();
    assert_eq!(stats.write_failures, 1);
    assert_eq!(stats.events_dropped, 2);
    assert_eq!(stats.events_sent, 0);
    assert_eq!(stats.frames_sent, 0);
}

/// With no transport installed, events are consumed and counted as dropped.
///
/// The alternative — leaving them queued forever — turns a missing `pp_init` into a ring that
/// is permanently full, which then loses every later event too and reports the wrong cause.
#[test]
fn events_without_a_transport_are_dropped_not_stranded() {
    let _guard = pp::lock();
    pp::reset_without_transport();

    pp::event(0x0101);
    pp::event(0x0102);
    assert_eq!(pp::pending(), 2);

    assert_eq!(pp::flush(), 0);
    assert_eq!(pp::pending(), 0);
    assert_eq!(pp::stats().events_dropped, 2);
}

/// Flushing an empty ring is free and emits nothing.
#[test]
fn flushing_nothing_writes_nothing() {
    let _guard = pp::lock();
    pp::reset();

    assert_eq!(pp::flush(), 0);
    assert!(pp::sink().is_empty());
    assert_eq!(pp::stats(), wattson_target::Stats::default());
}

// ---------------------------------------------------------------------------
// Build-time guarantees
// ---------------------------------------------------------------------------

/// With `PP_ENABLED=0` the whole library compiles away.
///
/// The build script compiles the same sources a second time with instrumentation off; that it
/// links at all is the assertion. This test calls into it so the linker cannot discard it.
#[test]
fn the_disabled_build_links_and_does_nothing() {
    let _guard = pp::lock();
    pp::reset();

    // Three, because the three `PP_SCOPE` bodies still execute. Instrumentation disappearing
    // must not take the instrumented code with it.
    assert_eq!(pp::disabled_smoke(0x0101, 5), 3);
    assert!(
        pp::sink().is_empty(),
        "a disabled build must not reach the transport"
    );
    assert_eq!(pp::stats(), wattson_target::Stats::default());
}

/// The ring is a power of two and big enough to hold a realistic burst.
#[test]
fn the_ring_is_a_sensible_size() {
    let n = pp::ring_capacity();
    assert!(n.is_power_of_two(), "capacity {n} is not a power of two");
    assert!(n >= 32, "capacity {n} is too small for a burst of events");
}
