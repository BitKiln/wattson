//! Fuzz the capture reader.
//!
//! A `.pprof` can arrive from anywhere: a colleague, a CI artifact, a bug report. Opening one
//! must never panic, and — just as importantly — a damaged file must never yield samples that
//! were never written.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wattson_core::capture::CaptureReader;
use wattson_core::time::TimeSpan;

fuzz_target!(|data: &[u8]| {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let path = dir.path().join("fuzz.pprof");
    if std::fs::write(&path, data).is_err() {
        return;
    }

    if let Ok(mut reader) = CaptureReader::open(&path) {
        // Exercise every read path, not only the open.
        let _ = std::hint::black_box(reader.samples());
        let _ = std::hint::black_box(reader.events());
        let _ = std::hint::black_box(reader.gpio());
        let _ = std::hint::black_box(reader.sync_rows());
        let _ = std::hint::black_box(reader.downsample(TimeSpan::ALL, 64));
        let _ = std::hint::black_box(reader.samples_in(TimeSpan::new(0, 1_000_000)));
        std::hint::black_box(reader.integrity());
    }
});
