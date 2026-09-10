//! Summary statistics over a set of measurements.
//!
//! Percentiles are computed exactly, by selection, rather than estimated. For 500 occurrences
//! of a firmware event a t-digest would buy nothing, and "P95 energy" is a number people
//! write into budgets — an approximate one invites arguments that a sort settles.

use serde::{Deserialize, Serialize};

/// Distribution of a set of values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    pub n: u64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    /// Sample standard deviation, with Bessel's correction. Zero when `n < 2`.
    pub stddev: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    /// Sum of all values, which for energy is the total cost of every occurrence.
    pub total: f64,
}

impl Distribution {
    /// Compute over `values`, which is reordered in place.
    ///
    /// Takes `&mut` rather than cloning because event statistics build one of these per event
    /// over potentially many thousands of occurrences.
    pub fn of(values: &mut [f64]) -> Distribution {
        if values.is_empty() {
            return Distribution::default();
        }
        let n = values.len();

        let mut sum = super::KahanSum::new();
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for &v in values.iter() {
            sum.add(v);
            min = min.min(v);
            max = max.max(v);
        }
        let total = sum.total();
        let mean = total / n as f64;

        let stddev = if n < 2 {
            0.0
        } else {
            let mut ss = super::KahanSum::new();
            for &v in values.iter() {
                let d = v - mean;
                ss.add(d * d);
            }
            (ss.total() / (n - 1) as f64).sqrt()
        };

        Distribution {
            n: n as u64,
            mean,
            min,
            max,
            stddev,
            p50: percentile(values, 0.50),
            p95: percentile(values, 0.95),
            p99: percentile(values, 0.99),
            total,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The spread as a fraction of the mean. A useful one-number answer to "is this event
    /// consistent?", and undefined for a zero mean.
    pub fn coefficient_of_variation(&self) -> Option<f64> {
        (self.mean != 0.0).then(|| self.stddev / self.mean.abs())
    }
}

/// Exact percentile by selection, using the nearest-rank method.
///
/// `values` is partially reordered. `q` is clamped to `[0, 1]`.
fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let n = values.len();
    let q = q.clamp(0.0, 1.0);
    // Nearest-rank: the smallest value at or above which at least q of the data lies.
    let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
    let idx = rank - 1;
    let (_, nth, _) = values.select_nth_unstable_by(idx, |a, b| {
        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
    });
    *nth
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn computes_the_obvious_things() {
        let mut v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let d = Distribution::of(&mut v);
        assert_eq!(d.n, 100);
        assert_relative_eq!(d.mean, 50.5);
        assert_relative_eq!(d.min, 1.0);
        assert_relative_eq!(d.max, 100.0);
        assert_relative_eq!(d.total, 5050.0);
        assert_relative_eq!(d.p50, 50.0);
        assert_relative_eq!(d.p95, 95.0);
        assert_relative_eq!(d.p99, 99.0);
    }

    #[test]
    fn percentiles_are_exact_not_estimated() {
        // 500 occurrences, as the documented BLE_TX example has.
        let mut v: Vec<f64> = (0..500).map(|i| i as f64).collect();
        let d = Distribution::of(&mut v);
        // Nearest rank: ceil(0.95 * 500) = 475, so the 475th smallest, which is value 474.
        assert_relative_eq!(d.p95, 474.0);
        assert_relative_eq!(d.p99, 494.0);
    }

    /// A tail matters more than a mean for a power budget: one occurrence in twenty costing
    /// double is exactly what P95 exists to surface.
    #[test]
    fn a_heavy_tail_shows_up_in_p95_but_barely_in_the_mean() {
        let mut v = vec![245.0; 95];
        v.extend(std::iter::repeat_n(500.0, 5));
        let d = Distribution::of(&mut v);
        assert_relative_eq!(d.mean, 257.75);
        assert_relative_eq!(d.p50, 245.0);
        assert_relative_eq!(d.p95, 245.0);
        assert_relative_eq!(d.p99, 500.0);
        assert_eq!(d.max, 500.0);
    }

    #[test]
    fn standard_deviation_uses_bessels_correction() {
        let mut v = vec![2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let d = Distribution::of(&mut v);
        assert_relative_eq!(d.mean, 5.0);
        // Population sd is 2.0; sample sd is sqrt(32/7).
        assert_relative_eq!(d.stddev, (32.0f64 / 7.0).sqrt(), max_relative = 1e-12);
    }

    #[test]
    fn a_single_value_has_no_spread() {
        let mut v = vec![42.0];
        let d = Distribution::of(&mut v);
        assert_eq!(d.n, 1);
        assert_relative_eq!(d.mean, 42.0);
        assert_eq!(d.stddev, 0.0);
        assert_relative_eq!(d.p50, 42.0);
        assert_relative_eq!(d.p99, 42.0);
    }

    #[test]
    fn an_empty_set_is_empty_rather_than_a_panic() {
        let d = Distribution::of(&mut []);
        assert!(d.is_empty());
        assert_eq!(d.n, 0);
        assert!(d.coefficient_of_variation().is_none());
    }

    #[test]
    fn input_order_does_not_change_the_answer() {
        let base: Vec<f64> = (0..1000).map(|i| ((i * 37) % 991) as f64).collect();
        let mut a = base.clone();
        let mut b: Vec<f64> = base.iter().rev().copied().collect();
        assert_eq!(Distribution::of(&mut a), Distribution::of(&mut b));
    }

    #[test]
    fn coefficient_of_variation_measures_consistency() {
        let mut steady = vec![100.0; 50];
        assert_relative_eq!(
            Distribution::of(&mut steady)
                .coefficient_of_variation()
                .unwrap(),
            0.0
        );

        let mut jittery: Vec<f64> = (0..50).map(|i| 100.0 + (i % 10) as f64 * 10.0).collect();
        assert!(
            Distribution::of(&mut jittery)
                .coefficient_of_variation()
                .unwrap()
                > 0.1
        );
    }

    #[test]
    fn negative_values_are_handled() {
        let mut v = vec![-5.0, -1.0, 0.0, 1.0, 5.0];
        let d = Distribution::of(&mut v);
        assert_relative_eq!(d.mean, 0.0);
        assert_relative_eq!(d.min, -5.0);
        assert_relative_eq!(d.max, 5.0);
        assert_relative_eq!(d.p50, 0.0);
    }
}
