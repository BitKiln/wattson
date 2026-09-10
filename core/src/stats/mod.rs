//! Statistics over a capture.
//!
//! # Two numerical rules, both easy to get silently wrong
//!
//! **1. Compensated summation.** At 50 ksps for an hour, an energy integral accumulates 180
//! million terms of magnitude around 1e-6. A naive `f64` sum loses roughly seven significant
//! digits by the end — the answer still looks like an energy, it is just wrong. Every
//! accumulator here is a [`KahanSum`].
//!
//! **2. Integrate over real timestamps, never `count * nominal_period`.** Dropped blocks and
//! clock jitter are real. Multiplying a sample count by the configured period silently
//! under-reports energy across every hiccup, and the result is a plausible number, which is
//! worse than an error.
//!
//! # Gaps
//!
//! A span that crosses missing data cannot be integrated honestly. The policy is the caller's
//! to choose, but the *default* refuses: a CI gate that passes because samples went missing
//! is the worst outcome this project has.

pub mod dist;
pub mod event;
pub mod region;

pub use dist::Distribution;
pub use event::{EventOccurrence, EventStats, event_stats, occurrences_of};
pub use region::{Integration, RegionStats, StatsOptions, region_stats};

/// Neumaier compensated summation.
///
/// The improvement over plain Kahan matters here: sample-to-sample current differences can be
/// much larger than the running total early in a capture, which is exactly the case plain
/// Kahan handles poorly.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct KahanSum {
    sum: f64,
    compensation: f64,
}

impl KahanSum {
    pub const fn new() -> KahanSum {
        KahanSum {
            sum: 0.0,
            compensation: 0.0,
        }
    }

    #[inline]
    pub fn add(&mut self, value: f64) {
        let t = self.sum + value;
        if self.sum.abs() >= value.abs() {
            self.compensation += (self.sum - t) + value;
        } else {
            self.compensation += (value - t) + self.sum;
        }
        self.sum = t;
    }

    #[inline]
    pub fn total(&self) -> f64 {
        self.sum + self.compensation
    }
}

impl std::iter::Sum<f64> for KahanSum {
    fn sum<I: Iterator<Item = f64>>(iter: I) -> KahanSum {
        let mut k = KahanSum::new();
        for v in iter {
            k.add(v);
        }
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compensated_summation_beats_a_naive_sum_at_capture_scale() {
        // Two hours at 50 ksps is 360 million terms; 20 million is enough to show the
        // divergence without making the test slow.
        const N: usize = 20_000_000;
        let term = 1.0e-7_f64;
        let expected = N as f64 * term;

        let mut naive = 1.0e7_f64; // a large running total, as a long capture accumulates
        let mut kahan = KahanSum::new();
        kahan.add(1.0e7);
        for _ in 0..N {
            naive += term;
            kahan.add(term);
        }

        let naive_err = ((naive - 1.0e7) - expected).abs() / expected;
        let kahan_err = ((kahan.total() - 1.0e7) - expected).abs() / expected;

        assert!(
            kahan_err < naive_err / 100.0,
            "compensation bought nothing: naive {naive_err:e}, compensated {kahan_err:e}"
        );
        assert!(
            kahan_err < 1e-9,
            "compensated error {kahan_err:e} is too large"
        );
    }

    #[test]
    fn kahan_matches_exact_arithmetic_on_easy_input() {
        let mut k = KahanSum::new();
        for i in 1..=1000 {
            k.add(i as f64);
        }
        assert_eq!(k.total(), 500_500.0);
    }

    #[test]
    fn kahan_handles_a_small_running_total_and_large_terms() {
        // The case plain Kahan gets wrong and Neumaier does not.
        let mut k = KahanSum::new();
        k.add(1.0);
        k.add(1e100);
        k.add(1.0);
        k.add(-1e100);
        assert_eq!(k.total(), 2.0);
    }

    #[test]
    fn sums_through_the_iterator_adaptor() {
        let k: KahanSum = (0..100).map(|i| i as f64).sum();
        assert_eq!(k.total(), 4950.0);
    }
}
