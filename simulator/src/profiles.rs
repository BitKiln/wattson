//! Synthetic workloads.
//!
//! These numbers are chosen so the project's own documented example is meaningful: a BLE TX
//! burst on [`Profile::ble_sensor`] costs about 245 µJ, so
//! `wattson assert --event BLE_TX --max-energy 260uJ` **passes**; the same burst on
//! [`Profile::ble_sensor_regressed`] costs about 296 µJ, so the identical assertion **fails**
//! with a regression report. That pair is what the end-to-end CI test exercises — not merely
//! that the binary runs.

use wattson_protocol::EventId;
use wattson_protocol::ids::{
    EVENT_RADIO_START, EVENT_RADIO_STOP, EVENT_SENSOR_READ_START, EVENT_SENSOR_READ_STOP,
    EVENT_SLEEP_ENTER, EVENT_SLEEP_EXIT,
};

/// One power state the simulated device passes through.
#[derive(Clone, Debug, PartialEq)]
pub struct PowerState {
    /// Name, used to generate the capture's event metadata.
    pub name: &'static str,
    /// Steady-state current in microamps.
    pub current_ua: f64,
    /// Standard deviation of the sample-to-sample noise, in microamps.
    pub sigma_ua: f64,
    /// Mean duration in microseconds.
    pub duration_us: f64,
    /// Standard deviation of the duration, in microseconds.
    ///
    /// Non-zero on purpose: with a fixed duration every occurrence is identical, P95 equals
    /// the mean, and the percentile code is never actually tested by anything.
    pub jitter_us: f64,
    /// Exponential rise time constant in microseconds.
    ///
    /// A radio PA does not reach full draw instantaneously. Modelling the ramp is what makes
    /// the difference between rectangular and trapezoidal integration observable, which is
    /// the only way to know the integrator is right.
    pub tau_us: f64,
    /// Event emitted when this state is entered.
    pub start_id: EventId,
    /// Event emitted when it is left.
    pub stop_id: EventId,
}

/// A repeating schedule of power states.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: &'static str,
    /// The idle state between active states.
    pub idle: PowerState,
    /// Active states, run in order, each separated by a stretch of idle.
    pub states: Vec<PowerState>,
    /// Nominal period of one full cycle, in microseconds. Idle fills the remainder.
    pub cycle_us: f64,
    /// Nominal supply, in microvolts.
    pub supply_uv: u32,
    /// Source resistance in milliohms, so current draw visibly droops the rail.
    ///
    /// A simulator with a constant voltage hides every bug in the voltage path, and there is
    /// a voltage path precisely because energy is `V * I * dt`, not `I * dt`.
    pub esr_milliohm: f64,
}

impl Profile {
    /// Look a profile up by its CLI name.
    pub fn by_name(name: &str) -> Option<Profile> {
        Some(match name {
            "ble-sensor" => Profile::ble_sensor(),
            "ble-sensor-regressed" => Profile::ble_sensor_regressed(),
            "lora-node" => Profile::lora_node(),
            "always-on" => Profile::always_on(),
            _ => return None,
        })
    }

    /// A battery-powered BLE sensor node: sleep, read a sensor, transmit, sleep.
    ///
    /// `E_TX ≈ 3.3 V × 78 mA × 0.95 ms ≈ 245 µJ`.
    pub fn ble_sensor() -> Profile {
        Profile {
            name: "ble-sensor",
            idle: PowerState {
                name: "IDLE",
                current_ua: 3_000.0,
                sigma_ua: 50.0,
                duration_us: 0.0,
                jitter_us: 0.0,
                tau_us: 200.0,
                start_id: EVENT_SLEEP_ENTER,
                stop_id: EVENT_SLEEP_EXIT,
            },
            states: vec![
                PowerState {
                    name: "SENSOR_READ",
                    current_ua: 27_000.0,
                    sigma_ua: 400.0,
                    duration_us: 2_000.0,
                    jitter_us: 60.0,
                    tau_us: 30.0,
                    start_id: EVENT_SENSOR_READ_START,
                    stop_id: EVENT_SENSOR_READ_STOP,
                },
                PowerState {
                    name: "BLE_TX",
                    current_ua: 78_000.0,
                    sigma_ua: 1_500.0,
                    duration_us: 950.0,
                    jitter_us: 30.0,
                    tau_us: 50.0,
                    start_id: EVENT_RADIO_START,
                    stop_id: EVENT_RADIO_STOP,
                },
            ],
            cycle_us: 100_000.0,
            supply_uv: 3_300_000,
            esr_milliohm: 150.0,
        }
    }

    /// The same node after a firmware change stretched the transmission.
    ///
    /// TX runs 1.15 ms instead of 0.95 ms, so `E_TX ≈ 296 µJ` — a 21% regression that a
    /// 260 µJ budget must catch.
    pub fn ble_sensor_regressed() -> Profile {
        let mut p = Profile::ble_sensor();
        p.name = "ble-sensor-regressed";
        if let Some(tx) = p.states.iter_mut().find(|s| s.name == "BLE_TX") {
            tx.duration_us = 1_150.0;
        }
        p
    }

    /// A LoRa node: rare, long, high-current transmissions.
    pub fn lora_node() -> Profile {
        Profile {
            name: "lora-node",
            idle: PowerState {
                name: "IDLE",
                current_ua: 1_200.0,
                sigma_ua: 30.0,
                duration_us: 0.0,
                jitter_us: 0.0,
                tau_us: 300.0,
                start_id: EVENT_SLEEP_ENTER,
                stop_id: EVENT_SLEEP_EXIT,
            },
            states: vec![
                PowerState {
                    name: "SENSOR_READ",
                    current_ua: 18_000.0,
                    sigma_ua: 300.0,
                    duration_us: 5_000.0,
                    jitter_us: 200.0,
                    tau_us: 40.0,
                    start_id: EVENT_SENSOR_READ_START,
                    stop_id: EVENT_SENSOR_READ_STOP,
                },
                PowerState {
                    name: "LORA_TX",
                    current_ua: 120_000.0,
                    sigma_ua: 2_500.0,
                    duration_us: 60_000.0,
                    jitter_us: 1_500.0,
                    tau_us: 150.0,
                    start_id: EVENT_RADIO_START,
                    stop_id: EVENT_RADIO_STOP,
                },
            ],
            cycle_us: 1_000_000.0,
            supply_uv: 3_300_000,
            esr_milliohm: 250.0,
        }
    }

    /// A flat baseline that never sleeps. Useful for checking that statistics over a
    /// featureless trace come out exactly right.
    pub fn always_on() -> Profile {
        Profile {
            name: "always-on",
            idle: PowerState {
                name: "ACTIVE",
                current_ua: 15_000.0,
                sigma_ua: 100.0,
                duration_us: 0.0,
                jitter_us: 0.0,
                tau_us: 100.0,
                start_id: EVENT_SLEEP_ENTER,
                stop_id: EVENT_SLEEP_EXIT,
            },
            states: Vec::new(),
            cycle_us: 100_000.0,
            supply_uv: 3_300_000,
            esr_milliohm: 100.0,
        }
    }

    /// Analytic energy of one occurrence of a named state, in microjoules.
    ///
    /// Ignores the ramp and the ESR droop, so it is an approximation — but a good enough one
    /// to assert that the measured value is within a couple of percent, which catches an
    /// integrator that is wrong by a factor rather than by a rounding error.
    pub fn nominal_energy_uj(&self, state_name: &str) -> Option<f64> {
        let s = self.states.iter().find(|s| s.name == state_name)?;
        let volts = self.supply_uv as f64 / 1e6;
        let amps = s.current_ua / 1e6;
        let seconds = s.duration_us / 1e6;
        Some(volts * amps * seconds * 1e6)
    }

    /// Every event id this profile can emit, paired start-to-stop with its name.
    pub fn event_map(&self) -> Vec<(&'static str, EventId, EventId)> {
        let mut out = vec![(self.idle.name, self.idle.start_id, self.idle.stop_id)];
        out.extend(self.states.iter().map(|s| (s.name, s.start_id, s.stop_id)));
        out
    }

    /// Total active time in one cycle, in microseconds.
    pub fn active_us(&self) -> f64 {
        self.states.iter().map(|s| s.duration_us).sum()
    }

    /// Cycles per second.
    pub fn cycles_per_second(&self) -> f64 {
        1e6 / self.cycle_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    /// The documented example must actually hold, or the README is lying.
    #[test]
    fn ble_tx_costs_about_245_microjoules() {
        let p = Profile::ble_sensor();
        let e = p.nominal_energy_uj("BLE_TX").unwrap();
        assert_relative_eq!(e, 244.6, max_relative = 0.02);
        assert!(
            e < 260.0,
            "the documented 260uJ budget must pass on the good profile"
        );
    }

    /// And the regression must actually fail that same budget, or the CI gate proves nothing.
    #[test]
    fn the_regressed_profile_blows_the_documented_budget() {
        let good = Profile::ble_sensor().nominal_energy_uj("BLE_TX").unwrap();
        let bad = Profile::ble_sensor_regressed()
            .nominal_energy_uj("BLE_TX")
            .unwrap();
        assert!(
            bad > 260.0,
            "regressed BLE_TX was only {bad} uJ; the 260uJ gate would pass"
        );
        let increase = (bad - good) / good;
        assert!(
            increase > 0.15,
            "regression of {:.1}% is too subtle to be a useful test",
            increase * 100.0
        );
    }

    #[test]
    fn only_the_transmission_changed_in_the_regression() {
        let good = Profile::ble_sensor();
        let bad = Profile::ble_sensor_regressed();
        assert_eq!(good.idle, bad.idle);
        assert_relative_eq!(
            good.nominal_energy_uj("SENSOR_READ").unwrap(),
            bad.nominal_energy_uj("SENSOR_READ").unwrap()
        );
    }

    #[test]
    fn every_named_profile_resolves() {
        for name in [
            "ble-sensor",
            "ble-sensor-regressed",
            "lora-node",
            "always-on",
        ] {
            let p = Profile::by_name(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(p.name, name);
        }
        assert!(Profile::by_name("nope").is_none());
    }

    #[test]
    fn active_time_fits_inside_the_cycle() {
        for name in [
            "ble-sensor",
            "ble-sensor-regressed",
            "lora-node",
            "always-on",
        ] {
            let p = Profile::by_name(name).unwrap();
            assert!(
                p.active_us() < p.cycle_us,
                "{name}: active {} us does not fit in a {} us cycle",
                p.active_us(),
                p.cycle_us
            );
        }
    }

    /// Duration jitter is what makes P95 differ from the mean. Without it the percentile
    /// code would never be exercised by any test that uses the simulator.
    #[test]
    fn active_states_have_duration_jitter() {
        for name in ["ble-sensor", "lora-node"] {
            let p = Profile::by_name(name).unwrap();
            for s in &p.states {
                assert!(
                    s.jitter_us > 0.0,
                    "{name}/{} has no duration jitter",
                    s.name
                );
                assert!(s.sigma_ua > 0.0, "{name}/{} has no current noise", s.name);
                assert!(
                    s.tau_us > 0.0,
                    "{name}/{} has an instantaneous edge",
                    s.name
                );
            }
        }
    }

    #[test]
    fn event_ids_are_unique_within_a_profile() {
        for name in ["ble-sensor", "lora-node", "always-on"] {
            let p = Profile::by_name(name).unwrap();
            let mut ids: Vec<EventId> = p
                .event_map()
                .into_iter()
                .flat_map(|(_, a, b)| [a, b])
                .collect();
            ids.sort();
            let before = ids.len();
            ids.dedup();
            assert_eq!(before, ids.len(), "{name} reuses an event id");
        }
    }
}
