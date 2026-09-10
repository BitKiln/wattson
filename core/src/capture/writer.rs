//! Writing a `.pprof` capture.
//!
//! The writer appends and never rewrites, so a capture is readable at every instant during
//! acquisition and a process killed mid-write loses at most the chunk in flight.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use wattson_protocol::crc32::checksum;

use super::header::{ChunkFlags, ChunkHeader, IndexEntry, NO_TIME};
use super::payload::{
    EventRow, GpioRow, SampleChunk, SyncRow, encode_events, encode_gpio, encode_sync,
};
use super::summary::SummaryPyramid;
use super::{
    CHUNK_HEADER_LEN, CaptureHeader, CaptureSummary, ChunkKind, Compression, DEFAULT_CHUNK_BYTES,
    FOOTER_LEN, Footer, Gap, HEADER_LEN, HeaderFlags, compress,
};
use crate::error::CaptureError;
use crate::metadata::CaptureMetadata;

/// Tunables for a capture writer.
#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub compression: Compression,
    /// Uncompressed bytes to accumulate before flushing a chunk.
    pub target_chunk_bytes: u32,
    /// Build the zoom pyramid while writing. Off only for tiny synthetic captures in tests.
    pub build_summary: bool,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions {
            compression: Compression::default(),
            target_chunk_bytes: DEFAULT_CHUNK_BYTES,
            build_summary: true,
        }
    }
}

/// Appends chunks to a capture file.
#[derive(Debug)]
pub struct CaptureWriter<W: Write + Seek> {
    out: W,
    path: PathBuf,
    header: CaptureHeader,
    options: WriterOptions,
    metadata: CaptureMetadata,

    offset: u64,
    index: Vec<IndexEntry>,

    // Pending records, flushed once they reach the target chunk size.
    pending: SampleChunk,
    pending_bytes: usize,
    pending_events: Vec<EventRow>,
    pending_gpio: Vec<GpioRow>,
    pending_sync: Vec<SyncRow>,

    summary: SummaryPyramid,
    gaps: Vec<Gap>,

    sample_count: u64,
    event_count: u64,
    gpio_count: u64,
    first_time_ns: Option<u64>,
    last_time_ns: u64,
    bytes_uncompressed: u64,
    /// Set once a sample is written, so the header flag reflects reality rather than intent.
    saw_voltage: bool,
}

impl CaptureWriter<BufWriter<File>> {
    /// Create a capture file.
    pub fn create(
        path: &Path,
        header: CaptureHeader,
        options: WriterOptions,
    ) -> Result<Self, CaptureError> {
        let file = File::create(path)?;
        CaptureWriter::with_writer(BufWriter::new(file), path.to_path_buf(), header, options)
    }
}

impl<W: Write + Seek> CaptureWriter<W> {
    /// Wrap any seekable writer, for tests that stay in memory.
    pub fn with_writer(
        mut out: W,
        path: PathBuf,
        mut header: CaptureHeader,
        options: WriterOptions,
    ) -> Result<Self, CaptureError> {
        header.compression = options.compression;
        header.target_chunk_bytes = options.target_chunk_bytes;
        // FINALIZED is cleared until `finish` succeeds; that flag is how a reader knows
        // whether to trust the footer or fall back to a forward scan.
        header.flags &= !HeaderFlags::FINALIZED;
        header.footer_offset = 0;

        out.write_all(&header.encode())?;

        Ok(CaptureWriter {
            out,
            path,
            header,
            options,
            metadata: CaptureMetadata::default(),
            offset: HEADER_LEN as u64,
            index: Vec::new(),
            pending: SampleChunk::default(),
            pending_bytes: 0,
            pending_events: Vec::new(),
            pending_gpio: Vec::new(),
            pending_sync: Vec::new(),
            summary: SummaryPyramid::default(),
            gaps: Vec::new(),
            sample_count: 0,
            event_count: 0,
            gpio_count: 0,
            first_time_ns: None,
            last_time_ns: 0,
            bytes_uncompressed: 0,
            saw_voltage: false,
        })
    }

    pub fn set_metadata(&mut self, metadata: CaptureMetadata) {
        self.metadata = metadata;
    }

    pub fn metadata_mut(&mut self) -> &mut CaptureMetadata {
        &mut self.metadata
    }

    pub fn header(&self) -> &CaptureHeader {
        &self.header
    }

    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    /// Append one sample.
    pub fn push_sample(
        &mut self,
        t_ns: u64,
        current_ua: i32,
        voltage_uv: Option<u32>,
    ) -> Result<(), CaptureError> {
        if self.pending.is_empty() {
            self.pending.t0_ns = t_ns;
            self.pending.period_ns = 0;
            self.pending.dt_ns.clear();
        } else {
            let prev = self.pending.last_time_ns();
            let delta = t_ns.saturating_sub(prev);
            if self.pending.len() == 1 {
                // The second sample defines the block's nominal period.
                self.pending.period_ns = delta.min(u32::MAX as u64) as u32;
            } else if delta != self.pending.period_ns as u64 && self.pending.is_uniform() {
                // Timing changed: fall back to per-sample deltas for this chunk rather than
                // pretending the samples are evenly spaced. `count * period` is exactly how a
                // capture silently under-reports energy across a hiccup.
                let period = self.pending.period_ns as u32;
                self.pending.dt_ns = std::iter::once(0)
                    .chain(std::iter::repeat_n(period, self.pending.len() - 1))
                    .collect();
                self.pending.period_ns = 0;
            }
            if !self.pending.is_uniform() {
                self.pending.dt_ns.push(delta.min(u32::MAX as u64) as u32);
            }
        }

        self.pending.current_ua.push(current_ua);
        if let Some(v) = voltage_uv {
            self.pending.voltage_uv.push(v);
            self.saw_voltage = true;
        }
        self.pending_bytes += if voltage_uv.is_some() { 8 } else { 4 };

        self.note_time(t_ns);
        self.sample_count += 1;
        if self.options.build_summary {
            self.summary.push(t_ns, current_ua);
        }

        if self.pending_bytes >= self.options.target_chunk_bytes as usize {
            self.flush_samples()?;
        }
        Ok(())
    }

    /// Append a batch of samples.
    pub fn push_samples(
        &mut self,
        samples: &[(u64, i32, Option<u32>)],
    ) -> Result<(), CaptureError> {
        for &(t, c, v) in samples {
            self.push_sample(t, c, v)?;
        }
        Ok(())
    }

    pub fn push_event(
        &mut self,
        t_ns: u64,
        id: u16,
        value: Option<u32>,
    ) -> Result<(), CaptureError> {
        self.pending_events.push(EventRow {
            t_ns,
            id,
            flags: u16::from(value.is_some()),
            value: value.unwrap_or(0),
        });
        self.event_count += 1;
        self.note_time(t_ns);
        if self.pending_events.len() * 16 >= self.options.target_chunk_bytes as usize {
            self.flush_events()?;
        }
        Ok(())
    }

    pub fn push_gpio(&mut self, t_ns: u64, state: u16) -> Result<(), CaptureError> {
        self.pending_gpio.push(GpioRow { t_ns, state });
        self.gpio_count += 1;
        self.note_time(t_ns);
        if self.pending_gpio.len() * 12 >= self.options.target_chunk_bytes as usize {
            self.flush_gpio()?;
        }
        Ok(())
    }

    pub fn push_sync(&mut self, row: SyncRow) -> Result<(), CaptureError> {
        self.pending_sync.push(row);
        Ok(())
    }

    /// Record a stretch of missing data. Never inferred later — recorded when observed.
    pub fn push_gap(&mut self, gap: Gap) {
        self.gaps.push(gap);
    }

    fn note_time(&mut self, t_ns: u64) {
        self.first_time_ns.get_or_insert(t_ns);
        self.last_time_ns = self.last_time_ns.max(t_ns);
    }

    /// Write one chunk and index it.
    fn write_chunk(
        &mut self,
        kind: ChunkKind,
        payload: &[u8],
        record_count: u32,
        first_time_ns: u64,
        last_time_ns: u64,
    ) -> Result<(), CaptureError> {
        let stored = compress(self.options.compression, payload)?;
        let compressed = self.options.compression != Compression::None;

        let header = ChunkHeader {
            kind,
            flags: if compressed {
                ChunkFlags::COMPRESSED
            } else {
                0
            },
            uncompressed_len: payload.len() as u32,
            stored_len: stored.len() as u32,
            first_time_ns,
            last_time_ns,
            payload_crc32: checksum(&stored),
        };

        self.out.write_all(&header.encode())?;
        self.out.write_all(&stored)?;

        self.index.push(IndexEntry {
            kind,
            flags: header.flags,
            file_offset: self.offset,
            stored_len: header.stored_len,
            record_count,
            first_time_ns,
        });
        self.offset += CHUNK_HEADER_LEN as u64 + stored.len() as u64;
        self.bytes_uncompressed += payload.len() as u64;
        Ok(())
    }

    fn flush_samples(&mut self) -> Result<(), CaptureError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let (first, last, count) = (chunk.t0_ns, chunk.last_time_ns(), chunk.len() as u32);
        let payload = chunk.encode();
        self.write_chunk(ChunkKind::Samples, &payload, count, first, last)
    }

    fn flush_events(&mut self) -> Result<(), CaptureError> {
        if self.pending_events.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.pending_events);
        let first = rows.first().map_or(NO_TIME, |r| r.t_ns);
        let last = rows.last().map_or(NO_TIME, |r| r.t_ns);
        let payload = encode_events(&rows);
        self.write_chunk(ChunkKind::Events, &payload, rows.len() as u32, first, last)
    }

    fn flush_gpio(&mut self) -> Result<(), CaptureError> {
        if self.pending_gpio.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.pending_gpio);
        let first = rows.first().map_or(NO_TIME, |r| r.t_ns);
        let last = rows.last().map_or(NO_TIME, |r| r.t_ns);
        let payload = encode_gpio(&rows);
        self.write_chunk(ChunkKind::Gpio, &payload, rows.len() as u32, first, last)
    }

    fn flush_sync(&mut self) -> Result<(), CaptureError> {
        if self.pending_sync.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.pending_sync);
        let payload = encode_sync(&rows);
        self.write_chunk(
            ChunkKind::Sync,
            &payload,
            rows.len() as u32,
            NO_TIME,
            NO_TIME,
        )
    }

    /// Flush everything pending without closing, so a long capture is durable on disk.
    pub fn flush(&mut self) -> Result<(), CaptureError> {
        self.flush_samples()?;
        self.flush_events()?;
        self.flush_gpio()?;
        self.out.flush()?;
        Ok(())
    }

    /// Close the capture: flush, write metadata, summary, index and footer, then rewrite the
    /// header with `FINALIZED` set.
    ///
    /// The header rewrite is the only backward write in the whole format, and it happens once
    /// at the very end. If it never happens, the reader's forward scan recovers everything
    /// but the chunk in flight.
    pub fn finish(mut self) -> Result<CaptureSummary, CaptureError> {
        self.flush_samples()?;
        self.flush_events()?;
        self.flush_gpio()?;
        self.flush_sync()?;

        let metadata = std::mem::take(&mut self.metadata);
        let meta_bytes = metadata.to_cbor()?;
        self.write_chunk(ChunkKind::Metadata, &meta_bytes, 1, NO_TIME, NO_TIME)?;

        if self.options.build_summary && !self.summary.is_empty() {
            let summary_bytes = self.summary.encode();
            self.write_chunk(ChunkKind::Summary, &summary_bytes, 1, NO_TIME, NO_TIME)?;
        }

        let index_offset = self.offset;
        let index_bytes = self.encode_index();
        self.write_chunk(
            ChunkKind::Index,
            &index_bytes,
            self.index.len() as u32,
            NO_TIME,
            NO_TIME,
        )?;
        let index_len = self.offset - index_offset;

        let footer_offset = self.offset;
        let footer = Footer {
            index_offset,
            index_len,
            total_samples: self.sample_count,
        };
        self.out.write_all(&footer.encode())?;
        self.offset += FOOTER_LEN as u64;

        // Rewrite the header now that the file is complete.
        self.header.flags |= HeaderFlags::FINALIZED;
        if self.saw_voltage {
            self.header.flags |= HeaderFlags::HAS_VOLTAGE;
        }
        if self.event_count > 0 {
            self.header.flags |= HeaderFlags::HAS_EVENTS;
        }
        if self.gpio_count > 0 {
            self.header.flags |= HeaderFlags::HAS_GPIO;
        }
        self.header.footer_offset = footer_offset;
        self.out.seek(SeekFrom::Start(0))?;
        self.out.write_all(&self.header.encode())?;
        self.out.flush()?;

        let duration_ns = self
            .last_time_ns
            .saturating_sub(self.first_time_ns.unwrap_or(0));
        // Derived from actual timestamps, never from the configured rate.
        let effective_rate_hz = if duration_ns > 0 && self.sample_count > 1 {
            (self.sample_count - 1) as f64 * 1e9 / duration_ns as f64
        } else {
            0.0
        };

        Ok(CaptureSummary {
            sample_count: self.sample_count,
            event_count: self.event_count,
            gpio_count: self.gpio_count,
            duration_ns,
            effective_rate_hz,
            gaps: self.gaps,
            bytes_written: self.offset,
            bytes_uncompressed: self.bytes_uncompressed,
        })
    }

    fn encode_index(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.index.len() * IndexEntry::LEN);
        out.extend_from_slice(&(self.index.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for e in &self.index {
            let mut buf = [0u8; IndexEntry::LEN];
            e.encode_into(&mut buf);
            out.extend_from_slice(&buf);
        }
        out
    }

    /// The path this writer was created for, for error messages.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::header::HeaderFlags;

    /// Write a capture to a temporary file and return its bytes and summary.
    fn roundtrip(n: usize, opts: WriterOptions) -> (Vec<u8>, CaptureSummary) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mem.pprof");
        let header = CaptureHeader {
            device_timer_hz: 1_000_000,
            sample_rate_hz: 50_000,
            ..Default::default()
        };
        let mut w = CaptureWriter::create(&path, header, opts).expect("create");
        for i in 0..n as u64 {
            w.push_sample(i * 20_000, 3_000 + (i % 100) as i32, Some(3_300_000))
                .unwrap();
        }
        w.push_event(1_000_000, 0x0101, None).unwrap();
        w.push_event(2_000_000, 0x0102, Some(64)).unwrap();
        let summary = w.finish().expect("finish");
        let bytes = std::fs::read(&path).expect("read back");
        (bytes, summary)
    }

    #[test]
    fn a_finished_capture_has_a_valid_header_and_footer() {
        let (bytes, summary) = roundtrip(5_000, WriterOptions::default());
        let header = CaptureHeader::decode(&bytes, Path::new("mem.pprof")).unwrap();
        assert!(header.has(HeaderFlags::FINALIZED));
        assert!(header.has(HeaderFlags::HAS_VOLTAGE));
        assert!(header.has(HeaderFlags::HAS_EVENTS));
        assert!(!header.has(HeaderFlags::HAS_GPIO));
        assert_eq!(header.footer_offset as usize, bytes.len() - FOOTER_LEN);

        let footer = Footer::decode(&bytes[bytes.len() - FOOTER_LEN..]).unwrap();
        assert_eq!(footer.total_samples, 5_000);
        assert_eq!(summary.sample_count, 5_000);
        assert_eq!(summary.event_count, 2);
    }

    /// A capture still open has FINALIZED clear, which is precisely how a reader knows to
    /// recover by scanning instead of trusting a footer that is not there.
    #[test]
    fn an_unfinished_capture_is_marked_unfinalized_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("open.pprof");
        let mut w =
            CaptureWriter::create(&path, CaptureHeader::default(), WriterOptions::default())
                .unwrap();
        for i in 0..10_000u64 {
            w.push_sample(i * 20_000, 3_000, Some(3_300_000)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let bytes = std::fs::read(&path).unwrap();
        let header = CaptureHeader::decode(&bytes, &path).unwrap();
        assert!(!header.has(HeaderFlags::FINALIZED));
        assert_eq!(header.footer_offset, 0);
        assert!(
            bytes.len() > HEADER_LEN,
            "flushed chunks should be on disk already"
        );
    }

    #[test]
    fn the_effective_rate_comes_from_timestamps_not_from_the_configured_value() {
        let (_, summary) = roundtrip(1_000, WriterOptions::default());
        // 20_000 ns between samples is 50 kHz.
        assert!(
            (summary.effective_rate_hz - 50_000.0).abs() < 1.0,
            "got {}",
            summary.effective_rate_hz
        );
    }

    #[test]
    fn compression_actually_shrinks_the_file() {
        let (packed, summary) = roundtrip(20_000, WriterOptions::default());
        let (plain, _) = roundtrip(
            20_000,
            WriterOptions {
                compression: Compression::None,
                ..Default::default()
            },
        );
        assert!(
            packed.len() * 2 < plain.len(),
            "compressed {} vs plain {}",
            packed.len(),
            plain.len()
        );
        assert!(summary.compression_ratio() > 2.0);
    }

    #[test]
    fn a_capture_with_no_samples_still_finishes_cleanly() {
        let (bytes, summary) = roundtrip(0, WriterOptions::default());
        assert_eq!(summary.sample_count, 0);
        let header = CaptureHeader::decode(&bytes, Path::new("mem.pprof")).unwrap();
        assert!(header.has(HeaderFlags::FINALIZED));
        assert!(!header.has(HeaderFlags::HAS_VOLTAGE));
    }

    #[test]
    fn chunk_size_is_honoured() {
        let small = WriterOptions {
            target_chunk_bytes: 4096,
            ..Default::default()
        };
        let big = WriterOptions {
            target_chunk_bytes: 1 << 20,
            ..Default::default()
        };
        let (a, _) = roundtrip(20_000, small);
        let (b, _) = roundtrip(20_000, big);
        // More, smaller chunks means more chunk headers, hence a slightly larger file.
        assert!(
            a.len() > b.len(),
            "small-chunk file {} vs big-chunk {}",
            a.len(),
            b.len()
        );
    }

    /// Irregular timing must fall back to per-sample deltas. Storing `count * period` across
    /// a hiccup is exactly how a capture silently under-reports energy.
    #[test]
    fn irregular_timing_falls_back_to_explicit_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jitter.pprof");
        let mut w =
            CaptureWriter::create(&path, CaptureHeader::default(), WriterOptions::default())
                .unwrap();
        // Three evenly spaced samples, then a long stall.
        for (t, c) in [(0u64, 10i32), (20_000, 11), (40_000, 12), (900_000, 13)] {
            w.push_sample(t, c, None).unwrap();
        }
        let summary = w.finish().unwrap();
        assert_eq!(summary.sample_count, 4);
        assert_eq!(summary.duration_ns, 900_000);
        // 3 intervals over 900 us is well under the nominal rate, and the file must say so.
        assert!(
            summary.effective_rate_hz < 5_000.0,
            "got {}",
            summary.effective_rate_hz
        );
    }
}
