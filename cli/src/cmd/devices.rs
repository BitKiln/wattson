//! `wattson devices` — list connected profiler devices.

use anyhow::Result;
use wattson_core::transport::enumerate_devices;

use crate::exit;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,

    /// List every serial port, not only the ones that look like a profiler.
    #[arg(long)]
    pub all: bool,
}

pub fn run(args: &Args) -> Result<i32> {
    // Enumeration is slow on Windows (SetupAPI), which is why core keeps it a plain blocking
    // call the caller schedules rather than hiding a thread inside it.
    let mut found = enumerate_devices()?;
    if !args.all {
        found.retain(|d| d.likely_profiler);
    }

    if args.json {
        let rows: Vec<_> = found
            .iter()
            .map(|d| {
                serde_json::json!({
                    "uri": d.uri.label(),
                    "name": d.name,
                    "vid": d.vid,
                    "pid": d.pid,
                    "serial": d.serial,
                    "description": d.description,
                    "likely_profiler": d.likely_profiler,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(exit::SUCCESS);
    }

    if found.is_empty() {
        // Not an error: no hardware is the normal state, and the tool is designed to be
        // useful without any. Say so, and point at the way in.
        println!("No profiler devices found.");
        if !args.all {
            println!("Run with --all to list every serial port.");
        }
        println!();
        println!("No hardware? Start a synthetic device instead:");
        println!("    wattson sim --listen 127.0.0.1:9000");
        println!(
            "    wattson capture --device tcp://127.0.0.1:9000 --duration 10s --out cap.pprof"
        );
        return Ok(exit::SUCCESS);
    }

    println!("{:<24} {:<10} DESCRIPTION", "DEVICE", "IDS");
    for d in &found {
        let ids = match (d.vid, d.pid) {
            (Some(v), Some(p)) => format!("{v:04x}:{p:04x}"),
            _ => "-".to_string(),
        };
        println!(
            "{:<24} {:<10} {}",
            d.name,
            ids,
            d.description.as_deref().unwrap_or("-")
        );
    }
    Ok(exit::SUCCESS)
}
