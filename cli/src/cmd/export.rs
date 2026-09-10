//! `wattson export` — write a capture out for other tools.

use std::path::PathBuf;

use anyhow::Result;
use wattson_core::export::{
    ExportFormat, area_stats_json, ensure_implemented, event_stats_csv, events_csv, gpio_csv,
    samples_csv, write_json,
};
use wattson_core::stats::{event_stats, region_stats};
use wattson_core::units::Duration as UnitDuration;

use crate::exit;

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum What {
    Samples,
    Events,
    Gpio,
    Stats,
}

#[derive(clap::Args, Debug)]
pub struct Args {
    /// The capture to export.
    pub capture: PathBuf,

    /// Output format.
    #[arg(long, default_value = "csv")]
    pub format: String,

    /// Which part of the capture to write.
    #[arg(long, value_enum, default_value_t = What::Samples)]
    pub what: What,

    /// Keep every Nth sample. A 50 ksps hour is 180 million rows.
    #[arg(long, default_value_t = 1)]
    pub decimate: u32,

    /// Start of the exported region.
    #[arg(long)]
    pub from: Option<String>,

    /// End of the exported region.
    #[arg(long)]
    pub to: Option<String>,

    /// Where to write. Defaults to standard output.
    #[arg(long, short)]
    pub out: Option<PathBuf>,

    /// What to do when the exported span crosses missing data.
    #[arg(long, value_enum, default_value_t = super::GapPolicyArg::Skip)]
    pub on_gap: super::GapPolicyArg,
}

pub fn run(args: &Args, quiet: bool) -> Result<i32> {
    let format: ExportFormat = args
        .format
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    ensure_implemented(format)?;

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
    let options = super::stats_options(args.on_gap, None);

    let mut out = super::open_output(args.out.as_deref())?;

    let rows = match (args.what, format) {
        (What::Samples, ExportFormat::Csv) => {
            samples_csv(&mut reader, span, args.decimate, &mut out)?
        }
        (What::Events, ExportFormat::Csv) => events_csv(&mut reader, &mut out)?,
        (What::Gpio, ExportFormat::Csv) => gpio_csv(&mut reader, &mut out)?,
        (What::Stats, ExportFormat::Csv) => {
            let map = reader.metadata().event_map();
            let stats = event_stats(&mut reader, &map, &options)?;
            event_stats_csv(&stats, &mut out)?
        }
        (What::Samples, ExportFormat::Json) => {
            let samples = reader.samples_in(span)?;
            let step = args.decimate.max(1) as usize;
            let rows: Vec<_> = samples
                .iter()
                .step_by(step)
                .map(|s| {
                    serde_json::json!({
                        "t_s": s.t_ns as f64 / 1e9,
                        "current_uA": s.current_ua,
                        "voltage_uV": s.voltage_uv,
                    })
                })
                .collect();
            let n = rows.len() as u64;
            write_json(&rows, &mut out)?;
            n
        }
        (What::Events, ExportFormat::Json) => {
            let map = reader.metadata().event_map();
            let events = reader.events()?;
            let rows: Vec<_> = events
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "t_s": e.t_ns as f64 / 1e9,
                        "id": format!("0x{:04X}", e.id),
                        "name": map.name_of(e.id),
                        "value": e.value,
                    })
                })
                .collect();
            let n = rows.len() as u64;
            write_json(&rows, &mut out)?;
            n
        }
        (What::Gpio, ExportFormat::Json) => {
            let edges = reader.gpio()?;
            let rows: Vec<_> = edges
                .iter()
                .map(|g| serde_json::json!({ "t_s": g.t_ns as f64 / 1e9, "state": g.state }))
                .collect();
            let n = rows.len() as u64;
            write_json(&rows, &mut out)?;
            n
        }
        (What::Stats, ExportFormat::Json) => {
            let region = region_stats(&mut reader, span, &options).ok();
            let map = reader.metadata().event_map();
            let stats = event_stats(&mut reader, &map, &options)?;
            let n = stats.len() as u64;
            write_json(&area_stats_json(region, &stats), &mut out)?;
            n
        }
        // `ensure_implemented` above has already rejected these.
        (_, ExportFormat::Vcd | ExportFormat::Parquet) => unreachable!("refused above"),
    };

    std::io::Write::flush(&mut out)?;

    if let (false, Some(path)) = (quiet, args.out.as_ref()) {
        eprintln!("Wrote {rows} row(s) to {}", path.display());
    }
    Ok(exit::SUCCESS)
}
