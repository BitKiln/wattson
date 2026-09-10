//! End to end, with no hardware and no sockets.
//!
//! The synthetic device runs on a background thread behind an in-memory pipe; a real
//! [`Session`] handshakes with it, a real [`CaptureWriter`] records what arrives, and the
//! resulting `.pprof` is read back and analysed. Every layer in the product is exercised,
//! inside one `cargo test`, with nothing for parallel CI jobs to collide over.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use wattson_core::capture::{CaptureHeader, CaptureReader, CaptureWriter, WriterOptions};
use wattson_core::metadata::default_simulator_metadata;
use wattson_core::session::{CaptureConfig, CaptureSink, CountingSink, Session};
use wattson_core::stats::{StatsOptions, event_stats, region_stats};
use wattson_core::time::TimeSpan;
use wattson_core::transport::{PipeTransport, Transport};
use wattson_sim::device::SimDevice;
use wattson_sim::engine::SimConfig;
use wattson_sim::profiles::Profile;
use wattson_sim::server::{ByteChannel, run_device};

/// Let the simulator speak over the core's in-memory pipe.
///
/// The simulator crate deliberately does not depend on `wattson-core`, so the two are joined
/// here, in the test that needs both.
struct PipeChannel(PipeTransport);

impl ByteChannel for PipeChannel {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf).map_err(std::io::Error::other)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.write_all(buf).map_err(std::io::Error::other)
    }
}

/// A device running behind a pipe, with the host end of that pipe.
struct Rig {
    host: Option<Box<dyn Transport>>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Rig {
    fn start(config: SimConfig) -> Rig {
        let (mut host, mut device_end) = PipeTransport::pair();
        host.set_read_timeout(Duration::from_millis(50)).unwrap();
        device_end
            .set_read_timeout(Duration::from_millis(2))
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let join = thread::spawn(move || {
            run_device(SimDevice::new(config), PipeChannel(device_end), flag);
        });

        Rig {
            host: Some(Box::new(host)),
            stop,
            join: Some(join),
        }
    }

    fn session(&mut self) -> Session {
        Session::new(self.host.take().expect("the host end is taken once"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn sim_config(profile: Profile, rate_hz: u32) -> SimConfig {
    let mut c = SimConfig::new(profile);
    c.sample_rate_hz = rate_hz;
    c
}

/// Handshake, configure, capture for `duration`, stop.
fn capture_into(
    session: &mut Session,
    sink: &mut dyn CaptureSink,
    rate_hz: u32,
    duration: Duration,
) {
    let info = session
        .handshake(Duration::from_secs(5))
        .expect("the simulated device must answer HELLO");
    assert_eq!(info.timer_hz, 1_000_000);
    assert_eq!(info.serial_str(), "SIM-0001");

    session
        .configure(&CaptureConfig {
            sample_rate_hz: rate_hz,
            ..Default::default()
        })
        .expect("configure");
    session.start().expect("start");
    session.run_for(duration, sink).expect("capture");
    session.stop().expect("stop");
    session
        .drain(Duration::from_millis(200), sink)
        .expect("drain");
}

#[test]
fn a_session_captures_from_the_simulated_device() {
    let rate = 50_000;
    let mut rig = Rig::start(sim_config(Profile::ble_sensor(), rate));
    let mut session = rig.session();
    let mut sink = CountingSink::default();

    capture_into(&mut session, &mut sink, rate, Duration::from_millis(500));

    // Half a second at 50 ksps is 25000 samples. Wall-clock pacing and the drain window make
    // the exact count vary, so assert on the order of magnitude and on the derived rate.
    assert!(
        sink.samples > 15_000,
        "expected roughly 25000 samples in 500 ms, got {}",
        sink.samples
    );
    let span_ns = sink.last_ns.saturating_sub(sink.first_ns.unwrap_or(0));
    let derived = (sink.samples - 1) as f64 * 1e9 / span_ns as f64;
    assert!(
        (derived - rate as f64).abs() < rate as f64 * 0.05,
        "derived rate {derived:.0} Hz is not close to the configured {rate} Hz"
    );

    assert!(
        sink.events > 0,
        "a BLE sensor profile must produce firmware events"
    );
    assert!(
        sink.gaps.is_empty(),
        "a clean simulator must produce no gaps: {:?}",
        sink.gaps
    );
    assert_eq!(
        session.decoder_stats().crc_errors,
        0,
        "a clean simulator must produce no CRC errors"
    );
}

#[test]
fn a_rate_the_device_cannot_meet_is_refused_before_any_data_is_lost() {
    let mut rig = Rig::start(sim_config(Profile::always_on(), 50_000));
    let mut session = rig.session();
    session.handshake(Duration::from_secs(5)).unwrap();

    // The simulated device advertises 1 MHz; asking for 50 MHz must fail loudly rather than
    // quietly producing a capture that is missing 98% of its samples.
    let err = session
        .configure(&CaptureConfig {
            sample_rate_hz: 50_000_000,
            ..Default::default()
        })
        .expect_err("an impossible rate must be refused");
    let text = err.to_string();
    assert!(text.contains("50000000"), "{text}");
    assert!(
        text.contains("drop samples"),
        "the error should say why it matters: {text}"
    );
}

/// The full pipeline: device -> session -> capture file -> analysis.
#[test]
fn a_capture_written_from_the_simulator_reads_back_and_analyses() {
    let rate = 50_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e2e.pprof");

    {
        let mut rig = Rig::start(sim_config(Profile::ble_sensor(), rate));
        let mut session = rig.session();

        let header = CaptureHeader {
            sample_rate_hz: rate,
            ..Default::default()
        };
        let mut writer = CaptureWriter::create(&path, header, WriterOptions::default()).unwrap();
        writer.set_metadata(default_simulator_metadata());

        capture_into(&mut session, &mut writer, rate, Duration::from_millis(700));

        let summary = writer.finish().unwrap();
        assert!(
            summary.sample_count > 20_000,
            "captured only {}",
            summary.sample_count
        );
        assert!(summary.event_count > 0);
        assert!(
            summary.compression_ratio() > 1.5,
            "compression bought little: {:.2}x",
            summary.compression_ratio()
        );
    }

    let mut reader = CaptureReader::open(&path).unwrap();
    assert!(reader.integrity().is_clean(), "{:?}", reader.integrity());
    assert!(
        (reader.effective_rate_hz() - rate as f64).abs() < rate as f64 * 0.05,
        "effective rate was {}",
        reader.effective_rate_hz()
    );

    // Whole-capture statistics.
    let region = region_stats(&mut reader, TimeSpan::ALL, &StatsOptions::lenient()).unwrap();
    assert!(
        region.current_max_ua > 60_000,
        "no transmit burst: peak {} uA",
        region.current_max_ua
    );
    assert!(
        (2_000.0..12_000.0).contains(&region.current_mean_ua),
        "mean current {} uA is not plausible for a duty-cycled sensor",
        region.current_mean_ua
    );
    assert!(region.energy_uj > 0.0 && region.charge_uc > 0.0);
    assert!(region.current_rms_ua >= region.current_mean_ua);

    // Per-event statistics: the feature that makes this a firmware profiler.
    let map = reader.metadata().event_map();
    let stats = event_stats(&mut reader, &map, &StatsOptions::lenient()).unwrap();
    let tx = stats
        .iter()
        .find(|s| s.name == "BLE_TX")
        .expect("BLE_TX must appear in the analysis");

    assert!(
        tx.occurrences >= 4,
        "expected several bursts in 700 ms, got {}",
        tx.occurrences
    );
    assert_eq!(
        tx.orphaned_stops, 0,
        "pairing should not produce orphans on a clean capture"
    );
    assert!(
        tx.current_peak_ua > 60_000,
        "TX peak was only {} uA",
        tx.current_peak_ua
    );

    // 3.3 V * 78 mA * 0.95 ms is about 245 uJ. Allow generously for ramps, droop and jitter.
    let mean = tx
        .mean_energy_uj()
        .expect("BLE_TX must have a measurable energy");
    assert!(
        (150.0..400.0).contains(&mean),
        "BLE_TX mean energy {mean:.1} uJ is not near the expected 245 uJ"
    );
    assert!(tx.duration_us.mean > 500.0 && tx.duration_us.mean < 1_500.0);
}

/// The documented CI example, both halves: the same budget must pass on the good firmware and
/// fail on the regressed one. A gate that only ever passes proves nothing.
#[test]
fn the_documented_energy_budget_passes_on_good_firmware_and_fails_on_the_regression() {
    fn mean_tx_energy(profile: Profile, path: &Path) -> f64 {
        let rate = 50_000;
        {
            let mut rig = Rig::start(sim_config(profile, rate));
            let mut session = rig.session();
            let header = CaptureHeader {
                sample_rate_hz: rate,
                ..Default::default()
            };
            let mut writer = CaptureWriter::create(path, header, WriterOptions::default()).unwrap();
            writer.set_metadata(default_simulator_metadata());
            capture_into(&mut session, &mut writer, rate, Duration::from_millis(900));
            writer.finish().unwrap();
        }

        let mut reader = CaptureReader::open(path).unwrap();
        let map = reader.metadata().event_map();
        let stats = event_stats(&mut reader, &map, &StatsOptions::lenient()).unwrap();
        let tx = stats.iter().find(|s| s.name == "BLE_TX").expect("BLE_TX");
        assert!(
            tx.occurrences >= 5,
            "only {} bursts captured",
            tx.occurrences
        );
        tx.mean_energy_uj().expect("a measurable TX energy")
    }

    let dir = tempfile::tempdir().unwrap();
    let good = mean_tx_energy(Profile::ble_sensor(), &dir.path().join("good.pprof"));
    let bad = mean_tx_energy(
        Profile::ble_sensor_regressed(),
        &dir.path().join("bad.pprof"),
    );

    let regression = (bad - good) / good * 100.0;
    assert!(
        bad > good,
        "the regressed firmware must cost more: good {good:.1} uJ, bad {bad:.1} uJ"
    );
    assert!(
        regression > 10.0,
        "a 21% longer burst should read as a clear regression, got {regression:.1}% \
         (good {good:.1} uJ, bad {bad:.1} uJ)"
    );
}

/// Fault injection must actually reach the host's accounting, or the recovery paths are only
/// ever exercised by tests that construct the damage by hand.
#[test]
fn injected_faults_are_reported_rather_than_silently_absorbed() {
    let rate = 50_000;
    let mut config = sim_config(Profile::always_on(), rate);
    config.faults = wattson_sim::engine::FaultInjection {
        drop_block_prob: 0.02,
        corrupt_byte_prob: 0.02,
        error_frame_prob: 0.0,
    };

    let mut rig = Rig::start(config);
    let mut session = rig.session();
    let mut sink = CountingSink::default();
    capture_into(&mut session, &mut sink, rate, Duration::from_millis(700));

    let stats = session.decoder_stats();
    assert!(
        sink.samples > 5_000,
        "the device produced almost nothing: {}",
        sink.samples
    );
    assert!(
        stats.crc_errors > 0 || !sink.gaps.is_empty() || sink.device_errors > 0,
        "faults were injected but nothing was reported: decoder {stats:?}, gaps {:?}, \
         device errors {}",
        sink.gaps,
        sink.device_errors
    );
    // Whatever was corrupted, the decoder must have got back in step afterwards.
    if stats.crc_errors > 0 {
        assert!(
            stats.resyncs > 0,
            "the decoder never resynchronised after corruption"
        );
    }
}

/// A capture killed mid-write must survive, because Ctrl-C during a long run is normal.
#[test]
fn a_capture_interrupted_mid_write_is_still_readable() {
    let rate = 50_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("interrupted.pprof");

    {
        let mut rig = Rig::start(sim_config(Profile::always_on(), rate));
        let mut session = rig.session();
        let header = CaptureHeader {
            sample_rate_hz: rate,
            ..Default::default()
        };
        let mut writer = CaptureWriter::create(
            &path,
            header,
            WriterOptions {
                target_chunk_bytes: 8192,
                ..Default::default()
            },
        )
        .unwrap();

        session.handshake(Duration::from_secs(5)).unwrap();
        session
            .configure(&CaptureConfig {
                sample_rate_hz: rate,
                ..Default::default()
            })
            .unwrap();
        session.start().unwrap();
        session
            .run_for(Duration::from_millis(400), &mut writer)
            .unwrap();
        writer.flush().unwrap();
        // No finish(): this is what a killed process leaves behind.
        drop(writer);
    }

    let mut reader = CaptureReader::open(&path).unwrap();
    assert!(!reader.integrity().finalized);
    assert!(
        reader.integrity().recovered,
        "an unfinalized capture must be recovered"
    );
    let samples = reader.samples().unwrap();
    assert!(
        samples.len() > 5_000,
        "recovery kept only {} samples",
        samples.len()
    );
    for w in samples.windows(2) {
        assert!(
            w[1].t_ns >= w[0].t_ns,
            "recovered samples must stay in time order"
        );
    }
}
