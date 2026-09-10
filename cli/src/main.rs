//! The `wattson` command-line tool.
//!
//! This binary is a presentation shell. Every number it prints is computed by `wattson-core`,
//! which is what lets the future desktop app show the same numbers without a second
//! implementation drifting away from this one. Nothing here does arithmetic on measurements.
//!
//! # Exit codes are contractual
//!
//! ```text
//! 0  success, or all assertions passed
//! 1  an assertion failed
//! 2  usage error
//! 3  I/O or capture-format error
//! 4  capture integrity failure under a strict gap policy
//! ```
//!
//! They are documented in `docs/cli.md` and tested, because a CI pipeline branches on them.

mod cmd;
mod fmt;

use clap::{Parser, Subcommand};

/// Process exit codes. See the module docs: CI pipelines branch on these.
pub mod exit {
    pub const SUCCESS: i32 = 0;
    pub const ASSERTION_FAILED: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const IO: i32 = 3;
    pub const INTEGRITY: i32 = 4;
}

#[derive(Parser, Debug)]
#[command(
    name = "wattson",
    version,
    about = "Open-source firmware-aware power profiling",
    long_about = "Measure what your firmware costs, and assert on it in CI.\n\n\
                  No hardware is needed to try it: `wattson sim` starts a synthetic device \
                  that speaks the real protocol.",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Print less. Suppresses progress output, not results.
    #[arg(long, short, global = true)]
    quiet: bool,

    /// Print more. Repeat for tracing.
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List connected profiler devices.
    Devices(cmd::devices::Args),
    /// Record a capture from a device.
    Capture(cmd::capture::Args),
    /// Summarise a capture file.
    Info(cmd::info::Args),
    /// Compute statistics over a capture.
    Analyze(cmd::analyze::Args),
    /// Write a capture out as CSV or JSON.
    Export(cmd::export::Args),
    /// Check a capture against power budgets. Exits 1 on failure.
    Assert(cmd::assert_cmd::Args),
    /// Generate a C header of event ids from a metadata file.
    #[command(name = "gen-header")]
    GenHeader(cmd::gen_header::Args),
    /// Run a synthetic device, so the tool can be used with no hardware.
    #[cfg(feature = "sim")]
    Sim(cmd::sim::Args),
    /// Print a shell completion script.
    Completions(cmd::completions::Args),
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let code = match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            // The chain matters: "capture I/O failed: The system cannot find the file" is
            // useful; "capture I/O failed" alone is not.
            eprintln!("error: {err}");
            let mut source = std::error::Error::source(&*err);
            while let Some(e) = source {
                eprintln!("  caused by: {e}");
                source = e.source();
            }
            classify(&*err)
        }
    };
    std::process::exit(code);
}

fn run(cli: &Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Command::Devices(a) => cmd::devices::run(a),
        Command::Capture(a) => cmd::capture::run(a, cli.quiet),
        Command::Info(a) => cmd::info::run(a),
        Command::Analyze(a) => cmd::analyze::run(a),
        Command::Export(a) => cmd::export::run(a, cli.quiet),
        Command::Assert(a) => cmd::assert_cmd::run(a),
        Command::GenHeader(a) => cmd::gen_header::run(a),
        #[cfg(feature = "sim")]
        Command::Sim(a) => cmd::sim::run(a, cli.quiet),
        Command::Completions(a) => cmd::completions::run::<Cli>(a),
    }
}

/// Map an error to its documented exit code.
fn classify(err: &(dyn std::error::Error + 'static)) -> i32 {
    use wattson_core::error::{CaptureError, StatsError};

    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        if let Some(s) = e.downcast_ref::<StatsError>() {
            // A span crossing a gap is an integrity failure, not a plain I/O error: the data
            // is missing, and the caller needs to be able to tell those apart in CI.
            if matches!(s, StatsError::GapInSpan { .. }) {
                return exit::INTEGRITY;
            }
        }
        if let Some(c) = e.downcast_ref::<CaptureError>() {
            return match c {
                CaptureError::Truncated { .. } | CaptureError::Corrupt { .. } => exit::INTEGRITY,
                _ => exit::IO,
            };
        }
        current = e.source();
    }
    exit::IO
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::EnvFilter;

    let default = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("WATTSON_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// The exit codes are a contract with CI pipelines, so they are pinned here.
    #[test]
    fn exit_codes_are_what_the_docs_promise() {
        assert_eq!(exit::SUCCESS, 0);
        assert_eq!(exit::ASSERTION_FAILED, 1);
        assert_eq!(exit::USAGE, 2);
        assert_eq!(exit::IO, 3);
        assert_eq!(exit::INTEGRITY, 4);
    }

    #[test]
    fn a_missing_capture_is_an_io_error_not_an_integrity_one() {
        let err: anyhow::Error = wattson_core::error::CaptureError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ))
        .into();
        assert_eq!(classify(err.as_ref()), exit::IO);
    }

    /// Missing data must be distinguishable from a missing file, because a CI pipeline
    /// responds to them differently.
    #[test]
    fn a_gap_in_the_analysed_span_is_an_integrity_failure() {
        let err: anyhow::Error = wattson_core::error::StatsError::GapInSpan {
            count: 2,
            lost_ns: 1_000,
        }
        .into();
        assert_eq!(classify(err.as_ref()), exit::INTEGRITY);
    }

    #[test]
    fn a_corrupt_capture_is_an_integrity_failure() {
        let err: anyhow::Error = wattson_core::error::CaptureError::Corrupt {
            path: "x.pprof".into(),
            what: "file header",
        }
        .into();
        assert_eq!(classify(err.as_ref()), exit::INTEGRITY);
    }
}
