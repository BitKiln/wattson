//! Deterministic randomness.
//!
//! Golden capture files are checked into the repository and compared byte for byte, so the
//! random stream must be **bit-stable across dependency updates**, not merely seeded.
//!
//! Two decisions follow:
//!
//! 1. `ChaCha8Rng` with an explicit seed, because its output for a given seed is a documented,
//!    stable property of the algorithm rather than an implementation detail.
//! 2. Gaussian samples by hand-rolled Box-Muller rather than `rand_distr::Normal`.
//!    `rand_distr`'s ziggurat implementation is not guaranteed stable across versions, and a
//!    golden `.pprof` that shifts when a patch release lands is a recurring, mystifying CI
//!    failure that costs far more than the twenty lines below.

use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

/// A deterministic source of the distributions the simulator needs.
#[derive(Debug, Clone)]
pub struct SimRng {
    inner: ChaCha8Rng,
    /// Box-Muller produces two independent normals per pair of uniforms; the spare is kept.
    spare_normal: Option<f64>,
    /// State for the 1/f (pink) noise generator.
    pink: [f64; PINK_OCTAVES],
    pink_counter: u64,
}

/// Octaves in the pink-noise generator. Six gives a decent 1/f slope over the audio-ish range
/// that matters here without costing much.
const PINK_OCTAVES: usize = 6;

impl SimRng {
    pub fn new(seed: u64) -> SimRng {
        SimRng {
            inner: ChaCha8Rng::seed_from_u64(seed),
            spare_normal: None,
            pink: [0.0; PINK_OCTAVES],
            pink_counter: 0,
        }
    }

    /// Uniform in `[0, 1)`, using 53 bits so the value is exactly representable.
    #[inline]
    pub fn uniform(&mut self) -> f64 {
        let bits = self.inner.next_u64() >> 11;
        bits as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in `[low, high)`.
    #[inline]
    pub fn uniform_range(&mut self, low: f64, high: f64) -> f64 {
        low + self.uniform() * (high - low)
    }

    /// Standard normal, mean 0 and standard deviation 1, by polar Box-Muller.
    pub fn normal(&mut self) -> f64 {
        if let Some(spare) = self.spare_normal.take() {
            return spare;
        }
        // Rejection-sample inside the unit circle; the polar form avoids trig entirely.
        loop {
            let u = self.uniform_range(-1.0, 1.0);
            let v = self.uniform_range(-1.0, 1.0);
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let factor = (-2.0 * s.ln() / s).sqrt();
                self.spare_normal = Some(v * factor);
                return u * factor;
            }
        }
    }

    /// Normal with the given mean and standard deviation.
    #[inline]
    pub fn normal_with(&mut self, mean: f64, sigma: f64) -> f64 {
        if sigma <= 0.0 {
            mean
        } else {
            mean + sigma * self.normal()
        }
    }

    /// `true` with probability `p`.
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        self.uniform() < p
    }

    /// A `1/f`-weighted noise sample with unit-ish scale, by the Voss-McCartney method.
    ///
    /// Real current measurements have low-frequency wander, not just white noise. Without it
    /// a long-window average looks unrealistically stable, and any future drift-correction or
    /// baseline-tracking code would never be exercised.
    pub fn pink(&mut self) -> f64 {
        self.pink_counter = self.pink_counter.wrapping_add(1);
        let counter = self.pink_counter;
        // Octave k is refreshed every 2^k samples.
        for (k, slot) in self.pink.iter_mut().enumerate() {
            if counter % (1u64 << k) == 0 {
                *slot = 0.0;
            }
        }
        // Refresh exactly one octave per sample: the lowest set bit selects it.
        let k = (counter.trailing_zeros() as usize).min(PINK_OCTAVES - 1);
        let fresh = {
            let bits = self.inner.next_u64() >> 11;
            bits as f64 * (1.0 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        self.pink[k] = fresh;
        self.pink.iter().sum::<f64>() / (PINK_OCTAVES as f64).sqrt()
    }

    /// A raw `u64`, for seeding sub-generators.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: same seed, same stream, forever.
    #[test]
    fn the_same_seed_gives_the_same_stream() {
        let mut a = SimRng::new(42);
        let mut b = SimRng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut a = SimRng::new(42);
        let mut b = SimRng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.normal().to_bits(), b.normal().to_bits());
        }
    }

    #[test]
    fn different_seeds_give_different_streams() {
        let mut a = SimRng::new(1);
        let mut b = SimRng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn uniform_stays_in_range() {
        let mut r = SimRng::new(7);
        for _ in 0..10_000 {
            let u = r.uniform();
            assert!((0.0..1.0).contains(&u), "uniform produced {u}");
        }
        for _ in 0..1000 {
            let v = r.uniform_range(-3.0, 5.0);
            assert!((-3.0..5.0).contains(&v));
        }
    }

    #[test]
    fn normal_has_the_right_moments() {
        let mut r = SimRng::new(11);
        const N: usize = 200_000;
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        for _ in 0..N {
            let x = r.normal();
            sum += x;
            sum_sq += x * x;
        }
        let mean = sum / N as f64;
        let var = sum_sq / N as f64 - mean * mean;
        assert!(mean.abs() < 0.02, "mean was {mean}");
        assert!((var - 1.0).abs() < 0.03, "variance was {var}");
    }

    #[test]
    fn normal_with_scales_and_shifts() {
        let mut r = SimRng::new(3);
        const N: usize = 100_000;
        let mut sum = 0.0;
        for _ in 0..N {
            sum += r.normal_with(27_000.0, 400.0);
        }
        let mean = sum / N as f64;
        assert!((mean - 27_000.0).abs() < 20.0, "mean was {mean}");
        // A zero sigma must be exactly the mean, so a noiseless profile is truly noiseless.
        assert_eq!(r.normal_with(5.0, 0.0), 5.0);
    }

    #[test]
    fn chance_is_roughly_fair() {
        let mut r = SimRng::new(5);
        let hits = (0..100_000).filter(|_| r.chance(0.25)).count();
        assert!(
            (20_000..30_000).contains(&hits),
            "got {hits} hits out of 100000"
        );
    }

    #[test]
    fn pink_noise_is_bounded_and_wanders() {
        let mut r = SimRng::new(9);
        let mut values = Vec::with_capacity(4096);
        for _ in 0..4096 {
            let p = r.pink();
            assert!(p.abs() < 10.0, "pink noise blew up: {p}");
            values.push(p);
        }
        // Low-frequency wander means adjacent samples correlate more than distant ones.
        let adjacent: f64 =
            values.windows(2).map(|w| (w[0] - w[1]).abs()).sum::<f64>() / (values.len() - 1) as f64;
        let distant: f64 = values
            .windows(512)
            .map(|w| (w[0] - w[w.len() - 1]).abs())
            .sum::<f64>()
            / values.len().saturating_sub(512).max(1) as f64;
        assert!(
            adjacent < distant,
            "pink noise should wander: {adjacent} vs {distant}"
        );
    }
}
