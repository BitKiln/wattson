//! Instrumented C, through the wire format, into a capture, out as an energy figure.
//!
//! Every other test in this crate checks one link in the chain. This one runs the whole thing:
//! a C function bracketed with `PP_SCOPE` emits real frames, the host decoder parses them, the
//! events land in a `.pprof` alongside a current waveform, and `event_stats` answers the
//! question the project exists to answer — *what did this code path cost?*
//!
//! The waveform is synthetic and the answer is known analytically, which is the point: with no
//! hardware in the loop, an assertion against a number nobody can derive independently would
//! prove nothing.

use std::collections::HashMap;

use wattson_core::capture::{
    CaptureHeader, CaptureReader, CaptureWriter, HeaderFlags, WriterOptions,
};
use wattson_core::metadata::{CaptureMetadata, EventDef};
use wattson_core::stats::{StatsOptions, event_stats};
use wattson_protocol::{Decoder, Frame};
use wattson_target as pp;

/// Ids the instrumented code uses: adjacent pair, radio category — the convention in
/// `protocol/spec/event-ids.md`, which is what lets `PP_SCOPE` derive the stop id.
const BLE_TX_START: u16 = 0x0100;
const BLE_TX_STOP: u16 = 0x0101;

const TIMER_HZ: u64 = 1_000_000;
const SAMPLE_RATE_HZ: u32 = 50_000;
const PERIOD_NS: u64 = 1_000_000_000 / SAMPLE_RATE_HZ as u64;

const IDLE_UA: i32 = 3_000;
const TX_UA: i32 = 78_000;
const SUPPLY_UV: u32 = 3_300_000;

const OCCURRENCES: usize = 30;
const TX_TICKS: u32 = 950; // 950 us at 1 MHz
const GAP_TICKS: u32 = 20_000; // 20 ms between transmissions

fn ticks_to_ns(ticks: u32) -> u64 {
    ticks as u64 * (1_000_000_000 / TIMER_HZ)
}

#[test]
fn instrumented_c_produces_a_measurable_per_event_energy() {
    let _guard = pp::lock();
    pp::reset();

    // 1. Run the instrumented firmware. Nothing here knows about captures or statistics; it is
    //    the code a developer would actually write.
    pp::set_time(GAP_TICKS);
    for _ in 0..OCCURRENCES {
        pp::macro_scope(BLE_TX_START, TX_TICKS);
        pp::advance(GAP_TICKS);
    }
    pp::flush();

    let stats = pp::stats();
    assert_eq!(stats.events_dropped, 0, "the run lost events: {stats:?}");
    assert_eq!(stats.events_sent, (OCCURRENCES * 2) as u32);

    // 2. Decode the wire exactly as the host does.
    let mut decoder = Decoder::new();
    let mut events: Vec<(u32, u16)> = Vec::new();
    decoder.feed(&pp::sink(), &mut |_, frame| {
        if let Frame::Event(block) = frame {
            events.extend(block.iter().map(|e| (e.timestamp, e.id.0)));
        }
    });
    assert_eq!(decoder.stats().errors(), 0);
    assert_eq!(events.len(), OCCURRENCES * 2);

    // The windows the firmware actually reported, not the ones the test intended to produce.
    // Deriving the waveform from the events rather than from the constants is what makes the
    // energy figure a measurement of the instrumentation instead of a restatement of it.
    let windows: Vec<(u64, u64)> = events
        .chunks(2)
        .map(|pair| {
            assert_eq!(pair[0].1, BLE_TX_START);
            assert_eq!(pair[1].1, BLE_TX_STOP);
            (ticks_to_ns(pair[0].0), ticks_to_ns(pair[1].0))
        })
        .collect();

    // 3. Write a capture: the events, plus the current the device would have drawn.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("instrumented.pprof");

    let mut writer = CaptureWriter::create(
        &path,
        CaptureHeader {
            device_timer_hz: TIMER_HZ as u32,
            sample_rate_hz: SAMPLE_RATE_HZ,
            flags: HeaderFlags::SYNTHETIC,
            ..Default::default()
        },
        WriterOptions::default(),
    )
    .expect("create capture");

    writer.set_metadata(CaptureMetadata {
        events: vec![EventDef::scope("BLE_TX", BLE_TX_START, BLE_TX_STOP)],
        ..Default::default()
    });

    let end_ns = windows.last().expect("at least one window").1 + ticks_to_ns(GAP_TICKS);
    let mut t_ns = 0u64;
    while t_ns < end_ns {
        let transmitting = windows.iter().any(|(s, e)| t_ns >= *s && t_ns < *e);
        let current = if transmitting { TX_UA } else { IDLE_UA };
        writer
            .push_sample(t_ns, current, Some(SUPPLY_UV))
            .expect("push sample");
        t_ns += PERIOD_NS;
    }
    for (t_ticks, id) in &events {
        writer
            .push_event(ticks_to_ns(*t_ticks), *id, None)
            .expect("push event");
    }
    writer.finish().expect("finish");

    // 4. Ask the question.
    let mut reader = CaptureReader::open(&path).expect("open capture");
    let map = reader.metadata().event_map();
    let stats = event_stats(&mut reader, &map, &StatsOptions::lenient()).expect("event stats");

    let tx = stats.iter().find(|s| s.name == "BLE_TX").expect("BLE_TX");
    assert_eq!(tx.occurrences, OCCURRENCES as u64);
    assert_eq!(tx.unterminated, 0, "PP_SCOPE left a scope open");
    assert_eq!(tx.orphaned_stops, 0);
    assert_eq!(
        tx.unsampled, 0,
        "the events are shorter than the sample period"
    );
    assert_eq!(tx.current_peak_ua, TX_UA);

    assert!(
        (tx.duration_us.mean - 950.0).abs() < 1.0,
        "duration {} us, expected 950",
        tx.duration_us.mean
    );

    // 3.3 V * 78 mA * 950 us = 244.6 uJ, the figure this project's documentation quotes.
    //
    // The measured value sits slightly under it, and should. A 950 us window at 50 ksps spans
    // 47 or 48 sample points, so up to one sample period - 20 us, or 2.1% - of the window falls
    // outside the samples available to integrate over. That is a genuine limit of measuring a
    // 950 us event at 50 ksps, not an error to be tuned away, and the tolerance says so rather
    // than hiding it. Sampling faster narrows it; nothing in the software can.
    let analytic_uj = 3.3 * 0.078 * 950e-6 * 1e6;
    let mean = tx.mean_energy_uj().expect("measurable");
    assert!(
        mean <= analytic_uj,
        "measured {mean:.1} uJ exceeds the analytic {analytic_uj:.1} uJ, which sampling cannot do"
    );
    assert!(
        mean > analytic_uj * 0.97,
        "BLE_TX cost {mean:.1} uJ, over a sample period below the analytic {analytic_uj:.1} uJ"
    );
}

/// Nesting and interleaving survive the whole chain, not just the decoder.
///
/// A re-entrant instrumented function and an unrelated event stream in between are what real
/// firmware produces; if pairing only works on a tidy start/stop/start/stop sequence, it does
/// not work.
#[test]
fn nested_and_interleaved_events_pair_correctly_end_to_end() {
    let _guard = pp::lock();
    pp::reset();

    pp::set_time(1_000);
    pp::macro_scope_nested(BLE_TX_START, 500);
    pp::event_u32(0x0210, 64); // an unrelated point event in the middle
    pp::advance(500);
    pp::macro_scope(BLE_TX_START, 250);
    pp::flush();

    let mut decoder = Decoder::new();
    let mut by_id: HashMap<u16, usize> = HashMap::new();
    let mut total = 0usize;
    decoder.feed(&pp::sink(), &mut |_, frame| {
        if let Frame::Event(block) = frame {
            for e in block.iter() {
                *by_id.entry(e.id.0).or_default() += 1;
                total += 1;
            }
        }
    });

    assert_eq!(decoder.stats().errors(), 0);
    assert_eq!(
        total, 7,
        "two nested plus one plain scope, plus a point event"
    );
    assert_eq!(by_id.get(&BLE_TX_START).copied(), Some(3));
    assert_eq!(by_id.get(&BLE_TX_STOP).copied(), Some(3));
    assert_eq!(by_id.get(&0x0210).copied(), Some(1));
}
