//! Reading a `.pprof` capture.
//!
//! # Recovery
//!
//! A capture killed mid-write has no footer and no index. Rather than refusing to open it,
//! the reader falls back to a **forward scan** from the end of the file header, validating
//! each chunk by its magic, header CRC, and length, and stopping at the first thing that is
//! not a chunk. That costs about fifty lines and is the difference between "my forty-minute
//! capture is gone" and "fine, the last 164 ms are missing".
//!
//! The same scan is the fallback whenever the footer is present but damaged.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use wattson_protocol::crc32::checksum;

use super::header::{ChunkHeader, IndexEntry, NO_TIME};
use super::payload::{SampleChunk, SyncRow, decode_events, decode_gpio, decode_sync};
use super::summary::{Bucket, SummaryPyramid, bucketize};
use super::{
    CHUNK_HEADER_LEN, CaptureHeader, ChunkKind, FOOTER_LEN, Footer, Gap, GapCause, HEADER_LEN,
    HeaderFlags, decompress,
};
use crate::error::CaptureError;
use crate::metadata::CaptureMetadata;
use crate::time::TimeSpan;

/// What was wrong with a capture file, if anything.
///
/// Surfaced rather than swallowed: a capture with holes in it can still be analysed, but the
/// person reading the numbers has to know the holes are there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntegrityReport {
    /// The writer finished cleanly and the footer was trustworthy.
    pub finalized: bool,
    /// The index was rebuilt by scanning because the footer was missing or damaged.
    pub recovered: bool,
    /// Byte offset where the scan stopped, when the file was truncated.
    pub truncated_at: Option<u64>,
    /// Chunks whose payload CRC did not match, and which were therefore skipped.
    pub corrupt_chunks: usize,
    /// Chunk kinds this build does not understand, skipped by length.
    pub unknown_chunks: usize,
    pub gaps: Vec<Gap>,
}

impl IntegrityReport {
    /// `true` if the capture is complete and undamaged.
    pub fn is_clean(&self) -> bool {
        self.finalized
            && !self.recovered
            && self.truncated_at.is_none()
            && self.corrupt_chunks == 0
            && self.gaps.is_empty()
    }
}

/// One event as stored in a capture.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub t_ns: u64,
    pub id: u16,
    pub value: Option<u32>,
}

/// One GPIO edge as stored in a capture.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StoredGpio {
    pub t_ns: u64,
    pub state: u16,
}

/// One sample as read back.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ReadSample {
    pub t_ns: u64,
    pub current_ua: i32,
    pub voltage_uv: u32,
}

/// A capture file opened for reading.
#[derive(Debug)]
pub struct CaptureReader {
    file: File,
    path: PathBuf,
    header: CaptureHeader,
    metadata: CaptureMetadata,
    index: Vec<IndexEntry>,
    summary: Option<SummaryPyramid>,
    integrity: IntegrityReport,
    span: TimeSpan,
    sample_count: u64,
    /// Supply to assume for captures with no voltage channel.
    assumed_supply_uv: u32,
}

impl CaptureReader {
    /// Open a capture, recovering it if the writer never finished.
    pub fn open(path: &Path) -> Result<CaptureReader, CaptureError> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();

        let mut header_bytes = [0u8; HEADER_LEN];
        if file_len < HEADER_LEN as u64 {
            return Err(CaptureError::NotACapture {
                path: path.to_path_buf(),
            });
        }
        file.read_exact(&mut header_bytes)?;
        let header = CaptureHeader::decode(&header_bytes, path)?;

        let mut integrity = IntegrityReport {
            finalized: header.has(HeaderFlags::FINALIZED),
            ..Default::default()
        };

        // Trust the footer only when the writer said it finished *and* the footer verifies.
        let index = match Self::read_footer_index(&mut file, &header, file_len) {
            Some(index) if integrity.finalized => index,
            _ => {
                integrity.recovered = true;
                Self::scan_chunks(&mut file, file_len, &mut integrity)?
            }
        };

        let mut reader = CaptureReader {
            file,
            path: path.to_path_buf(),
            header,
            metadata: CaptureMetadata::default(),
            index,
            summary: None,
            integrity,
            span: TimeSpan::EMPTY,
            sample_count: 0,
            assumed_supply_uv: 0,
        };
        reader.load_sidecars()?;
        reader.compute_span();
        Ok(reader)
    }

    /// Read the index the writer stored, or `None` if there is not a usable one.
    fn read_footer_index(
        file: &mut File,
        header: &CaptureHeader,
        file_len: u64,
    ) -> Option<Vec<IndexEntry>> {
        if file_len < (HEADER_LEN + FOOTER_LEN) as u64 {
            return None;
        }
        let mut footer_bytes = [0u8; FOOTER_LEN];
        file.seek(SeekFrom::End(-(FOOTER_LEN as i64))).ok()?;
        file.read_exact(&mut footer_bytes).ok()?;
        let footer = Footer::decode(&footer_bytes)?;
        if header.footer_offset != file_len - FOOTER_LEN as u64 {
            return None;
        }

        let chunk = Self::read_chunk_at(file, footer.index_offset, header).ok()??;
        if chunk.0.kind != ChunkKind::Index {
            return None;
        }
        Self::decode_index(&chunk.1)
    }

    fn decode_index(payload: &[u8]) -> Option<Vec<IndexEntry>> {
        if payload.len() < 8 {
            return None;
        }
        let count = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        let need = 8 + count * IndexEntry::LEN;
        if payload.len() < need {
            return None;
        }
        Some(
            payload[8..need]
                .chunks_exact(IndexEntry::LEN)
                .filter_map(IndexEntry::decode)
                .collect(),
        )
    }

    /// Rebuild the index by walking the file, stopping at the first thing that is not a chunk.
    fn scan_chunks(
        file: &mut File,
        file_len: u64,
        integrity: &mut IntegrityReport,
    ) -> Result<Vec<IndexEntry>, CaptureError> {
        let mut index = Vec::new();
        let mut offset = HEADER_LEN as u64;
        let mut header_buf = [0u8; CHUNK_HEADER_LEN];

        while offset + CHUNK_HEADER_LEN as u64 <= file_len {
            file.seek(SeekFrom::Start(offset))?;
            if file.read_exact(&mut header_buf).is_err() {
                break;
            }
            let Some(ch) = ChunkHeader::decode(&header_buf) else {
                // Not a chunk: this is where the file stops being meaningful.
                integrity.truncated_at = Some(offset);
                break;
            };
            let end = offset + ch.total_len();
            if end > file_len {
                // The chunk header survived but its payload did not.
                integrity.truncated_at = Some(offset);
                break;
            }
            index.push(IndexEntry {
                kind: ch.kind,
                flags: ch.flags,
                file_offset: offset,
                stored_len: ch.stored_len,
                record_count: 0,
                first_time_ns: ch.first_time_ns,
            });
            offset = end;
        }

        // Trailing bytes that are neither a chunk nor the footer mean truncation.
        if integrity.truncated_at.is_none()
            && offset != file_len
            && offset + FOOTER_LEN as u64 != file_len
        {
            integrity.truncated_at = Some(offset);
        }
        if let Some(at) = integrity.truncated_at {
            integrity.gaps.push(Gap {
                start_ns: index.last().map_or(0, |e| e.first_time_ns),
                end_ns: u64::MAX,
                cause: GapCause::Truncation,
                lost_estimate: 0,
            });
            let _ = at;
        }
        Ok(index)
    }

    /// Read a chunk header and its decompressed payload.
    fn read_chunk_at(
        file: &mut File,
        offset: u64,
        header: &CaptureHeader,
    ) -> Result<Option<(ChunkHeader, Vec<u8>)>, CaptureError> {
        let mut hb = [0u8; CHUNK_HEADER_LEN];
        file.seek(SeekFrom::Start(offset))?;
        if file.read_exact(&mut hb).is_err() {
            return Ok(None);
        }
        let Some(ch) = ChunkHeader::decode(&hb) else {
            return Ok(None);
        };
        let mut stored = vec![0u8; ch.stored_len as usize];
        if file.read_exact(&mut stored).is_err() {
            return Ok(None);
        }
        if checksum(&stored) != ch.payload_crc32 {
            return Ok(None);
        }
        let payload = if ch.is_compressed() {
            decompress(header.compression, &stored, ch.uncompressed_len as usize)?.into_owned()
        } else {
            stored
        };
        Ok(Some((ch, payload)))
    }

    /// Load metadata and summary chunks, and count samples.
    fn load_sidecars(&mut self) -> Result<(), CaptureError> {
        let entries = self.index.clone();
        for e in &entries {
            match e.kind {
                ChunkKind::Metadata => {
                    if let Some((_, payload)) =
                        Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)?
                    {
                        self.metadata = CaptureMetadata::from_cbor(&payload)?;
                    }
                }
                ChunkKind::Summary => {
                    if let Some((_, payload)) =
                        Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)?
                    {
                        self.summary = SummaryPyramid::decode(&payload).ok();
                    }
                }
                ChunkKind::Unknown(_) => self.integrity.unknown_chunks += 1,
                _ => {}
            }
        }
        self.assumed_supply_uv = self.metadata.assumed_supply_uv.unwrap_or(3_300_000);
        Ok(())
    }

    fn compute_span(&mut self) {
        let mut start = u64::MAX;
        let mut end = 0u64;
        let mut samples = 0u64;

        let entries = self.index.clone();
        for e in &entries {
            if e.kind != ChunkKind::Samples {
                continue;
            }
            if let Ok(Some((ch, payload))) =
                Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)
                && let Ok(chunk) = SampleChunk::decode(&payload)
            {
                samples += chunk.len() as u64;
                if !chunk.is_empty() {
                    start = start.min(ch.first_time_ns);
                    end = end.max(ch.last_time_ns.saturating_add(1));
                }
            }
        }

        self.sample_count = samples;
        self.span = if start == u64::MAX {
            TimeSpan::EMPTY
        } else {
            TimeSpan::new(start, end)
        };
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &CaptureHeader {
        &self.header
    }

    pub fn metadata(&self) -> &CaptureMetadata {
        &self.metadata
    }

    pub fn integrity(&self) -> &IntegrityReport {
        &self.integrity
    }

    pub fn span(&self) -> TimeSpan {
        self.span
    }

    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    pub fn gaps(&self) -> &[Gap] {
        &self.integrity.gaps
    }

    /// The stored zoom pyramid, if the writer built one.
    pub fn summary(&self) -> Option<&SummaryPyramid> {
        self.summary.as_ref()
    }

    /// Effective sample rate, derived from the actual span and count.
    pub fn effective_rate_hz(&self) -> f64 {
        let d = self.span.duration_ns();
        if d == 0 || self.sample_count < 2 {
            0.0
        } else {
            (self.sample_count - 1) as f64 * 1e9 / d as f64
        }
    }

    /// Every sample in the capture, in time order.
    pub fn samples(&mut self) -> Result<Vec<ReadSample>, CaptureError> {
        self.samples_in(TimeSpan::ALL)
    }

    /// Samples overlapping `span`, seeking past chunks that cannot contribute.
    pub fn samples_in(&mut self, span: TimeSpan) -> Result<Vec<ReadSample>, CaptureError> {
        let mut out = Vec::new();
        let entries: Vec<IndexEntry> = self
            .index
            .iter()
            .filter(|e| e.kind == ChunkKind::Samples)
            .copied()
            .collect();
        let supply = self.assumed_supply_uv;

        for e in entries {
            let Some((ch, payload)) =
                Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)?
            else {
                self.integrity.corrupt_chunks += 1;
                continue;
            };
            // Chunk time bounds let whole chunks be skipped without decompressing them,
            // which is what makes a zoomed-in view cheap on a multi-gigabyte capture.
            if ch.first_time_ns != NO_TIME
                && (ch.first_time_ns >= span.end_ns || ch.last_time_ns < span.start_ns)
            {
                continue;
            }
            let chunk = SampleChunk::decode(&payload)?;
            for i in 0..chunk.len() {
                let t = chunk.time_ns(i);
                if span.contains(t) {
                    out.push(ReadSample {
                        t_ns: t,
                        current_ua: chunk.current_ua[i],
                        voltage_uv: chunk.voltage_or(i, supply),
                    });
                }
            }
        }
        out.sort_by_key(|s| s.t_ns);
        Ok(out)
    }

    /// Every stored event, in time order.
    pub fn events(&mut self) -> Result<Vec<StoredEvent>, CaptureError> {
        let entries: Vec<IndexEntry> = self
            .index
            .iter()
            .filter(|e| e.kind == ChunkKind::Events)
            .copied()
            .collect();
        let mut out = Vec::new();
        for e in entries {
            if let Some((_, payload)) =
                Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)?
            {
                for r in decode_events(&payload)? {
                    out.push(StoredEvent {
                        t_ns: r.t_ns,
                        id: r.id,
                        value: (r.flags & 1 != 0).then_some(r.value),
                    });
                }
            } else {
                self.integrity.corrupt_chunks += 1;
            }
        }
        out.sort_by_key(|e| e.t_ns);
        Ok(out)
    }

    /// Every stored GPIO edge, in time order.
    pub fn gpio(&mut self) -> Result<Vec<StoredGpio>, CaptureError> {
        let entries: Vec<IndexEntry> = self
            .index
            .iter()
            .filter(|e| e.kind == ChunkKind::Gpio)
            .copied()
            .collect();
        let mut out = Vec::new();
        for e in entries {
            if let Some((_, payload)) =
                Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)?
            {
                for r in decode_gpio(&payload)? {
                    out.push(StoredGpio {
                        t_ns: r.t_ns,
                        state: r.state,
                    });
                }
            }
        }
        out.sort_by_key(|g| g.t_ns);
        Ok(out)
    }

    /// Stored clock-correlation triples.
    pub fn sync_rows(&mut self) -> Result<Vec<SyncRow>, CaptureError> {
        let entries: Vec<IndexEntry> = self
            .index
            .iter()
            .filter(|e| e.kind == ChunkKind::Sync)
            .copied()
            .collect();
        let mut out = Vec::new();
        for e in entries {
            if let Some((_, payload)) =
                Self::read_chunk_at(&mut self.file, e.file_offset, &self.header)?
            {
                out.extend(decode_sync(&payload)?);
            }
        }
        Ok(out)
    }

    /// Aggregate `span` into `buckets` min/max/mean buckets.
    ///
    /// This is the API a zoomable chart is built on, which is why it exists in phase 1 with
    /// no GUI to use it yet: it is testable now against a brute-force scan, and it decides
    /// whether the format's seek design actually works.
    pub fn downsample(
        &mut self,
        span: TimeSpan,
        buckets: usize,
    ) -> Result<Vec<Bucket>, CaptureError> {
        if buckets == 0 {
            return Ok(Vec::new());
        }
        let span = match span.intersect(&self.span) {
            Some(s) => s,
            None if span == TimeSpan::ALL => self.span,
            None => return Ok(Vec::new()),
        };
        if span.is_empty() {
            return Ok(Vec::new());
        }

        // Answer from the stored pyramid when the request is coarse enough, so a
        // whole-capture overview never touches a sample chunk.
        let target = span.duration_ns() / buckets as u64;
        if let Some(level) = self.summary.as_ref().and_then(|p| p.level_for(target)) {
            let coarse: Vec<(u64, i32)> = level
                .range(span.start_ns, span.end_ns)
                .iter()
                .filter(|b| !b.is_empty())
                .flat_map(|b| [(b.t_ns, b.min_ua), (b.t_ns, b.max_ua)])
                .collect();
            if !coarse.is_empty() {
                return Ok(bucketize(
                    coarse.into_iter(),
                    span.start_ns,
                    span.end_ns,
                    buckets,
                ));
            }
        }

        let samples = self.samples_in(span)?;
        Ok(bucketize(
            samples.iter().map(|s| (s.t_ns, s.current_ua)),
            span.start_ns,
            span.end_ns,
            buckets,
        ))
    }

    /// Number of chunks of each kind, for `wattson info`.
    pub fn chunk_counts(&self) -> Vec<(ChunkKind, usize)> {
        let mut counts: std::collections::BTreeMap<u16, (ChunkKind, usize)> =
            std::collections::BTreeMap::new();
        for e in &self.index {
            let slot = counts.entry(e.kind.as_u16()).or_insert((e.kind, 0));
            slot.1 += 1;
        }
        counts.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::writer::{CaptureWriter, WriterOptions};
    use crate::capture::{Compression, summary::bucketize};

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        expected: Vec<ReadSample>,
    }

    /// A capture with a flat baseline, one tall spike, and a matched event pair.
    fn make_capture(n: usize, opts: WriterOptions) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pprof");
        let header = CaptureHeader {
            device_timer_hz: 1_000_000,
            sample_rate_hz: 50_000,
            ..Default::default()
        };
        let mut w = CaptureWriter::create(&path, header, opts).unwrap();

        let mut expected = Vec::with_capacity(n);
        for i in 0..n as u64 {
            let t = i * 20_000;
            let cur = if i == (n as u64) / 2 {
                91_200
            } else {
                3_000 + (i % 50) as i32
            };
            let volt = 3_300_000 - (i % 7) as u32;
            w.push_sample(t, cur, Some(volt)).unwrap();
            expected.push(ReadSample {
                t_ns: t,
                current_ua: cur,
                voltage_uv: volt,
            });
        }
        w.push_event(1_000_000, 0x0101, None).unwrap();
        w.push_event(2_000_000, 0x0102, Some(64)).unwrap();
        w.push_gpio(1_500_000, 0b0011).unwrap();
        w.set_metadata(crate::metadata::default_simulator_metadata());
        w.finish().unwrap();

        Fixture {
            _dir: dir,
            path,
            expected,
        }
    }

    #[test]
    fn every_sample_survives_a_write_read_round_trip() {
        let f = make_capture(20_000, WriterOptions::default());
        let mut r = CaptureReader::open(&f.path).unwrap();
        assert!(r.integrity().is_clean(), "{:?}", r.integrity());
        assert_eq!(r.sample_count(), 20_000);
        let got = r.samples().unwrap();
        assert_eq!(got, f.expected);
    }

    #[test]
    fn events_gpio_and_metadata_survive_the_round_trip() {
        let f = make_capture(1_000, WriterOptions::default());
        let mut r = CaptureReader::open(&f.path).unwrap();

        let events = r.events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            StoredEvent {
                t_ns: 1_000_000,
                id: 0x0101,
                value: None
            }
        );
        assert_eq!(
            events[1],
            StoredEvent {
                t_ns: 2_000_000,
                id: 0x0102,
                value: Some(64)
            }
        );

        assert_eq!(
            r.gpio().unwrap(),
            vec![StoredGpio {
                t_ns: 1_500_000,
                state: 0b0011
            }]
        );
        assert_eq!(r.metadata().event("BLE_TX").unwrap().start_id, 0x0101);
    }

    #[test]
    fn a_span_query_returns_only_what_falls_inside_it() {
        let f = make_capture(
            10_000,
            WriterOptions {
                target_chunk_bytes: 4096,
                ..Default::default()
            },
        );
        let mut r = CaptureReader::open(&f.path).unwrap();
        let span = TimeSpan::new(1_000_000, 2_000_000);
        let got = r.samples_in(span).unwrap();
        assert!(!got.is_empty());
        assert!(got.iter().all(|s| span.contains(s.t_ns)));
        let want = f.expected.iter().filter(|s| span.contains(s.t_ns)).count();
        assert_eq!(got.len(), want);
    }

    #[test]
    fn every_compression_mode_round_trips() {
        for c in [Compression::None, Compression::Zstd] {
            let f = make_capture(
                5_000,
                WriterOptions {
                    compression: c,
                    ..Default::default()
                },
            );
            let mut r = CaptureReader::open(&f.path).unwrap();
            assert_eq!(r.samples().unwrap(), f.expected, "{c:?} did not round-trip");
        }
    }

    /// The recovery path: a capture killed mid-write must lose at most the chunk in flight.
    #[test]
    fn a_capture_killed_mid_write_is_recovered_by_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("killed.pprof");
        let mut w = CaptureWriter::create(
            &path,
            CaptureHeader::default(),
            WriterOptions {
                target_chunk_bytes: 4096,
                ..Default::default()
            },
        )
        .unwrap();
        for i in 0..50_000u64 {
            w.push_sample(i * 20_000, 3_000, Some(3_300_000)).unwrap();
        }
        w.flush().unwrap();
        // Simulate a kill: drop the writer without finishing, so no index and no footer.
        drop(w);

        let mut r = CaptureReader::open(&path).unwrap();
        assert!(!r.integrity().finalized);
        assert!(
            r.integrity().recovered,
            "an unfinalized capture must be recovered, not refused"
        );
        let got = r.samples().unwrap();
        assert!(
            got.len() > 45_000,
            "recovery should keep nearly everything, kept {} of 50000",
            got.len()
        );
        for (i, s) in got.iter().enumerate() {
            assert_eq!(
                s.t_ns,
                i as u64 * 20_000,
                "recovered samples must stay in order"
            );
        }
    }

    /// Truncating anywhere must yield a clean error or a valid prefix — never a panic, never
    /// fabricated data.
    #[test]
    fn truncation_at_any_offset_never_panics_and_never_lies() {
        let f = make_capture(
            3_000,
            WriterOptions {
                target_chunk_bytes: 2048,
                ..Default::default()
            },
        );
        let full = std::fs::read(&f.path).unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Every offset would be slow; step through the file densely enough to hit every
        // structural boundary class.
        let step = (full.len() / 200).max(1);
        for cut in (0..full.len()).step_by(step) {
            let path = dir.path().join("cut.pprof");
            std::fs::write(&path, &full[..cut]).unwrap();

            match CaptureReader::open(&path) {
                Err(_) => {} // a clean refusal is fine
                Ok(mut r) => {
                    let samples = r.samples().unwrap_or_default();
                    // Whatever survived must be a genuine prefix of what was written.
                    for (i, s) in samples.iter().enumerate() {
                        assert_eq!(
                            *s, f.expected[i],
                            "truncating at {cut} produced a sample that was never written"
                        );
                    }
                    let _ = r.events();
                    let _ = r.downsample(TimeSpan::ALL, 64);
                }
            }
        }
    }

    #[test]
    fn a_damaged_footer_falls_back_to_scanning() {
        let f = make_capture(5_000, WriterOptions::default());
        let mut bytes = std::fs::read(&f.path).unwrap();
        let len = bytes.len();
        bytes[len - 8] ^= 0xFF;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("badfooter.pprof");
        std::fs::write(&path, &bytes).unwrap();

        let mut r = CaptureReader::open(&path).unwrap();
        assert!(r.integrity().recovered);
        assert_eq!(
            r.samples().unwrap(),
            f.expected,
            "scanning must recover every sample"
        );
    }

    /// Forward compatibility, tested rather than hoped for.
    #[test]
    fn a_chunk_kind_this_build_does_not_know_is_skipped_not_fatal() {
        let f = make_capture(
            2_000,
            WriterOptions {
                compression: Compression::None,
                ..Default::default()
            },
        );
        let mut bytes = std::fs::read(&f.path).unwrap();

        // Splice a well-formed chunk of an unknown kind in just after the file header.
        let payload = b"a future version put something here";
        let ch = ChunkHeader {
            kind: ChunkKind::Unknown(0x0777),
            flags: 0,
            uncompressed_len: payload.len() as u32,
            stored_len: payload.len() as u32,
            first_time_ns: NO_TIME,
            last_time_ns: NO_TIME,
            payload_crc32: checksum(payload),
        };
        let mut spliced = bytes[..HEADER_LEN].to_vec();
        spliced.extend_from_slice(&ch.encode());
        spliced.extend_from_slice(payload);
        spliced.extend_from_slice(&bytes[HEADER_LEN..]);
        // The stored index now has stale offsets, so clear FINALIZED to force a scan — which
        // is exactly what a v1.0 reader does with a file it cannot fully account for.
        let mut header = CaptureHeader::decode(&spliced, &f.path).unwrap();
        header.flags &= !HeaderFlags::FINALIZED;
        spliced[..HEADER_LEN].copy_from_slice(&header.encode());
        bytes = spliced;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.pprof");
        std::fs::write(&path, &bytes).unwrap();

        let mut r = CaptureReader::open(&path).unwrap();
        assert_eq!(
            r.integrity().unknown_chunks,
            1,
            "the unknown chunk should be counted"
        );
        assert_eq!(
            r.samples().unwrap(),
            f.expected,
            "an unknown chunk must be stepped over, not fatal"
        );
    }

    #[test]
    fn downsample_matches_a_brute_force_scan() {
        let f = make_capture(20_000, WriterOptions::default());
        let mut r = CaptureReader::open(&f.path).unwrap();
        let span = r.span();

        for n in [1usize, 13, 256, 1024] {
            let got = r.downsample(span, n).unwrap();
            let want = bucketize(
                f.expected.iter().map(|s| (s.t_ns, s.current_ua)),
                span.start_ns,
                span.end_ns,
                n,
            );
            assert_eq!(got.len(), want.len(), "{n} buckets");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(g.t_ns, w.t_ns);
                assert_eq!(g.min_ua, w.min_ua, "min differs at {n} buckets");
                assert_eq!(g.max_ua, w.max_ua, "max differs at {n} buckets");
            }
        }
    }

    /// The spike is the thing a user is looking for; downsampling must never average it away.
    #[test]
    fn downsampling_preserves_the_peak_at_every_zoom_level() {
        let f = make_capture(20_000, WriterOptions::default());
        let mut r = CaptureReader::open(&f.path).unwrap();
        let span = r.span();
        for n in [2usize, 8, 64, 512] {
            let buckets = r.downsample(span, n).unwrap();
            let peak = buckets.iter().map(|b| b.max_ua).max().unwrap();
            assert_eq!(peak, 91_200, "the 91.2 mA spike vanished at {n} buckets");
        }
    }

    #[test]
    fn an_empty_capture_reads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pprof");
        let w = CaptureWriter::create(&path, CaptureHeader::default(), WriterOptions::default())
            .unwrap();
        w.finish().unwrap();

        let mut r = CaptureReader::open(&path).unwrap();
        assert_eq!(r.sample_count(), 0);
        assert!(r.samples().unwrap().is_empty());
        assert!(r.span().is_empty());
        assert_eq!(r.effective_rate_hz(), 0.0);
        assert!(r.downsample(TimeSpan::ALL, 10).unwrap().is_empty());
    }

    #[test]
    fn a_file_that_is_not_a_capture_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.pprof");
        std::fs::write(&path, b"this is not a capture file, it is a poem about one").unwrap();
        assert!(matches!(
            CaptureReader::open(&path),
            Err(CaptureError::NotACapture { .. })
        ));
    }

    #[test]
    fn the_effective_rate_is_derived_from_the_data() {
        let f = make_capture(10_000, WriterOptions::default());
        let r = CaptureReader::open(&f.path).unwrap();
        assert!(
            (r.effective_rate_hz() - 50_000.0).abs() < 1.0,
            "got {}",
            r.effective_rate_hz()
        );
    }
}
