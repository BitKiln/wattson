//! Physical quantities and the parser for their human-written forms.
//!
//! This lives in `core`, not in the CLI, because the desktop app's input fields need exactly
//! the same parser. `--max-energy 260uJ` on the command line and `260uJ` typed into a text
//! box must mean the same thing, forever, without a second implementation drifting away from
//! the first.
//!
//! Everything is stored in a fixed base unit as `f64`, chosen so that captured integer values
//! (µA, µV) convert without loss:
//!
//! | Quantity | Base unit |
//! |---|---|
//! | [`Current`] | ampere |
//! | [`Voltage`] | volt |
//! | [`Energy`] | joule |
//! | [`Charge`] | coulomb |
//! | [`Power`] | watt |
//! | [`Resistance`] | ohm |
//! | [`Duration`] | second |

use std::fmt;
use std::str::FromStr;

use crate::error::UnitError;

/// Metric prefixes accepted on input, largest first so longest-match works naturally.
///
/// Both `u` and `µ` mean micro: nobody wants to type `µ` on a command line, and everybody
/// wants to paste it from a datasheet.
const PREFIXES: &[(&str, f64)] = &[
    ("p", 1e-12),
    ("n", 1e-9),
    ("u", 1e-6),
    ("\u{00B5}", 1e-6), // MICRO SIGN
    ("\u{03BC}", 1e-6), // GREEK SMALL LETTER MU
    ("m", 1e-3),
    ("k", 1e3),
    ("K", 1e3),
    ("M", 1e6),
    ("G", 1e9),
];

/// Parse `<number><prefix><unit>` where the unit suffix is one of `suffixes`.
///
/// The unit suffix is optional when `allow_bare` is set, so `--rate 50k` works where the unit
/// is unambiguous from context.
fn parse_quantity(
    input: &str,
    suffixes: &[&str],
    allow_bare: bool,
    what: &'static str,
) -> Result<f64, UnitError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(UnitError::Empty { what });
    }

    // Split the leading numeric literal from the rest.
    let split = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+' || *c == '_'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num_str, rest) = s.split_at(split);
    let num_str = num_str.replace('_', "");

    // Reject scientific notation: `1e-3A` is ambiguous with the exa prefix and nobody writes
    // it on a command line anyway.
    let value: f64 = num_str.parse().map_err(|_| UnitError::BadNumber {
        what,
        input: input.to_string(),
    })?;
    if !value.is_finite() {
        return Err(UnitError::BadNumber {
            what,
            input: input.to_string(),
        });
    }

    let rest = rest.trim();
    if rest.is_empty() {
        return if allow_bare {
            Ok(value)
        } else {
            Err(UnitError::MissingUnit {
                what,
                input: input.to_string(),
            })
        };
    }

    // Longest matching unit suffix wins, so `mohm` is milli-ohm and not metre-ohm-hour.
    let mut best: Option<(usize, f64)> = None;
    for suffix in suffixes {
        if let Some(prefix_part) = rest.strip_suffix(suffix) {
            let scale = if prefix_part.is_empty() {
                Some(1.0)
            } else {
                PREFIXES
                    .iter()
                    .find(|(p, _)| *p == prefix_part)
                    .map(|(_, m)| *m)
            };
            if let Some(scale) = scale {
                let len = suffix.len();
                if best.is_none_or(|(bl, _)| len > bl) {
                    best = Some((len, scale));
                }
            }
        }
    }

    match best {
        Some((_, scale)) => Ok(value * scale),
        None => Err(UnitError::UnknownUnit {
            what,
            input: input.to_string(),
        }),
    }
}

/// Define a newtype over `f64` in a fixed base unit, with parsing and display.
macro_rules! quantity {
    (
        $(#[$meta:meta])*
        $name:ident, $what:literal, base = $base:literal,
        suffixes = [$($suffix:literal),+ $(,)?],
        bare = $bare:literal,
        display = [$(($limit:literal, $dscale:literal, $dunit:literal)),+ $(,)?]
    ) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Default)]
        pub struct $name(pub f64);

        impl $name {
            #[doc = concat!("The value in ", $base, "s.")]
            #[inline]
            pub const fn value(self) -> f64 {
                self.0
            }
        }

        impl FromStr for $name {
            type Err = UnitError;
            fn from_str(s: &str) -> Result<Self, UnitError> {
                parse_quantity(s, &[$($suffix),+], $bare, $what).map($name)
            }
        }

        impl fmt::Display for $name {
            /// Render with the largest prefix that keeps the mantissa at or above 1.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let v = self.0;
                let a = v.abs();
                $(
                    if a >= $limit {
                        return write!(f, "{:.3} {}", v / $dscale, $dunit);
                    }
                )+
                write!(f, "{v:.3} {}", concat!($base))
            }
        }
    };
}

quantity! {
    /// Electric current, stored in amperes.
    Current, "current", base = "A",
    suffixes = ["A", "a"],
    bare = false,
    display = [(1.0, 1.0, "A"), (1e-3, 1e-3, "mA"), (1e-6, 1e-6, "\u{00B5}A"), (0.0, 1e-9, "nA")]
}

quantity! {
    /// Electric potential, stored in volts.
    Voltage, "voltage", base = "V",
    suffixes = ["V", "v"],
    bare = false,
    display = [(1.0, 1.0, "V"), (1e-3, 1e-3, "mV"), (0.0, 1e-6, "\u{00B5}V")]
}

quantity! {
    /// Energy, stored in joules.
    Energy, "energy", base = "J",
    suffixes = ["J", "j"],
    bare = false,
    display = [(1.0, 1.0, "J"), (1e-3, 1e-3, "mJ"), (1e-6, 1e-6, "\u{00B5}J"), (0.0, 1e-9, "nJ")]
}

quantity! {
    /// Electric charge, stored in coulombs.
    Charge, "charge", base = "C",
    suffixes = ["C"],
    bare = false,
    display = [(1.0, 1.0, "C"), (1e-3, 1e-3, "mC"), (0.0, 1e-6, "\u{00B5}C")]
}

quantity! {
    /// Power, stored in watts.
    Power, "power", base = "W",
    suffixes = ["W", "w"],
    bare = false,
    display = [(1.0, 1.0, "W"), (1e-3, 1e-3, "mW"), (0.0, 1e-6, "\u{00B5}W")]
}

quantity! {
    /// Resistance, stored in ohms. Accepts `ohm`, `ohms`, and `R` as the unit.
    Resistance, "resistance", base = "ohm",
    suffixes = ["ohms", "ohm", "R"],
    bare = false,
    display = [(1.0, 1.0, "ohm"), (0.0, 1e-3, "mohm")]
}

impl Charge {
    /// The same charge expressed in milliamp-hours, which is how battery budgets are written.
    #[inline]
    pub fn as_mah(self) -> f64 {
        self.0 / 3.6
    }

    /// Build a charge from milliamp-hours.
    #[inline]
    pub fn from_mah(mah: f64) -> Charge {
        Charge(mah * 3.6)
    }
}

/// A time interval, stored in seconds.
///
/// Separate from [`std::time::Duration`] because durations here are frequently sub-microsecond
/// and are compared and averaged as floats.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Default)]
pub struct Duration(pub f64);

impl Duration {
    #[inline]
    pub const fn secs(self) -> f64 {
        self.0
    }

    #[inline]
    pub fn as_nanos(self) -> u64 {
        (self.0 * 1e9).round().max(0.0) as u64
    }

    #[inline]
    pub fn from_nanos(ns: u64) -> Duration {
        Duration(ns as f64 / 1e9)
    }

    /// As a `std::time::Duration`, saturating at zero for negative values.
    pub fn to_std(self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.as_nanos())
    }
}

impl FromStr for Duration {
    type Err = UnitError;

    fn from_str(s: &str) -> Result<Self, UnitError> {
        // Longest suffixes first so `ms` is not read as `m` + `s`.
        let t = s.trim();
        for (suffix, scale) in [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("\u{00B5}s", 1e-6),
            ("ms", 1e-3),
            ("min", 60.0),
        ] {
            if let Some(num) = t.strip_suffix(suffix) {
                let v: f64 = num.trim().parse().map_err(|_| UnitError::BadNumber {
                    what: "duration",
                    input: s.to_string(),
                })?;
                return Ok(Duration(v * scale));
            }
        }
        if let Some(num) = t.strip_suffix('s') {
            let v: f64 = num.trim().parse().map_err(|_| UnitError::BadNumber {
                what: "duration",
                input: s.to_string(),
            })?;
            return Ok(Duration(v));
        }
        if let Some(num) = t.strip_suffix('h') {
            let v: f64 = num.trim().parse().map_err(|_| UnitError::BadNumber {
                what: "duration",
                input: s.to_string(),
            })?;
            return Ok(Duration(v * 3600.0));
        }
        // A bare number is seconds: `--duration 30` is unambiguous in context.
        t.parse::<f64>()
            .map(Duration)
            .map_err(|_| UnitError::BadNumber {
                what: "duration",
                input: s.to_string(),
            })
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = self.0.abs();
        if a >= 1.0 {
            write!(f, "{:.3} s", self.0)
        } else if a >= 1e-3 {
            write!(f, "{:.3} ms", self.0 * 1e3)
        } else if a >= 1e-6 {
            write!(f, "{:.3} \u{00B5}s", self.0 * 1e6)
        } else {
            write!(f, "{:.1} ns", self.0 * 1e9)
        }
    }
}

/// A dimensionless ratio, written either as a fraction or as a percentage.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Default)]
pub struct Ratio(pub f64);

impl Ratio {
    #[inline]
    pub fn as_percent(self) -> f64 {
        self.0 * 100.0
    }
}

impl FromStr for Ratio {
    type Err = UnitError;

    fn from_str(s: &str) -> Result<Self, UnitError> {
        let t = s.trim();
        if let Some(num) = t.strip_suffix('%') {
            return num
                .trim()
                .parse::<f64>()
                .map(|v| Ratio(v / 100.0))
                .map_err(|_| UnitError::BadNumber {
                    what: "ratio",
                    input: s.to_string(),
                });
        }
        t.parse::<f64>()
            .map(Ratio)
            .map_err(|_| UnitError::BadNumber {
                what: "ratio",
                input: s.to_string(),
            })
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}%", self.as_percent())
    }
}

/// A sample rate in hertz, accepting `50000`, `50k`, and `50kHz`.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Default)]
pub struct SampleRate(pub u32);

impl FromStr for SampleRate {
    type Err = UnitError;

    fn from_str(s: &str) -> Result<Self, UnitError> {
        let hz = parse_quantity(s, &["Hz", "hz", "HZ", ""], true, "sample rate")?;
        if !(hz.is_finite() && hz >= 1.0 && hz <= u32::MAX as f64) {
            return Err(UnitError::OutOfRange {
                what: "sample rate",
                input: s.to_string(),
            });
        }
        Ok(SampleRate(hz.round() as u32))
    }
}

impl fmt::Display for SampleRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= 1000 && self.0 % 1000 == 0 {
            write!(f, "{} kHz", self.0 / 1000)
        } else {
            write!(f, "{} Hz", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn parses_the_forms_the_cli_documents() {
        assert_relative_eq!(
            "260uJ".parse::<Energy>().unwrap().0,
            260e-6,
            max_relative = 1e-12
        );
        assert_relative_eq!(
            "1.5mA".parse::<Current>().unwrap().0,
            1.5e-3,
            max_relative = 1e-12
        );
        assert_relative_eq!(
            "100mohm".parse::<Resistance>().unwrap().0,
            0.1,
            max_relative = 1e-12
        );
        assert_relative_eq!(
            "10s".parse::<Duration>().unwrap().0,
            10.0,
            max_relative = 1e-12
        );
        assert_relative_eq!(
            "1.2ms".parse::<Duration>().unwrap().0,
            1.2e-3,
            max_relative = 1e-12
        );
        assert_eq!("50k".parse::<SampleRate>().unwrap(), SampleRate(50_000));
        assert_relative_eq!("5%".parse::<Ratio>().unwrap().0, 0.05, max_relative = 1e-12);
    }

    /// Nobody types a micro sign on a command line; everybody pastes one from a datasheet.
    #[test]
    fn micro_sign_and_ascii_u_are_the_same_thing() {
        let a = "260uJ".parse::<Energy>().unwrap();
        let b = "260\u{00B5}J".parse::<Energy>().unwrap();
        let c = "260\u{03BC}J".parse::<Energy>().unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(
            "500us".parse::<Duration>().unwrap(),
            "500\u{00B5}s".parse().unwrap()
        );
    }

    #[test]
    fn every_prefix_scales_correctly() {
        for (text, want) in [
            ("1pA", 1e-12),
            ("1nA", 1e-9),
            ("1uA", 1e-6),
            ("1mA", 1e-3),
            ("1A", 1.0),
            ("1kA", 1e3),
            ("1MA", 1e6),
            ("1GA", 1e9),
        ] {
            assert_relative_eq!(
                text.parse::<Current>().unwrap().0,
                want,
                max_relative = 1e-12,
                epsilon = 1e-18
            );
        }
    }

    #[test]
    fn whitespace_and_underscores_are_tolerated() {
        assert_eq!("  260uJ  ".parse::<Energy>().unwrap(), Energy(260e-6));
        assert_eq!("50_000".parse::<SampleRate>().unwrap(), SampleRate(50_000));
    }

    #[test]
    fn negative_current_parses_because_current_can_flow_backwards() {
        assert_relative_eq!(
            "-2.5mA".parse::<Current>().unwrap().0,
            -2.5e-3,
            max_relative = 1e-12
        );
    }

    #[test]
    fn missing_or_unknown_units_are_rejected_with_the_input_quoted() {
        assert!(matches!(
            "260".parse::<Energy>(),
            Err(UnitError::MissingUnit { .. })
        ));
        assert!(matches!(
            "260xJ".parse::<Energy>(),
            Err(UnitError::UnknownUnit { .. })
        ));
        assert!(matches!(
            "abc".parse::<Energy>(),
            Err(UnitError::BadNumber { .. })
        ));
        assert!(matches!("".parse::<Energy>(), Err(UnitError::Empty { .. })));
        assert!(matches!(
            "0Hz".parse::<SampleRate>(),
            Err(UnitError::OutOfRange { .. })
        ));
    }

    /// `ms` must not parse as milli + `s`... and `mohm` must not parse as anything but
    /// milliohm. Longest-suffix-first is what makes both true.
    #[test]
    fn longest_suffix_wins() {
        assert_relative_eq!(
            "5ms".parse::<Duration>().unwrap().0,
            5e-3,
            max_relative = 1e-12
        );
        assert_relative_eq!(
            "5min".parse::<Duration>().unwrap().0,
            300.0,
            max_relative = 1e-12
        );
        assert_relative_eq!(
            "2ohms".parse::<Resistance>().unwrap().0,
            2.0,
            max_relative = 1e-12
        );
    }

    #[test]
    fn display_picks_a_readable_prefix() {
        assert_eq!(Current(0.0782).to_string(), "78.200 mA");
        assert_eq!(Current(3.1e-6).to_string(), "3.100 \u{00B5}A");
        assert_eq!(Energy(245e-6).to_string(), "245.000 \u{00B5}J");
        assert_eq!(Duration(9.5e-4).to_string(), "950.000 \u{00B5}s");
        assert_eq!(SampleRate(50_000).to_string(), "50 kHz");
        assert_eq!(Ratio(0.1045).to_string(), "10.45%");
    }

    /// Display and parse must round-trip, or a value copied out of a report cannot be pasted
    /// back into an assertion.
    #[test]
    fn display_output_parses_back() {
        for e in [Energy(245e-6), Energy(2.3e-3), Energy(1.5)] {
            let text = e.to_string().replace(' ', "");
            let back: Energy = text.parse().expect(&text);
            assert_relative_eq!(back.0, e.0, max_relative = 1e-3);
        }
    }

    #[test]
    fn charge_converts_to_the_unit_battery_budgets_use() {
        // 1 mAh is 3.6 coulombs.
        assert_relative_eq!(Charge::from_mah(1.0).0, 3.6, max_relative = 1e-12);
        assert_relative_eq!(Charge(3.6).as_mah(), 1.0, max_relative = 1e-12);
    }
}
