//! `wattson gen-header` — turn a metadata file into a C header of event ids.
//!
//! The firmware and the metadata file both name every event id. Generating one from the other
//! is what stops them drifting, and drift here is silent: an event id is opaque on the wire,
//! so a capture that labels the wrong thing looks exactly like a capture that labels the right
//! one.

use std::path::PathBuf;

use anyhow::{Context, Result};
use wattson_core::codegen::{HeaderOptions, c_header, c_identifier};
use wattson_core::metadata::CaptureMetadata;

use crate::cmd::open_output;
use crate::exit;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// The metadata file, as passed to `wattson capture --metadata`.
    pub metadata: PathBuf,

    /// Where to write the header. Standard output if omitted.
    #[arg(long, short)]
    pub out: Option<PathBuf>,

    /// Prefix on every generated macro.
    #[arg(long, default_value = "PP_EVT_")]
    pub prefix: String,

    /// Include guard. Derived from the output file name by default.
    #[arg(long)]
    pub guard: Option<String>,
}

pub fn run(args: &Args) -> Result<i32> {
    let meta = CaptureMetadata::from_toml_file(&args.metadata)
        .with_context(|| format!("could not read metadata {}", args.metadata.display()))?;

    let opts = HeaderOptions {
        prefix: args.prefix.clone(),
        include_guard: args.guard.clone().unwrap_or_else(|| default_guard(args)),
        source: args.metadata.display().to_string(),
    };

    let header = c_header(&meta, &opts)?;
    let mut out = open_output(args.out.as_deref())?;
    std::io::Write::write_all(&mut out, header.as_bytes())?;
    std::io::Write::flush(&mut out)?;

    Ok(exit::SUCCESS)
}

/// `pp_events.h` becomes `PP_EVENTS_H`.
fn default_guard(args: &Args) -> String {
    args.out.as_deref().and_then(|p| p.file_name()).map_or_else(
        || "PP_EVENTS_H".to_string(),
        |n| c_identifier(&n.to_string_lossy()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(out: Option<&str>) -> Args {
        Args {
            metadata: PathBuf::from("events.toml"),
            out: out.map(PathBuf::from),
            prefix: "PP_EVT_".to_string(),
            guard: None,
        }
    }

    #[test]
    fn the_guard_comes_from_the_output_file_name() {
        assert_eq!(default_guard(&args(Some("pp_events.h"))), "PP_EVENTS_H");
        assert_eq!(default_guard(&args(Some("src/my-ids.h"))), "MY_IDS_H");
    }

    #[test]
    fn writing_to_stdout_still_has_a_guard() {
        assert_eq!(default_guard(&args(None)), "PP_EVENTS_H");
    }
}
