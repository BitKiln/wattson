//! Power budgets as build gates.
//!
//! This is what turns power consumption into a testable firmware metric:
//!
//! ```text
//! POWER REGRESSION
//!
//! BLE_TX energy
//!   Expected: <= 260 uJ
//!   Measured:    287 uJ
//!   Regression: +10.4%
//! ```
//!
//! # Two rules a CI gate has to follow
//!
//! **An unmeasurable event fails.** If an event never occurred, or every occurrence was too
//! short to sample, the rule cannot be evaluated — and a rule that silently passes because
//! there was nothing to check is worse than no rule at all. It is exactly the failure mode
//! where someone deletes the instrumentation and the build goes green.
//!
//! **A capture with gaps fails by default.** Integrating across missing data produces a
//! plausible number that is too low, which is the direction that turns a regression into a
//! pass.

use serde::{Deserialize, Serialize};

use crate::stats::EventStats;

/// Which number a rule constrains.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    MeanEnergy,
    P95Energy,
    MaxEnergy,
    TotalEnergy,
    MeanCurrent,
    PeakCurrent,
    MeanDuration,
    P95Duration,
    Occurrences,
}

impl Metric {
    pub const fn label(self) -> &'static str {
        match self {
            Metric::MeanEnergy => "mean energy",
            Metric::P95Energy => "P95 energy",
            Metric::MaxEnergy => "max energy",
            Metric::TotalEnergy => "total energy",
            Metric::MeanCurrent => "mean current",
            Metric::PeakCurrent => "peak current",
            Metric::MeanDuration => "mean duration",
            Metric::P95Duration => "P95 duration",
            Metric::Occurrences => "occurrences",
        }
    }

    /// The unit the metric is reported in.
    pub const fn unit(self) -> &'static str {
        match self {
            Metric::MeanEnergy | Metric::P95Energy | Metric::MaxEnergy | Metric::TotalEnergy => {
                "uJ"
            }
            Metric::MeanCurrent | Metric::PeakCurrent => "uA",
            Metric::MeanDuration | Metric::P95Duration => "us",
            Metric::Occurrences => "",
        }
    }

    /// Read the metric from a computed event.
    ///
    /// `None` when it could not be measured — which a gate must treat as a failure, not as a
    /// pass.
    pub fn read(self, s: &EventStats) -> Option<f64> {
        match self {
            Metric::MeanEnergy => (!s.energy_uj.is_empty()).then_some(s.energy_uj.mean),
            Metric::P95Energy => (!s.energy_uj.is_empty()).then_some(s.energy_uj.p95),
            Metric::MaxEnergy => (!s.energy_uj.is_empty()).then_some(s.energy_uj.max),
            Metric::TotalEnergy => (!s.energy_uj.is_empty()).then_some(s.total_energy_uj),
            Metric::MeanCurrent => (s.occurrences > 0).then_some(s.current_mean_ua),
            Metric::PeakCurrent => (s.occurrences > 0).then_some(s.current_peak_ua as f64),
            Metric::MeanDuration => (!s.duration_us.is_empty()).then_some(s.duration_us.mean),
            Metric::P95Duration => (!s.duration_us.is_empty()).then_some(s.duration_us.p95),
            Metric::Occurrences => Some(s.occurrences as f64),
        }
    }
}

/// How a measured value is compared against a limit.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    AtMost,
    AtLeast,
}

impl Comparison {
    pub const fn symbol(self) -> &'static str {
        match self {
            Comparison::AtMost => "<=",
            Comparison::AtLeast => ">=",
        }
    }

    pub fn holds(self, measured: f64, limit: f64) -> bool {
        match self {
            Comparison::AtMost => measured <= limit,
            Comparison::AtLeast => measured >= limit,
        }
    }
}

/// One budget to check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssertRule {
    /// Event name, as declared in the capture's metadata.
    pub event: String,
    pub metric: Metric,
    pub comparison: Comparison,
    /// The limit, in the metric's own unit.
    pub limit: f64,
}

impl AssertRule {
    pub fn at_most(event: impl Into<String>, metric: Metric, limit: f64) -> AssertRule {
        AssertRule {
            event: event.into(),
            metric,
            comparison: Comparison::AtMost,
            limit,
        }
    }

    pub fn at_least(event: impl Into<String>, metric: Metric, limit: f64) -> AssertRule {
        AssertRule {
            event: event.into(),
            metric,
            comparison: Comparison::AtLeast,
            limit,
        }
    }
}

/// A previous run's numbers, for relative comparison.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// Event name to that event's statistics.
    pub events: std::collections::BTreeMap<String, EventStats>,
    /// Free-form label: a git hash, a build number, a firmware version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Baseline {
    pub fn from_stats(stats: &[EventStats], label: Option<String>) -> Baseline {
        Baseline {
            events: stats.iter().map(|s| (s.name.clone(), s.clone())).collect(),
            label,
        }
    }

    pub fn get(&self, event: &str) -> Option<&EventStats> {
        self.events.get(event)
    }
}

/// Why a rule failed, when it did.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The measured value was outside the limit.
    OverBudget,
    /// The capture's metadata never declared this event.
    UnknownEvent,
    /// The event was declared but never occurred.
    NeverOccurred,
    /// The event occurred but could not be measured at this sample rate.
    Unmeasurable,
}

impl FailureKind {
    pub const fn describe(self) -> &'static str {
        match self {
            FailureKind::OverBudget => "over budget",
            FailureKind::UnknownEvent => "not declared in this capture's metadata",
            FailureKind::NeverOccurred => "never occurred in this capture",
            FailureKind::Unmeasurable => {
                "occurred, but every occurrence was too short to sample at this rate"
            }
        }
    }
}

/// The outcome of one rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssertResult {
    pub rule: AssertRule,
    pub passed: bool,
    /// The measured value, when there was one.
    pub measured: Option<f64>,
    /// The baseline value, when a baseline was supplied and had this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<f64>,
    /// Percentage change against the baseline. Positive means worse for an `AtMost` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureKind>,
    /// Warnings that do not fail the rule but qualify the number.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl AssertResult {
    /// A one-line human summary.
    pub fn summary(&self) -> String {
        let AssertRule {
            event,
            metric,
            comparison,
            limit,
        } = &self.rule;
        let unit = metric.unit();
        match (self.passed, self.measured) {
            (true, Some(m)) => {
                format!(
                    "PASS  {event} {} = {m:.3} {unit} ({} {limit:.3} {unit})",
                    metric.label(),
                    comparison.symbol()
                )
            }
            (false, Some(m)) => {
                format!(
                    "FAIL  {event} {} = {m:.3} {unit}, expected {} {limit:.3} {unit}",
                    metric.label(),
                    comparison.symbol()
                )
            }
            (_, None) => format!(
                "FAIL  {event} {}: {}",
                metric.label(),
                self.failure.map_or("unmeasurable", FailureKind::describe)
            ),
        }
    }
}

/// The whole run's outcome.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AssertReport {
    pub results: Vec<AssertResult>,
    pub passed: bool,
    /// Set when the capture had integrity problems the caller chose to tolerate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capture_warnings: Vec<String>,
}

impl AssertReport {
    pub fn failures(&self) -> impl Iterator<Item = &AssertResult> {
        self.results.iter().filter(|r| !r.passed)
    }

    pub fn failure_count(&self) -> usize {
        self.results.iter().filter(|r| !r.passed).count()
    }
}

/// Evaluate every rule against the measured statistics.
pub fn evaluate_assertions(
    stats: &[EventStats],
    baseline: Option<&Baseline>,
    rules: &[AssertRule],
) -> AssertReport {
    let mut results = Vec::with_capacity(rules.len());

    for rule in rules {
        let found = stats.iter().find(|s| s.name == rule.event);

        let Some(s) = found else {
            // A rule naming an event the capture never declared has to fail. Passing would
            // mean a typo in a budget silently disables the gate it was meant to be.
            results.push(AssertResult {
                rule: rule.clone(),
                passed: false,
                measured: None,
                baseline: None,
                change_pct: None,
                failure: Some(FailureKind::UnknownEvent),
                warnings: Vec::new(),
            });
            continue;
        };

        let mut warnings = Vec::new();
        if s.unterminated > 0 {
            warnings.push(format!(
                "{} occurrence(s) had no matching stop event",
                s.unterminated
            ));
        }
        if s.orphaned_stops > 0 {
            warnings.push(format!(
                "{} stop event(s) had no matching start",
                s.orphaned_stops
            ));
        }
        if s.unsampled > 0 {
            warnings.push(format!(
                "{} occurrence(s) were too short to sample at this rate",
                s.unsampled
            ));
        }

        let measured = rule.metric.read(s);
        let failure = match measured {
            None if s.occurrences == 0 => Some(FailureKind::NeverOccurred),
            None => Some(FailureKind::Unmeasurable),
            Some(_) => None,
        };

        let baseline_value = baseline
            .and_then(|b| b.get(&rule.event))
            .and_then(|b| rule.metric.read(b));
        let change_pct = match (measured, baseline_value) {
            (Some(m), Some(b)) if b != 0.0 => Some((m - b) / b * 100.0),
            _ => None,
        };

        let passed = match measured {
            Some(m) => rule.comparison.holds(m, rule.limit),
            None => false,
        };

        results.push(AssertResult {
            rule: rule.clone(),
            passed,
            measured,
            baseline: baseline_value,
            change_pct,
            failure: failure.or((!passed).then_some(FailureKind::OverBudget)),
            warnings,
        });
    }

    let passed = results.iter().all(|r| r.passed);
    AssertReport {
        results,
        passed,
        capture_warnings: Vec::new(),
    }
}

/// Compare every event against a baseline, failing anything that regressed by more than
/// `tolerance_pct`.
///
/// Used by `--baseline base.json --tolerance 5%`, which catches regressions in events nobody
/// thought to write an explicit budget for.
pub fn regression_rules(
    stats: &[EventStats],
    baseline: &Baseline,
    metric: Metric,
    tolerance_pct: f64,
) -> Vec<AssertRule> {
    let mut rules = Vec::new();
    for s in stats {
        let Some(b) = baseline.get(&s.name) else {
            continue;
        };
        let Some(b_value) = metric.read(b) else {
            continue;
        };
        rules.push(AssertRule::at_most(
            s.name.clone(),
            metric,
            b_value * (1.0 + tolerance_pct / 100.0),
        ));
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Distribution;

    fn event(name: &str, mean_uj: f64, occurrences: u64) -> EventStats {
        let mut values = vec![mean_uj; occurrences.max(1) as usize];
        EventStats {
            name: name.to_string(),
            start_id: 0x0101,
            stop_id: Some(0x0102),
            occurrences,
            energy_uj: if occurrences == 0 {
                Distribution::default()
            } else {
                Distribution::of(&mut values)
            },
            duration_us: Distribution::of(&mut [950.0]),
            current_mean_ua: 24_800.0,
            current_peak_ua: 71_200,
            total_energy_uj: mean_uj * occurrences as f64,
            ..Default::default()
        }
    }

    /// The documented example, both halves.
    #[test]
    fn the_documented_budget_passes_at_245_and_fails_at_287() {
        let rule = AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 260.0);

        let good = evaluate_assertions(
            &[event("BLE_TX", 245.0, 500)],
            None,
            std::slice::from_ref(&rule),
        );
        assert!(good.passed);
        assert_eq!(good.failure_count(), 0);

        let bad = evaluate_assertions(
            &[event("BLE_TX", 287.0, 500)],
            None,
            std::slice::from_ref(&rule),
        );
        assert!(!bad.passed);
        assert_eq!(bad.failure_count(), 1);
        let f = bad.failures().next().unwrap();
        assert_eq!(f.failure, Some(FailureKind::OverBudget));
        assert_eq!(f.measured, Some(287.0));
        assert!(f.summary().contains("287"), "{}", f.summary());
        assert!(f.summary().contains("260"), "{}", f.summary());
    }

    /// The failure mode that matters most: a gate that passes because the thing it was
    /// checking disappeared.
    #[test]
    fn a_rule_naming_an_event_the_capture_never_declared_fails() {
        let report = evaluate_assertions(
            &[event("BLE_TX", 245.0, 500)],
            None,
            &[AssertRule::at_most("BLE_RX", Metric::MeanEnergy, 260.0)],
        );
        assert!(
            !report.passed,
            "a typo in a budget must not silently disable the gate"
        );
        assert_eq!(report.results[0].failure, Some(FailureKind::UnknownEvent));
        assert!(report.results[0].summary().contains("not declared"));
    }

    #[test]
    fn an_event_that_never_occurred_fails_rather_than_passing_vacuously() {
        let report = evaluate_assertions(
            &[event("BLE_TX", 0.0, 0)],
            None,
            &[AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 260.0)],
        );
        assert!(!report.passed);
        assert_eq!(report.results[0].failure, Some(FailureKind::NeverOccurred));
        assert_eq!(report.results[0].measured, None);
    }

    #[test]
    fn an_unmeasurable_event_fails_and_says_why() {
        let mut s = event("BLE_TX", 0.0, 5);
        s.energy_uj = Distribution::default();
        s.unsampled = 5;
        let report = evaluate_assertions(
            &[s],
            None,
            &[AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 260.0)],
        );
        assert!(!report.passed);
        assert_eq!(report.results[0].failure, Some(FailureKind::Unmeasurable));
        assert!(
            !report.results[0].warnings.is_empty(),
            "the reason must be surfaced"
        );
    }

    #[test]
    fn a_minimum_occurrence_count_catches_instrumentation_that_stopped_firing() {
        let rule = AssertRule::at_least("BLE_TX", Metric::Occurrences, 100.0);
        assert!(
            evaluate_assertions(
                &[event("BLE_TX", 245.0, 500)],
                None,
                std::slice::from_ref(&rule)
            )
            .passed
        );
        assert!(
            !evaluate_assertions(
                &[event("BLE_TX", 245.0, 3)],
                None,
                std::slice::from_ref(&rule)
            )
            .passed
        );
    }

    #[test]
    fn a_baseline_adds_the_percentage_change() {
        let baseline = Baseline::from_stats(&[event("BLE_TX", 282.0, 500)], Some("v1.2".into()));
        let report = evaluate_assertions(
            &[event("BLE_TX", 241.0, 500)],
            Some(&baseline),
            &[AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 300.0)],
        );
        assert!(report.passed);
        let r = &report.results[0];
        assert_eq!(r.baseline, Some(282.0));
        // The documented 14.5% improvement.
        let change = r.change_pct.unwrap();
        assert!(
            (change + 14.5).abs() < 0.1,
            "expected about -14.5%, got {change:.2}%"
        );
    }

    /// Regression rules catch events nobody wrote an explicit budget for.
    #[test]
    fn tolerance_rules_catch_a_regression_in_an_unbudgeted_event() {
        let baseline = Baseline::from_stats(
            &[
                event("BLE_TX", 245.0, 500),
                event("SENSOR_READ", 100.0, 500),
            ],
            None,
        );
        let now = vec![
            event("BLE_TX", 250.0, 500),
            event("SENSOR_READ", 130.0, 500),
        ];

        let rules = regression_rules(&now, &baseline, Metric::MeanEnergy, 5.0);
        assert_eq!(rules.len(), 2);
        let report = evaluate_assertions(&now, Some(&baseline), &rules);

        assert!(
            !report.passed,
            "a 30% regression should fail a 5% tolerance"
        );
        let failed: Vec<&str> = report.failures().map(|r| r.rule.event.as_str()).collect();
        assert_eq!(failed, vec!["SENSOR_READ"]);
        // BLE_TX rose by 2%, inside tolerance.
        assert!(
            report
                .results
                .iter()
                .find(|r| r.rule.event == "BLE_TX")
                .unwrap()
                .passed
        );
    }

    #[test]
    fn every_metric_reads_from_a_measured_event() {
        let s = event("BLE_TX", 245.0, 500);
        for m in [
            Metric::MeanEnergy,
            Metric::P95Energy,
            Metric::MaxEnergy,
            Metric::TotalEnergy,
            Metric::MeanCurrent,
            Metric::PeakCurrent,
            Metric::MeanDuration,
            Metric::P95Duration,
            Metric::Occurrences,
        ] {
            assert!(m.read(&s).is_some(), "{} could not be read", m.label());
            assert!(!m.label().is_empty());
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let report = evaluate_assertions(
            &[event("BLE_TX", 287.0, 500)],
            None,
            &[AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 260.0)],
        );
        let json = serde_json::to_string(&report).unwrap();
        let back: AssertReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn several_rules_are_all_reported_not_just_the_first_failure() {
        let stats = vec![
            event("BLE_TX", 287.0, 500),
            event("SENSOR_READ", 500.0, 500),
        ];
        let rules = vec![
            AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 260.0),
            AssertRule::at_most("SENSOR_READ", Metric::MeanEnergy, 120.0),
            AssertRule::at_most("BLE_TX", Metric::PeakCurrent, 100_000.0),
        ];
        let report = evaluate_assertions(&stats, None, &rules);
        assert_eq!(report.results.len(), 3);
        assert_eq!(
            report.failure_count(),
            2,
            "every failing budget must be listed"
        );
    }
}
