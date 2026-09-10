//! Subcommands.
//!
//! Each module parses arguments, calls `wattson-core`, and prints. If a function here grows
//! past about thirty lines, or starts doing arithmetic on a measurement, it belongs in the
//! core crate instead — that is what keeps the future desktop app from needing a second
//! implementation of everything.

pub mod analyze;
#[path = "assert.rs"]
pub mod assert_cmd;
pub mod capture;
pub mod completions;
pub mod devices;
pub mod export;
pub mod gen_header;
pub mod info;
#[cfg(feature = "sim")]
pub mod sim;

use std::path::Path;

use anyhow::{Context, Result};
use wattson_core::capture::CaptureReader;
use wattson_core::stats::StatsOptions;
use wattson_core::time::TimeSpan;
use wattson_core::units::Duration as UnitDuration;

/// Open a capture, adding the path to any error.
pub fn open_capture(path: &Path) -> Result<CaptureReader> {
    CaptureReader::open(path).with_context(|| format!("could not open capture {}", path.display()))
}

/// Turn `--from` / `--to` into a span, defaulting to the whole capture.
pub fn resolve_span(
    reader: &CaptureReader,
    from: Option<UnitDuration>,
    to: Option<UnitDuration>,
) -> TimeSpan {
    let full = reader.span();
    let start = from.map_or(full.start_ns, |d| d.as_nanos());
    let end = to.map_or(full.end_ns, |d| d.as_nanos());
    TimeSpan::new(start, end)
}

/// Gap handling, as chosen on the command line.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum GapPolicyArg {
    /// Refuse to compute across missing data.
    Error,
    /// Compute over what exists and report what was skipped.
    #[default]
    Skip,
    /// Interpolate across the gap. Convenient for looking, never for asserting.
    Interpolate,
}

impl From<GapPolicyArg> for wattson_core::capture::GapPolicy {
    fn from(a: GapPolicyArg) -> Self {
        match a {
            GapPolicyArg::Error => wattson_core::capture::GapPolicy::Error,
            GapPolicyArg::Skip => wattson_core::capture::GapPolicy::Skip,
            GapPolicyArg::Interpolate => wattson_core::capture::GapPolicy::Interpolate,
        }
    }
}

/// Build statistics options from the common arguments.
pub fn stats_options(gap: GapPolicyArg, supply_uv: Option<u32>) -> StatsOptions {
    StatsOptions {
        integration: wattson_core::stats::Integration::Trapezoid,
        on_gap: gap.into(),
        supply_uv_override: supply_uv,
    }
}

/// Open an output file, or standard output for `-` and for no path at all.
pub fn open_output(path: Option<&Path>) -> Result<Box<dyn std::io::Write>> {
    match path {
        None => Ok(Box::new(std::io::stdout().lock())),
        Some(p) if p.as_os_str() == "-" => Ok(Box::new(std::io::stdout().lock())),
        Some(p) => {
            let f = std::fs::File::create(p)
                .with_context(|| format!("could not create {}", p.display()))?;
            Ok(Box::new(std::io::BufWriter::new(f)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wattson_core::capture::GapPolicy;

    #[test]
    fn the_default_gap_policy_for_the_cli_is_skip() {
        // `analyze` should show what it can; `assert` overrides this to Error, because a
        // CI gate must not pass over missing data.
        assert_eq!(GapPolicyArg::default(), GapPolicyArg::Skip);
        assert_eq!(GapPolicy::from(GapPolicyArg::default()), GapPolicy::Skip);
    }

    #[test]
    fn every_gap_policy_maps_across() {
        assert_eq!(GapPolicy::from(GapPolicyArg::Error), GapPolicy::Error);
        assert_eq!(GapPolicy::from(GapPolicyArg::Skip), GapPolicy::Skip);
        assert_eq!(
            GapPolicy::from(GapPolicyArg::Interpolate),
            GapPolicy::Interpolate
        );
    }

    #[test]
    fn stats_options_default_to_trapezoid_integration() {
        let o = stats_options(GapPolicyArg::Error, None);
        assert_eq!(o.integration, wattson_core::stats::Integration::Trapezoid);
        assert_eq!(o.on_gap, GapPolicy::Error);
        assert!(o.supply_uv_override.is_none());
    }
}
