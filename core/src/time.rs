//! Time bases, tick unwrapping, and host/device clock correlation.
//!
//! # The rule
//!
//! **All energy and duration arithmetic uses device time, exclusively.**
//!
//! Every power sample, firmware event, and GPIO edge is stamped by the *same* hardware timer
//! on the profiler MCU. Intra-capture integration is therefore exact regardless of how that
//! timer compares to the host clock. Device crystal and host clock differ by 20-50 ppm, which
//! is up to 30 ms of skew over ten minutes — enough to visibly corrupt an energy figure if it
//! ever leaks into a duration.
//!
//! Host time exists only to correlate a capture with something outside it: a wall-clock log,
//! an external instrument, a CI job id. It must never appear in a `Δt`.
//!
//! This is the most likely place in the whole project for a subtle, invisible correctness
//! bug, because getting it wrong produces plausible numbers rather than an error.
//!
//! # Three clocks, one canonical unit
//!
//! | Domain | Type | Source |
//! |---|---|---|
//! | Device ticks | [`DeviceTicks`] (`u32`) | the one hardware timer, wraps every 71.6 min at 1 MHz |
//! | Unwrapped device time | [`DeviceTime`] (`u64`) | after [`TickUnwrapper`] |
//! | Capture time | [`CaptureTime`] (`u64` ns) | canonical; the only unit above the session layer |

use std::time::Instant;

use crate::error::TimeError;

/// A raw device timer reading, exactly as it appears on the wire. Free-running and wrapping.
///
/// This type exists to make it awkward to use a wire timestamp by accident. Unwrap it once,
/// in the decode path, and nothing above the session layer ever sees a `u32` timestamp again.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DeviceTicks(pub u32);

/// Device ticks with the wrap epoch resolved: monotonic for the life of a capture.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DeviceTime(pub u64);

/// Nanoseconds since the start of a capture. The canonical time unit above the decoder.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct CaptureTime(pub u64);

impl CaptureTime {
    pub const ZERO: CaptureTime = CaptureTime(0);

    #[inline]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1e9
    }
}

/// A half-open time range `[start_ns, end_ns)` in capture time.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TimeSpan {
    pub start_ns: u64,
    pub end_ns: u64,
}

impl TimeSpan {
    /// The empty span at the origin.
    pub const EMPTY: TimeSpan = TimeSpan {
        start_ns: 0,
        end_ns: 0,
    };

    /// A span covering everything.
    pub const ALL: TimeSpan = TimeSpan {
        start_ns: 0,
        end_ns: u64::MAX,
    };

    #[inline]
    pub const fn new(start_ns: u64, end_ns: u64) -> TimeSpan {
        TimeSpan { start_ns, end_ns }
    }

    #[inline]
    pub const fn duration_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }

    #[inline]
    pub fn duration_s(&self) -> f64 {
        self.duration_ns() as f64 / 1e9
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.end_ns <= self.start_ns
    }

    #[inline]
    pub const fn contains(&self, t_ns: u64) -> bool {
        t_ns >= self.start_ns && t_ns < self.end_ns
    }

    #[inline]
    pub const fn overlaps(&self, other: &TimeSpan) -> bool {
        self.start_ns < other.end_ns && other.start_ns < self.end_ns
    }

    /// The overlap of two spans, or `None` if they are disjoint.
    pub fn intersect(&self, other: &TimeSpan) -> Option<TimeSpan> {
        let start = self.start_ns.max(other.start_ns);
        let end = self.end_ns.min(other.end_ns);
        (start < end).then_some(TimeSpan {
            start_ns: start,
            end_ns: end,
        })
    }

    /// The smallest span containing both.
    pub fn union(&self, other: &TimeSpan) -> TimeSpan {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        TimeSpan {
            start_ns: self.start_ns.min(other.start_ns),
            end_ns: self.end_ns.max(other.end_ns),
        }
    }
}

/// Resolves the wrap epoch of a free-running `u32` device timer.
///
/// At 1 MHz the counter wraps every **4294.97 s, about 71.6 minutes**. Inference alone
/// (a backward jump means a wrap) is correct only while no gap exceeds 2^31 ticks — about
/// 35.8 minutes. A host that stalls longer than that would silently emit a 71-minute time
/// jump, which is worse than an error because it looks like data.
///
/// So the device also reports an explicit `wrap_count` from its timer-overflow ISR in every
/// `SYNC` frame, and [`TickUnwrapper::check_wrap_count`] verifies the inference against it.
#[derive(Copy, Clone, Debug, Default)]
pub struct TickUnwrapper {
    /// High half: the number of wraps observed so far.
    epoch: u64,
    last: Option<u32>,
}

impl TickUnwrapper {
    /// Half the `u32` range. A backward step larger than this is read as a wrap; a forward
    /// step larger than this is read as an implausible gap.
    const HALF: u32 = 1 << 31;

    pub const fn new() -> Self {
        TickUnwrapper {
            epoch: 0,
            last: None,
        }
    }

    /// Start unwrapping from a known tick value without treating it as a wrap.
    pub fn seed(&mut self, raw: u32) {
        self.last = Some(raw);
    }

    /// Unwrap one raw tick reading.
    ///
    /// Returns [`TimeError::ImplausibleGap`] rather than silently absorbing a forward jump
    /// larger than half the counter range: such a jump is either a corrupt timestamp or a
    /// stall long enough that the epoch can no longer be inferred, and both must be visible.
    pub fn unwrap_ticks(&mut self, raw: u32) -> Result<DeviceTime, TimeError> {
        let Some(last) = self.last else {
            self.last = Some(raw);
            return Ok(DeviceTime(self.epoch << 32 | raw as u64));
        };

        if raw >= last {
            if raw - last > Self::HALF {
                return Err(TimeError::ImplausibleGap {
                    from: last,
                    to: raw,
                });
            }
        } else if last - raw > Self::HALF {
            // Backward by more than half the range: a wrap.
            self.epoch += 1;
        } else {
            return Err(TimeError::BackwardsTime {
                from: last,
                to: raw,
            });
        }

        self.last = Some(raw);
        Ok(DeviceTime((self.epoch << 32) | raw as u64))
    }

    /// Resolve a raw tick value against the current epoch **without advancing it**.
    ///
    /// Samples, events, GPIO edges and SYNC echoes all share one device timer, but they
    /// arrive interleaved and are not monotonic *relative to each other*: an event stamped
    /// before a sample block routinely arrives after it. Feeding all four streams into
    /// [`TickUnwrapper::unwrap_ticks`] would therefore read every such ordering as time
    /// going backwards.
    ///
    /// So the sample stream — the only one that is genuinely monotonic and dense — advances
    /// the epoch, and everything else is resolved against it here, picking whichever epoch
    /// puts the value closest to the last sample seen.
    pub fn resolve(&self, raw: u32) -> DeviceTime {
        let Some(last) = self.last else {
            return DeviceTime((self.epoch << 32) | raw as u64);
        };
        let epoch = if raw > last && raw - last > Self::HALF {
            // Far ahead of the last sample: this actually belongs to the previous epoch.
            self.epoch.saturating_sub(1)
        } else if last > raw && last - raw > Self::HALF {
            // Far behind: the counter has wrapped since.
            self.epoch + 1
        } else {
            self.epoch
        };
        DeviceTime((epoch << 32) | raw as u64)
    }

    /// Cross-check the inferred epoch against the device's own overflow counter.
    ///
    /// `wrap_count` is a `u16`, so only its low 16 bits are comparable; that still covers
    /// 65536 wraps, or roughly nine years at 1 MHz.
    pub fn check_wrap_count(&self, device_wrap_count: u16) -> Result<(), TimeError> {
        let inferred = (self.epoch & 0xFFFF) as u16;
        if inferred == device_wrap_count {
            Ok(())
        } else {
            Err(TimeError::WrapMismatch {
                inferred,
                reported: device_wrap_count,
            })
        }
    }

    /// Adopt the device's wrap count as authoritative, after a mismatch or on reconnect.
    pub fn resync(&mut self, raw: u32, device_wrap_count: u16) {
        self.epoch = device_wrap_count as u64;
        self.last = Some(raw);
    }

    /// Number of wraps observed so far.
    #[inline]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// A least-squares fit of host nanoseconds against device ticks: `host_ns ≈ a * ticks + b`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ClockFit {
    /// Host nanoseconds per device tick.
    pub a: f64,
    /// Offset in host nanoseconds.
    pub b: f64,
    /// How many SYNC triples the fit used.
    pub samples: usize,
    /// Residual standard deviation, in nanoseconds. Large values mean a noisy USB path.
    pub residual_ns: f64,
}

impl ClockFit {
    /// Implied clock error against the nominal timer rate, in parts per million. A device
    /// crystal is typically within +/-50 ppm; wildly larger values mean the fit is wrong or
    /// `timer_hz` is misreported.
    pub fn drift_ppm(&self, timer_hz: u32) -> f64 {
        let nominal = 1e9 / timer_hz as f64;
        (self.a - nominal) / nominal * 1e6
    }
}

/// One host/device clock correlation triple, from a SYNC round trip.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SyncSample {
    /// Host monotonic time when the SYNC was sent, in nanoseconds since capture start.
    pub host_send_ns: u64,
    /// Host monotonic time when the echo arrived.
    pub host_recv_ns: u64,
    /// The device's unwrapped tick count at the moment it stamped the echo.
    pub device_ticks: u64,
}

impl SyncSample {
    /// Round-trip time in nanoseconds.
    #[inline]
    pub const fn rtt_ns(&self) -> u64 {
        self.host_recv_ns.saturating_sub(self.host_send_ns)
    }
}

/// Fit host time against device ticks over a set of SYNC triples.
///
/// Only the fastest round trips are used. USB scheduling adds strictly **one-sided** latency:
/// a round trip can be delayed, never hurried, so the minimum-RTT samples are the accurate
/// ones. This is the standard NTP/PTP filter, and it is the difference between a fit good to
/// microseconds and one good to whatever the worst USB frame delay happened to be.
///
/// Returns `None` if there are too few samples to fit a line.
pub fn fit_clocks(samples: &[SyncSample]) -> Option<ClockFit> {
    if samples.len() < 3 {
        return None;
    }

    let mut rtts: Vec<u64> = samples.iter().map(SyncSample::rtt_ns).collect();
    rtts.sort_unstable();
    // 20th percentile, but never fewer than 3 points.
    let cutoff_idx = (rtts.len() / 5).max(2).min(rtts.len() - 1);
    let cutoff = rtts[cutoff_idx];

    let kept: Vec<&SyncSample> = samples.iter().filter(|s| s.rtt_ns() <= cutoff).collect();
    if kept.len() < 2 {
        return None;
    }

    // Estimate the device timestamp's host-time position as the midpoint of the round trip.
    let xs: Vec<f64> = kept.iter().map(|s| s.device_ticks as f64).collect();
    let ys: Vec<f64> = kept
        .iter()
        .map(|s| (s.host_send_ns as f64 + s.host_recv_ns as f64) / 2.0)
        .collect();

    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (x, y) in xs.iter().zip(&ys) {
        sxx += (x - mean_x) * (x - mean_x);
        sxy += (x - mean_x) * (y - mean_y);
    }
    if sxx == 0.0 {
        return None;
    }
    let a = sxy / sxx;
    let b = mean_y - a * mean_x;

    let mut ss = 0.0;
    for (x, y) in xs.iter().zip(&ys) {
        let r = y - (a * x + b);
        ss += r * r;
    }
    let residual_ns = (ss / n).sqrt();

    Some(ClockFit {
        a,
        b,
        samples: kept.len(),
        residual_ns,
    })
}

/// Converts device time to capture time, and optionally to host time.
#[derive(Clone, Debug)]
pub struct TimeBase {
    timer_hz: u32,
    /// Unwrapped device ticks at capture start; the zero of capture time.
    origin_ticks: u64,
    /// Host monotonic reference for capture start, if this is a live capture.
    host_origin: Option<Instant>,
    fit: Option<ClockFit>,
}

impl TimeBase {
    /// A time base for a live capture starting now.
    pub fn start_live(timer_hz: u32, origin_ticks: u64) -> TimeBase {
        TimeBase {
            timer_hz: timer_hz.max(1),
            origin_ticks,
            host_origin: Some(Instant::now()),
            fit: None,
        }
    }

    /// A time base for reading a stored capture, where no host clock is involved.
    pub fn from_stored(timer_hz: u32, origin_ticks: u64) -> TimeBase {
        TimeBase {
            timer_hz: timer_hz.max(1),
            origin_ticks,
            host_origin: None,
            fit: None,
        }
    }

    #[inline]
    pub const fn timer_hz(&self) -> u32 {
        self.timer_hz
    }

    #[inline]
    pub const fn origin_ticks(&self) -> u64 {
        self.origin_ticks
    }

    /// Re-anchor the capture origin. Used once, when the first sample arrives.
    pub fn set_origin(&mut self, origin_ticks: u64) {
        self.origin_ticks = origin_ticks;
    }

    pub fn set_fit(&mut self, fit: Option<ClockFit>) {
        self.fit = fit;
    }

    #[inline]
    pub const fn fit(&self) -> Option<ClockFit> {
        self.fit
    }

    /// Convert a tick *interval* to nanoseconds. Exact by construction: same clock, both ends.
    #[inline]
    pub fn ticks_to_ns(&self, ticks: u64) -> u64 {
        // 128-bit intermediate: at 1 MHz, `ticks * 1e9` overflows u64 after ~5 hours.
        ((ticks as u128 * 1_000_000_000u128) / self.timer_hz as u128) as u64
    }

    /// Convert nanoseconds to a tick interval.
    #[inline]
    pub fn ns_to_ticks(&self, ns: u64) -> u64 {
        ((ns as u128 * self.timer_hz as u128) / 1_000_000_000u128) as u64
    }

    /// Convert an unwrapped device timestamp to capture time.
    ///
    /// Timestamps before the origin clamp to zero rather than wrapping into an enormous
    /// positive value — a pre-trigger sample is at time zero, not at year 2554.
    #[inline]
    pub fn to_capture_ns(&self, t: DeviceTime) -> u64 {
        self.ticks_to_ns(t.0.saturating_sub(self.origin_ticks))
    }

    /// Convert an unwrapped device timestamp to host monotonic nanoseconds, if a fit exists.
    ///
    /// Only for correlating with something outside this capture. Never use this for a `Δt`.
    pub fn to_host_ns(&self, t: DeviceTime) -> Option<u64> {
        let fit = self.fit?;
        let ns = fit.a * t.0 as f64 + fit.b;
        (ns >= 0.0).then_some(ns as u64)
    }

    /// Host monotonic nanoseconds since capture start, for stamping SYNC round trips.
    pub fn host_elapsed_ns(&self) -> Option<u64> {
        self.host_origin.map(|o| o.elapsed().as_nanos() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_a_monotonic_counter_across_many_wraps() {
        let mut u = TickUnwrapper::new();
        let mut expected: u64 = 0;
        let step: u64 = 1_000_000_000; // large steps, but under 2^31
        let mut got_last = 0u64;
        for _ in 0..40 {
            let t = u.unwrap_ticks(expected as u32).expect("unwrap");
            assert!(t.0 >= got_last, "unwrapped time went backwards");
            assert_eq!(t.0, expected, "unwrapped value must equal the true counter");
            got_last = t.0;
            expected += step;
        }
        assert!(
            u.epoch() > 0,
            "the counter should have wrapped at least once"
        );
    }

    /// The exact wrap boundary is the case most likely to be off by one.
    #[test]
    fn wraps_exactly_at_the_boundary() {
        let mut u = TickUnwrapper::new();
        assert_eq!(
            u.unwrap_ticks(0xFFFF_FFFF).unwrap(),
            DeviceTime(0xFFFF_FFFF)
        );
        assert_eq!(u.unwrap_ticks(0).unwrap(), DeviceTime(0x1_0000_0000));
        assert_eq!(u.unwrap_ticks(5).unwrap(), DeviceTime(0x1_0000_0005));
        assert_eq!(u.epoch(), 1);
    }

    #[test]
    fn small_backward_step_is_an_error_not_a_wrap() {
        let mut u = TickUnwrapper::new();
        u.unwrap_ticks(1_000_000).unwrap();
        assert!(matches!(
            u.unwrap_ticks(999_000),
            Err(TimeError::BackwardsTime { .. })
        ));
    }

    #[test]
    fn implausible_forward_jump_is_an_error() {
        let mut u = TickUnwrapper::new();
        u.unwrap_ticks(0).unwrap();
        assert!(matches!(
            u.unwrap_ticks(0x9000_0000),
            Err(TimeError::ImplausibleGap { .. })
        ));
    }

    /// Events and samples share a timer but arrive interleaved, so resolving a non-sample
    /// timestamp must not be read as time going backwards.
    #[test]
    fn resolve_handles_out_of_order_streams_without_advancing_the_epoch() {
        let mut u = TickUnwrapper::new();
        u.unwrap_ticks(19_180).unwrap();

        // An event stamped earlier than the last sample block, arriving after it.
        assert_eq!(u.resolve(5_000), DeviceTime(5_000));
        assert_eq!(u.epoch(), 0, "resolving must not advance the epoch");

        // And across a wrap, with an unwrapper that reached it by advancing normally.
        let mut u = TickUnwrapper::new();
        u.unwrap_ticks(0xFFFF_FF00).unwrap();
        u.unwrap_ticks(0x0000_0100).unwrap();
        assert_eq!(u.epoch(), 1);
        // Just before the wrap: still the previous epoch.
        assert_eq!(u.resolve(0xFFFF_FFF0), DeviceTime(0xFFFF_FFF0));
        // Just after it: the current one.
        assert_eq!(u.resolve(0x0000_0200), DeviceTime(0x1_0000_0200));
    }

    #[test]
    fn wrap_count_verifies_the_inference() {
        let mut u = TickUnwrapper::new();
        u.unwrap_ticks(0xFFFF_FF00).unwrap();
        u.unwrap_ticks(0x0000_0100).unwrap();
        assert_eq!(u.epoch(), 1);
        assert!(u.check_wrap_count(1).is_ok());
        // A device reporting a different wrap count means the host inference is wrong, and
        // that must surface rather than becoming a 71-minute jump in the data.
        assert!(matches!(
            u.check_wrap_count(3),
            Err(TimeError::WrapMismatch {
                inferred: 1,
                reported: 3
            })
        ));
        u.resync(0x0000_0100, 3);
        assert!(u.check_wrap_count(3).is_ok());
    }

    #[test]
    fn tick_conversion_is_exact_at_one_mhz() {
        let tb = TimeBase::from_stored(1_000_000, 0);
        assert_eq!(tb.ticks_to_ns(1), 1_000);
        assert_eq!(tb.ticks_to_ns(1_000_000), 1_000_000_000);
        assert_eq!(tb.ns_to_ticks(1_000_000_000), 1_000_000);
    }

    /// A naive `ticks * 1e9` in u64 overflows after about 18 seconds at 1 MHz... in fact
    /// after ~5 hours; either way a long capture must not silently produce garbage.
    #[test]
    fn tick_conversion_does_not_overflow_on_long_captures() {
        let tb = TimeBase::from_stored(1_000_000, 0);
        let one_day_ticks = 86_400u64 * 1_000_000;
        assert_eq!(tb.ticks_to_ns(one_day_ticks), 86_400_000_000_000);
    }

    #[test]
    fn capture_time_is_relative_to_the_origin() {
        let tb = TimeBase::from_stored(1_000_000, 5_000_000);
        assert_eq!(tb.to_capture_ns(DeviceTime(5_000_000)), 0);
        assert_eq!(tb.to_capture_ns(DeviceTime(5_001_000)), 1_000_000);
        // Before the origin clamps to zero rather than wrapping to a huge value.
        assert_eq!(tb.to_capture_ns(DeviceTime(4_000_000)), 0);
    }

    #[test]
    fn spans_intersect_and_union() {
        let a = TimeSpan::new(0, 100);
        let b = TimeSpan::new(50, 200);
        assert_eq!(a.intersect(&b), Some(TimeSpan::new(50, 100)));
        assert_eq!(a.union(&b), TimeSpan::new(0, 200));
        assert_eq!(a.intersect(&TimeSpan::new(200, 300)), None);
        assert!(a.contains(0));
        assert!(!a.contains(100), "spans are half-open");
        assert!(TimeSpan::EMPTY.is_empty());
    }

    #[test]
    fn clock_fit_recovers_a_known_rate_and_ignores_delayed_round_trips() {
        // Device runs at 1 MHz, so 1000 host ns per tick, with a 5_000_000 ns offset.
        let a_true = 1000.0;
        let b_true = 5_000_000.0;
        let mut samples = Vec::new();
        for i in 0..40u64 {
            let ticks = i * 1_000;
            let mid = a_true * ticks as f64 + b_true;
            // Every fifth round trip is badly delayed, one-sided, as USB scheduling does.
            let rtt = if i % 5 == 0 { 4_000_000.0 } else { 20_000.0 };
            samples.push(SyncSample {
                host_send_ns: (mid - rtt / 2.0) as u64,
                host_recv_ns: (mid + rtt / 2.0) as u64,
                device_ticks: ticks,
            });
        }
        let fit = fit_clocks(&samples).expect("fit");
        assert!((fit.a - a_true).abs() < 1.0, "rate off: {}", fit.a);
        assert!((fit.b - b_true).abs() < 10_000.0, "offset off: {}", fit.b);
        assert!(fit.drift_ppm(1_000_000).abs() < 1000.0);
    }

    #[test]
    fn clock_fit_needs_enough_samples() {
        assert!(fit_clocks(&[]).is_none());
        assert!(
            fit_clocks(&[SyncSample {
                host_send_ns: 0,
                host_recv_ns: 1,
                device_ticks: 0
            }])
            .is_none()
        );
    }
}
