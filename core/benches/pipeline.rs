//! Throughput benchmarks for the paths that decide whether the tool keeps up with hardware.
//!
//! The number to beat is **400 KB/s**: 50 ksps × 8 bytes of payload. Everything here is
//! measured against that, because a host that cannot sustain it silently loses samples, and
//! lost samples become quietly wrong energy figures.
//!
//! ```text
//! cargo bench -p wattson-core
//! ```

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use wattson_core::capture::{
    CaptureHeader, CaptureReader, CaptureWriter, Compression, WriterOptions,
};
use wattson_core::stats::{StatsOptions, region_stats};
use wattson_core::time::TimeSpan;
use wattson_protocol::{Decoder, FrameType, MAX_ENCODED, MAX_PAYLOAD, SampleBlock, encode_frame};

/// Samples per protocol block, matching what a device actually sends.
const BLOCK: usize = 64;

/// A realistic-ish trace: mostly idle, with periodic bursts.
fn trace(n: usize) -> Vec<(i32, u32)> {
    (0..n)
        .map(|i| {
            let phase = i % 5_000;
            let current = if phase < 48 {
                78_000
            } else {
                3_000 + (i % 40) as i32
            };
            // The rail droops under load, as a real supply does.
            let voltage = 3_300_000 - (current / 8) as u32;
            (current, voltage)
        })
        .collect()
}

/// Encode a trace as a stream of framed sample blocks, exactly as a device would.
fn wire_stream(samples: &[(i32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut payload = [0u8; MAX_PAYLOAD];
    let mut frame = [0u8; MAX_ENCODED];
    let mut seq = 0u8;

    for (i, chunk) in samples.chunks(BLOCK).enumerate() {
        let t0 = (i * BLOCK * 20) as u32;
        let n = SampleBlock::encode_uniform(&mut payload, t0, 20, chunk).expect("encode");
        let m =
            encode_frame(FrameType::CurrentSamples, seq, &payload[..n], &mut frame).expect("frame");
        out.extend_from_slice(&frame[..m]);
        seq = seq.wrapping_add(1);
    }
    out
}

fn bench_decode(c: &mut Criterion) {
    let samples = trace(50_000); // one second at 50 ksps
    let stream = wire_stream(&samples);

    let mut group = c.benchmark_group("decode");
    // Measured against wire bytes, so the result compares directly to a link's throughput.
    group.throughput(Throughput::Bytes(stream.len() as u64));
    group.bench_function("frames_to_samples", |b| {
        b.iter(|| {
            let mut decoder = Decoder::new();
            let mut count = 0usize;
            decoder.feed(black_box(&stream), &mut |_, frame| {
                if let wattson_protocol::Frame::CurrentSamples(block) = frame {
                    for s in block.iter() {
                        count += black_box(s).current_ua as usize & 1;
                    }
                }
            });
            black_box(count)
        });
    });
    group.finish();
}

fn bench_capture_write(c: &mut Criterion) {
    let samples = trace(50_000);
    let dir = tempfile::tempdir().expect("tempdir");

    let mut group = c.benchmark_group("capture_write");
    group.throughput(Throughput::Elements(samples.len() as u64));

    for (name, compression) in [("zstd", Compression::Zstd), ("none", Compression::None)] {
        group.bench_function(name, |b| {
            let mut i = 0u32;
            b.iter(|| {
                i += 1;
                let path: PathBuf = dir.path().join(format!("bench{i}.pprof"));
                let mut w = CaptureWriter::create(
                    &path,
                    CaptureHeader {
                        sample_rate_hz: 50_000,
                        ..Default::default()
                    },
                    WriterOptions {
                        compression,
                        ..Default::default()
                    },
                )
                .expect("create");
                for (j, &(current, voltage)) in samples.iter().enumerate() {
                    w.push_sample(j as u64 * 20_000, current, Some(voltage))
                        .expect("push");
                }
                let summary = w.finish().expect("finish");
                let _ = std::fs::remove_file(&path);
                black_box(summary.bytes_written)
            });
        });
    }
    group.finish();
}

/// Build a capture once, for the read-side benchmarks.
fn reference_capture(dir: &std::path::Path, n: usize) -> PathBuf {
    let path = dir.join("read_bench.pprof");
    let samples = trace(n);
    let mut w = CaptureWriter::create(
        &path,
        CaptureHeader {
            sample_rate_hz: 50_000,
            ..Default::default()
        },
        WriterOptions::default(),
    )
    .expect("create");
    for (j, &(current, voltage)) in samples.iter().enumerate() {
        w.push_sample(j as u64 * 20_000, current, Some(voltage))
            .expect("push");
    }
    w.finish().expect("finish");
    path
}

fn bench_capture_read(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = reference_capture(dir.path(), 500_000); // ten seconds at 50 ksps

    let mut group = c.benchmark_group("capture_read");
    group.throughput(Throughput::Elements(500_000));
    group.bench_function("all_samples", |b| {
        b.iter(|| {
            let mut r = CaptureReader::open(&path).expect("open");
            black_box(r.samples().expect("samples").len())
        });
    });
    group.finish();

    // The zoom pyramid is what a GUI leans on, so its cost is the one that decides whether a
    // whole-capture overview feels instant.
    let mut group = c.benchmark_group("downsample");
    for buckets in [64usize, 1_024] {
        group.bench_function(format!("{buckets}_buckets"), |b| {
            let mut r = CaptureReader::open(&path).expect("open");
            let span = r.span();
            b.iter(|| black_box(r.downsample(span, buckets).expect("downsample").len()));
        });
    }
    group.finish();
}

fn bench_stats(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = reference_capture(dir.path(), 500_000);

    let mut group = c.benchmark_group("stats");
    group.throughput(Throughput::Elements(500_000));
    group.bench_function("region_stats", |b| {
        let mut r = CaptureReader::open(&path).expect("open");
        b.iter(|| {
            black_box(
                region_stats(&mut r, TimeSpan::ALL, &StatsOptions::lenient())
                    .expect("stats")
                    .energy_uj,
            )
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_decode,
    bench_capture_write,
    bench_capture_read,
    bench_stats
);
criterion_main!(benches);
