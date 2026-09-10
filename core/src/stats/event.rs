//! Per-event statistics.
//!
//! This is the feature that separates a power profiler from an oscilloscope. Given 500
//! occurrences of `BLE_TX`, it answers: how long does one take, what does one cost, and how
//! much does that vary?
//!
//! ```text
//! BLE_TX
//!   Occurrences        500
//!   Mean duration     3.42 ms
//!   Mean current      24.8 mA
//!   Peak current      71.2 mA
//!   Mean energy        282 uJ
//!   P95 energy         301 uJ
//! ```
//!
//! # Pairing
//!
//! Starts and stops are matched with a stack per event definition, so nested occurrences of
//! the same scope pair correctly. Three things are counted rather than hidden:
//!
//! - **unterminated** starts, where the capture ended mid-scope or the stop was lost;
//! - **orphaned** stops with no matching start, which usually means the capture began
//!   mid-scope;
//! - occurrences whose window contains **no samples**, which means the event is shorter than
//!   the sample period and its energy cannot be measured at this rate.
//!
//! Silently dropping any of these would make a firmware change look like an improvement.

use serde::{Deserialize, Serialize};

use super::dist::Distribution;
use super::region::{RegionStats, StatsOptions, compute};
use crate::capture::reader::{ReadSample, StoredEvent};
use crate::capture::{CaptureReader, GapPolicy};
use crate::error::StatsError;
use crate::metadata::{EventDef, EventMap};
use crate::time::TimeSpan;

/// One matched start/stop pair.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventOccurrence {
    pub start_ns: u64,
    pub end_ns: u64,
    /// The `value` carried by the start event, if any.
    pub value: Option<u32>,
    /// Statistics over this occurrence's window, when it contained samples.
    pub stats: Option<RegionStats>,
}

impl EventOccurrence {
    pub const fn duration_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }

    pub const fn span(&self) -> TimeSpan {
        TimeSpan::new(self.start_ns, self.end_ns)
    }
}

/// Aggregate statistics for one named event.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EventStats {
    pub name: String,
    pub start_id: u16,
    pub stop_id: Option<u16>,
    pub occurrences: u64,

    pub duration_us: Distribution,
    pub energy_uj: Distribution,
    pub charge_uc: Distribution,

    pub current_mean_ua: f64,
    pub current_peak_ua: i32,
    /// Sum of every occurrence's energy: what this event costs over the whole capture.
    pub total_energy_uj: f64,
    /// Fraction of the analysed span spent inside this event.
    pub duty_cycle: f64,

    /// Starts with no matching stop. The capture ended mid-scope, or a stop was lost.
    pub unterminated: u64,
    /// Stops with no matching start. Usually means the capture began mid-scope.
    pub orphaned_stops: u64,
    /// Occurrences too short to contain a sample at this rate, so their energy is unknown.
    ///
    /// Reported rather than counted as zero: an event costing "0 µJ" because nobody looked
    /// is how a power budget quietly becomes fiction.
    pub unsampled: u64,
}

impl EventStats {
    /// Mean energy per occurrence in microjoules, or `None` if nothing was measurable.
    pub fn mean_energy_uj(&self) -> Option<f64> {
        (!self.energy_uj.is_empty()).then_some(self.energy_uj.mean)
    }

    /// `true` if any occurrence could not be measured, so the numbers are incomplete.
    pub const fn has_warnings(&self) -> bool {
        self.unterminated > 0 || self.orphaned_stops > 0 || self.unsampled > 0
    }

    /// Percentage change in mean energy against a baseline. Positive means a regression.
    pub fn energy_regression_vs(&self, baseline_uj: f64) -> Option<f64> {
        let mine = self.mean_energy_uj()?;
        (baseline_uj != 0.0).then(|| (mine - baseline_uj) / baseline_uj * 100.0)
    }
}

/// One paired occurrence as `(start_ns, end_ns, value_from_the_start_event)`.
pub type PairedOccurrence = (u64, u64, Option<u32>);

/// The result of pairing: occurrences, unterminated starts, orphaned stops.
pub type PairingResult = (Vec<PairedOccurrence>, u64, u64);

/// Pair the start and stop events for one definition.
///
/// A stack, not a flag, so nested occurrences of the same scope pair correctly — which real
/// firmware produces the moment a scoped macro appears in a recursive or re-entrant path.
pub fn occurrences_of(def: &EventDef, events: &[StoredEvent]) -> PairingResult {
    let mut open: Vec<(u64, Option<u32>)> = Vec::new();
    let mut out = Vec::new();
    let mut orphaned = 0u64;

    let latency = def.latency_compensation_ns;
    let adjust = |t: u64| -> u64 {
        if latency >= 0 {
            t.saturating_sub(latency as u64)
        } else {
            t.saturating_add(latency.unsigned_abs())
        }
    };

    for e in events {
        if e.id == def.start_id {
            open.push((adjust(e.t_ns), e.value));
        } else if Some(e.id) == def.stop_id {
            match open.pop() {
                Some((start, value)) => out.push((start, adjust(e.t_ns), value)),
                None => orphaned += 1,
            }
        }
    }

    let unterminated = open.len() as u64;
    out.sort_by_key(|(s, _, _)| *s);
    (out, unterminated, orphaned)
}

/// Compute statistics for every scoped event the metadata declares.
pub fn event_stats(
    reader: &mut CaptureReader,
    map: &EventMap,
    options: &StatsOptions,
) -> Result<Vec<EventStats>, StatsError> {
    let events = reader.events()?;
    let span = reader.span();

    // Read the samples once. Re-reading per occurrence would decompress the same chunks
    // hundreds of times over, and a capture with 500 events is entirely ordinary.
    let samples = reader.samples_in(span)?;

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

    let mut out = Vec::new();
    for def in map.scopes() {
        out.push(stats_for(def, &events, &samples, span, options));
    }
    out.sort_by(|a, b| {
        b.total_energy_uj
            .partial_cmp(&a.total_energy_uj)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Statistics for one event definition against already-loaded events and samples.
///
/// Separate from `event_stats` so it can be tested directly against synthetic input.
pub fn stats_for(
    def: &EventDef,
    events: &[StoredEvent],
    samples: &[ReadSample],
    span: TimeSpan,
    options: &StatsOptions,
) -> EventStats {
    let (pairs, unterminated, orphaned_stops) = occurrences_of(def, events);

    let mut durations_us = Vec::with_capacity(pairs.len());
    let mut energies_uj = Vec::with_capacity(pairs.len());
    let mut charges_uc = Vec::with_capacity(pairs.len());
    let mut current_sum = super::KahanSum::new();
    let mut current_terms = 0u64;
    let mut peak_ua = i32::MIN;
    let mut busy_ns = 0u64;
    let mut unsampled = 0u64;

    for &(start, end, _value) in &pairs {
        durations_us.push((end.saturating_sub(start)) as f64 / 1e3);
        busy_ns += end.saturating_sub(start);

        // Binary search rather than a scan: with 500 occurrences over 180 M samples, a linear
        // scan per occurrence is the difference between milliseconds and minutes.
        let lo = samples.partition_point(|s| s.t_ns < start);
        let hi = samples.partition_point(|s| s.t_ns < end);
        let window = &samples[lo..hi];
        if window.is_empty() {
            unsampled += 1;
            continue;
        }

        let s = compute(window, TimeSpan::new(start, end), options);
        energies_uj.push(s.energy_uj);
        charges_uc.push(s.charge_uc);
        current_sum.add(s.current_mean_ua * window.len() as f64);
        current_terms += window.len() as u64;
        peak_ua = peak_ua.max(s.current_max_ua);
    }

    let span_ns = span.duration_ns();
    EventStats {
        name: def.name.clone(),
        start_id: def.start_id,
        stop_id: def.stop_id,
        occurrences: pairs.len() as u64,
        duration_us: Distribution::of(&mut durations_us),
        energy_uj: Distribution::of(&mut energies_uj),
        charge_uc: Distribution::of(&mut charges_uc),
        current_mean_ua: if current_terms > 0 {
            current_sum.total() / current_terms as f64
        } else {
            0.0
        },
        current_peak_ua: if peak_ua == i32::MIN { 0 } else { peak_ua },
        total_energy_uj: energies_uj_total(&energies_uj),
        duty_cycle: if span_ns > 0 {
            busy_ns as f64 / span_ns as f64
        } else {
            0.0
        },
        unterminated,
        orphaned_stops,
        unsampled,
    }
}

fn energies_uj_total(values: &[f64]) -> f64 {
    let mut k = super::KahanSum::new();
    for &v in values {
        k.add(v);
    }
    k.total()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::EventDef;
    use approx::assert_relative_eq;

    const START: u16 = 0x0101;
    const STOP: u16 = 0x0102;

    fn def() -> EventDef {
        EventDef::scope("BLE_TX", START, STOP)
    }

    fn ev(t_ns: u64, id: u16) -> StoredEvent {
        StoredEvent {
            t_ns,
            id,
            value: None,
        }
    }

    /// A trace at `idle_ua`, with `bursts` windows of `burst_ua`, and matching events.
    fn synthetic(
        n: usize,
        period_ns: u64,
        idle_ua: i32,
        bursts: &[(u64, u64, i32)],
    ) -> (Vec<ReadSample>, Vec<StoredEvent>) {
        let samples = (0..n)
            .map(|i| {
                let t = i as u64 * period_ns;
                let cur = bursts
                    .iter()
                    .find(|(s, e, _)| t >= *s && t < *e)
                    .map_or(idle_ua, |(_, _, c)| *c);
                ReadSample {
                    t_ns: t,
                    current_ua: cur,
                    voltage_uv: 1_000_000,
                }
            })
            .collect();
        let mut events = Vec::new();
        for (s, e, _) in bursts {
            events.push(ev(*s, START));
            events.push(ev(*e, STOP));
        }
        events.sort_by_key(|e| e.t_ns);
        (samples, events)
    }

    #[test]
    fn pairs_starts_with_stops_in_order() {
        let events = vec![ev(100, START), ev(200, STOP), ev(300, START), ev(450, STOP)];
        let (pairs, unterminated, orphaned) = occurrences_of(&def(), &events);
        assert_eq!(pairs, vec![(100, 200, None), (300, 450, None)]);
        assert_eq!(unterminated, 0);
        assert_eq!(orphaned, 0);
    }

    /// A stack, not a flag: real firmware nests scoped macros.
    #[test]
    fn nested_occurrences_pair_innermost_first() {
        let events = vec![ev(100, START), ev(150, START), ev(200, STOP), ev(300, STOP)];
        let (pairs, unterminated, orphaned) = occurrences_of(&def(), &events);
        assert_eq!(pairs.len(), 2);
        assert!(
            pairs.contains(&(150, 200, None)),
            "the inner scope should pair with the first stop"
        );
        assert!(
            pairs.contains(&(100, 300, None)),
            "the outer scope should pair with the second"
        );
        assert_eq!(unterminated, 0);
        assert_eq!(orphaned, 0);
    }

    /// A capture that ends mid-scope must say so, not quietly drop the occurrence.
    #[test]
    fn an_unterminated_start_is_counted() {
        let events = vec![ev(100, START), ev(200, STOP), ev(300, START)];
        let (pairs, unterminated, orphaned) = occurrences_of(&def(), &events);
        assert_eq!(pairs.len(), 1);
        assert_eq!(unterminated, 1);
        assert_eq!(orphaned, 0);
    }

    /// And one that begins mid-scope must say that too.
    #[test]
    fn an_orphaned_stop_is_counted() {
        let events = vec![ev(50, STOP), ev(100, START), ev(200, STOP)];
        let (pairs, unterminated, orphaned) = occurrences_of(&def(), &events);
        assert_eq!(pairs.len(), 1);
        assert_eq!(unterminated, 0);
        assert_eq!(orphaned, 1);
    }

    #[test]
    fn events_for_other_definitions_are_ignored() {
        let events = vec![ev(100, START), ev(120, 0x0999), ev(200, STOP)];
        let (pairs, _, _) = occurrences_of(&def(), &events);
        assert_eq!(pairs, vec![(100, 200, None)]);
    }

    /// The documented example, computed rather than asserted by hand.
    #[test]
    fn per_event_energy_matches_the_analytic_value() {
        // 78 mA for 950 us at 1 V, ten times, 3 mA idle, sampled at 50 ksps.
        let period = 20_000u64;
        let bursts: Vec<(u64, u64, i32)> = (0..10)
            .map(|i| {
                (
                    i * 10_000_000 + 1_000_000,
                    i * 10_000_000 + 1_950_000,
                    78_000,
                )
            })
            .collect();
        let (samples, events) = synthetic(5_000, period, 3_000, &bursts);
        let span = TimeSpan::new(0, 100_000_000);

        let s = stats_for(&def(), &events, &samples, span, &StatsOptions::default());
        assert_eq!(s.occurrences, 10);
        assert_eq!(s.unterminated, 0);
        assert_eq!(s.orphaned_stops, 0);
        assert_eq!(s.unsampled, 0);
        assert_eq!(s.current_peak_ua, 78_000);

        // 78 mA * 1 V * 950 us = 74.1 uJ, within one sample period of quantisation.
        assert_relative_eq!(s.energy_uj.mean, 74.1, max_relative = 0.05);
        assert_relative_eq!(s.duration_us.mean, 950.0, max_relative = 1e-9);
        assert_relative_eq!(
            s.total_energy_uj,
            s.energy_uj.mean * 10.0,
            max_relative = 1e-9
        );
        // 950 us every 10 ms is 9.5%.
        assert_relative_eq!(s.duty_cycle, 0.095, max_relative = 0.02);
    }

    /// An event shorter than the sample period cannot have its energy measured. Reporting
    /// 0 µJ would be a lie that a power budget would happily absorb.
    #[test]
    fn an_event_too_short_to_sample_is_flagged_not_reported_as_zero() {
        let (samples, _) = synthetic(100, 20_000, 3_000, &[]);
        // A 1 us window between two samples at 20 us spacing.
        let events = vec![ev(30_001, START), ev(30_002, STOP)];
        let s = stats_for(
            &def(),
            &events,
            &samples,
            TimeSpan::new(0, 2_000_000),
            &StatsOptions::default(),
        );

        assert_eq!(s.occurrences, 1);
        assert_eq!(s.unsampled, 1);
        assert!(
            s.energy_uj.is_empty(),
            "an unmeasurable event must not report an energy"
        );
        assert!(s.mean_energy_uj().is_none());
        assert!(s.has_warnings());
    }

    #[test]
    fn duration_jitter_makes_p95_differ_from_the_mean() {
        let bursts: Vec<(u64, u64, i32)> = (0..100)
            .map(|i| {
                let start = i * 1_000_000 + 100_000;
                // Every tenth burst runs 50% longer.
                let len = if i % 10 == 0 { 300_000 } else { 200_000 };
                (start, start + len, 50_000)
            })
            .collect();
        let (samples, events) = synthetic(50_000, 2_000, 3_000, &bursts);
        let s = stats_for(
            &def(),
            &events,
            &samples,
            TimeSpan::new(0, 100_000_000),
            &StatsOptions::default(),
        );

        assert_eq!(s.occurrences, 100);
        assert!(
            s.duration_us.p95 > s.duration_us.mean,
            "P95 {} should exceed the mean {}",
            s.duration_us.p95,
            s.duration_us.mean
        );
        assert!(s.energy_uj.p95 > s.energy_uj.mean);
    }

    /// The regression comparison the CI gate is built on.
    #[test]
    fn a_longer_burst_shows_up_as_an_energy_regression() {
        let make = |len_ns: u64| {
            let bursts: Vec<(u64, u64, i32)> = (0..10)
                .map(|i| {
                    (
                        i * 10_000_000 + 1_000_000,
                        i * 10_000_000 + 1_000_000 + len_ns,
                        78_000,
                    )
                })
                .collect();
            let (samples, events) = synthetic(5_000, 20_000, 3_000, &bursts);
            stats_for(
                &def(),
                &events,
                &samples,
                TimeSpan::new(0, 100_000_000),
                &StatsOptions::default(),
            )
        };

        let good = make(950_000);
        let bad = make(1_150_000);
        let regression = bad.energy_regression_vs(good.energy_uj.mean).unwrap();
        assert!(
            regression > 15.0,
            "a 21% longer burst should read as a clear regression, got {regression:.1}%"
        );
        assert!(
            good.energy_regression_vs(good.energy_uj.mean)
                .unwrap()
                .abs()
                < 1e-9
        );
    }

    /// Instrumentation latency is the accuracy floor for event boundaries, so the correction
    /// has to actually move the window.
    #[test]
    fn latency_compensation_shifts_event_boundaries() {
        let mut d = def();
        d.latency_compensation_ns = 10_000;
        let events = vec![ev(100_000, START), ev(200_000, STOP)];
        let (pairs, _, _) = occurrences_of(&d, &events);
        assert_eq!(pairs, vec![(90_000, 190_000, None)]);

        // A negative compensation shifts the other way, for a transport that reports early.
        d.latency_compensation_ns = -5_000;
        let (pairs, _, _) = occurrences_of(&d, &events);
        assert_eq!(pairs, vec![(105_000, 205_000, None)]);
    }

    #[test]
    fn an_event_that_never_occurs_reports_zero_occurrences_not_an_error() {
        let (samples, _) = synthetic(100, 20_000, 3_000, &[]);
        let s = stats_for(
            &def(),
            &[],
            &samples,
            TimeSpan::new(0, 2_000_000),
            &StatsOptions::default(),
        );
        assert_eq!(s.occurrences, 0);
        assert!(s.energy_uj.is_empty());
        assert_eq!(s.total_energy_uj, 0.0);
        assert_eq!(s.duty_cycle, 0.0);
        assert!(!s.has_warnings());
    }

    #[test]
    fn the_start_events_value_is_carried_through() {
        let events = vec![
            StoredEvent {
                t_ns: 100,
                id: START,
                value: Some(64),
            },
            StoredEvent {
                t_ns: 200,
                id: STOP,
                value: None,
            },
        ];
        let (pairs, _, _) = occurrences_of(&def(), &events);
        assert_eq!(pairs, vec![(100, 200, Some(64))]);
    }
}
