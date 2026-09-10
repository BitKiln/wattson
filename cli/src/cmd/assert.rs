//! `wattson assert` — power budgets as a build gate.
//!
//! Exits 1 when a budget is exceeded, which is what makes power consumption a testable
//! firmware metric rather than a number someone looks at occasionally.
//!
//! The gap policy defaults to `error` here, and only here. Everywhere else the tool shows what
//! it can; a CI gate that passes because samples went missing is worse than no gate.

use std::path::PathBuf;

use anyhow::{Result, bail};
use wattson_core::assert::{
    AssertReport, AssertRule, Baseline, Metric, evaluate_assertions, regression_rules,
};
use wattson_core::export::write_json;
use wattson_core::stats::event_stats;
use wattson_core::units::{Current, Duration as UnitDuration, Energy, Ratio};

use crate::exit;

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ReportFormat {
    Human,
    Json,
    /// JUnit XML, so a CI server shows the failure where it shows test failures.
    Junit,
}

#[derive(clap::Args, Debug)]
pub struct Args {
    /// The capture to check.
    pub capture: PathBuf,

    /// Event to check. Repeat to check several.
    #[arg(long)]
    pub event: Vec<String>,

    /// Fail if mean energy per occurrence exceeds this, e.g. 260uJ.
    #[arg(long)]
    pub max_energy: Option<String>,

    /// Fail if P95 energy per occurrence exceeds this.
    #[arg(long)]
    pub max_p95_energy: Option<String>,

    /// Fail if total energy across all occurrences exceeds this.
    #[arg(long)]
    pub max_total_energy: Option<String>,

    /// Fail if mean current during the event exceeds this, e.g. 30mA.
    #[arg(long)]
    pub max_mean_current: Option<String>,

    /// Fail if peak current during the event exceeds this.
    #[arg(long)]
    pub max_peak_current: Option<String>,

    /// Fail if mean duration exceeds this, e.g. 1.2ms.
    #[arg(long)]
    pub max_duration: Option<String>,

    /// Fail if the event occurred fewer than this many times.
    #[arg(long)]
    pub min_occurrences: Option<u64>,

    /// Compare against a baseline written by a previous run.
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Allowed regression against the baseline, e.g. 5%.
    #[arg(long)]
    pub tolerance: Option<String>,

    /// Write this run's numbers as a baseline for future comparisons.
    #[arg(long)]
    pub write_baseline: Option<PathBuf>,

    /// Write the full report here.
    #[arg(long)]
    pub report: Option<PathBuf>,

    /// How to print the report.
    #[arg(long, value_enum, default_value_t = ReportFormat::Human)]
    pub format: ReportFormat,

    /// What to do when the analysed span crosses missing data.
    #[arg(long, value_enum, default_value_t = super::GapPolicyArg::Error)]
    pub on_gap: super::GapPolicyArg,
}

pub fn run(args: &Args) -> Result<i32> {
    let mut reader = super::open_capture(&args.capture)?;
    let options = super::stats_options(args.on_gap, None);

    let map = reader.metadata().event_map();
    if map.is_empty() {
        // Asserting against a capture with no declared events would check nothing and pass,
        // which is the exact shape of a gate that has quietly stopped working.
        bail!(
            "{} declares no firmware events, so there is nothing to assert on. \
             Capture with --metadata events.toml.",
            args.capture.display()
        );
    }
    let stats = event_stats(&mut reader, &map, &options)?;

    let baseline = match &args.baseline {
        Some(p) => {
            let text = std::fs::read_to_string(p)?;
            Some(serde_json::from_str::<Baseline>(&text)?)
        }
        None => None,
    };

    let rules = build_rules(args, &stats, baseline.as_ref())?;
    if rules.is_empty() {
        bail!(
            "no budgets given: pass at least one limit (for example --event BLE_TX \
             --max-energy 260uJ) or --baseline with --tolerance"
        );
    }

    let mut report = evaluate_assertions(&stats, baseline.as_ref(), &rules);

    // The capture's own problems qualify every number in the report, so they travel with it.
    let integrity = reader.integrity();
    if integrity.recovered {
        report
            .capture_warnings
            .push("the capture was never finalized and its index was rebuilt by scanning".into());
    }
    if !integrity.gaps.is_empty() {
        report.capture_warnings.push(format!(
            "{} gap(s) of missing data were present",
            integrity.gaps.len()
        ));
    }

    if let Some(p) = &args.write_baseline {
        let baseline = Baseline::from_stats(&stats, Some(args.capture.display().to_string()));
        write_json(&baseline, std::fs::File::create(p)?)?;
    }
    if let Some(p) = &args.report {
        write_json(&report, std::fs::File::create(p)?)?;
    }

    match args.format {
        ReportFormat::Json => write_json(&report, std::io::stdout().lock())?,
        ReportFormat::Junit => print!("{}", junit(&report)),
        ReportFormat::Human => print_human(&report),
    }

    Ok(if report.passed {
        exit::SUCCESS
    } else {
        exit::ASSERTION_FAILED
    })
}

/// Turn the command-line limits into rules.
fn build_rules(
    args: &Args,
    stats: &[wattson_core::stats::EventStats],
    baseline: Option<&Baseline>,
) -> Result<Vec<AssertRule>> {
    let mut rules = Vec::new();

    // Limits apply to every named event, so one invocation can gate several at once.
    let events: Vec<String> = if args.event.is_empty() {
        stats.iter().map(|s| s.name.clone()).collect()
    } else {
        args.event.clone()
    };

    let push = |metric: Metric, value: Option<f64>, rules: &mut Vec<AssertRule>| {
        if let Some(v) = value {
            for e in &events {
                rules.push(AssertRule::at_most(e.clone(), metric, v));
            }
        }
    };

    let energy = |s: &Option<String>| -> Result<Option<f64>> {
        Ok(s.as_deref()
            .map(str::parse::<Energy>)
            .transpose()?
            .map(|e| e.0 * 1e6))
    };
    let current = |s: &Option<String>| -> Result<Option<f64>> {
        Ok(s.as_deref()
            .map(str::parse::<Current>)
            .transpose()?
            .map(|c| c.0 * 1e6))
    };

    push(Metric::MeanEnergy, energy(&args.max_energy)?, &mut rules);
    push(Metric::P95Energy, energy(&args.max_p95_energy)?, &mut rules);
    push(
        Metric::TotalEnergy,
        energy(&args.max_total_energy)?,
        &mut rules,
    );
    push(
        Metric::MeanCurrent,
        current(&args.max_mean_current)?,
        &mut rules,
    );
    push(
        Metric::PeakCurrent,
        current(&args.max_peak_current)?,
        &mut rules,
    );
    push(
        Metric::MeanDuration,
        args.max_duration
            .as_deref()
            .map(str::parse::<UnitDuration>)
            .transpose()?
            .map(|d| d.0 * 1e6),
        &mut rules,
    );

    if let Some(n) = args.min_occurrences {
        for e in &events {
            rules.push(AssertRule::at_least(
                e.clone(),
                Metric::Occurrences,
                n as f64,
            ));
        }
    }

    // A tolerance against a baseline catches regressions in events nobody wrote an explicit
    // budget for, which is most of them.
    if let Some(t) = &args.tolerance {
        let ratio: Ratio = t.parse()?;
        let Some(b) = baseline else {
            bail!("--tolerance needs --baseline to compare against");
        };
        rules.extend(regression_rules(
            stats,
            b,
            Metric::MeanEnergy,
            ratio.as_percent(),
        ));
    }

    Ok(rules)
}

fn print_human(report: &AssertReport) {
    for r in &report.results {
        println!("{}", r.summary());
        for w in &r.warnings {
            println!("        ! {w}");
        }
    }

    if report.passed {
        if !report.capture_warnings.is_empty() {
            println!();
            for w in &report.capture_warnings {
                println!("! {w}");
            }
        }
        return;
    }

    // The failure block is written to be readable in a CI log, where it will be surrounded by
    // thousands of other lines.
    println!();
    println!("POWER REGRESSION");
    for r in report.failures() {
        println!();
        let unit = r.rule.metric.unit();
        println!("{} {}", r.rule.event, r.rule.metric.label());
        match r.measured {
            Some(m) => {
                println!(
                    "  Expected: {} {:.3} {unit}",
                    r.rule.comparison.symbol(),
                    r.rule.limit
                );
                println!("  Measured: {m:.3} {unit}");
                if let Some(change) = r.change_pct {
                    let word = if change >= 0.0 {
                        "Regression"
                    } else {
                        "Improvement"
                    };
                    println!("  {word}: {change:+.1}% against the baseline");
                } else {
                    let over = (m - r.rule.limit) / r.rule.limit * 100.0;
                    if r.rule.comparison == wattson_core::assert::Comparison::AtMost {
                        println!("  Over budget by {over:+.1}%");
                    }
                }
            }
            None => {
                println!(
                    "  Not measured: {}",
                    r.failure.map_or("unknown reason", |f| f.describe())
                );
            }
        }
        for w in &r.warnings {
            println!("  ! {w}");
        }
    }

    if !report.capture_warnings.is_empty() {
        println!();
        for w in &report.capture_warnings {
            println!("! {w}");
        }
    }
}

/// JUnit XML, so a CI server shows a power regression where it shows test failures.
fn junit(report: &AssertReport) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    let failures = report.failure_count();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuite name=\"wattson power budgets\" tests=\"{}\" failures=\"{failures}\">\n",
        report.results.len()
    ));
    for r in &report.results {
        let name = format!("{} {}", r.rule.event, r.rule.metric.label());
        out.push_str(&format!(
            "  <testcase classname=\"wattson\" name=\"{}\"",
            esc(&name)
        ));
        if r.passed {
            out.push_str(" />\n");
        } else {
            out.push_str(">\n");
            out.push_str(&format!(
                "    <failure message=\"{}\" />\n",
                esc(&r.summary())
            ));
            out.push_str("  </testcase>\n");
        }
    }
    out.push_str("</testsuite>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wattson_core::assert::{AssertResult, Comparison, FailureKind};

    fn failing_report() -> AssertReport {
        AssertReport {
            results: vec![AssertResult {
                rule: AssertRule::at_most("BLE_TX", Metric::MeanEnergy, 260.0),
                passed: false,
                measured: Some(287.0),
                baseline: None,
                change_pct: None,
                failure: Some(FailureKind::OverBudget),
                warnings: vec![],
            }],
            passed: false,
            capture_warnings: vec![],
        }
    }

    #[test]
    fn junit_reports_the_failure_count_and_escapes_its_text() {
        let xml = junit(&failing_report());
        assert!(xml.contains("failures=\"1\""), "{xml}");
        assert!(xml.contains("<failure"), "{xml}");
        assert!(xml.contains("BLE_TX"), "{xml}");
        assert!(
            !xml.contains("<=\""),
            "the comparison symbol must be escaped: {xml}"
        );
        assert!(xml.contains("&lt;="), "{xml}");
    }

    #[test]
    fn junit_marks_a_passing_case_without_a_failure_element() {
        let mut report = failing_report();
        report.results[0].passed = true;
        report.results[0].failure = None;
        report.passed = true;
        let xml = junit(&report);
        assert!(xml.contains("failures=\"0\""), "{xml}");
        assert!(!xml.contains("<failure"), "{xml}");
    }

    #[test]
    fn the_comparison_symbol_survives_into_the_summary() {
        let r = &failing_report().results[0];
        assert_eq!(r.rule.comparison, Comparison::AtMost);
        assert!(r.summary().contains("<="), "{}", r.summary());
    }
}
