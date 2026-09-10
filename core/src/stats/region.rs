//! Statistics over a time region.
//!
//! This is what a UI selection reports, and what every event's energy is ultimately computed
//! from.

use serde::{Deserialize, Serialize};

use super::KahanSum;
use crate::capture::reader::ReadSample;
use crate::capture::{CaptureReader, GapPolicy};
use crate::error::StatsError;
use crate::time::TimeSpan;

/// How to integrate between samples.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Integration {
    /// Each sample holds until the next. Simple, and biased on any ramp.
    Rectangular,
    /// Average consecutive samples across the interval between them.
    ///
    /// The default, because real signals ramp: a radio PA takes tens of microseconds to reach
    /// full draw, and at 50 ksps that ramp spans only a handful of samples, where rectangular
    /// integration is visibly wrong.
    #[default]
    Trapezoid,
}

/// Knobs for a statistics computation.
#[derive(Clone, Debug, Default)]
pub struct StatsOptions {
    pub integration: Integration,
    pub on_gap: GapPolicy,
    /// Supply to assume when the capture has no voltage channel, in microvolts.
    pub supply_uv_override: Option<u32>,
}

impl StatsOptions {
    /// The settings `assert` uses: refuse to integrate across missing data.
    pub fn strict() -> StatsOptions {
        StatsOptions {
            on_gap: GapPolicy::Error,
            ..Default::default()
        }
    }

    /// The settings `analyze` uses: compute over what exists and say what was skipped.
    pub fn lenient() -> StatsOptions {
        StatsOptions {
            on_gap: GapPolicy::Skip,
            ..Default::default()
        }
    }
}

/// Everything a selected region costs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RegionStats {
    pub start_ns: u64,
    pub end_ns: u64,
    pub sample_count: u64,
    pub duration_s: f64,

    pub current_min_ua: i32,
    pub current_max_ua: i32,
    pub current_mean_ua: f64,
    /// RMS current, which is what matters for heating rather than for charge.
    pub current_rms_ua: f64,

    pub voltage_mean_uv: f64,

    pub charge_uc: f64,
    /// The same charge in milliamp-hours, which is the unit battery budgets use.
    pub charge_mah: f64,

    pub energy_uj: f64,
    pub power_mean_uw: f64,
    pub power_peak_uw: f64,
}

impl RegionStats {
    pub fn span(&self) -> TimeSpan {
        TimeSpan::new(self.start_ns, self.end_ns)
    }

    /// Extrapolated battery life on a given capacity, in hours.
    ///
    /// The question this whole tool exists to answer, once someone has a mean current.
    pub fn battery_hours(&self, capacity_mah: f64) -> Option<f64> {
        let mean_ma = self.current_mean_ua / 1000.0;
        (mean_ma > 0.0).then(|| capacity_mah / mean_ma)
    }
}

/// Compute statistics over `span`.
pub fn region_stats(
    reader: &mut CaptureReader,
    span: TimeSpan,
    options: &StatsOptions,
) -> Result<RegionStats, StatsError> {
    let span = match span.intersect(&reader.span()) {
        Some(s) => s,
        None if span == TimeSpan::ALL => reader.span(),
        None => return Err(StatsError::EmptySpan),
    };
    if span.is_empty() {
        return Err(StatsError::EmptySpan);
    }

    // Gaps first: refusing to compute is cheaper than computing a wrong answer, and far more
    // useful than one.
    let crossing: Vec<_> = reader
        .gaps()
        .iter()
        .filter(|g| span.overlaps(&TimeSpan::new(g.start_ns, g.end_ns)))
        .copied()
        .collect();
    if !crossing.is_empty() && options.on_gap == GapPolicy::Error {
        return Err(StatsError::GapInSpan {
            count: crossing.len(),
            lost_ns: crossing.iter().map(|g| g.duration_ns()).sum(),
        });
    }

    let samples = reader.samples_in(span)?;
    if samples.is_empty() {
        return Err(StatsError::EmptySpan);
    }

    Ok(compute(&samples, span, options))
}

/// The integrator itself, over an already-materialised sample slice.
///
/// Separate from `region_stats` so it can be tested directly against analytically known
/// waveforms, without a capture file in the way.
pub fn compute(samples: &[ReadSample], span: TimeSpan, options: &StatsOptions) -> RegionStats {
    let supply = options.supply_uv_override;

    let mut min_ua = i32::MAX;
    let mut max_ua = i32::MIN;
    let mut current_sum = KahanSum::new();
    let mut current_sq_sum = KahanSum::new();
    let mut voltage_sum = KahanSum::new();
    let mut charge = KahanSum::new(); // amp-seconds
    let mut energy = KahanSum::new(); // joules
    let mut peak_power_w = 0.0f64;

    let volts = |s: &ReadSample| -> f64 { supply.unwrap_or(s.voltage_uv) as f64 / 1e6 };

    for s in samples {
        min_ua = min_ua.min(s.current_ua);
        max_ua = max_ua.max(s.current_ua);
        current_sum.add(s.current_ua as f64);
        current_sq_sum.add((s.current_ua as f64) * (s.current_ua as f64));
        voltage_sum.add(supply.unwrap_or(s.voltage_uv) as f64);
        peak_power_w = peak_power_w.max((s.current_ua as f64 / 1e6) * volts(s));
    }

    // Integrate over the *actual* interval between consecutive timestamps. Using
    // `count * nominal_period` is exactly how a capture silently under-reports energy across
    // a dropped block.
    for w in samples.windows(2) {
        let dt = (w[1].t_ns.saturating_sub(w[0].t_ns)) as f64 / 1e9;
        if dt <= 0.0 {
            continue;
        }
        let (i0, i1) = (w[0].current_ua as f64 / 1e6, w[1].current_ua as f64 / 1e6);
        let (p0, p1) = (i0 * volts(&w[0]), i1 * volts(&w[1]));
        match options.integration {
            Integration::Rectangular => {
                charge.add(i0 * dt);
                energy.add(p0 * dt);
            }
            Integration::Trapezoid => {
                charge.add((i0 + i1) * 0.5 * dt);
                energy.add((p0 + p1) * 0.5 * dt);
            }
        }
    }

    // A single sample has no interval, so it carries no charge or energy — but the span it
    // was asked about still has a duration, and reporting that honestly beats inventing one.
    let n = samples.len() as f64;
    let measured_ns = samples
        .last()
        .zip(samples.first())
        .map_or(0, |(l, f)| l.t_ns.saturating_sub(f.t_ns));
    let duration_s = if samples.len() > 1 {
        measured_ns as f64 / 1e9
    } else {
        span.duration_ns() as f64 / 1e9
    };

    let charge_c = charge.total();
    let energy_j = energy.total();

    RegionStats {
        start_ns: span.start_ns,
        end_ns: span.end_ns,
        sample_count: samples.len() as u64,
        duration_s,
        current_min_ua: min_ua,
        current_max_ua: max_ua,
        current_mean_ua: current_sum.total() / n,
        current_rms_ua: (current_sq_sum.total() / n).sqrt(),
        voltage_mean_uv: voltage_sum.total() / n,
        charge_uc: charge_c * 1e6,
        charge_mah: charge_c / 3.6,
        energy_uj: energy_j * 1e6,
        power_mean_uw: if duration_s > 0.0 {
            energy_j / duration_s * 1e6
        } else {
            0.0
        },
        power_peak_uw: peak_power_w * 1e6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    /// A constant current at a constant voltage: every figure is known exactly.
    fn flat(n: usize, period_ns: u64, current_ua: i32, voltage_uv: u32) -> Vec<ReadSample> {
        (0..n)
            .map(|i| ReadSample {
                t_ns: i as u64 * period_ns,
                current_ua,
                voltage_uv,
            })
            .collect()
    }

    #[test]
    fn a_constant_load_gives_exactly_the_analytic_answer() {
        // 10 mA at 3.3 V for 1 s: 33 mW, 33 mJ, 10 mC.
        let samples = flat(50_001, 20_000, 10_000, 3_300_000);
        let span = TimeSpan::new(0, 1_000_000_001);
        let s = compute(&samples, span, &StatsOptions::default());

        assert_relative_eq!(s.duration_s, 1.0, max_relative = 1e-12);
        assert_relative_eq!(s.current_mean_ua, 10_000.0, max_relative = 1e-12);
        assert_relative_eq!(s.current_rms_ua, 10_000.0, max_relative = 1e-12);
        assert_relative_eq!(s.charge_uc, 10_000.0, max_relative = 1e-9);
        assert_relative_eq!(s.energy_uj, 33_000.0, max_relative = 1e-9);
        assert_relative_eq!(s.power_mean_uw, 33_000.0, max_relative = 1e-9);
        assert_relative_eq!(s.power_peak_uw, 33_000.0, max_relative = 1e-9);
        // 10 mA for one hour is 10 mAh, which is 1/360 of an hour's worth here.
        assert_relative_eq!(s.charge_mah, 10.0 / 3600.0, max_relative = 1e-9);
    }

    /// Rectangular and trapezoidal must differ on a ramp, or the integrator choice is
    /// meaningless and one of the two is silently wrong.
    #[test]
    fn integration_method_matters_on_a_ramp() {
        // Current rises linearly from 0 to 100 mA over 1 ms at 1 us intervals.
        let samples: Vec<ReadSample> = (0..=1000)
            .map(|i| ReadSample {
                t_ns: i as u64 * 1_000,
                current_ua: i * 100,
                voltage_uv: 1_000_000,
            })
            .collect();
        let span = TimeSpan::new(0, 1_000_001);

        let trap = compute(
            &samples,
            span,
            &StatsOptions {
                integration: Integration::Trapezoid,
                ..Default::default()
            },
        );
        let rect = compute(
            &samples,
            span,
            &StatsOptions {
                integration: Integration::Rectangular,
                ..Default::default()
            },
        );

        // Exact integral of a 0->0.1 A ramp over 1 ms at 1 V is 50 uJ.
        assert_relative_eq!(trap.energy_uj, 50.0, max_relative = 1e-9);
        // Rectangular lags by half a step: 100 uA * 1 us * 1 V = 0.1 uJ less... it
        // under-reports, and must visibly do so.
        assert!(
            rect.energy_uj < trap.energy_uj,
            "rectangular should under-report on a rising ramp"
        );
        assert!((trap.energy_uj - rect.energy_uj).abs() > 0.01);
    }

    /// The rule from the module docs, as an executable assertion.
    #[test]
    fn energy_is_integrated_over_real_timestamps_not_a_nominal_period() {
        // Ten samples at 10 mA, then a 100 ms stall, then ten more.
        let mut samples = flat(10, 20_000, 10_000, 1_000_000);
        let resume = 100_000_000u64;
        samples.extend((0..10).map(|i| ReadSample {
            t_ns: resume + i as u64 * 20_000,
            current_ua: 10_000,
            voltage_uv: 1_000_000,
        }));
        let span = TimeSpan::new(0, resume + 200_000);
        let s = compute(&samples, span, &StatsOptions::default());

        // If the stall were ignored, duration would be 19 * 20 us = 380 us. The real span is
        // over 100 ms, and the energy must reflect the current actually drawn across it.
        assert!(
            s.duration_s > 0.1,
            "the stall was swallowed: duration {}",
            s.duration_s
        );
        // 10 mA at 1 V across ~100.2 ms is about 1002 uJ.
        assert_relative_eq!(s.energy_uj, 1_001.8, max_relative = 0.01);
    }

    #[test]
    fn min_max_and_rms_are_reported() {
        let samples = vec![
            ReadSample {
                t_ns: 0,
                current_ua: 0,
                voltage_uv: 1_000_000,
            },
            ReadSample {
                t_ns: 1_000,
                current_ua: 100_000,
                voltage_uv: 1_000_000,
            },
            ReadSample {
                t_ns: 2_000,
                current_ua: 0,
                voltage_uv: 1_000_000,
            },
            ReadSample {
                t_ns: 3_000,
                current_ua: 100_000,
                voltage_uv: 1_000_000,
            },
        ];
        let s = compute(&samples, TimeSpan::new(0, 3_001), &StatsOptions::default());
        assert_eq!(s.current_min_ua, 0);
        assert_eq!(s.current_max_ua, 100_000);
        assert_relative_eq!(s.current_mean_ua, 50_000.0);
        // RMS of a 50% square wave between 0 and I is I/sqrt(2).
        assert_relative_eq!(
            s.current_rms_ua,
            100_000.0 / 2.0f64.sqrt(),
            max_relative = 1e-12
        );
        assert!(
            s.current_rms_ua > s.current_mean_ua,
            "RMS exceeds mean for anything but DC"
        );
    }

    #[test]
    fn a_voltage_override_replaces_a_missing_channel() {
        let samples = flat(1001, 1_000, 10_000, 0);
        let span = TimeSpan::new(0, 1_000_001);
        let s = compute(
            &samples,
            span,
            &StatsOptions {
                supply_uv_override: Some(3_300_000),
                ..Default::default()
            },
        );
        assert_relative_eq!(s.voltage_mean_uv, 3_300_000.0);
        // 10 mA at 3.3 V for 1 ms is 33 uJ.
        assert_relative_eq!(s.energy_uj, 33.0, max_relative = 1e-9);
    }

    /// Splitting a span in two must not change the total. Energy is additive or the
    /// integrator is broken.
    #[test]
    fn energy_over_adjacent_spans_adds_up() {
        let samples: Vec<ReadSample> = (0..=2000)
            .map(|i| ReadSample {
                t_ns: i as u64 * 1_000,
                current_ua: 5_000 + (i % 37) * 100,
                voltage_uv: 3_300_000,
            })
            .collect();

        let whole = compute(
            &samples,
            TimeSpan::new(0, 2_000_001),
            &StatsOptions::default(),
        );
        let first: Vec<ReadSample> = samples.iter().take(1001).copied().collect();
        let second: Vec<ReadSample> = samples.iter().skip(1000).copied().collect();
        let a = compute(
            &first,
            TimeSpan::new(0, 1_000_001),
            &StatsOptions::default(),
        );
        let b = compute(
            &second,
            TimeSpan::new(1_000_000, 2_000_001),
            &StatsOptions::default(),
        );

        assert_relative_eq!(
            a.energy_uj + b.energy_uj,
            whole.energy_uj,
            max_relative = 1e-9
        );
        assert_relative_eq!(
            a.charge_uc + b.charge_uc,
            whole.charge_uc,
            max_relative = 1e-9
        );
    }

    /// Scaling every current by k must scale charge by exactly k.
    #[test]
    fn charge_scales_linearly_with_current() {
        let base = flat(1001, 1_000, 7_000, 3_300_000);
        let scaled: Vec<ReadSample> = base
            .iter()
            .map(|s| ReadSample {
                current_ua: s.current_ua * 3,
                ..*s
            })
            .collect();
        let span = TimeSpan::new(0, 1_000_001);
        let a = compute(&base, span, &StatsOptions::default());
        let b = compute(&scaled, span, &StatsOptions::default());
        assert_relative_eq!(b.charge_uc, a.charge_uc * 3.0, max_relative = 1e-9);
        assert_relative_eq!(b.energy_uj, a.energy_uj * 3.0, max_relative = 1e-9);
    }

    #[test]
    fn a_single_sample_carries_no_energy_but_still_reports_the_span() {
        let samples = vec![ReadSample {
            t_ns: 500,
            current_ua: 10_000,
            voltage_uv: 3_300_000,
        }];
        let s = compute(&samples, TimeSpan::new(0, 1_000), &StatsOptions::default());
        assert_eq!(s.sample_count, 1);
        assert_eq!(s.energy_uj, 0.0);
        assert_relative_eq!(s.duration_s, 1e-6);
        assert_relative_eq!(s.current_mean_ua, 10_000.0);
    }

    #[test]
    fn battery_life_extrapolates_from_the_mean() {
        let samples = flat(1001, 1_000, 1_000, 3_300_000); // 1 mA
        let s = compute(
            &samples,
            TimeSpan::new(0, 1_000_001),
            &StatsOptions::default(),
        );
        // 1 mA from a 220 mAh cell is 220 hours.
        assert_relative_eq!(s.battery_hours(220.0).unwrap(), 220.0, max_relative = 1e-9);
        let idle = compute(
            &flat(10, 1_000, 0, 3_300_000),
            TimeSpan::new(0, 10_000),
            &StatsOptions::default(),
        );
        assert!(
            idle.battery_hours(220.0).is_none(),
            "zero draw has no finite life"
        );
    }

    #[test]
    fn the_default_integration_is_trapezoid() {
        assert_eq!(StatsOptions::default().integration, Integration::Trapezoid);
        assert_eq!(StatsOptions::strict().on_gap, GapPolicy::Error);
        assert_eq!(StatsOptions::lenient().on_gap, GapPolicy::Skip);
    }
}
