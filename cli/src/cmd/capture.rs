//! `wattson capture` — record from a device.
//!
//! # Ctrl-C
//!
//! An interrupt must call `finish()`, not `process::exit`. A long capture killed the wrong way
//! loses its index and footer, and while the reader can recover from that, "recoverable" is a
//! safety net rather than a plan. The handler sets a flag the capture loop checks, so the
//! normal shutdown path runs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail};
use wattson_core::capture::{
    CaptureHeader, CaptureWriter, Compression, HeaderFlags, WriterOptions,
};
use wattson_core::metadata::CaptureMetadata;
use wattson_core::session::{CaptureConfig, Session};
use wattson_core::units::{Duration as UnitDuration, Resistance, SampleRate};
use wattson_core::uri::DeviceUri;

use crate::exit;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Device to capture from: auto, serial:COM7, tcp://host:port, or sim://profile.
    #[arg(long, short, default_value = "auto")]
    pub device: String,

    /// Samples per second.
    #[arg(long, default_value = "50k")]
    pub rate: String,

    /// How long to capture. Omit to run until interrupted.
    #[arg(long)]
    pub duration: Option<String>,

    /// Where to write the capture.
    #[arg(long, short, default_value = "capture.pprof")]
    pub out: PathBuf,

    /// TOML file naming the firmware events this capture will contain.
    #[arg(long)]
    pub metadata: Option<PathBuf>,

    /// Shunt resistance, e.g. 100mohm.
    #[arg(long)]
    pub shunt: Option<String>,

    /// GPIO pins to capture edges on, as a bitmask.
    #[arg(long, value_parser = parse_mask)]
    pub gpio: Option<u32>,

    /// How to compress sample chunks.
    #[arg(long, default_value = "zstd")]
    pub compression: String,

    /// Supply voltage to record when the device has no voltage channel.
    #[arg(long)]
    pub supply: Option<String>,
}

fn parse_mask(s: &str) -> Result<u32, String> {
    let t = s.trim();
    let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16)
    } else {
        t.parse::<u32>()
    };
    parsed.map_err(|e| format!("invalid GPIO mask {s:?}: {e}"))
}

pub fn run(args: &Args, quiet: bool) -> Result<i32> {
    let uri: DeviceUri = args.device.parse()?;
    let rate: SampleRate = args.rate.parse()?;
    let compression: Compression = args
        .compression
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let duration = args
        .duration
        .as_deref()
        .map(str::parse::<UnitDuration>)
        .transpose()?;
    let shunt = args
        .shunt
        .as_deref()
        .map(str::parse::<Resistance>)
        .transpose()?;
    let supply = args
        .supply
        .as_deref()
        .map(str::parse::<wattson_core::units::Voltage>)
        .transpose()?;

    let transport = open_transport(&uri)?;
    let mut session = Session::new(transport);

    let info = session
        .handshake(StdDuration::from_secs(5))
        .with_context(|| format!("no response from {}", uri.label()))?;

    if !quiet {
        eprintln!(
            "Connected: {} fw {} @ {} Hz timer, max {} Hz",
            info.serial_str(),
            info.fw_version_str(),
            info.timer_hz,
            info.max_sample_rate_hz
        );
    }

    let mut header = CaptureHeader {
        created_unix_ns: unix_now_ns(),
        device_timer_hz: info.timer_hz,
        sample_rate_hz: rate.0,
        compression,
        device_serial: info.serial,
        fw_version: info.fw_version,
        fw_build_id: info.fw_build_id,
        shunt_micro_ohm: shunt.map_or(info.shunt_micro_ohm, |r| (r.0 * 1e6) as i32),
        ..Default::default()
    };
    // Synthetic-ness is decided by what the device says it is, not by how the host reached
    // it: a simulator on the other end of a TCP socket is still a simulator, and a capture of
    // one must never be mistaken for a measurement of real hardware.
    let synthetic = info.device_type == wattson_core::protocol::DEVICE_TYPE_SIMULATOR;
    if synthetic {
        header.flags |= HeaderFlags::SYNTHETIC;
    }

    let mut metadata = match &args.metadata {
        Some(p) => CaptureMetadata::from_toml_file(p)
            .with_context(|| format!("could not read metadata {}", p.display()))?,
        // The simulator's event ids are known, so a capture of it stays self-describing
        // without the user having to hand-write a metadata file to try the tool.
        None if synthetic => wattson_core::metadata::default_simulator_metadata(),
        None => CaptureMetadata::default(),
    };
    metadata.invocation = Some(std::env::args().collect::<Vec<_>>().join(" "));
    metadata.device = Some(uri.label());
    metadata.writer_version = Some(wattson_core::VERSION.to_string());
    metadata.host = Some(format!(
        "{} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    metadata.assumed_supply_uv = supply.map(|v| (v.0 * 1e6) as u32);

    let mut writer = CaptureWriter::create(
        &args.out,
        header,
        WriterOptions {
            compression,
            ..Default::default()
        },
    )
    .with_context(|| format!("could not create {}", args.out.display()))?;
    writer.set_metadata(metadata);

    session.configure(&CaptureConfig {
        sample_rate_hz: rate.0,
        gpio_mask: args.gpio.unwrap_or(0),
        shunt_micro_ohm: shunt.map_or(info.shunt_micro_ohm, |r| (r.0 * 1e6) as i32),
        ..Default::default()
    })?;
    session.start()?;

    // Ctrl-C sets a flag rather than exiting, so `finish()` still runs and the capture keeps
    // its index and footer.
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));

    let started = Instant::now();
    let deadline = duration.map(|d| started + d.to_std());
    let mut last_report = Instant::now();

    loop {
        if interrupted.load(Ordering::SeqCst) {
            if !quiet {
                eprintln!("\nInterrupted; finishing the capture cleanly.");
            }
            break;
        }
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            break;
        }

        session.poll(&mut writer)?;

        if !quiet && last_report.elapsed() >= StdDuration::from_millis(250) {
            let elapsed = started.elapsed().as_secs_f64();
            eprint!(
                "\r  {:>8.1} s   {:>12} samples   {:>10.1} ksps",
                elapsed,
                writer.sample_count(),
                writer.sample_count() as f64 / elapsed / 1000.0
            );
            let _ = std::io::Write::flush(&mut std::io::stderr());
            last_report = Instant::now();
        }
    }

    session.stop()?;
    session.drain(StdDuration::from_millis(300), &mut writer)?;

    let decoder = session.decoder_stats();
    let summary = writer.finish()?;

    if !quiet {
        eprintln!();
    }
    println!("Wrote {}", args.out.display());
    println!("  {:<18} {}", "Samples", summary.sample_count);
    println!("  {:<18} {}", "Events", summary.event_count);
    println!("  {:<18} {:.3} s", "Duration", summary.duration_s());
    println!(
        "  {:<18} {:.1} Hz",
        "Effective rate", summary.effective_rate_hz
    );
    println!(
        "  {:<18} {} ({:.1}x)",
        "Size",
        human_bytes(summary.bytes_written),
        summary.compression_ratio()
    );

    // Loss is reported, never hidden: a capture quietly missing data still produces
    // confident-looking numbers downstream.
    if !summary.gaps.is_empty() {
        println!();
        print!("{}", crate::fmt::gap_block(&summary.gaps));
    }
    if decoder.errors() > 0 {
        println!(
            "  ! {} malformed frame(s) were rejected ({} CRC, {} resync)",
            decoder.errors(),
            decoder.crc_errors,
            decoder.resyncs
        );
    }
    if summary.sample_count == 0 {
        bail!("the device produced no samples; check the rate and the connection");
    }

    Ok(exit::SUCCESS)
}

/// Open a transport, resolving `sim://` through the simulator when it is compiled in.
fn open_transport(uri: &DeviceUri) -> Result<Box<dyn wattson_core::transport::Transport>> {
    match uri {
        #[cfg(feature = "sim")]
        DeviceUri::Sim(spec) => crate::cmd::sim::in_process(*spec),
        #[cfg(not(feature = "sim"))]
        DeviceUri::Sim(_) => {
            bail!("this build has no simulator; rebuild with --features sim")
        }
        other => Ok(wattson_core::transport::open(other)?),
    }
}

fn unix_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpio_masks_parse_in_both_bases() {
        assert_eq!(parse_mask("0x0F").unwrap(), 15);
        assert_eq!(parse_mask("15").unwrap(), 15);
        assert!(parse_mask("nope").is_err());
    }

    #[test]
    fn byte_sizes_are_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
