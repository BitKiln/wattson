//! `wattson analyze` — what a capture, or part of it, cost.

use std::path::PathBuf;

use anyhow::Result;
use wattson_core::export::{AnalysisReport, CaptureInfo, write_json};
use wattson_core::stats::{event_stats, region_stats};
use wattson_core::units::{Duration as UnitDuration, Voltage};

use crate::exit;
use crate::fmt;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// The capture to analyse.
    pub capture: PathBuf,

    /// Show per-event statistics. Implied when neither --events nor --regions is given.
    #[arg(long)]
    pub events: bool,

    /// Show statistics over the selected region.
    #[arg(long)]
    pub regions: bool,

    /// Start of the region, e.g. 1.5s.
    #[arg(long)]
    pub from: Option<String>,

    /// End of the region, e.g. 3s.
    #[arg(long)]
    pub to: Option<String>,

    /// Show only the N events that cost the most in total.
    #[arg(long)]
    pub top: Option<usize>,

    /// What to do when the analysed span crosses missing data.
    #[arg(long, value_enum, default_value_t = super::GapPolicyArg::Skip)]
    pub on_gap: super::GapPolicyArg,

    /// Supply voltage to assume when the capture has no voltage channel.
    #[arg(long)]
    pub supply: Option<String>,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,

    /// Draw an ASCII overview of the current trace.
    #[arg(long)]
    pub plot: bool,
}

pub fn run(args: &Args) -> Result<i32> {
    let mut reader = super::open_capture(&args.capture)?;

    let from = args
        .from
        .as_deref()
        .map(str::parse::<UnitDuration>)
        .transpose()?;
    let to = args
        .to
        .as_deref()
        .map(str::parse::<UnitDuration>)
        .transpose()?;
    let span = super::resolve_span(&reader, from, to);

    let supply = args
        .supply
        .as_deref()
        .map(str::parse::<Voltage>)
        .transpose()?
        .map(|v| (v.0 * 1e6) as u32);
    let options = super::stats_options(args.on_gap, supply);

    // Default to both views: someone who typed `analyze cap.pprof` wants to know what is in
    // it, not to be asked which of two flags they meant.
    let show_events = args.events || !args.regions;
    let show_region = args.regions || !args.events;

    let region = if show_region {
        Some(region_stats(&mut reader, span, &options)?)
    } else {
        None
    };

    let map = reader.metadata().event_map();
    let mut events = if show_events && !map.is_empty() {
        event_stats(&mut reader, &map, &options)?
    } else {
        Vec::new()
    };
    if let Some(n) = args.top {
        events.truncate(n);
    }

    if args.json {
        let report = AnalysisReport {
            capture: CaptureInfo::gather(&mut reader)?,
            region,
            events,
        };
        write_json(&report, std::io::stdout().lock())?;
        return Ok(exit::SUCCESS);
    }

    if args.plot {
        let width = 72;
        let buckets = reader.downsample(span, width)?;
        if !buckets.is_empty() {
            let peak = buckets.iter().map(|b| b.max_ua).max().unwrap_or(0);
            println!("  {}", fmt::sparkline(&buckets));
            println!(
                "  0 s{:>width$}",
                format!(
                    "{}   peak {}",
                    fmt::duration_s(span.duration_s()),
                    fmt::current(peak as f64)
                ),
                width = width.saturating_sub(5)
            );
            println!();
        }
    }

    if let Some(r) = &region {
        println!("Selection: {}", fmt::duration_s(r.duration_s));
        println!("{}", fmt::region_block(r));

        // A battery figure is the question most of this is asked in service of.
        if let Some(hours) = r.battery_hours(220.0) {
            println!(
                "{}",
                fmt::row("Battery life", format!("{hours:.1} h on a 220 mAh cell"))
            );
        }
        println!();
    }

    if show_events {
        if map.is_empty() {
            println!(
                "No event metadata in this capture, so there is nothing to attribute energy to."
            );
            println!(
                "Capture with --metadata events.toml to name the firmware events, or see \
                 docs/instrumentation.md."
            );
        } else if events.is_empty() {
            println!("No declared events occurred in this capture.");
        } else {
            println!("Events, most expensive first:");
            for e in &events {
                println!();
                println!("{}", fmt::event_block(e));
            }
        }
    }

    let gaps = reader.gaps();
    if !gaps.is_empty() {
        println!();
        print!("{}", fmt::gap_block(gaps));
        println!("  These figures exclude the missing intervals.");
    }

    Ok(exit::SUCCESS)
}
