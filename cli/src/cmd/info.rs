//! `wattson info` — what is in this capture file.

use std::path::PathBuf;

use anyhow::Result;
use wattson_core::export::{CaptureInfo, write_json};

use crate::exit;
use crate::fmt;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// The capture to inspect.
    pub capture: PathBuf,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &Args) -> Result<i32> {
    let mut reader = super::open_capture(&args.capture)?;
    let info = CaptureInfo::gather(&mut reader)?;

    if args.json {
        write_json(&info, std::io::stdout().lock())?;
        return Ok(exit::SUCCESS);
    }

    println!("{}", info.path);
    println!(
        "{}",
        fmt::row("Format", format!("v{}", info.format_version))
    );
    println!("{}", fmt::row("Device", &info.device_serial));
    println!("{}", fmt::row("Firmware", &info.firmware_version));
    println!("{}", fmt::row("Timer", format!("{} Hz", info.timer_hz)));
    println!("{}", fmt::row("Samples", info.sample_count.to_string()));
    println!("{}", fmt::row("Events", info.event_count.to_string()));
    if info.gpio_count > 0 {
        println!("{}", fmt::row("GPIO edges", info.gpio_count.to_string()));
    }
    println!("{}", fmt::row("Duration", fmt::duration_s(info.duration_s)));

    // Both rates are shown, because they differ and only one is safe to integrate with. A
    // large divergence is the first sign that a capture is missing data.
    println!(
        "{}",
        fmt::row("Configured rate", format!("{} Hz", info.configured_rate_hz))
    );
    println!(
        "{}",
        fmt::row(
            "Effective rate",
            format!("{:.1} Hz", info.effective_rate_hz)
        )
    );
    println!(
        "{}",
        fmt::row(
            "Voltage channel",
            if info.has_voltage { "yes" } else { "no" }
        )
    );
    println!("{}", fmt::row("Compression", &info.compression));

    let chunks = reader.chunk_counts();
    if !chunks.is_empty() {
        let text: Vec<String> = chunks
            .iter()
            .map(|(k, n)| format!("{n}x {}", chunk_name(*k)))
            .collect();
        println!("{}", fmt::row("Chunks", text.join(", ")));
    }

    let events = reader.metadata().events.len();
    if events > 0 {
        let names: Vec<&str> = reader
            .metadata()
            .events
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        println!("{}", fmt::row("Declared events", names.join(", ")));
    } else {
        println!(
            "{}",
            fmt::row("Declared events", "none — event names will show as raw ids")
        );
    }

    // Everything below qualifies the numbers above, so it is printed after them rather than
    // buried in a header.
    let mut warned = false;
    if !info.finalized {
        println!("\n  ! This capture was never finalized: the writer did not complete.");
        warned = true;
    }
    if info.recovered {
        println!("  ! The index was rebuilt by scanning; the final chunk may be missing.");
        warned = true;
    }
    if info.corrupt_chunks > 0 {
        println!(
            "  ! {} chunk(s) failed their CRC and were skipped.",
            info.corrupt_chunks
        );
        warned = true;
    }
    if info.unknown_chunks > 0 {
        println!(
            "  ! {} chunk(s) of a kind this build does not know were skipped; \
             the file may come from a newer version.",
            info.unknown_chunks
        );
        warned = true;
    }
    if !info.gaps.is_empty() {
        print!("{}", fmt::gap_block(&info.gaps));
        warned = true;
    }
    if warned {
        println!("  Statistics over this capture will be incomplete.");
    }

    Ok(exit::SUCCESS)
}

fn chunk_name(k: wattson_core::capture::ChunkKind) -> String {
    use wattson_core::capture::ChunkKind as K;
    match k {
        K::Samples => "samples".into(),
        K::Events => "events".into(),
        K::Gpio => "gpio".into(),
        K::Markers => "markers".into(),
        K::Sync => "sync".into(),
        K::Metadata => "metadata".into(),
        K::Annotations => "annotations".into(),
        K::Summary => "summary".into(),
        K::Index => "index".into(),
        K::End => "end".into(),
        K::Unknown(v) => format!("unknown(0x{v:04X})"),
    }
}
