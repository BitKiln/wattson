//! The synthetic signal generator.
//!
//! Every "realism" feature here exists because omitting it would leave a real bug untested:
//!
//! | Feature | What it would otherwise hide |
//! |---|---|
//! | Exponential ramps on state edges | rectangular vs trapezoidal integration are identical on square waves |
//! | Voltage droop under load (`V = Vs - I·ESR`) | every bug in the voltage path; energy is `V·I·dt`, not `I·dt` |
//! | ADC quantisation | off-by-one-LSB handling and unrealistically smooth statistics |
//! | Gaussian + 1/f noise | baseline tracking, and averages that look impossibly stable |
//! | Duration jitter | P95 equals the mean, so the percentile code is never exercised |
//! | Fault injection | CRC rejection, COBS resync, and gap accounting are never reached |

use crate::profiles::{PowerState, Profile};
use crate::rng::SimRng;
use wattson_protocol::{EventId, EventRecord, Sample};

/// Fault injection settings, for exercising the host's recovery paths.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FaultInjection {
    /// Probability that any given sample block is dropped, creating a sequence gap.
    pub drop_block_prob: f64,
    /// Probability that a byte in an emitted frame is corrupted, so the CRC rejects it.
    pub corrupt_byte_prob: f64,
    /// Probability of emitting an ERROR frame reporting dropped samples.
    pub error_frame_prob: f64,
}

impl FaultInjection {
    /// A moderate fault rate: frequent enough to hit every recovery path in a few seconds of
    /// simulated capture, rare enough that the capture is still analysable.
    pub const NOISY: FaultInjection = FaultInjection {
        drop_block_prob: 0.002,
        corrupt_byte_prob: 0.001,
        error_frame_prob: 0.0005,
    };

    pub const fn is_enabled(&self) -> bool {
        self.drop_block_prob > 0.0 || self.corrupt_byte_prob > 0.0 || self.error_frame_prob > 0.0
    }
}

/// How to generate a synthetic capture.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub profile: Profile,
    pub seed: u64,
    pub sample_rate_hz: u32,
    /// Device timer frequency; 1 MHz means one tick per microsecond.
    pub timer_hz: u32,
    /// ADC resolution in microamps. Values are quantised to a multiple of this.
    pub adc_lsb_ua: f64,
    /// Broadband noise floor in microamps RMS, on top of each state's own sigma.
    pub noise_ua_rms: f64,
    /// Low-frequency wander amplitude in microamps.
    pub pink_ua: f64,
    /// Sample-clock error in parts per million, so the device clock is not exactly nominal.
    pub clock_ppm: f64,
    pub faults: FaultInjection,
}

impl SimConfig {
    pub fn new(profile: Profile) -> SimConfig {
        SimConfig {
            profile,
            seed: 42,
            sample_rate_hz: 50_000,
            timer_hz: 1_000_000,
            adc_lsb_ua: 10.0,
            noise_ua_rms: 25.0,
            pink_ua: 40.0,
            clock_ppm: 18.0,
            faults: FaultInjection::default(),
        }
    }

    /// Device ticks between consecutive samples.
    pub fn period_ticks(&self) -> u32 {
        (self.timer_hz / self.sample_rate_hz.max(1)).max(1)
    }
}

/// One scheduled occurrence of a power state within the cycle.
#[derive(Clone, Debug)]
struct Occurrence {
    state: PowerState,
    start_us: f64,
    end_us: f64,
}

/// A batch of generated samples plus whatever events fall inside it.
#[derive(Clone, Debug, Default)]
pub struct SimBlock {
    /// Device tick of the first sample.
    pub t0_ticks: u32,
    /// Ticks between samples.
    pub period_ticks: u32,
    /// `(current_uA, voltage_uV)` pairs.
    pub samples: Vec<(i32, u32)>,
    /// Firmware events whose timestamps fall within this block.
    pub events: Vec<EventRecord>,
}

impl SimBlock {
    /// Expand to canonical [`Sample`]s, for tests that want the decoded form.
    pub fn to_samples(&self) -> Vec<Sample> {
        self.samples
            .iter()
            .enumerate()
            .map(|(i, &(current_ua, voltage_uv))| Sample {
                timestamp: self
                    .t0_ticks
                    .wrapping_add((i as u32).wrapping_mul(self.period_ticks)),
                current_ua,
                voltage_uv,
            })
            .collect()
    }
}

/// Generates the synthetic stream.
#[derive(Debug)]
pub struct SimEngine {
    config: SimConfig,
    rng: SimRng,
    /// Absolute sample index since the start of the run.
    sample_index: u64,
    /// The current cycle's schedule, regenerated (with fresh jitter) each cycle.
    schedule: Vec<Occurrence>,
    /// Which cycle the schedule belongs to.
    schedule_cycle: i64,
    /// Which occurrences have already had their start/stop events emitted, by cycle.
    emitted_cycle: i64,
    emitted: Vec<(bool, bool)>,
    /// Slewed current, so state edges ramp rather than step.
    slewed_ua: f64,
}

impl SimEngine {
    pub fn new(config: SimConfig) -> SimEngine {
        let rng = SimRng::new(config.seed);
        let idle = config.profile.idle.current_ua;
        SimEngine {
            config,
            rng,
            sample_index: 0,
            schedule: Vec::new(),
            schedule_cycle: -1,
            emitted_cycle: -1,
            emitted: Vec::new(),
            slewed_ua: idle,
        }
    }

    pub fn config(&self) -> &SimConfig {
        &self.config
    }

    /// Samples generated so far.
    pub fn sample_count(&self) -> u64 {
        self.sample_index
    }

    /// Simulated time elapsed, in microseconds.
    pub fn elapsed_us(&self) -> f64 {
        self.sample_index as f64 * self.sample_period_us()
    }

    /// Microseconds between samples, including the simulated clock error.
    fn sample_period_us(&self) -> f64 {
        let nominal = 1e6 / self.config.sample_rate_hz as f64;
        nominal * (1.0 + self.config.clock_ppm / 1e6)
    }

    /// Build one cycle's schedule, drawing fresh duration jitter for each state.
    fn build_schedule(&mut self, cycle: i64) {
        let profile = self.config.profile.clone();
        let cycle_start_us = cycle as f64 * profile.cycle_us;
        // Leave a margin of idle before the first active state so a cycle boundary never
        // lands mid-burst.
        let mut cursor = cycle_start_us + profile.cycle_us * 0.05;

        self.schedule.clear();
        for state in &profile.states {
            let duration = (state.duration_us + self.rng.normal_with(0.0, state.jitter_us))
                .max(state.duration_us * 0.2);
            self.schedule.push(Occurrence {
                state: state.clone(),
                start_us: cursor,
                end_us: cursor + duration,
            });
            // A gap of idle between active states, so their events never coincide.
            cursor += duration + profile.cycle_us * 0.02;
        }
        self.schedule_cycle = cycle;
        if self.emitted_cycle != cycle {
            self.emitted = vec![(false, false); self.schedule.len()];
            self.emitted_cycle = cycle;
        }
    }

    /// The target current at a given absolute time, ignoring noise and slew.
    fn target_current_ua(&self, t_us: f64) -> (f64, Option<&PowerState>) {
        for occ in &self.schedule {
            if t_us >= occ.start_us && t_us < occ.end_us {
                return (occ.state.current_ua, Some(&occ.state));
            }
        }
        (self.config.profile.idle.current_ua, None)
    }

    /// Generate the next `count` samples, with any events that fall among them.
    pub fn next_block(&mut self, count: usize) -> SimBlock {
        let period_us = self.sample_period_us();
        let period_ticks = self.config.period_ticks();
        let ticks_per_us = self.config.timer_hz as f64 / 1e6;
        let t0_us = self.sample_index as f64 * period_us;
        let t0_ticks = (t0_us * ticks_per_us) as u64 as u32;

        let mut block = SimBlock {
            t0_ticks,
            period_ticks,
            samples: Vec::with_capacity(count),
            events: Vec::new(),
        };

        let supply_uv = self.config.profile.supply_uv as f64;
        let esr_ohm = self.config.profile.esr_milliohm / 1000.0;
        let cycle_us = self.config.profile.cycle_us;

        for i in 0..count {
            let t_us = (self.sample_index + i as u64) as f64 * period_us;
            let cycle = (t_us / cycle_us).floor() as i64;
            if cycle != self.schedule_cycle {
                self.build_schedule(cycle);
            }

            // Emit start/stop events at the moment the schedule crosses them. The event
            // timestamp is the scheduled instant, not the sample instant: real firmware
            // signals when it acts, not when the profiler happens to sample.
            self.emit_due_events(t_us, ticks_per_us, &mut block.events);

            let (target_ua, state) = self.target_current_ua(t_us);
            let sigma = state.map_or(self.config.profile.idle.sigma_ua, |s| s.sigma_ua);
            let tau_us = state.map_or(self.config.profile.idle.tau_us, |s| s.tau_us);

            // First-order slew toward the target: a PA ramps, it does not step.
            let alpha = if tau_us > 0.0 {
                1.0 - (-period_us / tau_us).exp()
            } else {
                1.0
            };
            self.slewed_ua += (target_ua - self.slewed_ua) * alpha;

            let noise = self
                .rng
                .normal_with(0.0, sigma.hypot(self.config.noise_ua_rms))
                + self.rng.pink() * self.config.pink_ua;
            let raw_ua = (self.slewed_ua + noise).max(0.0);

            // Quantise to the ADC grid, as any real front end does.
            let lsb = self.config.adc_lsb_ua.max(f64::EPSILON);
            let quantised_ua = (raw_ua / lsb).round() * lsb;

            // The rail droops under load. Without this the voltage channel is a constant and
            // proves nothing.
            let amps = quantised_ua / 1e6;
            let voltage_uv = (supply_uv - amps * esr_ohm * 1e6).max(0.0);

            block
                .samples
                .push((quantised_ua.round() as i32, voltage_uv.round() as u32));
        }

        self.sample_index += count as u64;
        block
    }

    /// Emit any start/stop events whose scheduled instant has been reached.
    fn emit_due_events(&mut self, t_us: f64, ticks_per_us: f64, out: &mut Vec<EventRecord>) {
        for idx in 0..self.schedule.len() {
            let (start_us, end_us, start_id, stop_id) = {
                let occ = &self.schedule[idx];
                (
                    occ.start_us,
                    occ.end_us,
                    occ.state.start_id,
                    occ.state.stop_id,
                )
            };
            if !self.emitted[idx].0 && t_us >= start_us {
                self.emitted[idx].0 = true;
                out.push(Self::event_at(start_us, ticks_per_us, start_id));
            }
            if !self.emitted[idx].1 && t_us >= end_us {
                self.emitted[idx].1 = true;
                out.push(Self::event_at(end_us, ticks_per_us, stop_id));
            }
        }
    }

    fn event_at(t_us: f64, ticks_per_us: f64, id: EventId) -> EventRecord {
        EventRecord {
            timestamp: (t_us * ticks_per_us) as u64 as u32,
            id,
            value: None,
        }
    }

    /// A mutable handle on the RNG, for the server's fault injection.
    pub fn rng_mut(&mut self) -> &mut SimRng {
        &mut self.rng
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use wattson_protocol::ids::{EVENT_RADIO_START, EVENT_RADIO_STOP};

    fn engine(profile: Profile) -> SimEngine {
        SimEngine::new(SimConfig::new(profile))
    }

    /// Golden files depend on this. Same seed, same bytes, always.
    #[test]
    fn generation_is_deterministic() {
        let a = engine(Profile::ble_sensor()).next_block(1000);
        let b = engine(Profile::ble_sensor()).next_block(1000);
        assert_eq!(a.samples, b.samples);
        assert_eq!(a.events, b.events);
    }

    #[test]
    fn a_different_seed_gives_a_different_trace() {
        let mut cfg = SimConfig::new(Profile::ble_sensor());
        cfg.seed = 1;
        let a = SimEngine::new(cfg.clone()).next_block(500);
        cfg.seed = 2;
        let b = SimEngine::new(cfg).next_block(500);
        assert_ne!(a.samples, b.samples);
    }

    #[test]
    fn idle_current_is_near_the_profile_value() {
        let mut e = engine(Profile::ble_sensor());
        // The first active state starts 5% into the 100 ms cycle, i.e. at 5 ms. 200 samples
        // at 50 ksps is 4 ms, so this window is entirely idle.
        let block = e.next_block(200);
        let mean: f64 =
            block.samples.iter().map(|(i, _)| *i as f64).sum::<f64>() / block.samples.len() as f64;
        assert_relative_eq!(mean, 3000.0, max_relative = 0.05);
        assert!(block.events.is_empty(), "no state should have started yet");
    }

    #[test]
    fn the_rail_droops_under_load() {
        let mut e = engine(Profile::ble_sensor());
        let mut min_v = u32::MAX;
        let mut max_v = 0u32;
        let mut peak_i = 0i32;
        for _ in 0..20 {
            let b = e.next_block(1000);
            for &(i, v) in &b.samples {
                min_v = min_v.min(v);
                max_v = max_v.max(v);
                peak_i = peak_i.max(i);
            }
        }
        assert!(
            peak_i > 60_000,
            "the transmit burst never happened, peak was {peak_i} uA"
        );
        assert!(
            max_v - min_v > 5_000,
            "voltage barely moved ({} uV); a constant rail hides every voltage bug",
            max_v - min_v
        );
        // 78 mA through 150 mohm is about 11.7 mV of droop.
        assert!(
            min_v < 3_295_000,
            "expected visible droop, got a minimum of {min_v} uV"
        );
    }

    #[test]
    fn current_is_quantised_to_the_adc_grid() {
        let mut cfg = SimConfig::new(Profile::always_on());
        cfg.adc_lsb_ua = 100.0;
        let block = SimEngine::new(cfg).next_block(500);
        for &(i, _) in &block.samples {
            assert_eq!(i % 100, 0, "sample {i} is not on the 100 uA ADC grid");
        }
    }

    #[test]
    fn state_edges_ramp_rather_than_step() {
        let mut cfg = SimConfig::new(Profile::ble_sensor());
        // No noise, so the ramp is the only thing moving.
        cfg.noise_ua_rms = 0.0;
        cfg.pink_ua = 0.0;
        cfg.adc_lsb_ua = 1.0;
        cfg.profile.states.iter_mut().for_each(|s| s.sigma_ua = 0.0);
        cfg.profile.idle.sigma_ua = 0.0;
        let mut e = SimEngine::new(cfg);

        let mut all = Vec::new();
        for _ in 0..15 {
            all.extend(e.next_block(1000).samples.into_iter().map(|(i, _)| i));
        }
        // Find the steepest single-sample jump into the transmit burst.
        let max_step = all.windows(2).map(|w| (w[1] - w[0]).abs()).max().unwrap();
        let span = all.iter().max().unwrap() - all.iter().min().unwrap();
        assert!(span > 50_000, "no burst in the trace");
        assert!(
            (max_step as f64) < (span as f64) * 0.8,
            "the edge is effectively instantaneous ({max_step} of {span} in one sample); \
             trapezoidal and rectangular integration would be indistinguishable"
        );
    }

    #[test]
    fn events_are_emitted_in_matched_pairs() {
        let mut e = engine(Profile::ble_sensor());
        let mut events = Vec::new();
        // 10 cycles at 100 ms each: 50 ksps * 1 s = 50_000 samples.
        for _ in 0..50 {
            events.extend(e.next_block(1000).events);
        }
        let starts = events
            .iter()
            .filter(|ev| ev.id == EVENT_RADIO_START)
            .count();
        let stops = events.iter().filter(|ev| ev.id == EVENT_RADIO_STOP).count();
        assert!(
            starts >= 8,
            "expected about 10 transmissions in 1 s, saw {starts}"
        );
        assert!(
            starts.abs_diff(stops) <= 1,
            "starts and stops must pair up: {starts} starts, {stops} stops"
        );
    }

    #[test]
    fn event_timestamps_rise_monotonically_and_land_inside_the_trace() {
        let mut e = engine(Profile::ble_sensor());
        let mut last = 0u32;
        let mut seen = 0;
        for _ in 0..50 {
            let b = e.next_block(1000);
            let end = b.t0_ticks + (b.samples.len() as u32) * b.period_ticks;
            for ev in &b.events {
                assert!(ev.timestamp >= last, "event time went backwards");
                assert!(
                    ev.timestamp <= end + b.period_ticks,
                    "event at {} is past the end of its block at {end}",
                    ev.timestamp
                );
                last = ev.timestamp;
                seen += 1;
            }
        }
        assert!(seen > 10, "no events were generated at all");
    }

    /// Without duration jitter, P95 equals the mean and the percentile code is dead weight.
    #[test]
    fn burst_durations_vary_between_occurrences() {
        let mut e = engine(Profile::ble_sensor());
        let mut starts = Vec::new();
        let mut stops = Vec::new();
        for _ in 0..200 {
            for ev in e.next_block(1000).events {
                if ev.id == EVENT_RADIO_START {
                    starts.push(ev.timestamp);
                } else if ev.id == EVENT_RADIO_STOP {
                    stops.push(ev.timestamp);
                }
            }
        }
        let n = starts.len().min(stops.len());
        assert!(n >= 20, "need several bursts, got {n}");
        let durations: Vec<u32> = (0..n).map(|i| stops[i].saturating_sub(starts[i])).collect();
        let distinct: std::collections::BTreeSet<u32> = durations.iter().copied().collect();
        assert!(distinct.len() > 5, "durations barely vary: {distinct:?}");
    }

    #[test]
    fn sample_timestamps_advance_by_exactly_one_period() {
        let mut e = engine(Profile::always_on());
        let block = e.next_block(64);
        let samples = block.to_samples();
        for w in samples.windows(2) {
            assert_eq!(w[1].timestamp - w[0].timestamp, block.period_ticks);
        }
        // The next block continues where this one left off.
        let next = e.next_block(64);
        assert_eq!(
            next.t0_ticks,
            block.t0_ticks + 64 * block.period_ticks,
            "blocks must be contiguous in device time"
        );
    }

    /// The measured burst energy must match the analytic figure, or the simulator is not a
    /// usable reference for testing the analysis engine.
    #[test]
    fn measured_burst_energy_matches_the_analytic_value() {
        let profile = Profile::ble_sensor();
        let mut cfg = SimConfig::new(profile.clone());
        cfg.noise_ua_rms = 0.0;
        cfg.pink_ua = 0.0;
        cfg.adc_lsb_ua = 1.0;
        cfg.clock_ppm = 0.0;
        cfg.profile.states.iter_mut().for_each(|s| {
            s.sigma_ua = 0.0;
            s.jitter_us = 0.0;
        });
        cfg.profile.idle.sigma_ua = 0.0;
        let period_s = 1.0 / cfg.sample_rate_hz as f64;
        let mut e = SimEngine::new(cfg);

        let mut samples = Vec::new();
        let mut events = Vec::new();
        for _ in 0..10 {
            let b = e.next_block(1000);
            let t0 = b.t0_ticks;
            let period = b.period_ticks;
            for (i, &(cur, volt)) in b.samples.iter().enumerate() {
                samples.push((t0 + i as u32 * period, cur, volt));
            }
            events.extend(b.events);
        }

        let start = events
            .iter()
            .find(|ev| ev.id == EVENT_RADIO_START)
            .expect("a TX start");
        let stop = events
            .iter()
            .find(|ev| ev.id == EVENT_RADIO_STOP)
            .expect("a TX stop");

        // Integrate V*I*dt over the burst window, plus the ramp tail on either side.
        let energy_uj: f64 = samples
            .iter()
            .filter(|(t, _, _)| *t >= start.timestamp && *t < stop.timestamp)
            .map(|(_, cur, volt)| (*cur as f64 / 1e6) * (*volt as f64 / 1e6) * period_s * 1e6)
            .sum();

        let analytic = profile.nominal_energy_uj("BLE_TX").unwrap();
        assert_relative_eq!(energy_uj, analytic, max_relative = 0.10);
        assert!(
            energy_uj < 260.0,
            "the documented 260uJ budget must pass on the good profile, measured {energy_uj:.1} uJ"
        );
    }

    #[test]
    fn the_regressed_profile_measures_over_the_budget() {
        let mut cfg = SimConfig::new(Profile::ble_sensor_regressed());
        cfg.noise_ua_rms = 0.0;
        cfg.pink_ua = 0.0;
        cfg.clock_ppm = 0.0;
        cfg.profile.states.iter_mut().for_each(|s| {
            s.sigma_ua = 0.0;
            s.jitter_us = 0.0;
        });
        let period_s = 1.0 / cfg.sample_rate_hz as f64;
        let mut e = SimEngine::new(cfg);

        let mut samples = Vec::new();
        let mut events = Vec::new();
        for _ in 0..10 {
            let b = e.next_block(1000);
            for (i, &(cur, volt)) in b.samples.iter().enumerate() {
                samples.push((b.t0_ticks + i as u32 * b.period_ticks, cur, volt));
            }
            events.extend(b.events);
        }
        let start = events.iter().find(|ev| ev.id == EVENT_RADIO_START).unwrap();
        let stop = events.iter().find(|ev| ev.id == EVENT_RADIO_STOP).unwrap();
        let energy_uj: f64 = samples
            .iter()
            .filter(|(t, _, _)| *t >= start.timestamp && *t < stop.timestamp)
            .map(|(_, cur, volt)| (*cur as f64 / 1e6) * (*volt as f64 / 1e6) * period_s * 1e6)
            .sum();

        assert!(
            energy_uj > 260.0,
            "the regression must fail the documented 260uJ budget, measured {energy_uj:.1} uJ"
        );
    }
}
