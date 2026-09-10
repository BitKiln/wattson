//! The zoom pyramid.
//!
//! A whole-capture overview must render without decompressing a single sample chunk, or the
//! UI stalls for seconds every time someone zooms out. So the writer stores min/max/mean per
//! fixed time bucket at two levels, and the reader answers coarse queries straight from them.
//!
//! # Why 10 ms and 1 s, and not 1 ms
//!
//! An hour of 1 ms buckets is 3.6 M buckets at 16 bytes, or 57 MB — larger than the
//! compressed samples it summarises, which defeats the purpose. At 10 ms and 1 s it is
//! 360 K + 3.6 K buckets, about 5.8 MB per capture-hour, and a full-capture overview renders
//! from roughly 3600 buckets. Intermediate zoom levels decompress the handful of chunks that
//! intersect the visible span.
//!
//! [`crate::capture::CaptureReader::downsample`] is built in phase 1 even though there is no
//! GUI yet, because it is the API the GUI is built on and it is trivially testable now
//! against a brute-force scan.

use crate::error::CaptureError;

/// One aggregated time bucket.
///
/// `min` and `max` are kept, not just the mean, because a peak that lands between two
/// rendered pixels is exactly the peak someone is looking for. Averaging it away turns a
/// 90 mA spike into a flat line.
#[derive(Copy, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Bucket {
    /// Capture-relative nanoseconds at the start of the bucket.
    pub t_ns: u64,
    pub min_ua: i32,
    pub max_ua: i32,
    pub mean_ua: f64,
    pub count: u32,
}

impl Bucket {
    /// An empty bucket at `t_ns`, which renders as a gap rather than as zero current.
    pub const fn empty(t_ns: u64) -> Bucket {
        Bucket {
            t_ns,
            min_ua: 0,
            max_ua: 0,
            mean_ua: 0.0,
            count: 0,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// A pyramid level: buckets of a fixed width.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryLevel {
    pub bucket_ns: u64,
    pub buckets: Vec<Bucket>,
}

impl SummaryLevel {
    pub fn new(bucket_ns: u64) -> SummaryLevel {
        SummaryLevel {
            bucket_ns: bucket_ns.max(1),
            buckets: Vec::new(),
        }
    }

    /// Fold one sample in, extending the level as needed.
    pub fn push(&mut self, t_ns: u64, current_ua: i32) {
        let idx = (t_ns / self.bucket_ns) as usize;
        if idx >= self.buckets.len() {
            // Intervening buckets stay empty, so a gap in the data reads as a gap on screen.
            let start = self.buckets.len();
            self.buckets.reserve(idx + 1 - start);
            for i in start..=idx {
                self.buckets.push(Bucket::empty(i as u64 * self.bucket_ns));
            }
        }
        let b = &mut self.buckets[idx];
        if b.count == 0 {
            b.min_ua = current_ua;
            b.max_ua = current_ua;
            b.mean_ua = current_ua as f64;
            b.count = 1;
        } else {
            b.min_ua = b.min_ua.min(current_ua);
            b.max_ua = b.max_ua.max(current_ua);
            // Running mean, so a long bucket cannot overflow a sum.
            b.count += 1;
            b.mean_ua += (current_ua as f64 - b.mean_ua) / b.count as f64;
        }
    }

    /// Buckets overlapping `[start_ns, end_ns)`.
    pub fn range(&self, start_ns: u64, end_ns: u64) -> &[Bucket] {
        if self.buckets.is_empty() || end_ns <= start_ns {
            return &[];
        }
        let first = (start_ns / self.bucket_ns) as usize;
        let last = ((end_ns - 1) / self.bucket_ns) as usize;
        let first = first.min(self.buckets.len());
        let last = (last + 1).min(self.buckets.len());
        if first >= last {
            &[]
        } else {
            &self.buckets[first..last]
        }
    }
}

/// Widths of the two stored levels.
pub const FINE_BUCKET_NS: u64 = 10_000_000; // 10 ms
pub const COARSE_BUCKET_NS: u64 = 1_000_000_000; // 1 s

/// The stored summary: two levels, fine and coarse.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryPyramid {
    pub fine: SummaryLevel,
    pub coarse: SummaryLevel,
}

impl Default for SummaryPyramid {
    fn default() -> Self {
        SummaryPyramid {
            fine: SummaryLevel::new(FINE_BUCKET_NS),
            coarse: SummaryLevel::new(COARSE_BUCKET_NS),
        }
    }
}

impl SummaryPyramid {
    pub fn push(&mut self, t_ns: u64, current_ua: i32) {
        self.fine.push(t_ns, current_ua);
        self.coarse.push(t_ns, current_ua);
    }

    pub fn is_empty(&self) -> bool {
        self.fine.buckets.is_empty()
    }

    /// The coarsest level whose buckets are still finer than `target_bucket_ns`, or `None`
    /// when the request is finer than the pyramid can answer and the sample chunks must be
    /// read instead.
    pub fn level_for(&self, target_bucket_ns: u64) -> Option<&SummaryLevel> {
        if target_bucket_ns >= COARSE_BUCKET_NS && !self.coarse.buckets.is_empty() {
            Some(&self.coarse)
        } else if target_bucket_ns >= FINE_BUCKET_NS && !self.fine.buckets.is_empty() {
            Some(&self.fine)
        } else {
            None
        }
    }

    /// Encode for the SUMMARY chunk.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&2u32.to_le_bytes()); // level count
        out.extend_from_slice(&0u32.to_le_bytes()); // flags, reserved
        for level in [&self.fine, &self.coarse] {
            out.extend_from_slice(&level.bucket_ns.to_le_bytes());
            out.extend_from_slice(&(level.buckets.len() as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            for b in &level.buckets {
                out.extend_from_slice(&b.t_ns.to_le_bytes());
                out.extend_from_slice(&b.min_ua.to_le_bytes());
                out.extend_from_slice(&b.max_ua.to_le_bytes());
                out.extend_from_slice(&b.mean_ua.to_le_bytes());
                out.extend_from_slice(&b.count.to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<SummaryPyramid, CaptureError> {
        const BUCKET_LEN: usize = 32;
        let bad = |what: &str| CaptureError::Metadata(format!("summary chunk: {what}"));
        if b.len() < 8 {
            return Err(bad("shorter than its header"));
        }
        let levels = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let mut off = 8;
        let mut parsed = Vec::with_capacity(levels);
        for _ in 0..levels {
            if b.len() < off + 16 {
                return Err(bad("truncated level header"));
            }
            let bucket_ns = u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"));
            let count =
                u32::from_le_bytes([b[off + 8], b[off + 9], b[off + 10], b[off + 11]]) as usize;
            off += 16;
            if b.len() < off + count * BUCKET_LEN {
                return Err(bad("truncated bucket array"));
            }
            let buckets = b[off..off + count * BUCKET_LEN]
                .chunks_exact(BUCKET_LEN)
                .map(|c| Bucket {
                    t_ns: u64::from_le_bytes(c[0..8].try_into().expect("8 bytes")),
                    min_ua: i32::from_le_bytes([c[8], c[9], c[10], c[11]]),
                    max_ua: i32::from_le_bytes([c[12], c[13], c[14], c[15]]),
                    mean_ua: f64::from_le_bytes(c[16..24].try_into().expect("8 bytes")),
                    count: u32::from_le_bytes([c[24], c[25], c[26], c[27]]),
                })
                .collect();
            off += count * BUCKET_LEN;
            parsed.push(SummaryLevel {
                bucket_ns: bucket_ns.max(1),
                buckets,
            });
        }

        let mut out = SummaryPyramid::default();
        if let Some(l) = parsed.first() {
            out.fine = l.clone();
        }
        if let Some(l) = parsed.get(1) {
            out.coarse = l.clone();
        }
        Ok(out)
    }
}

/// Aggregate `(t_ns, current_ua)` pairs into exactly `buckets` buckets over `[start, end)`.
///
/// The reference implementation, used directly for small spans and as the correctness oracle
/// for the pyramid path.
pub fn bucketize(
    samples: impl Iterator<Item = (u64, i32)>,
    start_ns: u64,
    end_ns: u64,
    buckets: usize,
) -> Vec<Bucket> {
    if buckets == 0 || end_ns <= start_ns {
        return Vec::new();
    }
    let span = end_ns - start_ns;
    let width = span.div_ceil(buckets as u64).max(1);
    let mut out: Vec<Bucket> = (0..buckets)
        .map(|i| Bucket::empty(start_ns + i as u64 * width))
        .collect();

    for (t, cur) in samples {
        if t < start_ns || t >= end_ns {
            continue;
        }
        let idx = (((t - start_ns) / width) as usize).min(buckets - 1);
        let b = &mut out[idx];
        if b.count == 0 {
            b.min_ua = cur;
            b.max_ua = cur;
            b.mean_ua = cur as f64;
            b.count = 1;
        } else {
            b.min_ua = b.min_ua.min(cur);
            b.max_ua = b.max_ua.max(cur);
            b.count += 1;
            b.mean_ua += (cur as f64 - b.mean_ua) / b.count as f64;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn a_level_tracks_min_max_and_mean() {
        let mut l = SummaryLevel::new(1_000);
        for (t, c) in [(0u64, 10i32), (100, 30), (900, 20), (1_500, 100)] {
            l.push(t, c);
        }
        assert_eq!(l.buckets.len(), 2);
        assert_eq!(l.buckets[0].min_ua, 10);
        assert_eq!(l.buckets[0].max_ua, 30);
        assert_relative_eq!(l.buckets[0].mean_ua, 20.0);
        assert_eq!(l.buckets[0].count, 3);
        assert_eq!(l.buckets[1].count, 1);
    }

    /// A gap in the data must read as a gap, not as zero current. Rendering missing data as
    /// 0 mA is how a tool convinces someone their device sleeps beautifully.
    #[test]
    fn skipped_buckets_stay_empty_rather_than_reading_as_zero() {
        let mut l = SummaryLevel::new(100);
        l.push(0, 5_000);
        l.push(1_000, 6_000);
        assert_eq!(l.buckets.len(), 11);
        for b in &l.buckets[1..10] {
            assert!(
                b.is_empty(),
                "an unfilled bucket must be empty, not zero-valued"
            );
        }
    }

    #[test]
    fn the_running_mean_survives_a_huge_bucket() {
        let mut l = SummaryLevel::new(u64::MAX / 2);
        for _ in 0..2_000_000 {
            l.push(0, 1_000_000);
        }
        assert_relative_eq!(l.buckets[0].mean_ua, 1_000_000.0, max_relative = 1e-9);
    }

    #[test]
    fn range_selects_overlapping_buckets() {
        let mut l = SummaryLevel::new(100);
        for i in 0..10u64 {
            l.push(i * 100, i as i32);
        }
        assert_eq!(l.range(0, 1_000).len(), 10);
        assert_eq!(l.range(250, 450).len(), 3);
        assert_eq!(l.range(100, 100).len(), 0);
        assert_eq!(l.range(5_000, 6_000).len(), 0);
    }

    #[test]
    fn the_pyramid_picks_a_level_by_requested_resolution() {
        let mut p = SummaryPyramid::default();
        for i in 0..300u64 {
            p.push(i * 10_000_000, 1_000 + i as i32);
        }
        assert_eq!(
            p.level_for(2_000_000_000).unwrap().bucket_ns,
            COARSE_BUCKET_NS
        );
        assert_eq!(p.level_for(50_000_000).unwrap().bucket_ns, FINE_BUCKET_NS);
        // Finer than the pyramid stores: the caller must read sample chunks instead.
        assert!(p.level_for(1_000).is_none());
    }

    #[test]
    fn the_pyramid_round_trips_through_its_chunk() {
        let mut p = SummaryPyramid::default();
        for i in 0..5_000u64 {
            p.push(i * 200_000, (i % 90_000) as i32);
        }
        let back = SummaryPyramid::decode(&p.encode()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn a_truncated_summary_chunk_errors() {
        let mut p = SummaryPyramid::default();
        p.push(0, 1);
        let bytes = p.encode();
        assert!(SummaryPyramid::decode(&bytes[..bytes.len() - 4]).is_err());
        assert!(SummaryPyramid::decode(&[]).is_err());
    }

    #[test]
    fn bucketize_returns_exactly_the_requested_count() {
        let samples: Vec<(u64, i32)> = (0..1000u64).map(|i| (i * 1_000, i as i32)).collect();
        for n in [1usize, 7, 100, 999] {
            let got = bucketize(samples.iter().copied(), 0, 1_000_000, n);
            assert_eq!(got.len(), n);
        }
    }

    /// The peak must survive downsampling, whatever the bucket count.
    #[test]
    fn downsampling_preserves_extremes() {
        let mut samples: Vec<(u64, i32)> = (0..10_000u64).map(|i| (i * 100, 3_000)).collect();
        samples[4_321] = (4_321 * 100, 91_200);
        samples[8_765] = (8_765 * 100, -500);

        for n in [4usize, 37, 1000] {
            let buckets = bucketize(samples.iter().copied(), 0, 1_000_000, n);
            let max = buckets
                .iter()
                .filter(|b| !b.is_empty())
                .map(|b| b.max_ua)
                .max()
                .unwrap();
            let min = buckets
                .iter()
                .filter(|b| !b.is_empty())
                .map(|b| b.min_ua)
                .min()
                .unwrap();
            assert_eq!(max, 91_200, "the peak vanished at {n} buckets");
            assert_eq!(min, -500, "the trough vanished at {n} buckets");
        }
    }

    #[test]
    fn samples_outside_the_span_are_ignored() {
        let samples = [(0u64, 1i32), (500, 2), (1_500, 999)];
        let got = bucketize(samples.iter().copied(), 0, 1_000, 2);
        assert_eq!(got.iter().map(|b| b.count).sum::<u32>(), 2);
        assert!(got.iter().all(|b| b.max_ua != 999));
    }

    #[test]
    fn degenerate_requests_return_nothing_rather_than_panicking() {
        let samples = [(0u64, 1i32)];
        assert!(bucketize(samples.iter().copied(), 0, 1_000, 0).is_empty());
        assert!(bucketize(samples.iter().copied(), 1_000, 1_000, 4).is_empty());
        assert!(bucketize(samples.iter().copied(), 1_000, 0, 4).is_empty());
    }
}
