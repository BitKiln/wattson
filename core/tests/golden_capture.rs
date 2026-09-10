//! The capture format's constitution.
//!
//! `cli/tests/golden/v1_0_reference.pprof` is a real capture written by the first release of
//! format v1.0. It is checked in permanently, and this test opens it and verifies its full
//! contents.
//!
//! **Any change that cannot read this file is, by definition, a major version bump.** That is
//! the whole point: the format was frozen before there was a GUI to shake out its
//! shortcomings, so the guard against quietly breaking it has to be mechanical rather than a
//! matter of remembering.
//!
//! To regenerate — which should happen approximately never, and never without bumping
//! `FORMAT_MAJOR`:
//!
//! ```text
//! WATTSON_BLESS=1 cargo test -p wattson-core --test golden_capture
//! ```

use std::path::{Path, PathBuf};

use wattson_core::capture::{
    CaptureHeader, CaptureReader, CaptureWriter, Compression, HeaderFlags, WriterOptions,
};
use wattson_core::metadata::{CaptureMetadata, EventDef};
use wattson_core::stats::{StatsOptions, event_stats, region_stats};
use wattson_core::time::TimeSpan;

/// Where the reference file lives. Under `cli/` because that is where the whole-tool tests
/// are, and the file is the tool's contract, not one crate's.
fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/tests/golden/v1_0_reference.pprof")
}

const SAMPLE_COUNT: usize = 5_000;
const PERIOD_NS: u64 = 20_000; // 50 kHz
const IDLE_UA: i32 = 3_000;
const BURST_UA: i32 = 78_000;
const SUPPLY_UV: u32 = 3_300_000;

/// Three bursts, at known offsets, with matched events. Fully deterministic: no RNG at all,
/// so the file cannot drift with a dependency update.
fn bursts() -> [(u64, u64); 3] {
    [
        (10_000_000, 10_950_000),
        (40_000_000, 40_950_000),
        (70_000_000, 70_950_000),
    ]
}

fn write_reference(path: &Path) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("create golden dir");

    let header = CaptureHeader {
        created_unix_ns: 1_770_000_000_000_000_000,
        device_timer_hz: 1_000_000,
        sample_rate_hz: 50_000,
        compression: Compression::Zstd,
        device_serial: *b"WS-GOLDEN-0001\0\0",
        fw_version: *b"1.0.0\0\0\0\0\0\0\0\0\0\0\0",
        fw_build_id: 0x0123_4567_89AB_CDEF,
        shunt_micro_ohm: 100_000,
        flags: HeaderFlags::SYNTHETIC,
        ..Default::default()
    };

    let mut w = CaptureWriter::create(path, header, WriterOptions::default()).expect("create");
    w.set_metadata(CaptureMetadata {
        events: vec![
            EventDef::scope("BLE_TX", 0x0101, 0x0102),
            EventDef::scope("SENSOR_READ", 0x0201, 0x0202),
        ],
        notes: [(
            "purpose".to_string(),
            "format v1.0 reference capture".to_string(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });

    let windows = bursts();
    for i in 0..SAMPLE_COUNT as u64 {
        let t = i * PERIOD_NS;
        let current = if windows.iter().any(|(s, e)| t >= *s && t < *e) {
            BURST_UA
        } else {
            IDLE_UA
        };
        w.push_sample(t, current, Some(SUPPLY_UV))
            .expect("push sample");
    }
    for (start, end) in windows {
        w.push_event(start, 0x0101, None).expect("start event");
        w.push_event(end, 0x0102, Some(64)).expect("stop event");
    }
    w.push_gpio(15_000_000, 0b0001).expect("gpio");
    w.finish().expect("finish");
}

#[test]
fn the_v1_0_reference_capture_still_reads() {
    let path = golden_path();

    if std::env::var("WATTSON_BLESS").is_ok() {
        write_reference(&path);
        eprintln!(
            "Wrote {}. This is a FORMAT CHANGE: bump FORMAT_MAJOR and update \
             docs/capture-format.md.",
            path.display()
        );
    }

    let mut reader = CaptureReader::open(&path).unwrap_or_else(|e| {
        panic!(
            "the v1.0 reference capture no longer opens: {e}\n\
             This is a breaking change to the capture format. Every .pprof anyone has ever\n\
             written is now unreadable by this build. If that is intended, bump FORMAT_MAJOR.\n\
             Path: {}",
            path.display()
        )
    });

    // Header.
    let header = reader.header();
    assert_eq!(header.format_major, 1, "the reference file is format v1");
    assert_eq!(header.format_minor, 0);
    assert_eq!(header.serial_str(), "WS-GOLDEN-0001");
    assert_eq!(header.fw_version_str(), "1.0.0");
    assert_eq!(header.device_timer_hz, 1_000_000);
    assert_eq!(header.sample_rate_hz, 50_000);
    assert_eq!(header.shunt_micro_ohm, 100_000);
    assert!(header.has(HeaderFlags::FINALIZED));
    assert!(header.has(HeaderFlags::HAS_VOLTAGE));
    assert!(header.has(HeaderFlags::HAS_EVENTS));
    assert!(
        header.has(HeaderFlags::SYNTHETIC),
        "the reference capture is synthetic and must say so"
    );

    // Integrity.
    assert!(reader.integrity().is_clean(), "{:?}", reader.integrity());
    assert_eq!(reader.sample_count(), SAMPLE_COUNT as u64);
    assert_eq!(
        reader.span(),
        TimeSpan::new(0, (SAMPLE_COUNT as u64 - 1) * PERIOD_NS + 1)
    );
    assert!((reader.effective_rate_hz() - 50_000.0).abs() < 0.1);

    // Every sample, exactly.
    let samples = reader.samples().expect("read samples");
    assert_eq!(samples.len(), SAMPLE_COUNT);
    let windows = bursts();
    for (i, s) in samples.iter().enumerate() {
        let t = i as u64 * PERIOD_NS;
        let expected = if windows.iter().any(|(a, b)| t >= *a && t < *b) {
            BURST_UA
        } else {
            IDLE_UA
        };
        assert_eq!(s.t_ns, t, "sample {i} has the wrong timestamp");
        assert_eq!(s.current_ua, expected, "sample {i} has the wrong current");
        assert_eq!(s.voltage_uv, SUPPLY_UV, "sample {i} has the wrong voltage");
    }

    // Events, and the value on the stop event.
    let events = reader.events().expect("read events");
    assert_eq!(events.len(), 6);
    assert_eq!(events[0].t_ns, 10_000_000);
    assert_eq!(events[0].id, 0x0101);
    assert_eq!(events[0].value, None);
    assert_eq!(events[1].id, 0x0102);
    assert_eq!(events[1].value, Some(64));

    // GPIO.
    let gpio = reader.gpio().expect("read gpio");
    assert_eq!(gpio.len(), 1);
    assert_eq!(gpio[0].t_ns, 15_000_000);
    assert_eq!(gpio[0].state, 0b0001);

    // Metadata, which is what keeps the file self-describing.
    let meta = reader.metadata().clone();
    assert_eq!(meta.events.len(), 2);
    assert_eq!(meta.event("BLE_TX").expect("BLE_TX").start_id, 0x0101);
    assert_eq!(meta.event("BLE_TX").expect("BLE_TX").stop_id, Some(0x0102));
    assert_eq!(
        meta.notes.get("purpose").map(String::as_str),
        Some("format v1.0 reference capture")
    );

    // The zoom pyramid, and the peak surviving it.
    let buckets = reader.downsample(TimeSpan::ALL, 32).expect("downsample");
    assert_eq!(buckets.len(), 32);
    assert_eq!(
        buckets.iter().map(|b| b.max_ua).max(),
        Some(BURST_UA),
        "the burst peak must survive downsampling"
    );
}

/// The numbers computed from the reference file are pinned too.
///
/// A format that still parses but yields different energy is broken in a way that is much
/// harder to notice, so the analysis is pinned against analytically known values rather than
/// against whatever the code happens to produce.
#[test]
fn the_reference_capture_still_analyses_to_the_same_numbers() {
    let mut reader = CaptureReader::open(&golden_path()).expect("open the reference capture");

    let region =
        region_stats(&mut reader, TimeSpan::ALL, &StatsOptions::lenient()).expect("region stats");
    assert_eq!(region.sample_count, SAMPLE_COUNT as u64);
    assert_eq!(region.current_min_ua, IDLE_UA);
    assert_eq!(region.current_max_ua, BURST_UA);

    // Three 950 us bursts at 78 mA inside 100 ms at 3 mA, at 3.3 V.
    //   idle:  3.3 V * 3 mA   * (0.1 - 0.00285) s = 961.8 uJ
    //   burst: 3.3 V * 78 mA  * 0.00285 s         = 733.6 uJ
    // The exact figure depends on trapezoid handling of the three step edges, so allow 1%.
    let expected_uj = 3.3 * 0.003 * (0.09998 - 0.00285) * 1e6 + 3.3 * 0.078 * 0.00285 * 1e6;
    assert!(
        (region.energy_uj - expected_uj).abs() < expected_uj * 0.01,
        "energy drifted: {:.1} uJ, expected about {:.1} uJ",
        region.energy_uj,
        expected_uj
    );

    let map = reader.metadata().event_map();
    let stats = event_stats(&mut reader, &map, &StatsOptions::lenient()).expect("event stats");
    let tx = stats.iter().find(|s| s.name == "BLE_TX").expect("BLE_TX");

    assert_eq!(tx.occurrences, 3);
    assert_eq!(tx.unterminated, 0);
    assert_eq!(tx.orphaned_stops, 0);
    assert_eq!(tx.unsampled, 0);
    assert_eq!(tx.current_peak_ua, BURST_UA);
    assert!(
        (tx.duration_us.mean - 950.0).abs() < 1.0,
        "duration {}",
        tx.duration_us.mean
    );

    // 3.3 V * 78 mA * 950 us = 244.6 uJ, the figure the project's documentation quotes.
    let mean = tx.mean_energy_uj().expect("measurable");
    assert!(
        (mean - 244.6).abs() < 244.6 * 0.02,
        "BLE_TX energy drifted: {mean:.1} uJ, expected about 244.6 uJ"
    );

    // SENSOR_READ is declared but never occurs, which must read as zero occurrences rather
    // than as an error.
    let sensor = stats
        .iter()
        .find(|s| s.name == "SENSOR_READ")
        .expect("SENSOR_READ");
    assert_eq!(sensor.occurrences, 0);
    assert!(sensor.energy_uj.is_empty());
}
