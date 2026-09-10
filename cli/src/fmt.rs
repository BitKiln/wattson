//! Terminal formatting.
//!
//! Formatting only. Anything that computes a number belongs in `wattson-core`, so the desktop
//! app can show the same figure without a second implementation.

use wattson_core::capture::Gap;
use wattson_core::stats::{Distribution, EventStats, RegionStats};
use wattson_core::units::{Charge, Current, Duration, Energy, Power, Voltage};

/// A current in microamps, rendered with a sensible prefix.
pub fn current(ua: f64) -> String {
    Current(ua / 1e6).to_string()
}

pub fn voltage(uv: f64) -> String {
    Voltage(uv / 1e6).to_string()
}

pub fn energy(uj: f64) -> String {
    Energy(uj / 1e6).to_string()
}

pub fn charge(uc: f64) -> String {
    Charge(uc / 1e6).to_string()
}

pub fn power(uw: f64) -> String {
    Power(uw / 1e6).to_string()
}

pub fn duration_us(us: f64) -> String {
    Duration(us / 1e6).to_string()
}

pub fn duration_s(s: f64) -> String {
    Duration(s).to_string()
}

/// Right-align a label and value into a two-column row.
pub fn row(label: &str, value: impl AsRef<str>) -> String {
    format!("  {label:<20} {}", value.as_ref())
}

/// Render the statistics a region selection reports.
pub fn region_block(stats: &RegionStats) -> String {
    let mut out = String::new();
    out.push_str(&row("Duration", duration_s(stats.duration_s)));
    out.push('\n');
    out.push_str(&row("Samples", stats.sample_count.to_string()));
    out.push('\n');
    out.push_str(&row("Min current", current(stats.current_min_ua as f64)));
    out.push('\n');
    out.push_str(&row("Max current", current(stats.current_max_ua as f64)));
    out.push('\n');
    out.push_str(&row("Avg current", current(stats.current_mean_ua)));
    out.push('\n');
    out.push_str(&row("RMS current", current(stats.current_rms_ua)));
    out.push('\n');
    out.push_str(&row("Avg voltage", voltage(stats.voltage_mean_uv)));
    out.push('\n');
    out.push_str(&row("Avg power", power(stats.power_mean_uw)));
    out.push('\n');
    out.push_str(&row("Peak power", power(stats.power_peak_uw)));
    out.push('\n');
    out.push_str(&row("Energy", energy(stats.energy_uj)));
    out.push('\n');
    out.push_str(&row(
        "Charge",
        format!("{} ({:.6} mAh)", charge(stats.charge_uc), stats.charge_mah),
    ));
    out
}

/// One line of a distribution: mean, P95, min and max.
fn dist_line(d: &Distribution, unit: impl Fn(f64) -> String) -> String {
    if d.is_empty() {
        return "not measurable".to_string();
    }
    format!(
        "{}  (p95 {}, min {}, max {})",
        unit(d.mean),
        unit(d.p95),
        unit(d.min),
        unit(d.max)
    )
}

/// Render one event's statistics, in the shape the project's documentation shows.
pub fn event_block(s: &EventStats) -> String {
    let mut out = String::new();
    out.push_str(&s.name);
    out.push('\n');
    out.push_str(&row("Occurrences", s.occurrences.to_string()));
    out.push('\n');
    out.push_str(&row("Duration", dist_line(&s.duration_us, duration_us)));
    out.push('\n');
    out.push_str(&row("Mean current", current(s.current_mean_ua)));
    out.push('\n');
    out.push_str(&row("Peak current", current(s.current_peak_ua as f64)));
    out.push('\n');
    out.push_str(&row("Energy", dist_line(&s.energy_uj, energy)));
    out.push('\n');
    out.push_str(&row("Total energy", energy(s.total_energy_uj)));
    out.push('\n');
    out.push_str(&row("Duty cycle", format!("{:.3}%", s.duty_cycle * 100.0)));

    // Warnings after the numbers, so they qualify what was just read rather than being
    // scrolled past before it.
    for w in event_warnings(s) {
        out.push('\n');
        out.push_str(&format!("  ! {w}"));
    }
    out
}

/// Everything that qualifies an event's numbers.
pub fn event_warnings(s: &EventStats) -> Vec<String> {
    let mut out = Vec::new();
    if s.unterminated > 0 {
        out.push(format!(
            "{} occurrence(s) had no matching stop event; the capture may have ended mid-scope",
            s.unterminated
        ));
    }
    if s.orphaned_stops > 0 {
        out.push(format!(
            "{} stop event(s) had no matching start; the capture may have begun mid-scope",
            s.orphaned_stops
        ));
    }
    if s.unsampled > 0 {
        out.push(format!(
            "{} occurrence(s) were shorter than the sample period, so their energy is unknown",
            s.unsampled
        ));
    }
    out
}

/// Render the gaps in a capture. An empty list prints nothing.
pub fn gap_block(gaps: &[Gap]) -> String {
    if gaps.is_empty() {
        return String::new();
    }
    let total_ns: u64 = gaps.iter().map(Gap::duration_ns).sum();
    let mut out = format!(
        "  ! {} gap(s) totalling {} of missing data\n",
        gaps.len(),
        duration_s(total_ns as f64 / 1e9)
    );
    for g in gaps.iter().take(5) {
        out.push_str(&format!(
            "      {} .. {}  {}\n",
            duration_s(g.start_ns as f64 / 1e9),
            duration_s(g.end_ns as f64 / 1e9),
            g.cause.describe()
        ));
    }
    if gaps.len() > 5 {
        out.push_str(&format!("      ... and {} more\n", gaps.len() - 5));
    }
    out
}

/// A simple ASCII sparkline of bucketed current, for `analyze` in a terminal.
///
/// Deliberately crude: it exists to show *where* the interesting parts are so someone knows
/// what to select, not to replace a plot.
pub fn sparkline(buckets: &[wattson_core::capture::Bucket]) -> String {
    const LEVELS: &[char] = &[
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    if buckets.is_empty() {
        return String::new();
    }
    let max = buckets
        .iter()
        .filter(|b| !b.is_empty())
        .map(|b| b.max_ua)
        .max()
        .unwrap_or(0);
    let min = buckets
        .iter()
        .filter(|b| !b.is_empty())
        .map(|b| b.min_ua)
        .min()
        .unwrap_or(0);
    let span = (max - min).max(1) as f64;

    buckets
        .iter()
        .map(|b| {
            if b.is_empty() {
                // A gap is drawn as a gap, never as a zero-current floor.
                ' '
            } else {
                let frac = ((b.max_ua - min) as f64 / span).clamp(0.0, 1.0);
                LEVELS[((frac * (LEVELS.len() - 1) as f64).round() as usize).min(LEVELS.len() - 1)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wattson_core::capture::{Bucket, GapCause};

    #[test]
    fn quantities_render_with_readable_prefixes() {
        assert_eq!(current(78_000.0), "78.000 mA");
        assert_eq!(energy(245.0), "245.000 \u{00B5}J");
        assert_eq!(voltage(3_300_000.0), "3.300 V");
        assert_eq!(duration_us(950.0), "950.000 \u{00B5}s");
        assert_eq!(duration_s(1.5), "1.500 s");
    }

    #[test]
    fn an_unmeasurable_distribution_says_so_rather_than_printing_zero() {
        let d = Distribution::default();
        assert_eq!(dist_line(&d, energy), "not measurable");
    }

    #[test]
    fn event_warnings_are_surfaced() {
        let s = EventStats {
            name: "BLE_TX".into(),
            occurrences: 10,
            unterminated: 1,
            orphaned_stops: 2,
            unsampled: 3,
            ..Default::default()
        };
        let warnings = event_warnings(&s);
        assert_eq!(warnings.len(), 3);
        let block = event_block(&s);
        assert!(block.contains("BLE_TX"));
        assert!(block.contains("no matching stop"), "{block}");
        assert!(block.contains("shorter than the sample period"), "{block}");
    }

    #[test]
    fn a_clean_event_has_no_warnings() {
        let s = EventStats {
            name: "BLE_TX".into(),
            occurrences: 10,
            ..Default::default()
        };
        assert!(event_warnings(&s).is_empty());
        assert!(!event_block(&s).contains('!'));
    }

    #[test]
    fn no_gaps_prints_nothing_at_all() {
        assert_eq!(gap_block(&[]), "");
    }

    #[test]
    fn gaps_are_listed_with_their_cause() {
        let gaps = vec![Gap {
            start_ns: 1_000_000,
            end_ns: 3_000_000,
            cause: GapCause::DeviceOverflow,
            lost_estimate: 100,
        }];
        let text = gap_block(&gaps);
        assert!(text.contains("1 gap"), "{text}");
        assert!(text.contains("device buffer overflow"), "{text}");
    }

    #[test]
    fn many_gaps_are_truncated_with_a_count() {
        let gaps: Vec<Gap> = (0..9)
            .map(|i| Gap {
                start_ns: i * 1000,
                end_ns: i * 1000 + 500,
                cause: GapCause::SequenceGap,
                lost_estimate: 1,
            })
            .collect();
        let text = gap_block(&gaps);
        assert!(text.contains("and 4 more"), "{text}");
    }

    /// Missing data must read as missing, not as a device that drew no current.
    #[test]
    fn a_sparkline_draws_an_empty_bucket_as_a_blank() {
        let buckets = vec![
            Bucket {
                t_ns: 0,
                min_ua: 3_000,
                max_ua: 3_000,
                mean_ua: 3_000.0,
                count: 10,
            },
            Bucket::empty(10),
            Bucket {
                t_ns: 20,
                min_ua: 78_000,
                max_ua: 78_000,
                mean_ua: 78_000.0,
                count: 10,
            },
        ];
        let line: Vec<char> = sparkline(&buckets).chars().collect();
        assert_eq!(line.len(), 3);
        assert_eq!(line[1], ' ', "an empty bucket must not render as a low bar");
        assert_ne!(line[0], line[2], "a 26x difference should be visible");
    }

    #[test]
    fn a_sparkline_of_nothing_is_empty() {
        assert_eq!(sparkline(&[]), "");
    }

    #[test]
    fn a_flat_trace_does_not_divide_by_zero() {
        let buckets: Vec<Bucket> = (0..5)
            .map(|i| Bucket {
                t_ns: i,
                min_ua: 3_000,
                max_ua: 3_000,
                mean_ua: 3_000.0,
                count: 1,
            })
            .collect();
        assert_eq!(sparkline(&buckets).chars().count(), 5);
    }
}
