//! Device addressing.
//!
//! One string names any source of samples — a real serial device, a TCP simulator, an
//! in-process simulator, or a stored capture replayed as if it were live. That uniformity is
//! what lets every test, the CLI, and the future GUI target the same code path:
//!
//! ```text
//! auto                              first device found
//! serial:COM7                       named serial port, default baud
//! serial:/dev/ttyACM0?baud=921600   named serial port, explicit baud
//! tcp://127.0.0.1:9000              a simulator or a network bridge
//! sim://ble-sensor?rate=50000&seed=42   in-process simulator
//! file://capture.pprof              replay a stored capture
//! ```

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::str::FromStr;

use crate::error::UriError;

/// Default baud for a USB CDC-ACM device, where the value is nominal anyway.
pub const DEFAULT_BAUD: u32 = 921_600;

/// Which simulated workload to generate.
///
/// Lives here rather than in `wattson-sim` so that `core` can parse a `sim://` URI without
/// depending on the simulator crate — the dependency runs the other way.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SimProfile {
    /// A BLE sensor node: idle, periodic sensor read, periodic radio burst.
    BleSensor,
    /// The same node with a regression: the radio burst runs longer.
    BleSensorRegressed,
    /// A LoRa node: long, infrequent, high-current transmissions.
    LoraNode,
    /// A device that never sleeps. Useful as a flat baseline.
    AlwaysOn,
}

impl SimProfile {
    pub const ALL: [SimProfile; 4] = [
        SimProfile::BleSensor,
        SimProfile::BleSensorRegressed,
        SimProfile::LoraNode,
        SimProfile::AlwaysOn,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            SimProfile::BleSensor => "ble-sensor",
            SimProfile::BleSensorRegressed => "ble-sensor-regressed",
            SimProfile::LoraNode => "lora-node",
            SimProfile::AlwaysOn => "always-on",
        }
    }

    pub fn from_name(s: &str) -> Option<SimProfile> {
        SimProfile::ALL.into_iter().find(|p| p.name() == s)
    }
}

impl std::fmt::Display for SimProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SimProfile {
    type Err = UriError;

    fn from_str(s: &str) -> Result<Self, UriError> {
        SimProfile::from_name(s).ok_or_else(|| UriError::UnknownProfile(s.to_string()))
    }
}

/// How to reach a simulated device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SimSpec {
    pub profile: SimProfile,
    pub sample_rate_hz: u32,
    pub seed: u64,
    /// Inject dropped blocks, sequence gaps, and corrupt bytes, to exercise recovery paths.
    pub faults: bool,
}

impl Default for SimSpec {
    fn default() -> Self {
        SimSpec {
            profile: SimProfile::BleSensor,
            sample_rate_hz: 50_000,
            seed: 42,
            faults: false,
        }
    }
}

/// Where samples come from.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DeviceUri {
    /// Pick the first device found. Fails if there are none, or more than one.
    #[default]
    Auto,
    Serial {
        port: String,
        baud: u32,
    },
    Tcp(SocketAddr),
    Sim(SimSpec),
    /// Replay a stored capture as if it were a live device.
    File(PathBuf),
}

/// Split `key=value&key=value` into pairs, tolerating an empty query.
fn query_pairs(q: &str) -> impl Iterator<Item = (&str, &str)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| kv.split_once('='))
}

fn parse_u32(uri: &str, field: &'static str, value: &str) -> Result<u32, UriError> {
    value
        .replace('_', "")
        .parse::<u32>()
        .map_err(|e| UriError::BadField {
            uri: uri.to_string(),
            field,
            detail: e.to_string(),
        })
}

impl DeviceUri {
    /// Parse a device string.
    pub fn parse(s: &str) -> Result<DeviceUri, UriError> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("auto") {
            return Ok(DeviceUri::Auto);
        }

        if let Some(rest) = s.strip_prefix("tcp://") {
            let addr = rest
                .to_socket_addrs()
                .map_err(|e| UriError::BadField {
                    uri: s.to_string(),
                    field: "address",
                    detail: e.to_string(),
                })?
                .next()
                .ok_or_else(|| UriError::BadField {
                    uri: s.to_string(),
                    field: "address",
                    detail: "resolved to no addresses".into(),
                })?;
            return Ok(DeviceUri::Tcp(addr));
        }

        if let Some(rest) = s.strip_prefix("sim://") {
            let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
            let mut spec = SimSpec {
                profile: if name.is_empty() {
                    SimProfile::BleSensor
                } else {
                    SimProfile::from_name(name)
                        .ok_or_else(|| UriError::UnknownProfile(name.to_string()))?
                },
                ..SimSpec::default()
            };
            for (k, v) in query_pairs(query) {
                match k {
                    "rate" => spec.sample_rate_hz = parse_u32(s, "rate", v)?,
                    "seed" => {
                        spec.seed =
                            v.replace('_', "")
                                .parse()
                                .map_err(|e: std::num::ParseIntError| UriError::BadField {
                                    uri: s.to_string(),
                                    field: "seed",
                                    detail: e.to_string(),
                                })?
                    }
                    "faults" => spec.faults = v != "0" && !v.eq_ignore_ascii_case("false"),
                    _ => {
                        return Err(UriError::BadField {
                            uri: s.to_string(),
                            field: "query",
                            detail: format!("unknown parameter {k:?}"),
                        });
                    }
                }
            }
            if spec.sample_rate_hz == 0 {
                return Err(UriError::BadField {
                    uri: s.to_string(),
                    field: "rate",
                    detail: "must be at least 1 Hz".into(),
                });
            }
            return Ok(DeviceUri::Sim(spec));
        }

        if let Some(rest) = s.strip_prefix("file://") {
            return Ok(DeviceUri::File(PathBuf::from(rest)));
        }

        // `serial:` rather than `serial://`, because a Windows port is a bare name (`COM7`)
        // and a Unix one is an absolute path (`/dev/ttyACM0`); neither is an authority.
        if let Some(rest) = s.strip_prefix("serial:") {
            let rest = rest.trim_start_matches("//");
            let (port, query) = rest.split_once('?').unwrap_or((rest, ""));
            if port.is_empty() {
                return Err(UriError::BadField {
                    uri: s.to_string(),
                    field: "port",
                    detail: "no port name".into(),
                });
            }
            let mut baud = DEFAULT_BAUD;
            for (k, v) in query_pairs(query) {
                match k {
                    "baud" => baud = parse_u32(s, "baud", v)?,
                    _ => {
                        return Err(UriError::BadField {
                            uri: s.to_string(),
                            field: "query",
                            detail: format!("unknown parameter {k:?}"),
                        });
                    }
                }
            }
            return Ok(DeviceUri::Serial {
                port: port.to_string(),
                baud,
            });
        }

        // Bare conveniences, because typing the scheme every time is friction: a path that
        // exists and ends in .pprof is a replay; anything that looks like a port is serial.
        if s.ends_with(".pprof") {
            return Ok(DeviceUri::File(PathBuf::from(s)));
        }
        if s.starts_with("COM") || s.starts_with("/dev/") {
            return Ok(DeviceUri::Serial {
                port: s.to_string(),
                baud: DEFAULT_BAUD,
            });
        }

        Err(UriError::Unrecognised(s.to_string()))
    }

    /// A short human label, as shown in `wattson devices` and in capture metadata.
    pub fn label(&self) -> String {
        match self {
            DeviceUri::Auto => "auto".to_string(),
            DeviceUri::Serial { port, baud } => format!("serial:{port}?baud={baud}"),
            DeviceUri::Tcp(addr) => format!("tcp://{addr}"),
            DeviceUri::Sim(spec) => format!(
                "sim://{}?rate={}&seed={}",
                spec.profile, spec.sample_rate_hz, spec.seed
            ),
            DeviceUri::File(path) => format!("file://{}", path.display()),
        }
    }

    /// `true` if this URI names something synthetic, so a capture can be labelled as such.
    /// A synthetic capture must never be mistaken for a measurement of real hardware.
    pub const fn is_synthetic(&self) -> bool {
        matches!(self, DeviceUri::Sim(_))
    }
}

impl FromStr for DeviceUri {
    type Err = UriError;

    fn from_str(s: &str) -> Result<Self, UriError> {
        DeviceUri::parse(s)
    }
}

impl std::fmt::Display for DeviceUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_documented_form() {
        assert_eq!(DeviceUri::parse("auto").unwrap(), DeviceUri::Auto);
        assert_eq!(DeviceUri::parse("").unwrap(), DeviceUri::Auto);
        assert_eq!(
            DeviceUri::parse("serial:COM7").unwrap(),
            DeviceUri::Serial {
                port: "COM7".into(),
                baud: DEFAULT_BAUD
            }
        );
        assert_eq!(
            DeviceUri::parse("serial:/dev/ttyACM0?baud=921600").unwrap(),
            DeviceUri::Serial {
                port: "/dev/ttyACM0".into(),
                baud: 921_600
            }
        );
        assert!(matches!(
            DeviceUri::parse("tcp://127.0.0.1:9000").unwrap(),
            DeviceUri::Tcp(_)
        ));
        assert_eq!(
            DeviceUri::parse("file://cap.pprof").unwrap(),
            DeviceUri::File(PathBuf::from("cap.pprof"))
        );
    }

    #[test]
    fn sim_uri_carries_profile_rate_and_seed() {
        let DeviceUri::Sim(spec) = DeviceUri::parse("sim://ble-sensor?rate=50000&seed=42").unwrap()
        else {
            panic!("expected a sim URI");
        };
        assert_eq!(spec.profile, SimProfile::BleSensor);
        assert_eq!(spec.sample_rate_hz, 50_000);
        assert_eq!(spec.seed, 42);
        assert!(!spec.faults);

        let DeviceUri::Sim(spec) = DeviceUri::parse("sim://lora-node?faults=1").unwrap() else {
            panic!("expected a sim URI");
        };
        assert_eq!(spec.profile, SimProfile::LoraNode);
        assert!(spec.faults);
        assert_eq!(spec.sample_rate_hz, SimSpec::default().sample_rate_hz);
    }

    #[test]
    fn bare_conveniences_work() {
        assert_eq!(
            DeviceUri::parse("cap.pprof").unwrap(),
            DeviceUri::File(PathBuf::from("cap.pprof"))
        );
        assert_eq!(
            DeviceUri::parse("COM3").unwrap(),
            DeviceUri::Serial {
                port: "COM3".into(),
                baud: DEFAULT_BAUD
            }
        );
    }

    #[test]
    fn nonsense_is_rejected_with_the_input_quoted() {
        assert!(matches!(
            DeviceUri::parse("wat"),
            Err(UriError::Unrecognised(_))
        ));
        assert!(matches!(
            DeviceUri::parse("sim://nope"),
            Err(UriError::UnknownProfile(_))
        ));
        assert!(matches!(
            DeviceUri::parse("serial:COM7?baud=fast"),
            Err(UriError::BadField { field: "baud", .. })
        ));
        assert!(matches!(
            DeviceUri::parse("serial:COM7?parity=none"),
            Err(UriError::BadField { field: "query", .. })
        ));
        assert!(matches!(
            DeviceUri::parse("sim://ble-sensor?rate=0"),
            Err(UriError::BadField { field: "rate", .. })
        ));
    }

    /// A label must parse back to the same URI, so it can be written into capture metadata
    /// and used later to reproduce the capture.
    #[test]
    fn labels_round_trip() {
        for text in [
            "auto",
            "serial:COM7?baud=921600",
            "tcp://127.0.0.1:9000",
            "sim://ble-sensor?rate=50000&seed=42",
            "file://cap.pprof",
        ] {
            let uri = DeviceUri::parse(text).unwrap();
            let back = DeviceUri::parse(&uri.label()).unwrap();
            assert_eq!(uri, back, "{text} did not round-trip through its label");
        }
    }

    #[test]
    fn simulated_sources_are_flagged() {
        assert!(DeviceUri::parse("sim://ble-sensor").unwrap().is_synthetic());
        assert!(!DeviceUri::parse("serial:COM7").unwrap().is_synthetic());
    }

    #[test]
    fn profile_names_round_trip() {
        for p in SimProfile::ALL {
            assert_eq!(SimProfile::from_name(p.name()), Some(p));
        }
    }
}
